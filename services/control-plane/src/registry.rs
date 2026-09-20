use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use tokio::sync::{Mutex, Semaphore};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    artifacts::JobOutputRequirement,
    credentials::{self, CredentialKind, IssuedCredential},
};

const HEARTBEAT_INTERVAL_SECONDS: u32 = 30;
const REGISTRATION_TTL_MINUTES: i64 = 15;
const REGISTRATION_POLL_SECONDS: u32 = 3;
const MAX_OPEN_REGISTRATIONS: i64 = 1_000;
const MAX_VERIFICATIONS_PER_MINUTE: usize = 10;
const MAX_CONCURRENT_VERIFICATIONS: usize = 4;

#[derive(Clone)]
pub(crate) struct VerificationGate {
    attempts: Arc<Mutex<HashMap<Uuid, VecDeque<Instant>>>>,
    permits: Arc<Semaphore>,
}

impl Default for VerificationGate {
    fn default() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_VERIFICATIONS)),
        }
    }
}

impl VerificationGate {
    async fn verify(
        &self,
        identifier: Uuid,
        supplied: String,
        verifier: String,
    ) -> Result<bool, ApiError> {
        let now = Instant::now();
        {
            let mut attempts = self.attempts.lock().await;
            let recent = attempts.entry(identifier).or_default();
            while recent
                .front()
                .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
            {
                recent.pop_front();
            }
            if recent.len() >= MAX_VERIFICATIONS_PER_MINUTE {
                return Err(ApiError::too_many_requests());
            }
            recent.push_back(now);
        }

        let permit = Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| ApiError::too_many_requests())?;
        let authenticated =
            tokio::task::spawn_blocking(move || credentials::verify(&supplied, &verifier))
                .await
                .map_err(|_| ApiError::internal())?;
        drop(permit);
        Ok(authenticated)
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GpuHealthStatus {
    Unavailable,
    Unverified,
    Healthy,
    Unhealthy,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GpuCapability {
    pub index: u32,
    pub name: String,
    pub memory_total_bytes: u64,
    pub driver_version: String,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GpuHealthEvidence {
    pub schema_version: String,
    pub status: GpuHealthStatus,
    pub checked_at: DateTime<Utc>,
    pub image_reference: String,
    pub device_index: Option<u32>,
    pub device_name: Option<String>,
    pub operation: Option<String>,
    pub matrix_size: Option<u32>,
    pub max_absolute_error: Option<f64>,
    pub duration_ms: Option<f64>,
    pub cuda_driver_api_version: Option<String>,
    pub cuda_runtime_version: Option<String>,
    pub error_type: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GpuHealth {
    pub status: GpuHealthStatus,
    pub detail: String,
    pub evidence: Option<GpuHealthEvidence>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerCapabilities {
    pub protocol_version: String,
    pub collected_at: DateTime<Utc>,
    pub hostname: String,
    pub operating_system: String,
    pub operating_system_version: String,
    pub architecture: String,
    pub logical_cpu_count: u32,
    pub memory_total_bytes: u64,
    pub storage_available_bytes: u64,
    pub python_version: String,
    pub gpus: Vec<GpuCapability>,
    pub gpu_health: GpuHealth,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrolmentRequest {
    pub protocol_version: String,
    pub agent_instance_id: Uuid,
    pub display_name: String,
    pub capabilities: WorkerCapabilities,
}

#[derive(Serialize, ToSchema)]
pub struct EnrolmentResponse {
    pub worker_id: Uuid,
    pub worker_credential: String,
    pub state: WorkerState,
    pub heartbeat_interval_seconds: u32,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistrationRequest {
    pub protocol_version: String,
    pub agent_instance_id: Uuid,
    pub display_name: String,
    /// URL-safe base64 encoding of the raw 32-byte Ed25519 public key.
    pub public_key: String,
    pub capabilities: WorkerCapabilities,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegistrationCreatedResponse {
    pub registration_id: Uuid,
    pub confirmation_code: String,
    pub expires_at: DateTime<Utc>,
    pub poll_interval_seconds: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationState {
    Pending,
    Approved,
    Rejected,
    Expired,
    Claimed,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegistrationStatusResponse {
    pub registration_id: Uuid,
    pub state: RegistrationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_challenge: Option<String>,
    pub poll_interval_seconds: u32,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimRegistrationRequest {
    /// URL-safe base64 Ed25519 signature over the documented claim message.
    pub signature: String,
}

#[derive(FromRow)]
struct RegistrationRecord {
    agent_instance_id: Uuid,
    display_name: String,
    protocol_version: String,
    capabilities: serde_json::Value,
    public_key: Vec<u8>,
    claim_challenge: Option<Vec<u8>>,
    expires_at: DateTime<Utc>,
    approved_at: Option<DateTime<Utc>>,
    approved_by_identity_id: Option<Uuid>,
    rejected_at: Option<DateTime<Utc>>,
    claimed_at: Option<DateTime<Utc>>,
    worker_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatRequest {
    pub protocol_version: String,
    pub sequence: i64,
    pub observed_at: DateTime<Utc>,
    pub capabilities: WorkerCapabilities,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HeartbeatResponse {
    pub worker_id: Uuid,
    pub state: WorkerState,
    pub accepted_sequence: i64,
    pub next_heartbeat_seconds: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment: Option<JobAssignment>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct JobAssignment {
    pub attempt_id: Uuid,
    pub job_id: Uuid,
    pub name: String,
    pub image_reference: String,
    pub gpu_index: u32,
    pub timeout_seconds: u32,
    pub lease_expires_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output_requirements: Vec<JobOutputRequirement>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobResultRequest {
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub failure_message: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct JobResultResponse {
    pub attempt_id: Uuid,
    pub job_id: Uuid,
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Unapproved,
    Idle,
    Busy,
    Draining,
    Quarantined,
}

impl WorkerState {
    fn parse(value: &str) -> Result<Self, ApiError> {
        match value {
            "unapproved" => Ok(Self::Unapproved),
            "idle" => Ok(Self::Idle),
            "busy" => Ok(Self::Busy),
            "draining" => Ok(Self::Draining),
            "quarantined" => Ok(Self::Quarantined),
            "revoked" => Err(ApiError::unauthorized()),
            _ => Err(ApiError::internal()),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    body: ErrorResponse,
}

impl ApiError {
    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            body: ErrorResponse { code, message },
        }
    }

    const fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "The credential is invalid or unavailable.",
        )
    }

    pub(crate) const fn invalid_request() -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            "The request is invalid.",
        )
    }

    const fn unsupported_protocol() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "unsupported_protocol",
            "Protocol major version 1 is required.",
        )
    }

    pub(crate) const fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "persistence_unavailable",
            "Worker registration is unavailable.",
        )
    }

    pub(crate) const fn conflict(code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    const fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "registration_not_found",
            "The registration request was not found.",
        )
    }

    const fn too_many_requests() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "verification_limited",
            "Credential verification is temporarily limited.",
        )
    }

    pub(crate) const fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "The request could not be completed.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[derive(FromRow)]
struct EnrolmentRecord {
    owner_identity_id: Uuid,
    token_verifier: String,
    expires_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct WorkerAuthenticationRecord {
    token_verifier: String,
    expires_at: Option<DateTime<Utc>>,
    credential_revoked_at: Option<DateTime<Utc>>,
    status: String,
}

#[derive(FromRow)]
struct HeartbeatAuthenticationRecord {
    token_verifier: String,
    expires_at: Option<DateTime<Utc>>,
    credential_revoked_at: Option<DateTime<Utc>>,
    status: String,
    heartbeat_sequence: i64,
    protocol_version: String,
    capabilities: serde_json::Value,
}

#[derive(FromRow)]
struct AssignmentRecord {
    attempt_id: Uuid,
    job_id: Uuid,
    name: String,
    image_reference: String,
    timeout_seconds: i32,
    lease_expires_at: DateTime<Utc>,
}

struct ExpectedHeartbeat<'a> {
    sequence: i64,
    protocol_version: &'a str,
    capabilities: &'a serde_json::Value,
}

impl TryFrom<AssignmentRecord> for JobAssignment {
    type Error = ApiError;

    fn try_from(record: AssignmentRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            attempt_id: record.attempt_id,
            job_id: record.job_id,
            name: record.name,
            image_reference: record.image_reference,
            gpu_index: 0,
            timeout_seconds: u32::try_from(record.timeout_seconds)
                .map_err(|_| ApiError::internal())?,
            lease_expires_at: record.lease_expires_at,
            output_requirements: Vec::new(),
        })
    }
}

async fn load_output_requirements(
    pool: &PgPool,
    job_id: Uuid,
) -> Result<Vec<JobOutputRequirement>, ApiError> {
    let rows = sqlx::query_as::<_, (String, String, String, bool, i64)>(
        "SELECT logical_path, role, media_type, mandatory, max_bytes \
         FROM job_output_requirements WHERE job_id = $1 ORDER BY logical_path",
    )
    .bind(job_id)
    .fetch_all(pool)
    .await
    .map_err(|error| database_error(&error, "load job output contract"))?;
    rows.into_iter()
        .map(|(logical_path, role, media_type, mandatory, max_bytes)| {
            Ok(JobOutputRequirement {
                logical_path,
                role,
                media_type,
                mandatory,
                max_bytes: u64::try_from(max_bytes).map_err(|_| ApiError::internal())?,
            })
        })
        .collect()
}

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or_else(ApiError::unauthorized)
}

pub(crate) fn validate_protocol(version: &str) -> Result<(), ApiError> {
    if version.split_once('.').is_some_and(|(major, minor)| {
        major == "1" && !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(())
    } else {
        Err(ApiError::unsupported_protocol())
    }
}

pub(crate) async fn authenticate_worker(
    state: &AppState,
    headers: &HeaderMap,
    worker_id: Uuid,
    operation: &'static str,
) -> Result<(), ApiError> {
    let pool = database(state)?;
    let supplied = bearer(headers)?.to_owned();
    let credential_id = credentials::identifier(CredentialKind::Worker, &supplied)
        .map_err(|_| ApiError::unauthorized())?;
    let authentication = sqlx::query_as::<_, WorkerAuthenticationRecord>(
        "SELECT c.token_verifier, c.expires_at, c.revoked_at AS credential_revoked_at, w.status \
         FROM worker_credentials c JOIN workers w ON w.id = c.worker_id \
         WHERE c.id = $1 AND c.worker_id = $2",
    )
    .bind(credential_id)
    .bind(worker_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error, operation))?
    .ok_or_else(ApiError::unauthorized)?;
    if authentication.credential_revoked_at.is_some()
        || authentication
            .expires_at
            .is_some_and(|expires| expires <= Utc::now())
        || authentication.status == "revoked"
        || !state
            .verification_gate
            .verify(credential_id, supplied, authentication.token_verifier)
            .await?
    {
        return Err(ApiError::unauthorized());
    }
    Ok(())
}

fn validate_capabilities(capabilities: &WorkerCapabilities) -> Result<(), ApiError> {
    validate_protocol(&capabilities.protocol_version)?;
    let strings_valid = [
        capabilities.hostname.as_str(),
        capabilities.operating_system.as_str(),
        capabilities.architecture.as_str(),
        capabilities.python_version.as_str(),
        capabilities.gpu_health.detail.as_str(),
    ]
    .iter()
    .all(|value| !value.trim().is_empty());
    let gpus_valid = capabilities.gpus.iter().all(|gpu| {
        gpu.memory_total_bytes > 0
            && !gpu.name.trim().is_empty()
            && !gpu.driver_version.trim().is_empty()
    });
    if strings_valid
        && gpus_valid
        && valid_gpu_health(&capabilities.gpu_health)
        && capabilities.logical_cpu_count > 0
        && capabilities.memory_total_bytes > 0
    {
        Ok(())
    } else {
        Err(ApiError::invalid_request())
    }
}

fn valid_gpu_health(health: &GpuHealth) -> bool {
    match (&health.status, &health.evidence) {
        (GpuHealthStatus::Unavailable | GpuHealthStatus::Unverified, None) => true,
        (GpuHealthStatus::Healthy, Some(evidence)) => {
            evidence.status == health.status
                && validate_protocol(&evidence.schema_version).is_ok()
                && immutable_sha256_reference(&evidence.image_reference)
                && evidence.device_index.is_some()
                && evidence
                    .device_name
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty())
                && evidence
                    .operation
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty())
                && evidence.matrix_size.is_some_and(|value| value > 0)
                && evidence.max_absolute_error.is_some_and(f64::is_finite)
                && evidence
                    .duration_ms
                    .is_some_and(|value| value.is_finite() && value >= 0.0)
                && evidence
                    .cuda_driver_api_version
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty())
                && evidence
                    .cuda_runtime_version
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty())
        }
        (GpuHealthStatus::Unhealthy, Some(evidence)) => {
            evidence.status == health.status
                && validate_protocol(&evidence.schema_version).is_ok()
                && immutable_sha256_reference(&evidence.image_reference)
                && evidence
                    .error_type
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty())
                && evidence
                    .detail
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty())
        }
        _ => false,
    }
}

pub(crate) fn immutable_sha256_reference(reference: &str) -> bool {
    let digest = reference.strip_prefix("sha256:").or_else(|| {
        reference
            .split_once("@sha256:")
            .filter(|(name, _)| {
                !name.is_empty() && !name.contains('@') && !name.contains(char::is_whitespace)
            })
            .map(|(_, digest)| digest)
    });
    digest.is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn database(state: &AppState) -> Result<&PgPool, ApiError> {
    state.database.as_ref().ok_or_else(ApiError::unavailable)
}

fn database_error(error: &sqlx::Error, operation: &'static str) -> ApiError {
    tracing::error!(%error, operation, "worker registry database operation failed");
    ApiError::internal()
}

async fn issue_worker_credential() -> Result<IssuedCredential, ApiError> {
    tokio::task::spawn_blocking(|| credentials::issue(CredentialKind::Worker))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())
}

#[allow(clippy::too_many_lines)]
#[utoipa::path(
    post,
    path = "/api/v1/worker-enrolments",
    tag = "workers",
    security(("bearer_credential" = [])),
    request_body = EnrolmentRequest,
    responses(
        (status = 201, description = "Worker enrolled", body = EnrolmentResponse),
        (status = 401, description = "Credential rejected", body = ErrorResponse),
        (status = 409, description = "Credential already consumed", body = ErrorResponse),
        (status = 503, description = "Persistence unavailable", body = ErrorResponse)
    )
)]
pub(crate) async fn enrol_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<EnrolmentRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<EnrolmentResponse>), ApiError> {
    let pool = database(&state)?;
    let supplied = bearer(&headers)?.to_owned();
    let credential_id = credentials::identifier(CredentialKind::Enrolment, &supplied)
        .map_err(|_| ApiError::unauthorized())?;
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_protocol(&request.protocol_version)?;
    validate_capabilities(&request.capabilities)?;
    if request.capabilities.protocol_version != request.protocol_version
        || request.display_name.trim().is_empty()
        || request.display_name.chars().count() > 100
    {
        return Err(ApiError::invalid_request());
    }

    let record = sqlx::query_as::<_, EnrolmentRecord>(
        "SELECT owner_identity_id, token_verifier, expires_at, consumed_at, revoked_at \
         FROM worker_enrolments WHERE id = $1",
    )
    .bind(credential_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error, "load enrolment"))?
    .ok_or_else(ApiError::unauthorized)?;

    if record.revoked_at.is_some() || record.expires_at <= Utc::now() {
        return Err(ApiError::unauthorized());
    }
    if !state
        .verification_gate
        .verify(credential_id, supplied, record.token_verifier)
        .await?
    {
        return Err(ApiError::unauthorized());
    }
    if record.consumed_at.is_some() {
        return Err(ApiError::conflict(
            "enrolment_consumed",
            "The enrolment credential has already been consumed.",
        ));
    }

    let worker_credential = issue_worker_credential().await?;
    let worker_id = Uuid::new_v4();
    let capabilities =
        serde_json::to_value(&request.capabilities).map_err(|_| ApiError::internal())?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| database_error(&error, "begin enrolment"))?;
    let locked = sqlx::query_as::<_, EnrolmentRecord>(
        "SELECT owner_identity_id, token_verifier, expires_at, consumed_at, revoked_at \
         FROM worker_enrolments WHERE id = $1 FOR UPDATE",
    )
    .bind(credential_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "lock enrolment"))?;
    if locked.revoked_at.is_some() || locked.expires_at <= Utc::now() {
        return Err(ApiError::unauthorized());
    }
    if locked.consumed_at.is_some() {
        return Err(ApiError::conflict(
            "enrolment_consumed",
            "The enrolment credential has already been consumed.",
        ));
    }

    sqlx::query(
        "INSERT INTO workers \
         (id, owner_identity_id, agent_instance_id, display_name, protocol_version, capabilities) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(worker_id)
    .bind(record.owner_identity_id)
    .bind(request.agent_instance_id)
    .bind(request.display_name.trim())
    .bind(&request.protocol_version)
    .bind(&capabilities)
    .execute(&mut *transaction)
    .await
    .map_err(|error| {
        if error
            .as_database_error()
            .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
        {
            ApiError::conflict(
                "agent_already_enrolled",
                "This agent instance is already enrolled.",
            )
        } else {
            database_error(&error, "create worker")
        }
    })?;
    sqlx::query(
        "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, $3)",
    )
    .bind(worker_credential.id)
    .bind(worker_id)
    .bind(&worker_credential.verifier)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "create worker credential"))?;
    sqlx::query(
        "UPDATE worker_enrolments SET consumed_at = now(), consumed_by_worker_id = $2 WHERE id = $1",
    )
    .bind(credential_id)
    .bind(worker_id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "consume enrolment"))?;
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'service', 'worker.enrolled', 'worker', $2, 'succeeded', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(worker_id)
    .bind(json!({ "agent_instance_id": request.agent_instance_id }))
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "audit enrolment"))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error(&error, "commit enrolment"))?;

    Ok((
        StatusCode::CREATED,
        Json(EnrolmentResponse {
            worker_id,
            worker_credential: worker_credential.plaintext.into_string(),
            state: WorkerState::Unapproved,
            heartbeat_interval_seconds: HEARTBEAT_INTERVAL_SECONDS,
        }),
    ))
}

fn decode_public_key(encoded: &str) -> Result<[u8; 32], ApiError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| ApiError::invalid_request())?;
    bytes.try_into().map_err(|_| ApiError::invalid_request())
}

fn confirmation_code(public_key: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut value = u64::from_be_bytes([
        public_key[0],
        public_key[1],
        public_key[2],
        public_key[3],
        public_key[4],
        0,
        0,
        0,
    ]) >> 24;
    let mut result = String::with_capacity(9);
    for index in 0..8 {
        if index == 4 {
            result.push('-');
        }
        result.push(ALPHABET[(value & 31) as usize] as char);
        value >>= 5;
    }
    result
}

fn claim_message(registration_id: Uuid, challenge: &[u8]) -> Vec<u8> {
    format!(
        "kratos-worker-claim-v1\n{registration_id}\n{}",
        URL_SAFE_NO_PAD.encode(challenge)
    )
    .into_bytes()
}

#[utoipa::path(
    post,
    path = "/api/v1/worker-registration-requests",
    tag = "workers",
    request_body = RegistrationRequest,
    responses(
        (status = 201, description = "Pending worker registration created", body = RegistrationCreatedResponse),
        (status = 409, description = "Agent already has an open request", body = ErrorResponse),
        (status = 429, description = "Pending registration limit reached", body = ErrorResponse),
        (status = 503, description = "Persistence unavailable", body = ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn request_registration(
    State(state): State<AppState>,
    payload: Result<Json<RegistrationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<RegistrationCreatedResponse>), ApiError> {
    let pool = database(&state)?;
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_protocol(&request.protocol_version)?;
    validate_capabilities(&request.capabilities)?;
    if request.capabilities.protocol_version != request.protocol_version
        || request.display_name.trim().is_empty()
        || request.display_name.chars().count() > 100
    {
        return Err(ApiError::invalid_request());
    }
    let public_key = decode_public_key(&request.public_key)?;
    VerifyingKey::from_bytes(&public_key).map_err(|_| ApiError::invalid_request())?;
    let existing = sqlx::query_as::<_, (Uuid, Vec<u8>, String, DateTime<Utc>)>(
        "SELECT id, public_key, confirmation_code, expires_at \
         FROM worker_registration_requests WHERE agent_instance_id = $1 \
           AND claimed_at IS NULL AND rejected_at IS NULL",
    )
    .bind(request.agent_instance_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error, "load open registration"))?;
    if let Some((registration_id, stored_key, code, expires_at)) = existing {
        if stored_key != public_key {
            return Err(ApiError::conflict(
                "registration_already_pending",
                "This agent already has an open registration request.",
            ));
        }
        if expires_at <= Utc::now() {
            sqlx::query("DELETE FROM worker_registration_requests WHERE id = $1")
                .bind(registration_id)
                .execute(pool)
                .await
                .map_err(|error| database_error(&error, "remove expired registration request"))?;
        } else {
            return Ok((
                StatusCode::OK,
                Json(RegistrationCreatedResponse {
                    registration_id,
                    confirmation_code: code,
                    expires_at,
                    poll_interval_seconds: REGISTRATION_POLL_SECONDS,
                }),
            ));
        }
    }
    let open_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worker_registration_requests \
         WHERE claimed_at IS NULL AND rejected_at IS NULL AND expires_at > now()",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| database_error(&error, "count open registrations"))?;
    if open_count >= MAX_OPEN_REGISTRATIONS {
        return Err(ApiError::too_many_requests());
    }

    let registration_id = Uuid::new_v4();
    let expires_at = Utc::now() + TimeDelta::minutes(REGISTRATION_TTL_MINUTES);
    let code = confirmation_code(&public_key);
    let capabilities =
        serde_json::to_value(&request.capabilities).map_err(|_| ApiError::internal())?;
    sqlx::query(
        "INSERT INTO worker_registration_requests \
         (id, agent_instance_id, display_name, protocol_version, capabilities, public_key, \
          confirmation_code, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(registration_id)
    .bind(request.agent_instance_id)
    .bind(request.display_name.trim())
    .bind(&request.protocol_version)
    .bind(capabilities)
    .bind(public_key.as_slice())
    .bind(&code)
    .bind(expires_at)
    .execute(pool)
    .await
    .map_err(|error| {
        if error
            .as_database_error()
            .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
        {
            ApiError::conflict(
                "registration_already_pending",
                "This agent already has an open registration request.",
            )
        } else {
            database_error(&error, "create registration request")
        }
    })?;

    Ok((
        StatusCode::CREATED,
        Json(RegistrationCreatedResponse {
            registration_id,
            confirmation_code: code,
            expires_at,
            poll_interval_seconds: REGISTRATION_POLL_SECONDS,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/worker-registration-requests/{registration_id}",
    tag = "workers",
    params(("registration_id" = Uuid, Path, description = "Registration request identifier")),
    responses(
        (status = 200, description = "Current registration state", body = RegistrationStatusResponse),
        (status = 404, description = "Registration request not found", body = ErrorResponse)
    )
)]
pub(crate) async fn registration_status(
    State(state): State<AppState>,
    Path(registration_id): Path<Uuid>,
) -> Result<Json<RegistrationStatusResponse>, ApiError> {
    let pool = database(&state)?;
    let row = sqlx::query_as::<
        _,
        (
            DateTime<Utc>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<Vec<u8>>,
        ),
    >(
        "SELECT expires_at, approved_at, rejected_at, claimed_at, claim_challenge \
         FROM worker_registration_requests WHERE id = $1",
    )
    .bind(registration_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error, "load registration status"))?
    .ok_or_else(ApiError::not_found)?;
    let registration_state = if row.3.is_some() {
        RegistrationState::Claimed
    } else if row.2.is_some() {
        RegistrationState::Rejected
    } else if row.0 <= Utc::now() {
        RegistrationState::Expired
    } else if row.1.is_some() {
        RegistrationState::Approved
    } else {
        RegistrationState::Pending
    };
    Ok(Json(RegistrationStatusResponse {
        registration_id,
        state: registration_state,
        claim_challenge: row.4.map(|value| URL_SAFE_NO_PAD.encode(value)),
        poll_interval_seconds: REGISTRATION_POLL_SECONDS,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/worker-registration-requests/{registration_id}/claim",
    tag = "workers",
    params(("registration_id" = Uuid, Path, description = "Registration request identifier")),
    request_body = ClaimRegistrationRequest,
    responses(
        (status = 201, description = "Approved worker identity claimed", body = EnrolmentResponse),
        (status = 401, description = "Signature rejected", body = ErrorResponse),
        (status = 409, description = "Request is not claimable", body = ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn claim_registration(
    State(state): State<AppState>,
    Path(registration_id): Path<Uuid>,
    payload: Result<Json<ClaimRegistrationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<EnrolmentResponse>), ApiError> {
    let pool = database(&state)?;
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(request.signature)
        .map_err(|_| ApiError::unauthorized())?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| ApiError::unauthorized())?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| database_error(&error, "begin registration claim"))?;
    let record = sqlx::query_as::<_, RegistrationRecord>(
        "SELECT agent_instance_id, display_name, protocol_version, capabilities, public_key, \
                claim_challenge, expires_at, approved_at, approved_by_identity_id, rejected_at, \
                claimed_at, worker_id FROM worker_registration_requests WHERE id = $1 FOR UPDATE",
    )
    .bind(registration_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "lock registration request"))?
    .ok_or_else(ApiError::not_found)?;
    if record.expires_at <= Utc::now()
        || record.rejected_at.is_some()
        || record.approved_at.is_none()
    {
        return Err(ApiError::conflict(
            "registration_not_claimable",
            "The registration request is not claimable.",
        ));
    }
    let challenge = record.claim_challenge.ok_or_else(ApiError::internal)?;
    let public_key: [u8; 32] = record
        .public_key
        .try_into()
        .map_err(|_| ApiError::internal())?;
    VerifyingKey::from_bytes(&public_key)
        .map_err(|_| ApiError::internal())?
        .verify(&claim_message(registration_id, &challenge), &signature)
        .map_err(|_| ApiError::unauthorized())?;

    let credential_secret: [u8; 32] = Sha256::digest(signature.to_bytes()).into();
    let worker_credential =
        credentials::from_secret(CredentialKind::Worker, registration_id, &credential_secret)
            .map_err(|_| ApiError::internal())?;
    if record.claimed_at.is_some() {
        return Ok((
            StatusCode::OK,
            Json(EnrolmentResponse {
                worker_id: record.worker_id.ok_or_else(ApiError::internal)?,
                worker_credential: worker_credential.plaintext.into_string(),
                state: WorkerState::Idle,
                heartbeat_interval_seconds: HEARTBEAT_INTERVAL_SECONDS,
            }),
        ));
    }

    let owner_identity_id = record
        .approved_by_identity_id
        .ok_or_else(ApiError::internal)?;
    let worker_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workers (id, owner_identity_id, agent_instance_id, display_name, \
         protocol_version, capabilities, status) VALUES ($1, $2, $3, $4, $5, $6, 'idle')",
    )
    .bind(worker_id)
    .bind(owner_identity_id)
    .bind(record.agent_instance_id)
    .bind(record.display_name)
    .bind(record.protocol_version)
    .bind(record.capabilities)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "create approved worker"))?;
    sqlx::query(
        "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, $3)",
    )
    .bind(worker_credential.id)
    .bind(worker_id)
    .bind(&worker_credential.verifier)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "create approved worker credential"))?;
    sqlx::query(
        "UPDATE worker_registration_requests SET claimed_at = now(), worker_id = $2 WHERE id = $1",
    )
    .bind(registration_id)
    .bind(worker_id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "complete registration claim"))?;
    sqlx::query(
        "INSERT INTO audit_events (id, actor_type, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'service', 'worker.registration.claimed', 'worker', $2, 'succeeded', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(worker_id)
    .bind(json!({ "registration_id": registration_id }))
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "audit registration claim"))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error(&error, "commit registration claim"))?;

    Ok((
        StatusCode::CREATED,
        Json(EnrolmentResponse {
            worker_id,
            worker_credential: worker_credential.plaintext.into_string(),
            state: WorkerState::Idle,
            heartbeat_interval_seconds: HEARTBEAT_INTERVAL_SECONDS,
        }),
    ))
}

#[allow(clippy::too_many_lines)]
async fn current_or_assign_job(
    pool: &PgPool,
    worker_id: Uuid,
    eligible: bool,
    expected_heartbeat: Option<ExpectedHeartbeat<'_>>,
) -> Result<Option<JobAssignment>, ApiError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| database_error(&error, "begin job assignment"))?;
    let worker = sqlx::query_as::<_, (String, i64, String, serde_json::Value)>(
        "SELECT status, heartbeat_sequence, protocol_version, capabilities \
         FROM workers WHERE id = $1 FOR UPDATE",
    )
    .bind(worker_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "lock worker for assignment"))?
    .ok_or_else(ApiError::unauthorized)?;
    if expected_heartbeat.is_some_and(|expected| {
        worker.1 != expected.sequence
            || worker.2 != expected.protocol_version
            || worker.3 != *expected.capabilities
    }) {
        return Err(ApiError::conflict(
            "stale_sequence",
            "A newer heartbeat was accepted before scheduling completed.",
        ));
    }
    let worker_status = worker.0;
    let existing = sqlx::query_as::<_, AssignmentRecord>(
        "SELECT a.id AS attempt_id, j.id AS job_id, j.name, j.image_reference, \
                j.timeout_seconds, a.lease_expires_at \
         FROM job_attempts a JOIN jobs j ON j.id = a.job_id \
         WHERE a.worker_id = $1 AND a.status IN ('assigned', 'running') \
         ORDER BY a.assigned_at LIMIT 1",
    )
    .bind(worker_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "load active assignment"))?;
    if let Some(record) = existing {
        let job_id = record.job_id;
        transaction
            .commit()
            .await
            .map_err(|error| database_error(&error, "commit active assignment"))?;
        let mut assignment: JobAssignment = record.try_into()?;
        assignment.output_requirements = load_output_requirements(pool, job_id).await?;
        return Ok(Some(assignment));
    }
    if !eligible || worker_status != "idle" {
        transaction
            .commit()
            .await
            .map_err(|error| database_error(&error, "commit empty assignment"))?;
        return Ok(None);
    }
    let queued = sqlx::query_as::<_, (Uuid, String, String, i32)>(
        "SELECT id, name, image_reference, timeout_seconds FROM jobs \
         WHERE status = 'queued' AND gpu_count = 1 \
           AND ( \
               NOT EXISTS (SELECT 1 FROM job_output_requirements r WHERE r.job_id = jobs.id) \
               OR EXISTS ( \
                   SELECT 1 FROM workers w WHERE w.id = $1 \
                     AND split_part(w.protocol_version, '.', 1) = '1' \
                     AND split_part(w.protocol_version, '.', 2)::integer >= 1 \
               ) \
           ) \
         ORDER BY submitted_at \
         FOR UPDATE SKIP LOCKED LIMIT 1",
    )
    .bind(worker_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "select queued job"))?;
    let Some((job_id, name, image_reference, timeout_seconds)) = queued else {
        transaction
            .commit()
            .await
            .map_err(|error| database_error(&error, "commit empty queue"))?;
        return Ok(None);
    };
    let attempt_id = Uuid::new_v4();
    let lease_expires_at = Utc::now()
        .checked_add_signed(TimeDelta::seconds(i64::from(timeout_seconds) + 120))
        .ok_or_else(ApiError::internal)?;
    sqlx::query(
        "INSERT INTO job_attempts \
         (id, job_id, attempt_number, worker_id, lease_expires_at) VALUES ($1, $2, 1, $3, $4)",
    )
    .bind(attempt_id)
    .bind(job_id)
    .bind(worker_id)
    .bind(lease_expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "create job attempt"))?;
    sqlx::query(
        "UPDATE jobs SET status = 'assigned', assigned_worker_id = $2 WHERE id = $1 AND status = 'queued'",
    )
    .bind(job_id)
    .bind(worker_id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "assign job"))?;
    sqlx::query("UPDATE workers SET status = 'busy', updated_at = now() WHERE id = $1")
        .bind(worker_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| database_error(&error, "mark worker busy"))?;
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'service', NULL, 'job.assigned', 'job', $2, 'succeeded', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(job_id)
    .bind(json!({ "attempt_id": attempt_id, "worker_id": worker_id, "lease_expires_at": lease_expires_at }))
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "audit job assignment"))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error(&error, "commit job assignment"))?;
    Ok(Some(JobAssignment {
        attempt_id,
        job_id,
        name,
        image_reference,
        gpu_index: 0,
        timeout_seconds: u32::try_from(timeout_seconds).map_err(|_| ApiError::internal())?,
        lease_expires_at,
        output_requirements: load_output_requirements(pool, job_id).await?,
    }))
}

#[allow(clippy::too_many_lines)]
#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/heartbeat",
    tag = "workers",
    security(("bearer_credential" = [])),
    params(("worker_id" = Uuid, Path, description = "Worker identifier")),
    request_body = HeartbeatRequest,
    responses(
        (status = 200, description = "Heartbeat accepted", body = HeartbeatResponse),
        (status = 401, description = "Credential rejected", body = ErrorResponse),
        (status = 409, description = "Sequence is stale", body = ErrorResponse),
        (status = 503, description = "Persistence unavailable", body = ErrorResponse)
    )
)]
pub(crate) async fn heartbeat(
    State(state): State<AppState>,
    Path(worker_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<HeartbeatRequest>, JsonRejection>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    let pool = database(&state)?;
    let supplied = bearer(&headers)?.to_owned();
    let credential_id = credentials::identifier(CredentialKind::Worker, &supplied)
        .map_err(|_| ApiError::unauthorized())?;
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_protocol(&request.protocol_version)?;
    validate_capabilities(&request.capabilities)?;
    if request.capabilities.protocol_version != request.protocol_version || request.sequence < 0 {
        return Err(ApiError::invalid_request());
    }

    let authentication = sqlx::query_as::<_, HeartbeatAuthenticationRecord>(
        "SELECT c.token_verifier, c.expires_at, c.revoked_at AS credential_revoked_at, \
                w.status, w.heartbeat_sequence, w.protocol_version, w.capabilities \
         FROM worker_credentials c JOIN workers w ON w.id = c.worker_id \
         WHERE c.id = $1 AND c.worker_id = $2",
    )
    .bind(credential_id)
    .bind(worker_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error, "load worker credential"))?
    .ok_or_else(ApiError::unauthorized)?;
    if authentication.credential_revoked_at.is_some()
        || authentication
            .expires_at
            .is_some_and(|expires| expires <= Utc::now())
        || authentication.status == "revoked"
    {
        return Err(ApiError::unauthorized());
    }
    if !state
        .verification_gate
        .verify(credential_id, supplied, authentication.token_verifier)
        .await?
    {
        return Err(ApiError::unauthorized());
    }

    let capabilities =
        serde_json::to_value(&request.capabilities).map_err(|_| ApiError::internal())?;

    if request.sequence < authentication.heartbeat_sequence {
        return Err(ApiError::conflict(
            "stale_sequence",
            "A newer heartbeat has already been accepted.",
        ));
    }
    let state_value = WorkerState::parse(&authentication.status)?;
    if request.sequence == authentication.heartbeat_sequence {
        if request.protocol_version != authentication.protocol_version
            || capabilities != authentication.capabilities
        {
            return Err(ApiError::conflict(
                "heartbeat_replay_mismatch",
                "The heartbeat sequence was replayed with different protocol or capability data.",
            ));
        }
        let assignment = current_or_assign_job(
            pool,
            worker_id,
            state_value == WorkerState::Idle
                && request.capabilities.gpu_health.status == GpuHealthStatus::Healthy
                && !request.capabilities.gpus.is_empty(),
            Some(ExpectedHeartbeat {
                sequence: request.sequence,
                protocol_version: &request.protocol_version,
                capabilities: &capabilities,
            }),
        )
        .await?;
        return Ok(Json(HeartbeatResponse {
            worker_id,
            state: if assignment.is_some() {
                WorkerState::Busy
            } else {
                state_value
            },
            accepted_sequence: authentication.heartbeat_sequence,
            next_heartbeat_seconds: HEARTBEAT_INTERVAL_SECONDS,
            assignment,
        }));
    }

    let updated = sqlx::query_as::<_, (String, i64)>(
        "UPDATE workers SET protocol_version = $2, capabilities = $3, heartbeat_sequence = $4, \
                last_seen_at = now(), last_observed_at = $5, updated_at = now() \
         WHERE id = $1 AND status <> 'revoked' AND heartbeat_sequence < $4 \
           AND EXISTS (SELECT 1 FROM worker_credentials c WHERE c.id = $6 \
                       AND c.worker_id = workers.id AND c.revoked_at IS NULL \
                       AND (c.expires_at IS NULL OR c.expires_at > now())) \
         RETURNING status, heartbeat_sequence",
    )
    .bind(worker_id)
    .bind(&request.protocol_version)
    .bind(&capabilities)
    .bind(request.sequence)
    .bind(request.observed_at)
    .bind(credential_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error, "update heartbeat"))?;

    let Some((status, accepted_sequence)) = updated else {
        let current = sqlx::query_as::<_, (String, i64, String, serde_json::Value)>(
            "SELECT status, heartbeat_sequence, protocol_version, capabilities \
             FROM workers WHERE id = $1",
        )
        .bind(worker_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| database_error(&error, "reload heartbeat sequence"))?
        .ok_or_else(ApiError::unauthorized)?;
        if current.0 == "revoked" {
            return Err(ApiError::unauthorized());
        }
        if current.1 == request.sequence {
            if current.2 != request.protocol_version || current.3 != capabilities {
                return Err(ApiError::conflict(
                    "heartbeat_replay_mismatch",
                    "The heartbeat sequence was replayed with different protocol or capability data.",
                ));
            }
            let current_state = WorkerState::parse(&current.0)?;
            let assignment = current_or_assign_job(
                pool,
                worker_id,
                current_state == WorkerState::Idle
                    && request.capabilities.gpu_health.status == GpuHealthStatus::Healthy
                    && !request.capabilities.gpus.is_empty(),
                Some(ExpectedHeartbeat {
                    sequence: request.sequence,
                    protocol_version: &request.protocol_version,
                    capabilities: &capabilities,
                }),
            )
            .await?;
            return Ok(Json(HeartbeatResponse {
                worker_id,
                state: if assignment.is_some() {
                    WorkerState::Busy
                } else {
                    current_state
                },
                accepted_sequence: current.1,
                next_heartbeat_seconds: HEARTBEAT_INTERVAL_SECONDS,
                assignment,
            }));
        }
        return Err(ApiError::conflict(
            "stale_sequence",
            "A newer heartbeat has already been accepted.",
        ));
    };

    let response_state = WorkerState::parse(&status)?;
    let assignment = current_or_assign_job(
        pool,
        worker_id,
        response_state == WorkerState::Idle
            && request.capabilities.gpu_health.status == GpuHealthStatus::Healthy
            && !request.capabilities.gpus.is_empty(),
        Some(ExpectedHeartbeat {
            sequence: request.sequence,
            protocol_version: &request.protocol_version,
            capabilities: &capabilities,
        }),
    )
    .await?;
    Ok(Json(HeartbeatResponse {
        worker_id,
        state: if assignment.is_some() {
            WorkerState::Busy
        } else {
            response_state
        },
        accepted_sequence,
        next_heartbeat_seconds: HEARTBEAT_INTERVAL_SECONDS,
        assignment,
    }))
}

#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/result",
    tag = "workers",
    security(("bearer_credential" = [])),
    params(
        ("worker_id" = Uuid, Path, description = "Worker identifier"),
        ("attempt_id" = Uuid, Path, description = "Job attempt identifier")
    ),
    request_body = JobResultRequest,
    responses(
        (status = 200, description = "Result accepted idempotently", body = JobResultResponse),
        (status = 401, description = "Credential rejected", body = ErrorResponse),
        (status = 422, description = "Result is invalid", body = ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn report_job_result(
    State(state): State<AppState>,
    Path((worker_id, attempt_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<JobResultRequest>, JsonRejection>,
) -> Result<Json<JobResultResponse>, ApiError> {
    let pool = database(&state)?;
    let supplied = bearer(&headers)?.to_owned();
    let credential_id = credentials::identifier(CredentialKind::Worker, &supplied)
        .map_err(|_| ApiError::unauthorized())?;
    let authentication = sqlx::query_as::<_, WorkerAuthenticationRecord>(
        "SELECT c.token_verifier, c.expires_at, c.revoked_at AS credential_revoked_at, w.status \
         FROM worker_credentials c JOIN workers w ON w.id = c.worker_id \
         WHERE c.id = $1 AND c.worker_id = $2",
    )
    .bind(credential_id)
    .bind(worker_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error, "load result credential"))?
    .ok_or_else(ApiError::unauthorized)?;
    if authentication.credential_revoked_at.is_some()
        || authentication
            .expires_at
            .is_some_and(|expires| expires <= Utc::now())
        || authentication.status == "revoked"
        || !state
            .verification_gate
            .verify(credential_id, supplied, authentication.token_verifier)
            .await?
    {
        return Err(ApiError::unauthorized());
    }
    let Json(result) = payload.map_err(|_| ApiError::invalid_request())?;
    if result.stdout.len() > 65_536
        || result.stderr.len() > 65_536
        || result
            .failure_message
            .as_ref()
            .is_some_and(|message| message.chars().count() > 1_000)
    {
        return Err(ApiError::invalid_request());
    }
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| database_error(&error, "begin job result"))?;
    let attempt = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT job_id, status FROM job_attempts WHERE id = $1 AND worker_id = $2 FOR UPDATE",
    )
    .bind(attempt_id)
    .bind(worker_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "load job attempt"))?
    .ok_or_else(ApiError::unauthorized)?;
    if matches!(attempt.1.as_str(), "succeeded" | "failed") {
        transaction
            .commit()
            .await
            .map_err(|error| database_error(&error, "commit repeated job result"))?;
        return Ok(Json(JobResultResponse {
            attempt_id,
            job_id: attempt.0,
            status: attempt.1,
        }));
    }
    let succeeded = result.exit_code == 0 && !result.timed_out && result.failure_message.is_none();
    if succeeded {
        let missing_artifacts = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM job_output_requirements r \
             WHERE r.job_id = $1 AND r.mandatory \
               AND NOT EXISTS ( \
                   SELECT 1 FROM job_artifacts ar \
                   WHERE ar.output_requirement_id = r.id AND ar.attempt_id = $2 \
                     AND ar.status = 'verified' AND ar.upload_completed_at IS NOT NULL \
                     AND ar.verified_at IS NOT NULL \
                     AND ar.verification_source = 'gcs_metadata' \
                     AND ar.verified_storage_generation = ar.storage_generation \
                     AND ar.verified_byte_length = ar.byte_length \
                     AND ar.verified_byte_length = ar.uploaded_byte_length \
                     AND ar.verified_crc32c = ar.crc32c \
                     AND ar.verified_crc32c = ar.uploaded_crc32c \
               )",
        )
        .bind(attempt.0)
        .bind(attempt_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| database_error(&error, "check mandatory job artefacts"))?;
        if missing_artifacts > 0 {
            return Err(ApiError::conflict(
                "artifacts_incomplete",
                "Mandatory output artefacts have not been durably verified.",
            ));
        }
    }
    let final_status = if succeeded { "succeeded" } else { "failed" };
    sqlx::query(
        "UPDATE job_attempts SET status = $2, started_at = COALESCE(started_at, assigned_at), \
                finished_at = now() WHERE id = $1",
    )
    .bind(attempt_id)
    .bind(final_status)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "finish job attempt"))?;
    sqlx::query(
        "UPDATE jobs SET status = $2, started_at = COALESCE(started_at, submitted_at), \
                finished_at = now(), exit_code = $3, stdout = $4, stderr = $5, \
                failure_message = $6 WHERE id = $1",
    )
    .bind(attempt.0)
    .bind(final_status)
    .bind(result.exit_code)
    .bind(&result.stdout)
    .bind(&result.stderr)
    .bind(&result.failure_message)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "finish job"))?;
    sqlx::query(
        "UPDATE workers SET status = 'idle', updated_at = now() WHERE id = $1 AND status = 'busy'",
    )
    .bind(worker_id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "release worker"))?;
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'worker', $2, 'job.finished', 'job', $3, $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(worker_id)
    .bind(attempt.0)
    .bind(if succeeded { "succeeded" } else { "failed" })
    .bind(json!({ "attempt_id": attempt_id, "exit_code": result.exit_code, "timed_out": result.timed_out }))
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "audit job result"))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error(&error, "commit job result"))?;
    Ok(Json(JobResultResponse {
        attempt_id,
        job_id: attempt.0,
        status: final_status.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::AUTHORIZATION},
    };
    use chrono::{Duration, Utc};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::{
        app,
        credentials::{self, CredentialKind},
    };

    use super::{
        ExpectedHeartbeat, GpuHealth, GpuHealthEvidence, GpuHealthStatus, current_or_assign_job,
        immutable_sha256_reference, valid_gpu_health,
    };

    fn healthy_evidence() -> GpuHealthEvidence {
        GpuHealthEvidence {
            schema_version: "1.0".to_owned(),
            status: GpuHealthStatus::Healthy,
            checked_at: Utc::now(),
            image_reference: format!("example.test/health@sha256:{}", "a".repeat(64)),
            device_index: Some(0),
            device_name: Some("Test GPU".to_owned()),
            operation: Some("matrix multiplication".to_owned()),
            matrix_size: Some(512),
            max_absolute_error: Some(0.0),
            duration_ms: Some(12.5),
            cuda_driver_api_version: Some("13.3".to_owned()),
            cuda_runtime_version: Some("12.9".to_owned()),
            error_type: None,
            detail: None,
        }
    }

    async fn insert_owner(pool: &PgPool) -> Uuid {
        let owner_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
             VALUES ($1, 'test', $2, 'Owner')",
        )
        .bind(owner_id)
        .bind(Uuid::new_v4().to_string())
        .execute(pool)
        .await
        .unwrap();
        owner_id
    }

    async fn insert_worker(pool: &PgPool, owner_id: Uuid, status: &str) -> Uuid {
        let worker_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities) \
             VALUES ($1, $2, $3, 'GPU worker', '1.0', $4, '{}'::jsonb)",
        )
        .bind(worker_id)
        .bind(owner_id)
        .bind(Uuid::new_v4())
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
        worker_id
    }

    async fn insert_job(pool: &PgPool, owner_id: Uuid) -> Uuid {
        let job_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO jobs (id, owner_identity_id, name, image_reference, timeout_seconds) \
             VALUES ($1, $2, 'Matrix check', $3, 120)",
        )
        .bind(job_id)
        .bind(owner_id)
        .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
        .execute(pool)
        .await
        .unwrap();
        job_id
    }

    #[test]
    fn health_evidence_requires_an_immutable_lowercase_digest() {
        assert!(immutable_sha256_reference(&format!(
            "sha256:{}",
            "a".repeat(64)
        )));
        assert!(!immutable_sha256_reference("health:latest"));
        assert!(!immutable_sha256_reference(&format!(
            "sha256:{}",
            "A".repeat(64)
        )));
    }

    #[test]
    fn reported_health_must_match_its_evidence() {
        let valid = GpuHealth {
            status: GpuHealthStatus::Healthy,
            detail: "computation passed".to_owned(),
            evidence: Some(healthy_evidence()),
        };
        assert!(valid_gpu_health(&valid));

        let mismatched = GpuHealth {
            status: GpuHealthStatus::Unhealthy,
            detail: "mismatch".to_owned(),
            evidence: Some(healthy_evidence()),
        };
        assert!(!valid_gpu_health(&mismatched));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn queued_job_is_atomically_replayed_to_one_worker(pool: PgPool) {
        let owner_id = insert_owner(&pool).await;
        let worker_id = insert_worker(&pool, owner_id, "idle").await;
        let job_id = insert_job(&pool, owner_id).await;

        let first = current_or_assign_job(&pool, worker_id, true, None)
            .await
            .unwrap()
            .unwrap();
        let replay = current_or_assign_job(&pool, worker_id, true, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first.job_id, job_id);
        assert_eq!(first.attempt_id, replay.attempt_id);
        let state: (String, String, i64) = sqlx::query_as(
            "SELECT j.status, w.status, count(a.id) \
             FROM jobs j JOIN workers w ON w.id = j.assigned_worker_id \
             JOIN job_attempts a ON a.job_id = j.id WHERE j.id = $1 \
             GROUP BY j.status, w.status",
        )
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, ("assigned".to_owned(), "busy".to_owned(), 1));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_scheduler_replay_creates_one_active_attempt(pool: PgPool) {
        let owner_id = insert_owner(&pool).await;
        let first_worker = insert_worker(&pool, owner_id, "idle").await;
        let second_worker = insert_worker(&pool, owner_id, "idle").await;
        let job_id = insert_job(&pool, owner_id).await;

        let (first, second) = tokio::join!(
            current_or_assign_job(&pool, first_worker, true, None),
            current_or_assign_job(&pool, second_worker, true, None),
        );
        let assignments: Vec<_> = [first.unwrap(), second.unwrap()]
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].job_id, job_id);

        let assigned_worker: Uuid =
            sqlx::query_scalar("SELECT assigned_worker_id FROM jobs WHERE id = $1")
                .bind(job_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let replay = current_or_assign_job(&pool, assigned_worker, true, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.attempt_id, assignments[0].attempt_id);

        let duplicate = sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, lease_expires_at) \
             VALUES ($1, $2, 2, $3, now() + interval '5 minutes')",
        )
        .bind(Uuid::new_v4())
        .bind(job_id)
        .bind(if assigned_worker == first_worker {
            second_worker
        } else {
            first_worker
        })
        .execute(&pool)
        .await
        .unwrap_err();
        assert_eq!(
            duplicate
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("job_attempts_one_active_job")
        );

        let (attempts, active_attempts, busy_workers): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(DISTINCT a.id), \
                    count(DISTINCT a.id) FILTER (WHERE a.status IN ('assigned', 'running')), \
                    count(DISTINCT w.id) FILTER (WHERE w.status = 'busy') \
             FROM jobs j LEFT JOIN job_attempts a ON a.job_id = j.id \
             CROSS JOIN workers w WHERE j.id = $1",
        )
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((attempts, active_attempts, busy_workers), (1, 1, 1));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn scheduler_rejects_heartbeat_superseded_while_waiting_for_worker_lock(pool: PgPool) {
        let owner_id = insert_owner(&pool).await;
        let worker_id = insert_worker(&pool, owner_id, "idle").await;
        let job_id = insert_job(&pool, owner_id).await;
        sqlx::query(
            "INSERT INTO job_output_requirements \
             (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
             VALUES ($1, $2, 'model.pt', 'checkpoint', 'application/octet-stream', true, 1024)",
        )
        .bind(Uuid::new_v4())
        .bind(job_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut newer_heartbeat = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE workers SET heartbeat_sequence = 1, protocol_version = '1.1', \
                    capabilities = '{\"protocol_version\":\"1.1\"}'::jsonb WHERE id = $1",
        )
        .bind(worker_id)
        .execute(&mut *newer_heartbeat)
        .await
        .unwrap();

        let scheduler_pool = pool.clone();
        let mut scheduler = tokio::spawn(async move {
            let old_capabilities = json!({});
            current_or_assign_job(
                &scheduler_pool,
                worker_id,
                true,
                Some(ExpectedHeartbeat {
                    sequence: 0,
                    protocol_version: "1.0",
                    capabilities: &old_capabilities,
                }),
            )
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut scheduler)
                .await
                .is_err(),
            "scheduler should wait for the heartbeat's worker-row lock"
        );
        newer_heartbeat.commit().await.unwrap();

        let error = scheduler.await.unwrap().unwrap_err();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.body.code, "stale_sequence");
        let job_state: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let attempt_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM job_attempts WHERE job_id = $1")
                .bind(job_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(job_state, "queued");
        assert_eq!(attempt_count, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn late_result_cannot_overwrite_replacement_attempt(pool: PgPool) {
        let owner_id = insert_owner(&pool).await;
        let lost_worker = insert_worker(&pool, owner_id, "idle").await;
        let replacement_worker = insert_worker(&pool, owner_id, "busy").await;
        let job_id = insert_job(&pool, owner_id).await;
        let lost_attempt = Uuid::new_v4();
        let replacement_attempt = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, status, assigned_at, lease_expires_at, finished_at) \
             VALUES ($1, $2, 1, $3, 'failed', now() - interval '10 minutes', \
                     now() - interval '5 minutes', now() - interval '5 minutes'), \
                    ($4, $2, 2, $5, 'assigned', now(), now() + interval '5 minutes', NULL)",
        )
        .bind(lost_attempt)
        .bind(job_id)
        .bind(lost_worker)
        .bind(replacement_attempt)
        .bind(replacement_worker)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE jobs SET status = 'assigned', assigned_worker_id = $2 WHERE id = $1")
            .bind(job_id)
            .bind(replacement_worker)
            .execute(&pool)
            .await
            .unwrap();

        let credential = credentials::issue(CredentialKind::Worker).unwrap();
        sqlx::query(
            "INSERT INTO worker_credentials (id, worker_id, token_verifier, expires_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(credential.id)
        .bind(lost_worker)
        .bind(&credential.verifier)
        .bind(Utc::now() + Duration::minutes(10))
        .execute(&pool)
        .await
        .unwrap();

        let response = app(None, Some(pool.clone()))
            .oneshot(
                Request::put(format!(
                    "/api/v1/workers/{lost_worker}/job-attempts/{lost_attempt}/result"
                ))
                .header(
                    AUTHORIZATION,
                    format!("Bearer {}", credential.plaintext.expose()),
                )
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "exit_code": 0,
                        "timed_out": false,
                        "stdout": "stale completion",
                        "stderr": "",
                        "failure_message": null
                    })
                    .to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let response: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(response["status"], "failed");

        let job: (String, Option<Uuid>, Option<i32>, Option<String>) = sqlx::query_as(
            "SELECT status, assigned_worker_id, exit_code, stdout FROM jobs WHERE id = $1",
        )
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            job,
            ("assigned".to_owned(), Some(replacement_worker), None, None)
        );
        let replacement_status: String =
            sqlx::query_scalar("SELECT status FROM job_attempts WHERE id = $1")
                .bind(replacement_attempt)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(replacement_status, "assigned");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn output_jobs_require_worker_protocol_one_point_one(pool: PgPool) {
        let owner_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
             VALUES ($1, 'test', $2, 'Owner')",
        )
        .bind(owner_id)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities) \
             VALUES ($1, $2, $3, 'GPU worker', '1.0', 'idle', '{}'::jsonb)",
        )
        .bind(worker_id)
        .bind(owner_id)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, owner_identity_id, name, image_reference, timeout_seconds) \
             VALUES ($1, $2, 'Training', $3, 120)",
        )
        .bind(job_id)
        .bind(owner_id)
        .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_output_requirements \
             (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
             VALUES ($1, $2, 'model.pt', 'checkpoint', 'application/octet-stream', true, 1024)",
        )
        .bind(Uuid::new_v4())
        .bind(job_id)
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            current_or_assign_job(&pool, worker_id, true, None)
                .await
                .unwrap()
                .is_none()
        );
        sqlx::query("UPDATE workers SET protocol_version = '1.1' WHERE id = $1")
            .bind(worker_id)
            .execute(&pool)
            .await
            .unwrap();
        let assignment = current_or_assign_job(&pool, worker_id, true, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(assignment.job_id, job_id);
        assert_eq!(assignment.output_requirements.len(), 1);
        assert_eq!(assignment.output_requirements[0].logical_path, "model.pt");
    }
}

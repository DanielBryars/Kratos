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

#[derive(Debug, Deserialize, Serialize, ToSchema)]
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
pub struct GpuHealth {
    pub status: GpuHealthStatus,
    pub detail: String,
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

#[derive(Clone, Copy, Debug, Serialize, ToSchema)]
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
}

#[derive(Clone, Copy, Debug, Serialize, ToSchema)]
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

    const fn invalid_request() -> Self {
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

    const fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "persistence_unavailable",
            "Worker registration is unavailable.",
        )
    }

    const fn conflict(code: &'static str, message: &'static str) -> Self {
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

    const fn internal() -> Self {
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
    heartbeat_sequence: i64,
}

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or_else(ApiError::unauthorized)
}

fn validate_protocol(version: &str) -> Result<(), ApiError> {
    if version.split_once('.').is_some_and(|(major, minor)| {
        major == "1" && !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(())
    } else {
        Err(ApiError::unsupported_protocol())
    }
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
        && capabilities.logical_cpu_count > 0
        && capabilities.memory_total_bytes > 0
    {
        Ok(())
    } else {
        Err(ApiError::invalid_request())
    }
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
    .bind(capabilities)
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

    let authentication = sqlx::query_as::<_, WorkerAuthenticationRecord>(
        "SELECT c.token_verifier, c.expires_at, c.revoked_at AS credential_revoked_at, \
                w.status, w.heartbeat_sequence \
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

    if request.sequence < authentication.heartbeat_sequence {
        return Err(ApiError::conflict(
            "stale_sequence",
            "A newer heartbeat has already been accepted.",
        ));
    }
    let state_value = WorkerState::parse(&authentication.status)?;
    if request.sequence == authentication.heartbeat_sequence {
        return Ok(Json(HeartbeatResponse {
            worker_id,
            state: state_value,
            accepted_sequence: authentication.heartbeat_sequence,
            next_heartbeat_seconds: HEARTBEAT_INTERVAL_SECONDS,
        }));
    }

    let capabilities =
        serde_json::to_value(&request.capabilities).map_err(|_| ApiError::internal())?;
    let updated = sqlx::query_as::<_, (String, i64)>(
        "UPDATE workers SET capabilities = $2, heartbeat_sequence = $3, \
                last_seen_at = now(), last_observed_at = $4, updated_at = now() \
         WHERE id = $1 AND status <> 'revoked' AND heartbeat_sequence < $3 \
           AND EXISTS (SELECT 1 FROM worker_credentials c WHERE c.id = $5 \
                       AND c.worker_id = workers.id AND c.revoked_at IS NULL \
                       AND (c.expires_at IS NULL OR c.expires_at > now())) \
         RETURNING status, heartbeat_sequence",
    )
    .bind(worker_id)
    .bind(capabilities)
    .bind(request.sequence)
    .bind(request.observed_at)
    .bind(credential_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error, "update heartbeat"))?;

    let Some((status, accepted_sequence)) = updated else {
        let current = sqlx::query_as::<_, (String, i64)>(
            "SELECT status, heartbeat_sequence FROM workers WHERE id = $1",
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
            return Ok(Json(HeartbeatResponse {
                worker_id,
                state: WorkerState::parse(&current.0)?,
                accepted_sequence: current.1,
                next_heartbeat_seconds: HEARTBEAT_INTERVAL_SECONDS,
            }));
        }
        return Err(ApiError::conflict(
            "stale_sequence",
            "A newer heartbeat has already been accepted.",
        ));
    };

    Ok(Json(HeartbeatResponse {
        worker_id,
        state: WorkerState::parse(&status)?,
        accepted_sequence,
        next_heartbeat_seconds: HEARTBEAT_INTERVAL_SECONDS,
    }))
}

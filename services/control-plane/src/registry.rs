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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use tokio::sync::{Mutex, Semaphore};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    credentials::{self, CredentialKind, IssuedCredential},
};

const HEARTBEAT_INTERVAL_SECONDS: u32 = 30;
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

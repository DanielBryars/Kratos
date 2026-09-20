use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, TimeDelta, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    artifacts::{
        JobOutputRequirement, cancel_job_upload_sessions, cancel_worker_upload_sessions,
        reconcile_pending_session_cancellations, validate_output_requirements,
    },
    credentials::{self, CredentialKind},
    human_auth::{HumanIdentity, VerifyError},
    registry::{ErrorResponse, WorkerCapabilities, reconcile_expired_attempts},
};

const DEFAULT_EXPIRY_SECONDS: i64 = 900;
const MIN_EXPIRY_SECONDS: i64 = 300;
const MAX_EXPIRY_SECONDS: i64 = 3600;

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateEnrolmentRequest {
    /// Lifetime of the single-use credential. Defaults to 15 minutes.
    pub expires_in_seconds: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct CreateEnrolmentResponse {
    pub enrolment_credential: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PendingRegistrationResponse {
    pub registration_id: Uuid,
    pub agent_instance_id: Uuid,
    pub display_name: String,
    pub confirmation_code: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub capabilities: WorkerCapabilities,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegistrationDecisionResponse {
    pub registration_id: Uuid,
    pub state: &'static str,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WorkerGroupResponse {
    pub id: Uuid,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerConnectivity {
    NeverSeen,
    Online,
    Stale,
    Offline,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OperatorWorkerResponse {
    pub worker_id: Uuid,
    pub agent_instance_id: Uuid,
    pub display_name: String,
    pub state: String,
    pub connectivity: WorkerConnectivity,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub capabilities: WorkerCapabilities,
    pub compute_groups: Vec<WorkerGroupResponse>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveWorkerRequest {
    pub compute_group_name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WorkerActionResponse {
    pub worker_id: Uuid,
    pub state: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateJobRequest {
    pub name: String,
    /// Immutable OCI image reference selected and approved by the operator.
    pub image_reference: String,
    pub timeout_seconds: i32,
    /// Exact output paths and limits approved as part of the immutable job specification.
    #[serde(default)]
    pub output_requirements: Vec<JobOutputRequirement>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OperatorJobResponse {
    pub job_id: Uuid,
    pub name: String,
    pub image_reference: String,
    pub gpu_count: i32,
    pub timeout_seconds: i32,
    pub status: String,
    pub assigned_worker_id: Option<Uuid>,
    pub submitted_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub failure_message: Option<String>,
    pub output_requirements: Vec<JobOutputRequirement>,
    pub current_attempt: Option<OperatorAttemptIdentity>,
    pub artifacts: Vec<OperatorArtifactResponse>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VerifiedArtifactEvidence {
    /// Decimal Cloud Storage generation. Encoded as a string because generations exceed JSON's
    /// exact integer range.
    pub storage_generation: String,
    pub byte_length: u64,
    pub sha256: String,
    pub crc32c: String,
    pub verification_source: String,
    pub verified_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperatorArtifactStatus {
    Declared,
    Uploading,
    Verifying,
    Verified,
    Rejected,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OperatorArtifactResponse {
    pub artifact_id: Uuid,
    pub attempt_id: Uuid,
    pub attempt_number: i32,
    pub logical_path: String,
    pub role: String,
    pub media_type: String,
    pub mandatory: bool,
    pub max_bytes: u64,
    pub byte_length: u64,
    pub sha256: String,
    pub crc32c: String,
    pub status: OperatorArtifactStatus,
    pub declared_at: DateTime<Utc>,
    pub verified: Option<VerifiedArtifactEvidence>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OperatorArtifactListResponse {
    pub job_id: Uuid,
    pub current_attempt: Option<OperatorAttemptIdentity>,
    pub artifacts: Vec<OperatorArtifactResponse>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct OperatorAttemptIdentity {
    pub attempt_id: Uuid,
    pub attempt_number: i32,
    pub observation_stream: Option<OperatorObservationStream>,
    pub observation_counters: Option<HashMap<String, i64>>,
    pub structured_result: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct OperatorObservationStream {
    pub stream_id: Uuid,
    pub accepted_through_sequence: i64,
    pub mlflow_run_id: Option<String>,
    pub mlflow_created_at: Option<DateTime<Utc>>,
    pub mlflow_last_error: Option<String>,
}

#[derive(FromRow)]
struct PendingRegistrationRecord {
    id: Uuid,
    agent_instance_id: Uuid,
    display_name: String,
    confirmation_code: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    capabilities: serde_json::Value,
}

#[derive(FromRow)]
struct WorkerRecord {
    id: Uuid,
    agent_instance_id: Uuid,
    display_name: String,
    status: String,
    capabilities: serde_json::Value,
    last_seen_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct IdentityRecord {
    id: Uuid,
    role: String,
    disabled_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct JobRecord {
    id: Uuid,
    name: String,
    image_reference: String,
    gpu_count: i32,
    timeout_seconds: i32,
    status: String,
    assigned_worker_id: Option<Uuid>,
    submitted_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    stdout: Option<String>,
    stderr: Option<String>,
    failure_message: Option<String>,
}

#[derive(FromRow)]
struct OperatorArtifactRecord {
    job_id: Uuid,
    artifact_id: Uuid,
    attempt_id: Uuid,
    attempt_number: i32,
    logical_path: String,
    role: String,
    media_type: String,
    mandatory: bool,
    max_bytes: i64,
    byte_length: i64,
    sha256: String,
    crc32c: String,
    visibility_status: String,
    declared_at: DateTime<Utc>,
    verified_storage_generation: Option<i64>,
    verified_byte_length: Option<i64>,
    verified_sha256: Option<String>,
    verified_crc32c: Option<String>,
    verification_source: Option<String>,
    verified_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct OperatorAttemptObservationRecord {
    job_id: Uuid,
    attempt_id: Uuid,
    attempt_number: i32,
    observation_counters: Option<serde_json::Value>,
    structured_result: Option<serde_json::Value>,
    stream_id: Option<Uuid>,
    accepted_through_sequence: Option<i64>,
    mlflow_run_id: Option<String>,
    mlflow_created_at: Option<DateTime<Utc>>,
    mlflow_last_error: Option<String>,
}

impl TryFrom<OperatorArtifactRecord> for OperatorArtifactResponse {
    type Error = OperatorError;

    fn try_from(record: OperatorArtifactRecord) -> Result<Self, Self::Error> {
        let verified = match (
            record.verified_storage_generation,
            record.verified_byte_length,
            record.verified_sha256,
            record.verified_crc32c,
            record.verification_source,
            record.verified_at,
        ) {
            (Some(generation), Some(bytes), Some(sha256), Some(crc32c), Some(source), Some(at)) => {
                Some(VerifiedArtifactEvidence {
                    storage_generation: generation.to_string(),
                    byte_length: u64::try_from(bytes).map_err(|_| OperatorError::internal())?,
                    sha256,
                    crc32c,
                    verification_source: source,
                    verified_at: at,
                })
            }
            (None, None, None, None, None, None) => None,
            _ => return Err(OperatorError::internal()),
        };
        Ok(Self {
            artifact_id: record.artifact_id,
            attempt_id: record.attempt_id,
            attempt_number: record.attempt_number,
            logical_path: record.logical_path,
            role: record.role,
            media_type: record.media_type,
            mandatory: record.mandatory,
            max_bytes: u64::try_from(record.max_bytes).map_err(|_| OperatorError::internal())?,
            byte_length: u64::try_from(record.byte_length)
                .map_err(|_| OperatorError::internal())?,
            sha256: record.sha256,
            crc32c: record.crc32c,
            status: match record.visibility_status.as_str() {
                "declared" => OperatorArtifactStatus::Declared,
                "uploading" => OperatorArtifactStatus::Uploading,
                "verifying" => OperatorArtifactStatus::Verifying,
                "verified" => OperatorArtifactStatus::Verified,
                "rejected" => OperatorArtifactStatus::Rejected,
                _ => return Err(OperatorError::internal()),
            },
            declared_at: record.declared_at,
            verified,
        })
    }
}

impl JobRecord {
    fn into_response(
        self,
        output_requirements: Vec<JobOutputRequirement>,
        visibility: OperatorArtifactListResponse,
    ) -> OperatorJobResponse {
        let record = self;
        OperatorJobResponse {
            job_id: record.id,
            name: record.name,
            image_reference: record.image_reference,
            gpu_count: record.gpu_count,
            timeout_seconds: record.timeout_seconds,
            status: record.status,
            assigned_worker_id: record.assigned_worker_id,
            submitted_at: record.submitted_at,
            started_at: record.started_at,
            finished_at: record.finished_at,
            exit_code: record.exit_code,
            stdout: record.stdout,
            stderr: record.stderr,
            failure_message: record.failure_message,
            output_requirements,
            current_attempt: visibility.current_attempt,
            artifacts: visibility.artifacts,
        }
    }
}

#[derive(Debug)]
pub(crate) struct OperatorError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl OperatorError {
    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    const fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication is required.",
        )
    }

    const fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Operator access is required.",
        )
    }

    const fn invalid_request() -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            "The request is invalid.",
        )
    }

    const fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "registration_not_found",
            "The pending registration was not found.",
        )
    }

    const fn project_required() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "project_required",
            "This account belongs to several projects; the request must name one.",
        )
    }

    const fn conflict() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "registration_not_pending",
            "The registration request is no longer pending.",
        )
    }

    const fn worker_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "worker_not_found",
            "The worker was not found.",
        )
    }

    const fn worker_conflict() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "worker_state_conflict",
            "The worker cannot be changed from its current state.",
        )
    }

    const fn job_conflict() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "job_state_conflict",
            "A succeeded or failed job cannot be cancelled.",
        )
    }

    const fn job_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "job_not_found",
            "The job was not found.",
        )
    }

    const fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "operator_unavailable",
            "Operator access is unavailable.",
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

impl IntoResponse for OperatorError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/worker-enrolments",
    tag = "operator",
    security(("human_bearer" = [])),
    request_body = CreateEnrolmentRequest,
    responses(
        (status = 201, description = "Single-use worker enrolment credential created", body = CreateEnrolmentResponse),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator", body = ErrorResponse),
        (status = 422, description = "Invalid expiry", body = ErrorResponse),
        (status = 503, description = "Authentication or persistence unavailable", body = ErrorResponse)
    )
)]
pub(crate) async fn create_worker_enrolment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateEnrolmentRequest>,
) -> Result<(StatusCode, Json<CreateEnrolmentResponse>), OperatorError> {
    let database = state.database.ok_or_else(OperatorError::unavailable)?;
    let auth = state.human_auth.ok_or_else(OperatorError::unavailable)?;
    let bearer = bearer_token(&headers)?;
    let identity = auth.verify(bearer).await.map_err(|error| match error {
        VerifyError::Rejected => OperatorError::unauthorized(),
        VerifyError::Unavailable => OperatorError::unavailable(),
    })?;
    let expiry_seconds = request.expires_in_seconds.unwrap_or(DEFAULT_EXPIRY_SECONDS);
    if !(MIN_EXPIRY_SECONDS..=MAX_EXPIRY_SECONDS).contains(&expiry_seconds) {
        return Err(OperatorError::invalid_request());
    }

    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let caller = authorize_operator(&mut transaction, &auth, &identity).await?;
    let owner_identity_id = caller.identity_id;
    // The enrolment carries the project forward to the worker that consumes it, which is
    // the only place that path can learn it: the agent presenting the credential has no
    // identity of its own to resolve one from.
    let project_id = caller.sole_project()?;
    let credential =
        credentials::issue(CredentialKind::Enrolment).map_err(|_| OperatorError::internal())?;
    let expires_at = Utc::now()
        .checked_add_signed(TimeDelta::seconds(expiry_seconds))
        .ok_or_else(OperatorError::invalid_request)?;

    sqlx::query(
        "INSERT INTO worker_enrolments \
         (id, owner_identity_id, project_id, token_verifier, expires_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(credential.id)
    .bind(owner_identity_id)
    .bind(project_id)
    .bind(&credential.verifier)
    .bind(expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'human', $2, 'worker.enrolment.created', 'worker_enrolment', $3, \
         'succeeded', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_identity_id)
    .bind(credential.id)
    .bind(json!({ "expires_at": expires_at }))
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;

    Ok((
        StatusCode::CREATED,
        Json(CreateEnrolmentResponse {
            enrolment_credential: credential.plaintext.into_string(),
            expires_at,
        }),
    ))
}

async fn authenticate_operator(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Caller, OperatorError> {
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let auth = state
        .human_auth
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let bearer = bearer_token(headers)?;
    let identity = auth.verify(bearer).await.map_err(|error| match error {
        VerifyError::Rejected => OperatorError::unauthorized(),
        VerifyError::Unavailable => OperatorError::unavailable(),
    })?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let caller = authorize_operator(&mut transaction, auth, &identity).await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok(caller)
}

#[utoipa::path(
    get,
    path = "/api/v1/operator/worker-registration-requests",
    tag = "operator",
    security(("human_bearer" = [])),
    responses(
        (status = 200, description = "Pending worker registration requests", body = [PendingRegistrationResponse]),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator", body = ErrorResponse)
    )
)]
pub(crate) async fn list_worker_registration_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<PendingRegistrationResponse>>, OperatorError> {
    authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let records = sqlx::query_as::<_, PendingRegistrationRecord>(
        "SELECT id, agent_instance_id, display_name, confirmation_code, created_at, expires_at, \
                capabilities FROM worker_registration_requests \
         WHERE approved_at IS NULL AND rejected_at IS NULL AND claimed_at IS NULL \
           AND expires_at > now() ORDER BY created_at",
    )
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    let mut responses = Vec::with_capacity(records.len());
    for record in records {
        responses.push(PendingRegistrationResponse {
            registration_id: record.id,
            agent_instance_id: record.agent_instance_id,
            display_name: record.display_name,
            confirmation_code: record.confirmation_code,
            created_at: record.created_at,
            expires_at: record.expires_at,
            capabilities: serde_json::from_value(record.capabilities)
                .map_err(|_| OperatorError::internal())?,
        });
    }
    Ok(Json(responses))
}

async fn decide_registration(
    state: AppState,
    headers: HeaderMap,
    registration_id: Uuid,
    approve: bool,
) -> Result<Json<RegistrationDecisionResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM worker_registration_requests WHERE id = $1)",
    )
    .bind(registration_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    if !exists {
        return Err(OperatorError::not_found());
    }
    let affected = if approve {
        let mut challenge = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut challenge);
        sqlx::query(
            "UPDATE worker_registration_requests SET approved_at = now(), \
             approved_by_identity_id = $2, claim_challenge = $3 \
             WHERE id = $1 AND approved_at IS NULL AND rejected_at IS NULL \
               AND claimed_at IS NULL AND expires_at > now()",
        )
        .bind(registration_id)
        .bind(caller.identity_id)
        .bind(challenge.as_slice())
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?
        .rows_affected()
    } else {
        sqlx::query(
            "UPDATE worker_registration_requests SET rejected_at = now(), \
             rejected_by_identity_id = $2 WHERE id = $1 AND approved_at IS NULL \
               AND rejected_at IS NULL AND claimed_at IS NULL AND expires_at > now()",
        )
        .bind(registration_id)
        .bind(caller.identity_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?
        .rows_affected()
    };
    if affected != 1 {
        return Err(OperatorError::conflict());
    }
    let action = if approve {
        "worker.registration.approved"
    } else {
        "worker.registration.rejected"
    };
    let decision = if approve { "approved" } else { "rejected" };
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome) \
         VALUES ($1, 'human', $2, $3, 'worker_registration_request', $4, 'succeeded')",
    )
    .bind(Uuid::new_v4())
    .bind(caller.identity_id)
    .bind(action)
    .bind(registration_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok(Json(RegistrationDecisionResponse {
        registration_id,
        state: decision,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/worker-registration-requests/{registration_id}/approve",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("registration_id" = Uuid, Path, description = "Registration request identifier")),
    responses((status = 200, description = "Registration approved", body = RegistrationDecisionResponse))
)]
pub(crate) async fn approve_worker_registration(
    State(state): State<AppState>,
    axum::extract::Path(registration_id): axum::extract::Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<RegistrationDecisionResponse>, OperatorError> {
    decide_registration(state, headers, registration_id, true).await
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/worker-registration-requests/{registration_id}/reject",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("registration_id" = Uuid, Path, description = "Registration request identifier")),
    responses((status = 200, description = "Registration rejected", body = RegistrationDecisionResponse))
)]
pub(crate) async fn reject_worker_registration(
    State(state): State<AppState>,
    axum::extract::Path(registration_id): axum::extract::Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<RegistrationDecisionResponse>, OperatorError> {
    decide_registration(state, headers, registration_id, false).await
}

fn connectivity(last_seen_at: Option<DateTime<Utc>>) -> WorkerConnectivity {
    let Some(last_seen_at) = last_seen_at else {
        return WorkerConnectivity::NeverSeen;
    };
    let age = Utc::now().signed_duration_since(last_seen_at);
    if age <= TimeDelta::seconds(90) {
        WorkerConnectivity::Online
    } else if age <= TimeDelta::minutes(5) {
        WorkerConnectivity::Stale
    } else {
        WorkerConnectivity::Offline
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/operator/workers",
    tag = "operator",
    security(("human_bearer" = [])),
    responses(
        (status = 200, description = "Worker fleet inventory", body = [OperatorWorkerResponse]),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator", body = ErrorResponse)
    )
)]
pub(crate) async fn list_workers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<OperatorWorkerResponse>>, OperatorError> {
    authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let workers = sqlx::query_as::<_, WorkerRecord>(
        "SELECT id, agent_instance_id, display_name, status, capabilities, last_seen_at, created_at \
         FROM workers ORDER BY display_name, created_at",
    )
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    let memberships = sqlx::query_as::<_, (Uuid, Uuid, String)>(
        "SELECT m.worker_id, g.id, g.name FROM compute_group_members m \
         JOIN compute_groups g ON g.id = m.compute_group_id ORDER BY g.name",
    )
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    let mut groups: HashMap<Uuid, Vec<WorkerGroupResponse>> = HashMap::new();
    for (worker_id, id, name) in memberships {
        groups
            .entry(worker_id)
            .or_default()
            .push(WorkerGroupResponse { id, name });
    }
    workers
        .into_iter()
        .map(|worker| {
            let capabilities = serde_json::from_value(worker.capabilities)
                .map_err(|_| OperatorError::internal())?;
            Ok(OperatorWorkerResponse {
                worker_id: worker.id,
                agent_instance_id: worker.agent_instance_id,
                display_name: worker.display_name,
                state: worker.status,
                connectivity: connectivity(worker.last_seen_at),
                last_seen_at: worker.last_seen_at,
                created_at: worker.created_at,
                capabilities,
                compute_groups: groups.remove(&worker.id).unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

async fn approve_and_group_worker(
    state: AppState,
    headers: HeaderMap,
    worker_id: Uuid,
    request: ApproveWorkerRequest,
) -> Result<Json<WorkerActionResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let group_name = request
        .compute_group_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if group_name.is_some_and(|value| value.chars().count() > 100) {
        return Err(OperatorError::invalid_request());
    }
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let worker = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT owner_identity_id, status FROM workers WHERE id = $1 FOR UPDATE",
    )
    .bind(worker_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?
    .ok_or_else(OperatorError::worker_not_found)?;
    if worker.1 == "revoked" {
        return Err(OperatorError::worker_conflict());
    }
    let approving = matches!(worker.1.as_str(), "unapproved" | "quarantined");
    let resulting_state = if approving {
        sqlx::query("UPDATE workers SET status = 'idle', updated_at = now() WHERE id = $1")
            .bind(worker_id)
            .execute(&mut *transaction)
            .await
            .map_err(|_| OperatorError::internal())?;
        "idle"
    } else {
        worker.1.as_str()
    };
    if let Some(group_name) = group_name {
        let group_id: Uuid = sqlx::query_scalar(
            "INSERT INTO compute_groups (id, owner_identity_id, name) VALUES ($1, $2, $3) \
             ON CONFLICT (owner_identity_id, name) DO UPDATE SET name = EXCLUDED.name \
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(worker.0)
        .bind(group_name)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
        sqlx::query(
            "INSERT INTO compute_group_members (compute_group_id, worker_id) VALUES ($1, $2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(group_id)
        .bind(worker_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
    }
    let audit_action = if approving {
        "worker.approved"
    } else {
        "worker.group.added"
    };
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'human', $2, $3, 'worker', $4, 'succeeded', $5)",
    )
    .bind(Uuid::new_v4())
    .bind(caller.identity_id)
    .bind(audit_action)
    .bind(worker_id)
    .bind(json!({ "compute_group_name": group_name }))
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok(Json(WorkerActionResponse {
        worker_id,
        state: resulting_state.to_owned(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/workers/{worker_id}/approve",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("worker_id" = Uuid, Path, description = "Worker identifier")),
    request_body = ApproveWorkerRequest,
    responses((status = 200, description = "Worker approved", body = WorkerActionResponse))
)]
pub(crate) async fn approve_worker(
    State(state): State<AppState>,
    axum::extract::Path(worker_id): axum::extract::Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<ApproveWorkerRequest>,
) -> Result<Json<WorkerActionResponse>, OperatorError> {
    approve_and_group_worker(state, headers, worker_id, request).await
}

async fn change_worker_state(
    state: AppState,
    headers: HeaderMap,
    worker_id: Uuid,
    target_state: &'static str,
) -> Result<Json<WorkerActionResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let affected = sqlx::query(
        "UPDATE workers SET status = $2, updated_at = now() \
         WHERE id = $1 AND status <> 'revoked'",
    )
    .bind(worker_id)
    .bind(target_state)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?
    .rows_affected();
    if affected != 1 {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM workers WHERE id = $1)")
                .bind(worker_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(|_| OperatorError::internal())?;
        return Err(if exists {
            OperatorError::worker_conflict()
        } else {
            OperatorError::worker_not_found()
        });
    }
    if target_state == "revoked" {
        sqlx::query(
            "UPDATE worker_credentials SET revoked_at = now() \
             WHERE worker_id = $1 AND revoked_at IS NULL",
        )
        .bind(worker_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
        cancel_worker_upload_sessions(&mut transaction, worker_id, "worker_or_credential_revoked")
            .await
            .map_err(|_| OperatorError::internal())?;
    }
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome) \
         VALUES ($1, 'human', $2, $3, 'worker', $4, 'succeeded')",
    )
    .bind(Uuid::new_v4())
    .bind(caller.identity_id)
    .bind(format!("worker.{target_state}"))
    .bind(worker_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    if target_state == "revoked"
        && let Some(storage) = &state.artifact_storage
    {
        reconcile_pending_session_cancellations(database, storage).await;
    }
    Ok(Json(WorkerActionResponse {
        worker_id,
        state: target_state.to_owned(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/workers/{worker_id}/quarantine",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("worker_id" = Uuid, Path, description = "Worker identifier")),
    responses((status = 200, description = "Worker quarantined", body = WorkerActionResponse))
)]
pub(crate) async fn quarantine_worker(
    State(state): State<AppState>,
    axum::extract::Path(worker_id): axum::extract::Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<WorkerActionResponse>, OperatorError> {
    change_worker_state(state, headers, worker_id, "quarantined").await
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/workers/{worker_id}/revoke",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("worker_id" = Uuid, Path, description = "Worker identifier")),
    responses((status = 200, description = "Worker and credentials revoked", body = WorkerActionResponse))
)]
pub(crate) async fn revoke_worker(
    State(state): State<AppState>,
    axum::extract::Path(worker_id): axum::extract::Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<WorkerActionResponse>, OperatorError> {
    change_worker_state(state, headers, worker_id, "revoked").await
}

const JOB_COLUMNS: &str = "id, name, image_reference, gpu_count, timeout_seconds, status, assigned_worker_id, \
     submitted_at, started_at, finished_at, exit_code, stdout, stderr, failure_message";

async fn job_output_requirements(
    database: &sqlx::PgPool,
    job_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<JobOutputRequirement>>, OperatorError> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, String, bool, i64)>(
        "SELECT job_id, logical_path, role, media_type, mandatory, max_bytes \
         FROM job_output_requirements WHERE job_id = ANY($1) ORDER BY logical_path",
    )
    .bind(job_ids)
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    let mut by_job = HashMap::new();
    for (job_id, logical_path, role, media_type, mandatory, max_bytes) in rows {
        by_job
            .entry(job_id)
            .or_insert_with(Vec::new)
            .push(JobOutputRequirement {
                logical_path,
                role,
                media_type,
                mandatory,
                max_bytes: u64::try_from(max_bytes).map_err(|_| OperatorError::internal())?,
            });
    }
    Ok(by_job)
}

async fn job_artifact_visibility(
    database: &sqlx::PgPool,
    job_ids: &[Uuid],
) -> Result<HashMap<Uuid, OperatorArtifactListResponse>, OperatorError> {
    let mut by_job: HashMap<Uuid, OperatorArtifactListResponse> = job_ids
        .iter()
        .copied()
        .map(|job_id| {
            (
                job_id,
                OperatorArtifactListResponse {
                    job_id,
                    current_attempt: None,
                    artifacts: Vec::new(),
                },
            )
        })
        .collect();
    let attempts = sqlx::query_as::<_, OperatorAttemptObservationRecord>(
        "SELECT DISTINCT ON (a.job_id) a.job_id, a.id AS attempt_id, a.attempt_number, \
                a.observation_counters, a.structured_result, s.id AS stream_id, \
                s.accepted_through_sequence, s.mlflow_run_id, \
                s.mlflow_created_at, s.mlflow_last_error \
         FROM job_attempts a LEFT JOIN observation_streams s ON s.attempt_id = a.id \
         WHERE a.job_id = ANY($1) ORDER BY a.job_id, a.attempt_number DESC",
    )
    .bind(job_ids)
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    let attempt_ids: Vec<Uuid> = attempts.iter().map(|record| record.attempt_id).collect();
    for attempt in attempts {
        if let Some(visibility) = by_job.get_mut(&attempt.job_id) {
            visibility.current_attempt = Some(OperatorAttemptIdentity {
                attempt_id: attempt.attempt_id,
                attempt_number: attempt.attempt_number,
                observation_stream: attempt
                    .stream_id
                    .map(|stream_id| OperatorObservationStream {
                        stream_id,
                        accepted_through_sequence: attempt
                            .accepted_through_sequence
                            .unwrap_or_default(),
                        mlflow_run_id: attempt.mlflow_run_id,
                        mlflow_created_at: attempt.mlflow_created_at,
                        mlflow_last_error: attempt.mlflow_last_error,
                    }),
                observation_counters: attempt
                    .observation_counters
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|_| OperatorError::internal())?,
                structured_result: attempt.structured_result,
            });
        }
    }
    let records = sqlx::query_as::<_, OperatorArtifactRecord>(
        "SELECT a.job_id, a.id AS artifact_id, a.attempt_id, ja.attempt_number, a.logical_path, \
                a.role, a.media_type, a.mandatory, r.max_bytes, a.byte_length, a.sha256, a.crc32c, \
                CASE WHEN a.status = 'uploading' AND \
                          (a.upload_completed_at IS NOT NULL OR a.protection_pending) \
                     THEN 'verifying' ELSE a.status END AS visibility_status, a.declared_at, \
                CASE WHEN a.status = 'verified' THEN a.verified_storage_generation END \
                    AS verified_storage_generation, \
                CASE WHEN a.status = 'verified' THEN a.verified_byte_length END \
                    AS verified_byte_length, \
                CASE WHEN a.status = 'verified' THEN a.verified_sha256 END AS verified_sha256, \
                CASE WHEN a.status = 'verified' THEN a.verified_crc32c END AS verified_crc32c, \
                CASE WHEN a.status = 'verified' THEN a.verification_source END \
                    AS verification_source, \
                CASE WHEN a.status = 'verified' THEN a.verified_at END AS verified_at \
         FROM job_artifacts a \
         JOIN job_attempts ja ON ja.id = a.attempt_id AND ja.job_id = a.job_id \
         JOIN job_output_requirements r ON r.id = a.output_requirement_id AND r.job_id = a.job_id \
         WHERE a.attempt_id = ANY($1) AND a.status <> 'deleted' \
         ORDER BY a.job_id, a.logical_path",
    )
    .bind(&attempt_ids)
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    for record in records {
        let job_id = record.job_id;
        let artifact = OperatorArtifactResponse::try_from(record)?;
        if let Some(visibility) = by_job.get_mut(&job_id) {
            visibility.artifacts.push(artifact);
        }
    }
    Ok(by_job)
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/jobs",
    tag = "operator",
    security(("human_bearer" = [])),
    request_body = CreateJobRequest,
    responses(
        (status = 201, description = "GPU job queued", body = OperatorJobResponse),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator", body = ErrorResponse),
        (status = 422, description = "Job request is invalid", body = ErrorResponse)
    )
)]
pub(crate) async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<OperatorJobResponse>), OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    // Resolved before anything else, because a caller with no project cannot own a job and
    // one with several has not said which they meant.
    let project_id = caller.sole_project()?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let name = request.name.trim();
    if name.is_empty()
        || name.chars().count() > 120
        || !(30..=3600).contains(&request.timeout_seconds)
        || !crate::registry::immutable_sha256_reference(&request.image_reference)
    {
        return Err(OperatorError::invalid_request());
    }
    validate_output_requirements(&request.output_requirements)
        .map_err(|_| OperatorError::invalid_request())?;
    let id = Uuid::new_v4();
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let query = format!(
        "INSERT INTO jobs \
         (id, owner_identity_id, project_id, name, image_reference, timeout_seconds) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {JOB_COLUMNS}"
    );
    // Both meanings are written, and they are not the same value: the identity records who
    // queued this, the project decides who can see it afterwards.
    let record = sqlx::query_as::<_, JobRecord>(&query)
        .bind(id)
        .bind(caller.identity_id)
        .bind(project_id)
        .bind(name)
        .bind(&request.image_reference)
        .bind(request.timeout_seconds)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
    for output in &request.output_requirements {
        sqlx::query(
            "INSERT INTO job_output_requirements \
             (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(&output.logical_path)
        .bind(&output.role)
        .bind(&output.media_type)
        .bind(output.mandatory)
        .bind(i64::try_from(output.max_bytes).map_err(|_| OperatorError::invalid_request())?)
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
    }
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'human', $2, 'job.queued', 'job', $3, 'succeeded', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(caller.identity_id)
    .bind(id)
    .bind(json!({
        "image_reference": request.image_reference,
        "timeout_seconds": request.timeout_seconds,
        "output_count": request.output_requirements.len()
    }))
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok((
        StatusCode::CREATED,
        Json(record.into_response(
            request.output_requirements,
            OperatorArtifactListResponse {
                job_id: id,
                current_attempt: None,
                artifacts: Vec::new(),
            },
        )),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/operator/jobs",
    tag = "operator",
    security(("human_bearer" = [])),
    responses((status = 200, description = "Jobs owned by the operator", body = [OperatorJobResponse]))
)]
pub(crate) async fn list_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<OperatorJobResponse>>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    reconcile_expired_attempts(database)
        .await
        .map_err(|_| OperatorError::internal())?;
    // Scoped by project, not by who created the row: a co-owner sees the project's jobs,
    // including ones another member queued. `owner_identity_id` stays on the row as the record
    // of who queued it.
    let query = format!(
        "SELECT {JOB_COLUMNS} FROM jobs \
         WHERE project_id = ANY($1) ORDER BY submitted_at DESC LIMIT 100"
    );
    let records = sqlx::query_as::<_, JobRecord>(&query)
        .bind(&caller.project_ids)
        .fetch_all(database)
        .await
        .map_err(|_| OperatorError::internal())?;
    let job_ids: Vec<Uuid> = records.iter().map(|record| record.id).collect();
    let mut requirements = job_output_requirements(database, &job_ids).await?;
    let mut visibility = job_artifact_visibility(database, &job_ids).await?;
    Ok(Json(
        records
            .into_iter()
            .map(|record| {
                let outputs = requirements.remove(&record.id).unwrap_or_default();
                let artifacts =
                    visibility
                        .remove(&record.id)
                        .unwrap_or(OperatorArtifactListResponse {
                            job_id: record.id,
                            current_attempt: None,
                            artifacts: Vec::new(),
                        });
                record.into_response(outputs, artifacts)
            })
            .collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/operator/jobs/{job_id}/artifacts",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("job_id" = Uuid, Path, description = "Job identifier")),
    responses(
        (status = 200, description = "Declared artifacts owned by the operator, without storage locations or upload authority", body = OperatorArtifactListResponse),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator", body = ErrorResponse),
        (status = 404, description = "Job is absent or belongs to another operator", body = ErrorResponse)
    )
)]
pub(crate) async fn list_job_artifacts(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<OperatorArtifactListResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    // Answers identically for a job in another project and a job that does not exist, so the
    // probe cannot be used to discover what else is running.
    let job_in_scope: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE id = $1 AND project_id = ANY($2))",
    )
    .bind(job_id)
    .bind(&caller.project_ids)
    .fetch_one(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    if !job_in_scope {
        return Err(OperatorError::job_not_found());
    }

    let response = job_artifact_visibility(database, &[job_id])
        .await?
        .remove(&job_id)
        .ok_or_else(OperatorError::internal)?;
    Ok(Json(response))
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/jobs/{job_id}/cancel",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("job_id" = Uuid, Path, description = "Job identifier")),
    responses(
        (status = 200, description = "Cancellation requested or completed idempotently", body = OperatorJobResponse),
        (status = 409, description = "Job is already terminal", body = ErrorResponse)
    )
)]
pub(crate) async fn cancel_job(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<OperatorJobResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    reconcile_expired_attempts(database)
        .await
        .map_err(|_| OperatorError::internal())?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let query = format!(
        "UPDATE jobs SET \
             status = CASE WHEN status IN ('assigned', 'running', 'cancelling') \
                           THEN 'cancelling' ELSE 'cancelled' END, \
             cancel_requested_at = COALESCE(cancel_requested_at, now()), \
             finished_at = CASE WHEN status IN ('queued', 'cancelled') \
                                THEN COALESCE(finished_at, now()) ELSE NULL END \
         WHERE id = $1 AND project_id = ANY($2) \
           AND status IN ('queued', 'assigned', 'running', 'cancelling', 'cancelled') \
         RETURNING {JOB_COLUMNS}"
    );
    // Authorisation is fused into the mutation predicate, so a job outside the caller's projects
    // simply does not match and the handler answers exactly as it does for a job in the wrong
    // state. That is deliberate: distinguishing them would say whether it exists.
    let record = sqlx::query_as::<_, JobRecord>(&query)
        .bind(job_id)
        .bind(&caller.project_ids)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?
        .ok_or_else(OperatorError::job_conflict)?;
    cancel_job_upload_sessions(&mut transaction, job_id, "job_cancellation_requested")
        .await
        .map_err(|_| OperatorError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    if let Some(storage) = &state.artifact_storage {
        reconcile_pending_session_cancellations(database, storage).await;
    }
    let outputs = job_output_requirements(database, &[record.id])
        .await?
        .remove(&record.id)
        .unwrap_or_default();
    let visibility = job_artifact_visibility(database, &[record.id])
        .await?
        .remove(&record.id)
        .ok_or_else(OperatorError::internal)?;
    Ok(Json(record.into_response(outputs, visibility)))
}

/// The project every existing resource was migrated into, and the one a first operator joins.
const DEFAULT_PROJECT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_d00f);

/// Who is calling, and what they may act on -- deliberately two fields rather than one id.
///
/// Until projects existed these were the same UUID: the caller's identity was both the thing
/// written into rows and the thing every query filtered on. That conflation is why handlers could
/// record who acted while performing a mutation nobody had checked they were allowed to make, and
/// why nothing flagged it.
///
/// Keeping them apart makes the mistake hard to write rather than merely discouraged. A query
/// filtering on `identity_id` now reads wrongly, and a handler that touches neither field stands
/// out instead of blending in.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    /// Attribution: who did this. Written into rows and audit events, never filtered on.
    pub(crate) identity_id: Uuid,
    /// Authorisation: the projects this caller may act within. Empty authorises nothing.
    pub(crate) project_ids: Vec<Uuid>,
}

impl Caller {
    /// The project to place a new resource in.
    ///
    /// Fails closed while a caller belongs to more than one project: nothing in the request says
    /// which was meant, and guessing would put a resource somewhere its creator cannot see. This
    /// becomes a request field when a second project can exist.
    pub(crate) fn sole_project(&self) -> Result<Uuid, OperatorError> {
        match self.project_ids.as_slice() {
            [only] => Ok(*only),
            [] => Err(OperatorError::forbidden()),
            _ => Err(OperatorError::project_required()),
        }
    }
}

async fn authorize_operator(
    transaction: &mut Transaction<'_, Postgres>,
    auth: &crate::human_auth::HumanAuth,
    identity: &HumanIdentity,
) -> Result<Caller, OperatorError> {
    let existing = sqlx::query_as::<_, IdentityRecord>(
        "SELECT id, role, disabled_at FROM human_identities \
         WHERE provider = 'identity-platform' AND provider_subject = $1 FOR UPDATE",
    )
    .bind(&identity.subject)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| OperatorError::internal())?;

    let record = if let Some(record) = existing {
        sqlx::query("UPDATE human_identities SET display_name = $2, email = $3 WHERE id = $1")
            .bind(record.id)
            .bind(truncate(&identity.display_name, 200))
            .bind(&identity.email)
            .execute(&mut **transaction)
            .await
            .map_err(|_| OperatorError::internal())?;
        record
    } else {
        if !auth.is_bootstrap_operator(identity) {
            return Err(OperatorError::forbidden());
        }
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities \
             (id, provider, provider_subject, display_name, email, role) \
             VALUES ($1, 'identity-platform', $2, $3, $4, 'operator')",
        )
        .bind(id)
        .bind(&identity.subject)
        .bind(truncate(&identity.display_name, 200))
        .bind(&identity.email)
        .execute(&mut **transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
        // In the same transaction as the identity itself, so a first operator cannot be created
        // into a state where it owns nothing: it would authenticate perfectly, see an empty
        // console, and have no way to tell that from having no data.
        sqlx::query("INSERT INTO project_memberships (project_id, identity_id) VALUES ($1, $2)")
            .bind(DEFAULT_PROJECT_ID)
            .bind(id)
            .execute(&mut **transaction)
            .await
            .map_err(|_| OperatorError::internal())?;
        IdentityRecord {
            id,
            role: "operator".to_owned(),
            disabled_at: None,
        }
    };

    if record.disabled_at.is_some() || record.role != "operator" {
        return Err(OperatorError::forbidden());
    }

    // Read once here rather than per query, so every handler in a request authorises against the
    // same set and a revocation cannot take effect halfway through one.
    let project_ids = sqlx::query_as::<_, (Uuid,)>(
        "SELECT project_id FROM project_memberships \
         WHERE identity_id = $1 AND revoked_at IS NULL",
    )
    .bind(record.id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|_| OperatorError::internal())?
    .into_iter()
    .map(|row| row.0)
    .collect();

    Ok(Caller {
        identity_id: record.id,
        project_ids,
    })
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, OperatorError> {
    let value = headers
        .get(AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .ok_or_else(OperatorError::unauthorized)?;
    value
        .strip_prefix("Bearer ")
        .filter(|token| !token.trim().is_empty() && !token.contains(char::is_whitespace))
        .ok_or_else(OperatorError::unauthorized)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{HeaderMap, HeaderValue, Request, StatusCode, header::AUTHORIZATION},
    };
    use chrono::{Duration, Utc};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::{
        app_with_human_auth,
        credentials::{self, CredentialKind},
        human_auth::{ClientAuthConfig, HumanAuth, HumanIdentity, IdentityVerifier, VerifyError},
    };

    use super::bearer_token;

    struct FakeVerifier {
        identity: HumanIdentity,
    }

    #[async_trait]
    impl IdentityVerifier for FakeVerifier {
        async fn verify(&self, id_token: &str) -> Result<HumanIdentity, VerifyError> {
            if id_token == "valid-token" {
                Ok(self.identity.clone())
            } else {
                Err(VerifyError::Rejected)
            }
        }
    }

    fn healthy_worker_capabilities() -> Value {
        json!({
            "protocol_version": "1.0",
            "collected_at": "2026-09-20T08:00:00Z",
            "hostname": "eligible-gpu-worker",
            "operating_system": "linux",
            "operating_system_version": "6.8",
            "architecture": "x86_64",
            "logical_cpu_count": 16,
            "memory_total_bytes": 34_359_738_368_u64,
            "storage_available_bytes": 107_374_182_400_u64,
            "python_version": "3.12.7",
            "gpus": [{
                "index": 0,
                "name": "Test GPU",
                "memory_total_bytes": 8_589_934_592_u64,
                "driver_version": "560.35"
            }],
            "gpu_health": {
                "status": "healthy",
                "detail": "GPU computation passed on Test GPU in 10.000 ms",
                "evidence": {
                    "schema_version": "1.0",
                    "status": "healthy",
                    "checked_at": "2026-09-20T07:59:59Z",
                    "image_reference": concat!(
                        "example.test/health@sha256:",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    ),
                    "device_index": 0,
                    "device_name": "Test GPU",
                    "operation": "matrix multiplication",
                    "matrix_size": 512,
                    "max_absolute_error": 0.0,
                    "duration_ms": 10.0,
                    "cuda_driver_api_version": "13.3",
                    "cuda_runtime_version": "12.9"
                }
            }
        })
    }

    #[test]
    fn bearer_tokens_are_strictly_parsed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer token-value"),
        );
        assert_eq!(bearer_token(&headers).unwrap(), "token-value");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("bearer token-value"),
        );
        assert!(bearer_token(&headers).is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn bootstrap_operator_can_issue_a_hashed_single_use_credential(pool: PgPool) {
        let verifier = FakeVerifier {
            identity: HumanIdentity {
                subject: "stable-subject".to_owned(),
                email: "operator@example.com".to_owned(),
                display_name: "Test Operator".to_owned(),
            },
        };
        let auth = HumanAuth::new(
            Arc::new(verifier),
            "operator@example.com",
            ClientAuthConfig {
                api_key: "test-api-key".to_owned(),
                auth_domain: "example.test".to_owned(),
                project_id: "test-project".to_owned(),
            },
        );
        let response = app_with_human_auth(None, Some(pool.clone()), Some(auth))
            .oneshot(
                Request::post("/api/v1/operator/worker-enrolments")
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        let plaintext = response["enrolment_credential"].as_str().unwrap();
        assert!(plaintext.starts_with("ken_"));

        let (role, stored_verifier): (String, String) = sqlx::query_as(
            "SELECT i.role, e.token_verifier FROM human_identities i \
             JOIN worker_enrolments e ON e.owner_identity_id = i.id \
             WHERE i.provider_subject = 'stable-subject'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "operator");
        assert_ne!(stored_verifier, plaintext);
        assert!(credentials::verify(plaintext, &stored_verifier));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn operator_can_list_and_approve_a_pending_machine(pool: PgPool) {
        let verifier = FakeVerifier {
            identity: HumanIdentity {
                subject: "stable-subject".to_owned(),
                email: "operator@example.com".to_owned(),
                display_name: "Test Operator".to_owned(),
            },
        };
        let auth = HumanAuth::new(
            Arc::new(verifier),
            "operator@example.com",
            ClientAuthConfig {
                api_key: "test-api-key".to_owned(),
                auth_domain: "example.test".to_owned(),
                project_id: "test-project".to_owned(),
            },
        );
        let operator_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities \
             (id, provider, provider_subject, display_name, email, role) \
             VALUES ($1, 'identity-platform', 'stable-subject', 'Test Operator', \
                     'operator@example.com', 'operator')",
        )
        .bind(caller.identity_id)
        .execute(&pool)
        .await
        .unwrap();
        let registration_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO worker_registration_requests \
             (id, agent_instance_id, display_name, protocol_version, capabilities, public_key, \
              confirmation_code, expires_at) VALUES ($1, $2, 'Rented GPU', '1.0', $3, $4, \
              'ABCD-2345', $5)",
        )
        .bind(registration_id)
        .bind(Uuid::new_v4())
        .bind(json!({
            "protocol_version": "1.0",
            "collected_at": "2026-09-19T08:00:00Z",
            "hostname": "rented-gpu",
            "operating_system": "linux",
            "operating_system_version": "6.8",
            "architecture": "x86_64",
            "logical_cpu_count": 8,
            "memory_total_bytes": 16_000_000_000_u64,
            "storage_available_bytes": 100_000_000_000_u64,
            "python_version": "3.12",
            "gpus": [],
            "gpu_health": { "status": "unverified", "detail": "pending" }
        }))
        .bind([1_u8; 32].as_slice())
        .bind(Utc::now() + Duration::minutes(15))
        .execute(&pool)
        .await
        .unwrap();

        let router = app_with_human_auth(None, Some(pool.clone()), Some(auth));
        let listed = router
            .clone()
            .oneshot(
                Request::get("/api/v1/operator/worker-registration-requests")
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let body = to_bytes(listed.into_body(), 1024 * 1024).await.unwrap();
        let registrations: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(registrations[0]["confirmation_code"], "ABCD-2345");

        let approved = router
            .oneshot(
                Request::post(format!(
                    "/api/v1/operator/worker-registration-requests/{registration_id}/approve"
                ))
                .header(AUTHORIZATION, "Bearer valid-token")
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(approved.status(), StatusCode::OK);
        let stored: (bool, Option<Uuid>) = sqlx::query_as(
            "SELECT claim_challenge IS NOT NULL, approved_by_identity_id \
             FROM worker_registration_requests WHERE id = $1",
        )
        .bind(registration_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, (true, Some(caller.identity_id)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn operator_can_inventory_group_quarantine_and_revoke_a_worker(pool: PgPool) {
        let verifier = FakeVerifier {
            identity: HumanIdentity {
                subject: "fleet-operator".to_owned(),
                email: "operator@example.com".to_owned(),
                display_name: "Fleet Operator".to_owned(),
            },
        };
        let auth = HumanAuth::new(
            Arc::new(verifier),
            "operator@example.com",
            ClientAuthConfig {
                api_key: "test-api-key".to_owned(),
                auth_domain: "example.test".to_owned(),
                project_id: "test-project".to_owned(),
            },
        );
        let operator_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities \
             (id, provider, provider_subject, display_name, email, role) \
             VALUES ($1, 'identity-platform', 'fleet-operator', 'Fleet Operator', \
                     'operator@example.com', 'operator')",
        )
        .bind(caller.identity_id)
        .execute(&pool)
        .await
        .unwrap();
        let worker_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, \
              capabilities, last_seen_at) VALUES ($1, $2, $3, 'THESHED2', '1.0', $4, now())",
        )
        .bind(worker_id)
        .bind(caller.identity_id)
        .bind(Uuid::new_v4())
        .bind(json!({
            "protocol_version": "1.0",
            "collected_at": "2026-09-19T08:00:00Z",
            "hostname": "theshed2",
            "operating_system": "linux",
            "operating_system_version": "6.8",
            "architecture": "x86_64",
            "logical_cpu_count": 16,
            "memory_total_bytes": 32_000_000_000_u64,
            "storage_available_bytes": 100_000_000_000_u64,
            "python_version": "3.12",
            "gpus": [{
                "index": 0,
                "name": "Test GPU",
                "memory_total_bytes": 12_000_000_000_u64,
                "driver_version": "1.0"
            }],
            "gpu_health": { "status": "unverified", "detail": "pending" }
        }))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, $3)",
        )
        .bind(Uuid::new_v4())
        .bind(worker_id)
        .bind("test-verifier")
        .execute(&pool)
        .await
        .unwrap();

        let router = app_with_human_auth(None, Some(pool.clone()), Some(auth));
        let inventory = router
            .clone()
            .oneshot(
                Request::get("/api/v1/operator/workers")
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(inventory.status(), StatusCode::OK);
        let body = to_bytes(inventory.into_body(), 1024 * 1024).await.unwrap();
        let workers: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(workers[0]["connectivity"], "online");
        assert_eq!(workers[0]["capabilities"]["gpus"][0]["name"], "Test GPU");

        let approved = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/operator/workers/{worker_id}/approve"))
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"compute_group_name":"Home"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(approved.status(), StatusCode::OK);
        let (status, group): (String, String) = sqlx::query_as(
            "SELECT w.status, g.name FROM workers w \
             JOIN compute_group_members m ON m.worker_id = w.id \
             JOIN compute_groups g ON g.id = m.compute_group_id WHERE w.id = $1",
        )
        .bind(worker_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((status.as_str(), group.as_str()), ("idle", "Home"));

        for action in ["quarantine", "revoke"] {
            let response = router
                .clone()
                .oneshot(
                    Request::post(format!("/api/v1/operator/workers/{worker_id}/{action}"))
                        .header(AUTHORIZATION, "Bearer valid-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let (status, revoked): (String, bool) = sqlx::query_as(
            "SELECT w.status, c.revoked_at IS NOT NULL FROM workers w \
             JOIN worker_credentials c ON c.worker_id = w.id WHERE w.id = $1",
        )
        .bind(worker_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((status.as_str(), revoked), ("revoked", true));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn cancelled_queued_job_is_idempotent_and_never_assigned(pool: PgPool) {
        let operator_id = Uuid::new_v4();
        let auth = HumanAuth::new(
            Arc::new(FakeVerifier {
                identity: HumanIdentity {
                    subject: "job-operator".to_owned(),
                    email: "operator@example.com".to_owned(),
                    display_name: "Job Operator".to_owned(),
                },
            }),
            "operator@example.com",
            ClientAuthConfig {
                api_key: "test-api-key".to_owned(),
                auth_domain: "example.test".to_owned(),
                project_id: "test-project".to_owned(),
            },
        );
        sqlx::query(
            "INSERT INTO human_identities \
             (id, provider, provider_subject, display_name, email, role) \
             VALUES ($1, 'identity-platform', 'job-operator', 'Job Operator', \
                     'operator@example.com', 'operator')",
        )
        .bind(caller.identity_id)
        .execute(&pool)
        .await
        .unwrap();

        let worker_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, \
              status, capabilities) VALUES ($1, $2, $3, 'Eligible GPU', '1.0', 'idle', $4)",
        )
        .bind(worker_id)
        .bind(caller.identity_id)
        .bind(Uuid::new_v4())
        .bind(healthy_worker_capabilities())
        .execute(&pool)
        .await
        .unwrap();
        let worker_credential = credentials::issue(CredentialKind::Worker).unwrap();
        sqlx::query(
            "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, $3)",
        )
        .bind(worker_credential.id)
        .bind(worker_id)
        .bind(&worker_credential.verifier)
        .execute(&pool)
        .await
        .unwrap();

        let router = app_with_human_auth(None, Some(pool.clone()), Some(auth));
        let created = router
            .clone()
            .oneshot(
                Request::post("/api/v1/operator/jobs")
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "name": "Cancelled before assignment",
                            "image_reference": concat!(
                                "example.test/work@sha256:",
                                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            ),
                            "timeout_seconds": 120
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = to_bytes(created.into_body(), 1024 * 1024).await.unwrap();
        let created: Value = serde_json::from_slice(&created).unwrap();
        let job_id = Uuid::parse_str(created["job_id"].as_str().unwrap()).unwrap();

        let mut first_finished_at = None;
        for _ in 0..2 {
            let cancelled = router
                .clone()
                .oneshot(
                    Request::post(format!("/api/v1/operator/jobs/{job_id}/cancel"))
                        .header(AUTHORIZATION, "Bearer valid-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(cancelled.status(), StatusCode::OK);
            let cancelled = to_bytes(cancelled.into_body(), 1024 * 1024).await.unwrap();
            let cancelled: Value = serde_json::from_slice(&cancelled).unwrap();
            assert_eq!(cancelled["status"], "cancelled");
            assert_eq!(cancelled["assigned_worker_id"], Value::Null);
            let finished_at = cancelled["finished_at"].as_str().unwrap().to_owned();
            if let Some(first) = &first_finished_at {
                assert_eq!(&finished_at, first);
            } else {
                first_finished_at = Some(finished_at);
            }
        }

        let heartbeat = router
            .oneshot(
                Request::put(format!("/api/v1/workers/{worker_id}/heartbeat"))
                    .header(
                        AUTHORIZATION,
                        format!("Bearer {}", worker_credential.plaintext.expose()),
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "protocol_version": "1.0",
                            "sequence": 1,
                            "observed_at": Utc::now(),
                            "capabilities": healthy_worker_capabilities()
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::OK);
        let heartbeat = to_bytes(heartbeat.into_body(), 1024 * 1024).await.unwrap();
        let heartbeat: Value = serde_json::from_slice(&heartbeat).unwrap();
        assert_eq!(heartbeat["state"], "idle");
        assert_eq!(heartbeat["assignment"], Value::Null);

        let persisted: (String, bool, i64) = sqlx::query_as(
            "SELECT j.status, j.finished_at IS NOT NULL, count(a.id) \
             FROM jobs j LEFT JOIN job_attempts a ON a.job_id = j.id WHERE j.id = $1 \
             GROUP BY j.status, j.finished_at",
        )
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(persisted, ("cancelled".to_owned(), true, 0));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn active_cancellation_is_idempotent_and_finishes_when_the_lease_expires(pool: PgPool) {
        let operator_id = Uuid::new_v4();
        let auth = HumanAuth::new(
            Arc::new(FakeVerifier {
                identity: HumanIdentity {
                    subject: "cancellation-operator".to_owned(),
                    email: "operator@example.com".to_owned(),
                    display_name: "Cancellation Operator".to_owned(),
                },
            }),
            "operator@example.com",
            ClientAuthConfig {
                api_key: "test-api-key".to_owned(),
                auth_domain: "example.test".to_owned(),
                project_id: "test-project".to_owned(),
            },
        );
        sqlx::query(
            "INSERT INTO human_identities \
             (id, provider, provider_subject, display_name, email, role) \
             VALUES ($1, 'identity-platform', 'cancellation-operator', \
                     'Cancellation Operator', 'operator@example.com', 'operator')",
        )
        .bind(caller.identity_id)
        .execute(&pool)
        .await
        .unwrap();

        let worker_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, \
              status, capabilities) VALUES ($1, $2, $3, 'Busy GPU', '1.0', 'busy', $4)",
        )
        .bind(worker_id)
        .bind(caller.identity_id)
        .bind(Uuid::new_v4())
        .bind(healthy_worker_capabilities())
        .execute(&pool)
        .await
        .unwrap();
        let worker_credential = credentials::issue(CredentialKind::Worker).unwrap();
        sqlx::query(
            "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, $3)",
        )
        .bind(worker_credential.id)
        .bind(worker_id)
        .bind(&worker_credential.verifier)
        .execute(&pool)
        .await
        .unwrap();

        let job_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO jobs \
             (id, owner_identity_id, name, image_reference, timeout_seconds, status, assigned_worker_id) \
             VALUES ($1, $2, 'Cancel active work', $3, 120, 'running', $4)",
        )
        .bind(job_id)
        .bind(caller.identity_id)
        .bind(format!("example.test/work@sha256:{}", "b".repeat(64)))
        .bind(worker_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, status, started_at, lease_expires_at) \
             VALUES ($1, $2, 1, $3, 'running', now(), now() + interval '5 minutes')",
        )
        .bind(attempt_id)
        .bind(job_id)
        .bind(worker_id)
        .execute(&pool)
        .await
        .unwrap();

        let router = app_with_human_auth(None, Some(pool.clone()), Some(auth));
        let mut first_requested_at = None;
        for _ in 0..2 {
            let cancelled = router
                .clone()
                .oneshot(
                    Request::post(format!("/api/v1/operator/jobs/{job_id}/cancel"))
                        .header(AUTHORIZATION, "Bearer valid-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(cancelled.status(), StatusCode::OK);
            let cancelled = to_bytes(cancelled.into_body(), 1024 * 1024).await.unwrap();
            let cancelled: Value = serde_json::from_slice(&cancelled).unwrap();
            assert_eq!(cancelled["status"], "cancelling");
            assert_eq!(cancelled["finished_at"], Value::Null);
            let requested_at: chrono::DateTime<Utc> =
                sqlx::query_scalar("SELECT cancel_requested_at FROM jobs WHERE id = $1")
                    .bind(job_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if let Some(first) = first_requested_at {
                assert_eq!(requested_at, first);
            } else {
                first_requested_at = Some(requested_at);
            }
        }

        let heartbeat = router
            .clone()
            .oneshot(
                Request::put(format!("/api/v1/workers/{worker_id}/heartbeat"))
                    .header(
                        AUTHORIZATION,
                        format!("Bearer {}", worker_credential.plaintext.expose()),
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "protocol_version": "1.0",
                            "sequence": 1,
                            "observed_at": Utc::now(),
                            "capabilities": healthy_worker_capabilities()
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::OK);
        let heartbeat = to_bytes(heartbeat.into_body(), 1024 * 1024).await.unwrap();
        let heartbeat: Value = serde_json::from_slice(&heartbeat).unwrap();
        assert_eq!(heartbeat["assignment"], Value::Null);

        sqlx::query(
            "UPDATE job_attempts SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
        )
        .bind(attempt_id)
        .execute(&pool)
        .await
        .unwrap();
        let listed = router
            .clone()
            .oneshot(
                Request::get("/api/v1/operator/jobs")
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let listed = to_bytes(listed.into_body(), 1024 * 1024).await.unwrap();
        let listed: Value = serde_json::from_slice(&listed).unwrap();
        assert_eq!(listed[0]["status"], "cancelled");

        let stale_result = router
            .oneshot(
                Request::put(format!(
                    "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/result"
                ))
                .header(
                    AUTHORIZATION,
                    format!("Bearer {}", worker_credential.plaintext.expose()),
                )
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "exit_code": 0,
                        "timed_out": false,
                        "stdout": "late success",
                        "stderr": "",
                        "failure_message": null
                    })
                    .to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale_result.status(), StatusCode::OK);
        let stale_result = to_bytes(stale_result.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let stale_result: Value = serde_json::from_slice(&stale_result).unwrap();
        assert_eq!(stale_result["status"], "cancelled");

        let persisted: (String, bool, Option<Uuid>, String, bool, String) = sqlx::query_as(
            "SELECT j.status, j.finished_at IS NOT NULL, j.assigned_worker_id, \
                    a.status, a.finished_at IS NOT NULL, w.status \
             FROM jobs j JOIN job_attempts a ON a.job_id = j.id \
             JOIN workers w ON w.id = a.worker_id WHERE j.id = $1",
        )
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            persisted,
            (
                "cancelled".to_owned(),
                true,
                None,
                "cancelled".to_owned(),
                true,
                "idle".to_owned()
            )
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn artifact_visibility_is_owner_scoped_and_excludes_storage_locations(pool: PgPool) {
        let owner_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let auth = HumanAuth::new(
            Arc::new(FakeVerifier {
                identity: HumanIdentity {
                    subject: "artifact-owner".to_owned(),
                    email: "owner@example.com".to_owned(),
                    display_name: "Artifact Owner".to_owned(),
                },
            }),
            "owner@example.com",
            ClientAuthConfig {
                api_key: "test-api-key".to_owned(),
                auth_domain: "example.test".to_owned(),
                project_id: "test-project".to_owned(),
            },
        );
        for (id, subject, email) in [
            (owner_id, "artifact-owner", "owner@example.com"),
            (other_id, "other-owner", "other@example.com"),
        ] {
            sqlx::query(
                "INSERT INTO human_identities \
                 (id, provider, provider_subject, display_name, email, role) \
                 VALUES ($1, 'identity-platform', $2, $2, $3, 'operator')",
            )
            .bind(id)
            .bind(subject)
            .bind(email)
            .execute(&pool)
            .await
            .unwrap();
        }
        let job_id = Uuid::new_v4();
        let other_job_id = Uuid::new_v4();
        for (id, owner) in [(job_id, owner_id), (other_job_id, other_id)] {
            sqlx::query(
                "INSERT INTO jobs (id, owner_identity_id, name, image_reference, timeout_seconds) \
                 VALUES ($1, $2, 'Artifact visibility', $3, 120)",
            )
            .bind(id)
            .bind(owner)
            .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
            .execute(&pool)
            .await
            .unwrap();
        }
        let worker_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities) \
             VALUES ($1, $2, $3, 'Artifact worker', '1.1', 'busy', $4)",
        )
        .bind(worker_id)
        .bind(owner_id)
        .bind(Uuid::new_v4())
        .bind(healthy_worker_capabilities())
        .execute(&pool)
        .await
        .unwrap();
        let attempt_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, status, started_at, lease_expires_at) \
             VALUES ($1, $2, 1, $3, 'running', now(), now() + interval '5 minutes')",
        )
        .bind(attempt_id)
        .bind(job_id)
        .bind(worker_id)
        .execute(&pool)
        .await
        .unwrap();
        let stream_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO observation_streams \
             (id, attempt_id, job_id, worker_id, accepted_through_sequence, mlflow_run_id, mlflow_created_at) \
             VALUES ($1, $2, $3, $4, 17, 'mlflow-run-17', now())",
        )
        .bind(stream_id)
        .bind(attempt_id)
        .bind(job_id)
        .bind(worker_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE job_attempts SET observation_counters = \
             '{\"dropped.rate\":2}'::jsonb, structured_result = \
             '{\"model\":\"smolvla\",\"final_loss\":0.125}'::jsonb WHERE id = $1",
        )
        .bind(attempt_id)
        .execute(&pool)
        .await
        .unwrap();
        let manifest_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO job_artifact_manifests (id, attempt_id, job_id) VALUES ($1, $2, $3)",
        )
        .bind(manifest_id)
        .bind(attempt_id)
        .bind(job_id)
        .execute(&pool)
        .await
        .unwrap();
        for (index, status) in ["verified", "uploading"].into_iter().enumerate() {
            let requirement_id = Uuid::new_v4();
            let path = format!("outputs/result-{index}.bin");
            sqlx::query(
                "INSERT INTO job_output_requirements \
                 (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
                 VALUES ($1, $2, $3, 'model', 'application/octet-stream', $4, 2048)",
            )
            .bind(requirement_id)
            .bind(job_id)
            .bind(&path)
            .bind(index == 0)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO job_artifacts \
                 (id, manifest_id, attempt_id, job_id, output_requirement_id, logical_path, role, \
                  media_type, mandatory, byte_length, sha256, crc32c, object_key, status, \
                  storage_bucket, storage_generation, uploaded_byte_length, uploaded_crc32c, \
                  upload_started_at, upload_completed_at, verified_storage_generation, \
                  verified_byte_length, verified_crc32c, verified_sha256, verification_source, \
                  verified_at, protection_pending, state_reason) \
                 VALUES ($1, $2, $3, $4, $5, $6, 'model', 'application/octet-stream', $7, 512, \
                         $8, 'ImIEBA==', $9, $10, 'private-artifacts', 9007199254740993, 512, 'ImIEBA==', \
                         now(), now(), 9007199254740993, 512, 'ImIEBA==', $8, 'gcs_metadata', now(), $11, $12)",
            )
            .bind(Uuid::new_v4())
            .bind(manifest_id)
            .bind(attempt_id)
            .bind(job_id)
            .bind(requirement_id)
            .bind(&path)
            .bind(index == 0)
            .bind("b".repeat(64))
            .bind(format!(
                "v1/owners/{owner_id}/jobs/{job_id}/private-{index}"
            ))
            .bind(status)
            .bind(status == "uploading")
            .bind((status == "uploading").then_some("gcs_protection_pending"))
            .execute(&pool)
            .await
            .unwrap();
        }

        let router = app_with_human_auth(None, Some(pool.clone()), Some(auth));
        let response = router
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/operator/jobs/{job_id}/artifacts"))
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body_text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body_text.contains("private-artifacts"));
        assert!(!body_text.contains("private-0"));
        assert!(!body_text.contains("session_uri"));
        let body: Value = serde_json::from_str(&body_text).unwrap();
        assert_eq!(body["job_id"], job_id.to_string());
        assert_eq!(
            body["current_attempt"]["attempt_id"],
            attempt_id.to_string()
        );
        assert_eq!(body["current_attempt"]["attempt_number"], 1);
        assert_eq!(
            body["current_attempt"]["observation_stream"]["stream_id"],
            stream_id.to_string()
        );
        assert_eq!(
            body["current_attempt"]["observation_stream"]["accepted_through_sequence"],
            17
        );
        assert_eq!(
            body["current_attempt"]["observation_stream"]["mlflow_run_id"],
            "mlflow-run-17"
        );
        assert_eq!(
            body["current_attempt"]["observation_counters"]["dropped.rate"],
            2
        );
        assert_eq!(
            body["current_attempt"]["structured_result"],
            json!({"model": "smolvla", "final_loss": 0.125})
        );
        assert_eq!(body["artifacts"][0]["status"], "verified");
        assert_eq!(body["artifacts"][0]["mandatory"], true);
        assert_eq!(
            body["artifacts"][0]["verified"]["storage_generation"],
            "9007199254740993"
        );
        assert_eq!(body["artifacts"][0]["verified"]["sha256"], "b".repeat(64));
        assert_eq!(body["artifacts"][1]["status"], "verifying");
        assert_eq!(body["artifacts"][1]["verified"], Value::Null);

        let initial_batch = router
            .clone()
            .oneshot(
                Request::get("/api/v1/operator/jobs")
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let initial_batch = to_bytes(initial_batch.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let initial_batch_text = String::from_utf8(initial_batch.to_vec()).unwrap();
        assert!(!initial_batch_text.contains("private-artifacts"));
        assert!(!initial_batch_text.contains("private-0"));
        let initial_batch: Value = serde_json::from_str(&initial_batch_text).unwrap();
        let initial_job = initial_batch
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["job_id"] == job_id.to_string())
            .unwrap();
        assert_eq!(
            initial_job["artifacts"][0]["verified"]["storage_generation"],
            "9007199254740993"
        );

        // A retry with no manifest is the current attempt. The older verified output must not be
        // presented as current in either the batch job view or the per-job endpoint.
        sqlx::query("UPDATE job_attempts SET status = 'failed', finished_at = now() WHERE id = $1")
            .bind(attempt_id)
            .execute(&pool)
            .await
            .unwrap();
        let retry_attempt_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, status, started_at, lease_expires_at) \
             VALUES ($1, $2, 2, $3, 'running', now(), now() + interval '5 minutes')",
        )
        .bind(retry_attempt_id)
        .bind(job_id)
        .bind(worker_id)
        .execute(&pool)
        .await
        .unwrap();
        let listed = router
            .clone()
            .oneshot(
                Request::get("/api/v1/operator/jobs")
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let listed = to_bytes(listed.into_body(), 1024 * 1024).await.unwrap();
        let listed: Value = serde_json::from_slice(&listed).unwrap();
        let listed_job = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["job_id"] == job_id.to_string())
            .unwrap();
        assert_eq!(
            listed_job["current_attempt"]["attempt_id"],
            retry_attempt_id.to_string()
        );
        assert_eq!(listed_job["current_attempt"]["attempt_number"], 2);
        assert_eq!(listed_job["artifacts"], json!([]));

        let retry_visibility = router
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/operator/jobs/{job_id}/artifacts"))
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let retry_visibility = to_bytes(retry_visibility.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let retry_visibility: Value = serde_json::from_slice(&retry_visibility).unwrap();
        assert_eq!(retry_visibility["current_attempt"]["attempt_number"], 2);
        assert_eq!(retry_visibility["artifacts"], json!([]));

        let hidden = router
            .oneshot(
                Request::get(format!("/api/v1/operator/jobs/{other_job_id}/artifacts"))
                    .header(AUTHORIZATION, "Bearer valid-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    }
}

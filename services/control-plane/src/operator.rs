use std::collections::HashMap;

use axum::{
    Json,
    extract::State,
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
    credentials::{self, CredentialKind},
    human_auth::{HumanIdentity, VerifyError},
    registry::{ErrorResponse, WorkerCapabilities},
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
    let owner_identity_id = authorize_operator(&mut transaction, &auth, &identity).await?;
    let credential =
        credentials::issue(CredentialKind::Enrolment).map_err(|_| OperatorError::internal())?;
    let expires_at = Utc::now()
        .checked_add_signed(TimeDelta::seconds(expiry_seconds))
        .ok_or_else(OperatorError::invalid_request)?;

    sqlx::query(
        "INSERT INTO worker_enrolments (id, owner_identity_id, token_verifier, expires_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(credential.id)
    .bind(owner_identity_id)
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
) -> Result<Uuid, OperatorError> {
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
    let operator_id = authorize_operator(&mut transaction, auth, &identity).await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok(operator_id)
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
    let operator_id = authenticate_operator(&state, &headers).await?;
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
        .bind(operator_id)
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
        .bind(operator_id)
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
    .bind(operator_id)
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
    let operator_id = authenticate_operator(&state, &headers).await?;
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
    .bind(operator_id)
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
    let operator_id = authenticate_operator(&state, &headers).await?;
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
    }
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome) \
         VALUES ($1, 'human', $2, $3, 'worker', $4, 'succeeded')",
    )
    .bind(Uuid::new_v4())
    .bind(operator_id)
    .bind(format!("worker.{target_state}"))
    .bind(worker_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
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

async fn authorize_operator(
    transaction: &mut Transaction<'_, Postgres>,
    auth: &crate::human_auth::HumanAuth,
    identity: &HumanIdentity,
) -> Result<Uuid, OperatorError> {
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
        IdentityRecord {
            id,
            role: "operator".to_owned(),
            disabled_at: None,
        }
    };

    if record.disabled_at.is_some() || record.role != "operator" {
        return Err(OperatorError::forbidden());
    }
    Ok(record.id)
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
        app_with_human_auth, credentials,
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
        .bind(operator_id)
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
        assert_eq!(stored, (true, Some(operator_id)));
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
        .bind(operator_id)
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
        .bind(operator_id)
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
}

//! Per-project scheduling policy. Today that is only the concurrency limit the scheduler enforces
//! in `registry::project_at_capacity`; null means unlimited, and there is no default quota.

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::HeaderMap,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    operator::{OperatorError, authenticate_operator},
    registry::{ACTIVE_PROJECT_JOBS, ErrorResponse},
};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateSchedulingPolicyRequest {
    /// Required. A positive integer sets the limit; an explicit null removes it. Omitting the
    /// field is rejected rather than read as null, so a partial client cannot clear a limit.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<i32>, required = true, minimum = 1)]
    max_concurrent_jobs: Option<Option<i64>>,
}

/// Distinguishes an explicit null from an absent field, which serde otherwise conflates.
fn present<'de, D>(deserializer: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<i64>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct SchedulingPolicyResponse {
    project_id: Uuid,
    /// Null means unlimited.
    max_concurrent_jobs: Option<i32>,
    /// Jobs assigned, running or cancelling, which are the jobs holding a slot.
    active_jobs: i64,
}

/// The requested limit, or `None` to clear it. Zero, negative, oversized and absent are refused.
fn requested_limit(value: Option<Option<i64>>) -> Result<Option<i32>, OperatorError> {
    match value {
        None => Err(OperatorError::invalid_request()),
        Some(None) => Ok(None),
        Some(Some(value)) => i32::try_from(value)
            .ok()
            .filter(|limit| *limit > 0)
            .map(Some)
            .ok_or_else(OperatorError::invalid_request),
    }
}

async fn count_active_jobs(
    connection: &mut PgConnection,
    project_id: Uuid,
) -> Result<i64, OperatorError> {
    sqlx::query_scalar(ACTIVE_PROJECT_JOBS)
        .bind(project_id)
        .fetch_one(connection)
        .await
        .map_err(|_| OperatorError::internal())
}

#[utoipa::path(
    get,
    path = "/api/v1/operator/scheduling-policy",
    tag = "operator",
    security(("human_bearer" = [])),
    responses(
        (status = 200, description = "Concurrency limit and active job count for the caller's project", body = SchedulingPolicyResponse),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator, or belongs to no project", body = ErrorResponse),
        (status = 409, description = "The caller belongs to several projects", body = ErrorResponse),
    )
)]
pub(crate) async fn get_scheduling_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SchedulingPolicyResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let project_id = caller.sole_project()?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut connection = database
        .acquire()
        .await
        .map_err(|_| OperatorError::internal())?;
    let max_concurrent_jobs: Option<i32> =
        sqlx::query_scalar("SELECT max_concurrent_jobs FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| OperatorError::internal())?;
    let active_jobs = count_active_jobs(&mut connection, project_id).await?;
    Ok(Json(SchedulingPolicyResponse {
        project_id,
        max_concurrent_jobs,
        active_jobs,
    }))
}

/// Set or clear the caller's project concurrency limit.
///
/// Lowering the limit never preempts: jobs already holding a slot finish through their normal
/// lifecycle, and new assignments wait until the active count falls below the new limit.
#[utoipa::path(
    put,
    path = "/api/v1/operator/scheduling-policy",
    tag = "operator",
    security(("human_bearer" = [])),
    request_body = UpdateSchedulingPolicyRequest,
    responses(
        (status = 200, description = "Policy updated", body = SchedulingPolicyResponse),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator, or is not a member of the project", body = ErrorResponse),
        (status = 409, description = "The caller belongs to several projects", body = ErrorResponse),
        (status = 422, description = "Missing field, or a limit that is not a positive integer", body = ErrorResponse),
    )
)]
pub(crate) async fn update_scheduling_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<UpdateSchedulingPolicyRequest>, JsonRejection>,
) -> Result<Json<SchedulingPolicyResponse>, OperatorError> {
    // Authenticate before looking at the body, so an unauthenticated caller learns nothing about
    // which payloads would be accepted.
    let caller = authenticate_operator(&state, &headers).await?;
    let project_id = caller.sole_project()?;
    let Json(request) = payload.map_err(|_| OperatorError::invalid_request())?;
    let limit = requested_limit(request.max_concurrent_jobs)?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;

    // The same lock the scheduler takes before counting, so a change lands entirely before or
    // entirely after any assignment decision. Membership removal takes this row `FOR UPDATE`,
    // which conflicts, so rechecking membership under it cannot race a concurrent revocation.
    let previous: Option<i32> = sqlx::query_scalar(
        "SELECT max_concurrent_jobs FROM projects WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(project_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    let still_member: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 FROM project_memberships m \
             JOIN human_identities i ON i.id = m.identity_id \
             WHERE m.project_id = $1 AND m.identity_id = $2 AND m.revoked_at IS NULL \
               AND i.role = 'operator' AND i.disabled_at IS NULL)",
    )
    .bind(project_id)
    .bind(caller.identity_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    if !still_member {
        return Err(OperatorError::forbidden());
    }

    sqlx::query("UPDATE projects SET max_concurrent_jobs = $2 WHERE id = $1")
        .bind(project_id)
        .bind(limit)
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'human', $2, 'project.scheduling_policy.updated', 'project', $3, \
         'succeeded', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(caller.identity_id)
    .bind(project_id)
    .bind(json!({
        "previous_max_concurrent_jobs": previous,
        "max_concurrent_jobs": limit,
    }))
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    let active_jobs = count_active_jobs(&mut transaction, project_id).await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;

    Ok(Json(SchedulingPolicyResponse {
        project_id,
        max_concurrent_jobs: limit,
        active_jobs,
    }))
}

#[cfg(test)]
mod tests;

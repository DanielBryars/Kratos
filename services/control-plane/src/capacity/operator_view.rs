//! The read-only operator view of capacity that Kratos cannot settle by itself.
//!
//! Both lists mean "a machine may be running and Kratos has stopped trying", which is the
//! situation that costs money quietly, so a person has to be able to see them before any real
//! provider is switched on. This is only the seeing: settling an unreconciled request is a
//! judgement about something Kratos could not determine, and has no route here.
//!
//! Scoped to the caller's projects like every other operator list. Membership is broad access
//! within a project; it is not made into sight of anybody else's capacity.

use axum::{Json, extract::State, http::HeaderMap};
use serde::Serialize;
use utoipa::ToSchema;

use super::{AmbiguousProvision, OutstandingRelease, enabled, needing_attention};
use crate::{
    AppState,
    operator::{OperatorError, authenticate_operator},
    registry::ErrorResponse,
};

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct CapacityAttentionResponse {
    /// Whether this instance would dispatch to a provider at all. Rows can exist while it is off,
    /// and switching it off does not settle them.
    provider_enabled: bool,
    /// Asked for and never usably answered: a machine may or may not exist. Oldest first.
    ambiguous_provisions: Vec<AmbiguousProvision>,
    /// Releases Kratos has stopped retrying: the machine is presumed alive. Oldest first.
    outstanding_releases: Vec<OutstandingRelease>,
}

/// Capacity requests in the caller's projects that need a person.
#[utoipa::path(
    get,
    path = "/api/v1/operator/capacity/attention",
    tag = "operator",
    security(("human_bearer" = [])),
    responses(
        (status = 200, description = "Both lists, read from one snapshot. Read-only.", body = CapacityAttentionResponse),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator", body = ErrorResponse),
        (status = 500, description = "The capacity tables could not be read", body = ErrorResponse),
        (status = 503, description = "Authentication or persistence unavailable", body = ErrorResponse)
    )
)]
pub(crate) async fn capacity_attention(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CapacityAttentionResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let (ambiguous_provisions, outstanding_releases) =
        needing_attention(database, &caller.project_ids)
            .await
            .map_err(|_| OperatorError::internal())?;
    Ok(Json(CapacityAttentionResponse {
        provider_enabled: enabled(),
        ambiguous_provisions,
        outstanding_releases,
    }))
}

#[cfg(test)]
mod tests;

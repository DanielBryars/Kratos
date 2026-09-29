//! Server-side idempotency for job submission (TRN-011).
//!
//! A client that cannot tell whether its submission arrived -- a dropped connection, a timed-out
//! proxy, a reloaded tab -- may send it again with the same `Idempotency-Key` and be answered with
//! the job it already queued instead of a second one. The guarantee is the database's, not this
//! module's: a partial unique index on (project, submitter, key) decides which of two simultaneous
//! submissions creates the job, and the loser reads the winner's committed row.

use axum::http::{HeaderMap, StatusCode};
use chrono::SecondsFormat;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    Caller, CreateJobRequest, JOB_COLUMNS, JobRecord, OperatorError, OperatorJobResponse,
    job_artifact_visibility, job_output_requirements,
};
use crate::{
    artifacts::JobOutputRequirement, dataset_inputs::JobDatasetInputRequest,
    registry::reconcile_expired_attempts,
};

#[cfg(test)]
mod tests;

pub(crate) const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";

/// Bumped if the canonical form ever changes, so a fingerprint from one form can never be read as
/// agreeing with a request canonicalised by another.
const FINGERPRINT_VERSION: u32 = 1;

impl OperatorError {
    const fn invalid_idempotency_key() -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_idempotency_key",
            "The Idempotency-Key header must be a single hyphenated UUID.",
        )
    }

    const fn idempotency_key_reused() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "idempotency_key_reused",
            "This Idempotency-Key was already used for a different job request.",
        )
    }
}

/// The caller's idempotency key, if it sent one.
///
/// Absent is not an error: clients that predate the header keep creating a job per request. Present
/// but unusable is, because silently ignoring a malformed key would turn the retry the caller
/// believes is safe into a duplicate job. Two headers are refused rather than choosing one.
pub(super) fn idempotency_key(headers: &HeaderMap) -> Result<Option<Uuid>, OperatorError> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(OperatorError::invalid_idempotency_key());
    }
    let text = value
        .to_str()
        .map_err(|_| OperatorError::invalid_idempotency_key())?;
    // Only the hyphenated form, so the simple, braced and URN spellings of one UUID are not three
    // different-looking ways to send the same key. Case is irrelevant: the parsed value is stored.
    if text.len() != 36 {
        return Err(OperatorError::invalid_idempotency_key());
    }
    Uuid::try_parse(text)
        .map(Some)
        .map_err(|_| OperatorError::invalid_idempotency_key())
}

#[derive(Serialize)]
struct CanonicalJobRequest<'a> {
    version: u32,
    name: &'a str,
    image_reference: &'a str,
    timeout_seconds: i32,
    earliest_start_at: Option<String>,
    output_requirements: Vec<CanonicalOutput<'a>>,
    dataset_inputs: Vec<CanonicalDatasetInput<'a>>,
}

#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalOutput<'a> {
    logical_path: &'a str,
    role: &'a str,
    media_type: &'a str,
    mandatory: bool,
    max_bytes: u64,
}

#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalDatasetInput<'a> {
    alias: &'a str,
    dataset_version_id: Uuid,
    dataset_view_id: Option<Uuid>,
}

/// SHA-256 of the request in a canonical form, as 64 lowercase hex digits.
///
/// The canonical request comparison includes every submitted field: the name is
/// trimmed as it is before storage, omitted collections and omitted `earliest_start_at` equal their
/// explicit empty and null forms, the start time is compared as a UTC instant at full input precision regardless of its offset, and outputs and dataset inputs are
/// sorted because neither list's order means anything once stored.
///
/// Every request type is destructured field by field, so adding a field to any of them fails to
/// compile here instead of quietly leaving the new field out of the comparison.
pub(super) fn fingerprint(request: &CreateJobRequest) -> Result<String, OperatorError> {
    let CreateJobRequest {
        name,
        image_reference,
        timeout_seconds,
        earliest_start_at,
        output_requirements,
        dataset_inputs,
    } = request;
    let mut outputs: Vec<CanonicalOutput<'_>> = output_requirements
        .iter()
        .map(|output| {
            let JobOutputRequirement {
                logical_path,
                role,
                media_type,
                mandatory,
                max_bytes,
            } = output;
            CanonicalOutput {
                logical_path,
                role,
                media_type,
                mandatory: *mandatory,
                max_bytes: *max_bytes,
            }
        })
        .collect();
    outputs.sort_unstable();
    let mut inputs: Vec<CanonicalDatasetInput<'_>> = dataset_inputs
        .iter()
        .map(|input| {
            let JobDatasetInputRequest {
                alias,
                dataset_version_id,
                dataset_view_id,
            } = input;
            CanonicalDatasetInput {
                alias,
                dataset_version_id: *dataset_version_id,
                dataset_view_id: *dataset_view_id,
            }
        })
        .collect();
    inputs.sort_unstable();
    let canonical = CanonicalJobRequest {
        version: FINGERPRINT_VERSION,
        name: name.trim(),
        image_reference,
        timeout_seconds: *timeout_seconds,
        earliest_start_at: earliest_start_at
            .map(|time| time.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        output_requirements: outputs,
        dataset_inputs: inputs,
    };
    let bytes = serde_json::to_vec(&canonical).map_err(|_| OperatorError::internal())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Answer a submission whose key another request already used, from the committed job.
///
/// Called only after `INSERT .. ON CONFLICT DO NOTHING` returned no row, which `PostgreSQL` does only
/// once the conflicting row is committed: an in-flight winner is waited for, and a winner that rolls
/// back lets the insert through instead. So the row read here exists and is final.
///
/// The lookup is scoped to the caller's current project and identity, both freshly authorised by
/// this request, so a replay cannot reach a job its sender may no longer see. Nothing is
/// revalidated against the job: in particular dataset readiness was decided when the job was
/// created, and a version that has since changed does not un-create it.
pub(super) async fn replay(
    database: &PgPool,
    caller: &Caller,
    project_id: Uuid,
    key: Uuid,
    fingerprint: &str,
) -> Result<OperatorJobResponse, OperatorError> {
    let (job_id, stored_fingerprint) = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, submission_fingerprint FROM jobs \
         WHERE project_id = $1 AND owner_identity_id = $2 AND submission_idempotency_key = $3",
    )
    .bind(project_id)
    .bind(caller.identity_id)
    .bind(key)
    .fetch_optional(database)
    .await
    .map_err(|_| OperatorError::internal())?
    .ok_or_else(OperatorError::internal)?;
    if stored_fingerprint != fingerprint {
        return Err(OperatorError::idempotency_key_reused());
    }
    // The same freshness the job list gives, so a replay never reports an attempt as running after
    // its lease has already lapsed.
    reconcile_expired_attempts(database)
        .await
        .map_err(|_| OperatorError::internal())?;
    let query = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = $1");
    let record = sqlx::query_as::<_, JobRecord>(&query)
        .bind(job_id)
        .fetch_one(database)
        .await
        .map_err(|_| OperatorError::internal())?;
    let outputs = job_output_requirements(database, &[job_id])
        .await?
        .remove(&job_id)
        .unwrap_or_default();
    let inputs = crate::dataset_inputs::load_job_dataset_inputs(database, &[job_id])
        .await?
        .remove(&job_id)
        .unwrap_or_default();
    let visibility = job_artifact_visibility(database, &[job_id])
        .await?
        .remove(&job_id)
        .ok_or_else(OperatorError::internal)?;
    Ok(record.into_response(outputs, inputs, visibility))
}

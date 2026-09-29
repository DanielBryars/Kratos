//! Protect the last project slot from repeated overtaking across protocol capabilities.
//! No peer worker is locked: scheduling already holds this worker then its project, and
//! locking a peer here would deadlock two concurrent heartbeats. Peer availability is a
//! bounded hint; assignment still happens only in that peer's own authenticated heartbeat.

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    ApiError, GpuHealthStatus, HEARTBEAT_INTERVAL_SECONDS, WorkerCapabilities, database_error,
    protocol_minor, validate_capabilities,
};

type QueuedJob = (Uuid, String, String, i32);

/// Three heartbeat intervals. A disconnected peer can delay a newer job only until this
/// window expires; replaying an old heartbeat does not refresh `last_seen_at`.
const PEER_FRESHNESS_SECONDS: u32 = HEARTBEAT_INTERVAL_SECONDS * 3;

async fn peer_protocol_ceiling(
    transaction: &mut Transaction<'_, Postgres>,
    worker_id: Uuid,
    current_minor: u32,
) -> Result<u32, ApiError> {
    let peers = sqlx::query_as::<_, (String, serde_json::Value)>(
        "SELECT w.protocol_version, w.capabilities FROM workers w \
         WHERE w.project_id = (SELECT project_id FROM workers WHERE id = $1) \
           AND w.id <> $1 AND w.status = 'idle' \
           AND w.last_seen_at > statement_timestamp() - make_interval(secs => $2) \
           AND w.last_seen_at <= statement_timestamp() \
           AND EXISTS (SELECT 1 FROM worker_credentials c WHERE c.worker_id = w.id \
               AND c.revoked_at IS NULL \
               AND (c.expires_at IS NULL OR c.expires_at > statement_timestamp())) \
           AND NOT EXISTS (SELECT 1 FROM job_attempts a WHERE a.worker_id = w.id \
               AND a.status IN ('assigned', 'running'))",
    )
    .bind(worker_id)
    .bind(f64::from(PEER_FRESHNESS_SECONDS))
    .fetch_all(&mut **transaction)
    .await
    .map_err(|error| database_error(&error, "read fairness peers"))?;

    let mut ceiling = current_minor;
    for (protocol, value) in peers {
        let Ok(capabilities) = serde_json::from_value::<WorkerCapabilities>(value) else {
            continue;
        };
        if capabilities.protocol_version == protocol
            && capabilities.gpu_health.status == GpuHealthStatus::Healthy
            && !capabilities.gpus.is_empty()
            && validate_capabilities(&capabilities).is_ok()
        {
            ceiling = ceiling.max(protocol_minor(&protocol).unwrap_or(0));
        }
    }
    Ok(ceiling)
}

pub(super) async fn select_queued_job(
    transaction: &mut Transaction<'_, Postgres>,
    worker_id: Uuid,
    protocol: &str,
    protect_last_slot: bool,
) -> Result<Option<QueuedJob>, ApiError> {
    let current_minor = protocol_minor(protocol).ok_or_else(ApiError::unsupported_protocol)?;
    let ceiling = if protect_last_slot {
        peer_protocol_ceiling(transaction, worker_id, current_minor).await?
    } else {
        current_minor
    };
    // Current single-GPU protocol features are cumulative. Derive the requirement once for
    // both this worker and peer eligibility. If placement gains non-cumulative constraints,
    // replace the ceiling with exact worker/job compatibility; do not extend it by guessing.
    let queued = sqlx::query_as::<_, (Uuid, String, String, i32, i32)>(
        "SELECT j.id, j.name, j.image_reference, j.timeout_seconds, r.required_minor \
         FROM jobs j CROSS JOIN LATERAL (SELECT CASE \
             WHEN EXISTS (SELECT 1 FROM job_dataset_inputs i WHERE i.job_id = j.id) THEN 3 \
             WHEN EXISTS (SELECT 1 FROM job_output_requirements o WHERE o.job_id = j.id) THEN 1 \
             ELSE 0 END AS required_minor) r \
         WHERE j.status = 'queued' AND j.gpu_count = 1 \
           AND (j.earliest_start_at IS NULL OR j.earliest_start_at <= statement_timestamp()) \
           AND j.project_id = (SELECT project_id FROM workers WHERE id = $1) \
           AND (SELECT count(*) FROM job_attempts a WHERE a.job_id = j.id) < j.max_attempts \
           AND r.required_minor <= $2 \
         ORDER BY j.submitted_at, j.id FOR UPDATE OF j SKIP LOCKED LIMIT 1",
    )
    .bind(worker_id)
    .bind(i64::from(ceiling))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| database_error(&error, "select fair queued job"))?;

    Ok(queued.and_then(|(id, name, image, timeout, required)| {
        (i64::from(required) <= i64::from(current_minor)).then_some((id, name, image, timeout))
    }))
}

#[cfg(test)]
mod tests;

//! The capacity-provider boundary, and a fake provider.
//!
//! Kratos stays the authoritative queue. A provider supplies bounded machines and decides nothing
//! about fairness, budgets, job state or result identity — so everything durable lives here, and
//! the provider is asked questions rather than trusted to remember answers.
//!
//! Nothing in this module starts any capacity. There is no cloud dependency, no Terraform and no
//! spend: the boundary and a fake exist so the behaviour can be settled before anything costs
//! money, and the feature is off unless explicitly enabled.
//!
//! The two properties worth stating plainly, because they are what the schema is shaped around:
//!
//! *A request is replayable, not repeatable.* Its idempotency key is derived from the attempt, so
//! asking twice asks the same question. A provider that already has that machine returns it
//! rather than building a second one.
//!
//! *Deciding and telling are separate.* The request's status is what Kratos believes; a dispatch
//! is an attempt to make the provider agree. Collapsing them would make a crash between the two
//! indistinguishable from never having decided.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

pub mod fake;

#[cfg(test)]
mod tests;

/// The first half of the advisory-lock key, so these locks cannot collide with the projector's.
const CONTROLLER_LOCK_NAMESPACE: i32 = 0x4b52_4341;
/// How many times a dispatch is tried before it is left failed for a person to look at.
const MAX_DISPATCH_ATTEMPTS: i32 = 8;
/// Bounded so a pathological provider message cannot fill the column or the console.
const MAX_ERROR_BYTES: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum CapacityError {
    #[error("the provider is unavailable")]
    Unavailable,
    #[error("the provider refused: {0}")]
    Refused(String),
    #[error("capacity state is inconsistent: {0}")]
    Inconsistent(String),
}

/// What a provider is asked for.
///
/// Deliberately small. Anything a provider could decide for itself — placement, pricing, retries —
/// is absent, because a provider that decided those would be making scheduling decisions that
/// belong to Kratos.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacitySpec {
    /// Derived from the attempt. The provider must treat two requests with one key as one machine.
    pub idempotency_key: String,
    pub attempt_id: Uuid,
    pub job_id: Uuid,
}

/// What the provider calls the machine it made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionedCapacity {
    pub external_id: String,
}

/// The seam a real provider would implement.
///
/// `provision` must be idempotent on `idempotency_key`: called twice with one key it returns the
/// same `external_id` and creates nothing further. That is what makes a restart mid-dispatch safe,
/// and it is the single most important thing a real implementation has to get right.
///
/// `release` must be idempotent on `external_id`, and releasing something already gone is success
/// rather than an error — otherwise a crash after releasing leaves a request that can never
/// converge.
#[async_trait]
pub trait CapacityProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn provision(&self, spec: &CapacitySpec) -> Result<ProvisionedCapacity, CapacityError>;

    async fn release(&self, external_id: &str) -> Result<(), CapacityError>;
}

/// The lifecycle of one request, as the database records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestStatus {
    Requested,
    Provisioning,
    Ready,
    Releasing,
    Released,
    Failed,
}

impl RequestStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Releasing => "releasing",
            Self::Released => "released",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for RequestStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The key a provider is asked to deduplicate on.
///
/// One attempt is one machine. Derived rather than random so that a controller which has lost its
/// memory — a restart, a second instance — computes the same key and is therefore answered with
/// the same machine.
#[must_use]
pub fn idempotency_key(attempt_id: Uuid) -> String {
    format!("kratos-attempt-{attempt_id}")
}

#[derive(Debug, sqlx::FromRow)]
struct DispatchRow {
    id: Uuid,
    request_id: Uuid,
    action: String,
    attempts: i32,
}

#[derive(Debug, sqlx::FromRow)]
struct RequestRow {
    id: Uuid,
    attempt_id: Uuid,
    job_id: Uuid,
    status: String,
    external_id: Option<String>,
    idempotency_key: String,
    provider: String,
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    value.chars().take(limit).collect()
}

/// Ask for capacity for one attempt, if it does not already have a request.
///
/// Returns the request id. Calling twice for one attempt returns the existing request rather than
/// creating a second, which is the whole point of keying on the attempt.
///
/// # Errors
/// Returns an error when the database cannot be reached or the attempt does not exist.
pub async fn request_capacity(
    pool: &PgPool,
    attempt_id: Uuid,
    provider: &str,
) -> Result<Uuid, CapacityError> {
    let key = idempotency_key(attempt_id);
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;

    // The attempt supplies the job and project rather than the caller, so a request cannot be
    // filed against a job it does not belong to.
    let context = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT j.id, j.project_id FROM job_attempts a JOIN jobs j ON j.id = a.job_id \
         WHERE a.id = $1",
    )
    .bind(attempt_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?
    .ok_or_else(|| CapacityError::Inconsistent("no such attempt".to_owned()))?;

    let request_id = Uuid::new_v4();
    // ON CONFLICT rather than a check-then-insert: two controllers asking at once must produce one
    // request, and the unique constraint is the only thing that can promise that.
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO capacity_requests \
         (id, attempt_id, job_id, project_id, provider, idempotency_key) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (attempt_id) DO UPDATE SET updated_at = now() \
         RETURNING id",
    )
    .bind(request_id)
    .bind(attempt_id)
    .bind(context.0)
    .bind(context.1)
    .bind(provider)
    .bind(&key)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;

    // The intention to tell the provider, recorded in the same transaction as the decision.
    sqlx::query(
        "INSERT INTO capacity_dispatches (id, request_id, action) VALUES ($1, $2, 'provision') \
         ON CONFLICT (request_id, action) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;

    transaction
        .commit()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    Ok(id)
}

/// Begin releasing a request's capacity, whatever state it is in.
///
/// Converges rather than commands: a request still provisioning is marked `releasing` and the
/// release is dispatched once an `external_id` exists, because there is nothing to release until
/// the provider has answered. A request that never reached the provider is released immediately,
/// since nothing was ever made.
///
/// # Errors
/// Returns an error when the database cannot be reached.
pub async fn release_capacity(pool: &PgPool, attempt_id: Uuid) -> Result<(), CapacityError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    let request = sqlx::query_as::<_, RequestRow>(
        "SELECT id, attempt_id, job_id, status, external_id, idempotency_key, provider \
         FROM capacity_requests WHERE attempt_id = $1 FOR UPDATE",
    )
    .bind(attempt_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    let Some(request) = request else {
        // Nothing was ever asked for, which is the ordinary case for a job cancelled while queued.
        return Ok(());
    };
    if matches!(request.status.as_str(), "released" | "failed") {
        return Ok(());
    }

    if request.external_id.is_some() {
        sqlx::query(
            "UPDATE capacity_requests SET status = 'releasing', updated_at = now() WHERE id = $1",
        )
        .bind(request.id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
        sqlx::query(
            "INSERT INTO capacity_dispatches (id, request_id, action) VALUES ($1, $2, 'release') \
             ON CONFLICT (request_id, action) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(request.id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    } else {
        // Nothing exists to release. Cancel the provision instead of chasing a machine that was
        // never made, and converge straight to released.
        sqlx::query(
            "DELETE FROM capacity_dispatches WHERE request_id = $1 AND completed_at IS NULL",
        )
        .bind(request.id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
        sqlx::query(
            "UPDATE capacity_requests \
             SET status = 'released', released_at = now(), updated_at = now() WHERE id = $1",
        )
        .bind(request.id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    }

    transaction
        .commit()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    Ok(())
}

/// Run every dispatch that is due, and return how many were completed.
///
/// One controller handles one dispatch at a time: authority is an advisory lock, exactly as the
/// `MLflow` projector takes it, because the control plane runs as more than one instance and two
/// of them sending the same provision is the failure this whole design exists to prevent.
pub async fn dispatch_once(pool: &PgPool, provider: &dyn CapacityProvider) -> u64 {
    // Only this provider's requests, and only dispatches with attempts left. The provider is
    // persisted on the request because a machine can only be released by the provider that made
    // it; dispatching whichever provider object the caller happened to pass would send one
    // provider another's identifiers.
    let due = match sqlx::query_as::<_, DispatchRow>(
        "SELECT d.id, d.request_id, d.action, d.attempts \
         FROM capacity_dispatches d JOIN capacity_requests r ON r.id = d.request_id \
         WHERE d.completed_at IS NULL AND d.next_attempt_at <= now() \
           AND d.attempts < $2 AND r.provider = $1 \
         ORDER BY d.next_attempt_at LIMIT 100",
    )
    .bind(provider.name())
    .bind(MAX_DISPATCH_ATTEMPTS)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            warn!(%error, "could not list due capacity dispatches");
            return 0;
        }
    };

    let mut completed = 0;
    for dispatch in due {
        // Failures are recorded inside `run_locked`, while authority is still held.
        if matches!(run_locked(pool, provider, &dispatch).await, Ok(true)) {
            completed += 1;
        }
    }
    completed
}

/// Hold one request's dispatch authority for the duration of the call.
async fn run_locked(
    pool: &PgPool,
    provider: &dyn CapacityProvider,
    dispatch: &DispatchRow,
) -> Result<bool, CapacityError> {
    let mut lock = pool
        .acquire()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    let key = dispatch.request_id.to_string();
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, hashtext($2))")
        .bind(CONTROLLER_LOCK_NAMESPACE)
        .bind(&key)
        .fetch_one(&mut *lock)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    if !held {
        // Another controller has it. The next pass will find it if it is still due.
        return Ok(false);
    }

    let outcome = run_dispatch(pool, provider, dispatch).await;
    // Bookkeeping belongs inside the lock, not after it. Released first, this controller's give-up
    // can land after another has already provisioned successfully -- marking `failed` a request
    // that holds a live machine. `failed` is excluded from the open-request index, so the machine
    // would be both unreleased and invisible.
    if let Err(error) = &outcome {
        warn!(dispatch = %dispatch.id, %error, "capacity dispatch failed");
        record_failure(pool, dispatch, &error.to_string()).await;
    }

    let _ = sqlx::query("SELECT pg_advisory_unlock($1, hashtext($2))")
        .bind(CONTROLLER_LOCK_NAMESPACE)
        .bind(&key)
        .execute(&mut *lock)
        .await;
    outcome
}

async fn run_dispatch(
    pool: &PgPool,
    provider: &dyn CapacityProvider,
    dispatch: &DispatchRow,
) -> Result<bool, CapacityError> {
    // Re-read under the lock: another controller may have moved this on between the listing and
    // the lock being granted, and re-sending a completed provision is a second machine. The whole
    // read is re-checked, not just completion -- a dispatch another controller has failed and
    // backed off is no longer due, and acting on the stale `attempts` read would let this
    // controller's bookkeeping overwrite theirs.
    let still_due: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM capacity_dispatches \
         WHERE id = $1 AND completed_at IS NULL AND next_attempt_at <= now() AND attempts = $2",
    )
    .bind(dispatch.id)
    .bind(dispatch.attempts)
    .fetch_optional(pool)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    if still_due.is_none() {
        return Ok(false);
    }

    let request = sqlx::query_as::<_, RequestRow>(
        "SELECT id, attempt_id, job_id, status, external_id, idempotency_key, provider \
         FROM capacity_requests WHERE id = $1",
    )
    .bind(dispatch.request_id)
    .fetch_one(pool)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;

    // The persisted provider is the authority, not whichever provider object the caller supplied.
    // Belt and braces with the filter in the due query: an `external_id` means nothing to a
    // provider that did not issue it, and asking the wrong one to release it either errors or --
    // worse -- succeeds against something else entirely.
    if request.provider != provider.name() {
        warn!(
            request = %request.id,
            persisted = %request.provider,
            dispatching = %provider.name(),
            "refusing to dispatch capacity through a different provider"
        );
        return Ok(false);
    }

    match dispatch.action.as_str() {
        "provision" => provision(pool, provider, dispatch, &request).await,
        "release" => release(pool, provider, dispatch, &request).await,
        other => Err(CapacityError::Inconsistent(format!(
            "unknown dispatch action {other}"
        ))),
    }
}

async fn provision(
    pool: &PgPool,
    provider: &dyn CapacityProvider,
    dispatch: &DispatchRow,
    request: &RequestRow,
) -> Result<bool, CapacityError> {
    // A request that is already releasing was cancelled while this dispatch was queued. Asking
    // for the machine now would create one nobody wants.
    if matches!(request.status.as_str(), "releasing" | "released" | "failed") {
        complete(pool, dispatch.id).await?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE capacity_requests SET status = 'provisioning', updated_at = now() \
         WHERE id = $1 AND status = 'requested'",
    )
    .bind(request.id)
    .execute(pool)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;

    let spec = CapacitySpec {
        idempotency_key: request.idempotency_key.clone(),
        attempt_id: request.attempt_id,
        job_id: request.job_id,
    };
    let provisioned = provider.provision(&spec).await?;

    // The external id is written once and never overwritten: it is the only handle by which the
    // machine can be released, and a second value would strand the first.
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    // The previous status decides what happens next, so it is returned rather than assumed. A
    // request cancelled while the provider was answering is `releasing` or already `released`:
    // the machine exists now, so it must be handed back rather than left ready for nobody, and a
    // released request must not be resurrected by its own late answer.
    let previous: String = sqlx::query_scalar(
        "UPDATE capacity_requests \
         SET external_id = COALESCE(external_id, $2), \
             status = CASE \
                 WHEN status IN ('releasing', 'released') THEN 'releasing' \
                 WHEN status = 'failed' THEN 'failed' \
                 ELSE 'ready' END, \
             released_at = CASE WHEN status IN ('releasing', 'released') THEN NULL \
                                ELSE released_at END, \
             last_error = NULL, updated_at = now() \
         WHERE id = $1 \
         RETURNING (SELECT status FROM capacity_requests WHERE id = $1)",
    )
    .bind(request.id)
    .bind(&provisioned.external_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    sqlx::query("UPDATE capacity_dispatches SET completed_at = now() WHERE id = $1")
        .bind(dispatch.id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    // Cancelled while the provider was answering: the machine exists now, so it has to be released
    // rather than forgotten. Includes a request already marked released, whose machine would
    // otherwise run on with nothing pointing at it.
    if matches!(previous.as_str(), "releasing" | "released") {
        sqlx::query(
            "INSERT INTO capacity_dispatches (id, request_id, action) VALUES ($1, $2, 'release') \
             ON CONFLICT (request_id, action) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(request.id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    info!(request = %request.id, external_id = %provisioned.external_id, "capacity provisioned");
    Ok(true)
}

async fn release(
    pool: &PgPool,
    provider: &dyn CapacityProvider,
    dispatch: &DispatchRow,
    request: &RequestRow,
) -> Result<bool, CapacityError> {
    let Some(external_id) = &request.external_id else {
        // Nothing was ever made -- including the case of a request that failed before the provider
        // answered. Converge rather than wait for a handle that will never arrive, and do not ask
        // the provider to release something that does not exist.
        mark_released(pool, dispatch.id, request.id).await?;
        return Ok(true);
    };
    provider.release(external_id).await?;
    mark_released(pool, dispatch.id, request.id).await?;
    info!(request = %request.id, external_id = %external_id, "capacity released");
    Ok(true)
}

async fn mark_released(
    pool: &PgPool,
    dispatch_id: Uuid,
    request_id: Uuid,
) -> Result<(), CapacityError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    // `failed` is left alone: it is a state a person is meant to see, and a request that failed
    // before the provider answered holds nothing, so there is no machine being hidden by leaving
    // it. The dispatch below completes either way, so the release stops being retried.
    sqlx::query(
        "UPDATE capacity_requests \
         SET status = 'released', released_at = now(), last_error = NULL, updated_at = now() \
         WHERE id = $1 AND status NOT IN ('released', 'failed')",
    )
    .bind(request_id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    sqlx::query("UPDATE capacity_dispatches SET completed_at = now() WHERE id = $1")
        .bind(dispatch_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    transaction
        .commit()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))
}

async fn complete(pool: &PgPool, dispatch_id: Uuid) -> Result<(), CapacityError> {
    sqlx::query("UPDATE capacity_dispatches SET completed_at = now() WHERE id = $1")
        .bind(dispatch_id)
        .execute(pool)
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    Ok(())
}

/// Record a failed attempt and schedule the next one, or give up and say so.
///
/// Called with the request's advisory lock held, and conditional on the dispatch still being in
/// the state it was read in, so that a slow controller's give-up cannot overwrite a faster one's
/// success.
async fn record_failure(pool: &PgPool, dispatch: &DispatchRow, reason: &str) {
    let attempts = dispatch.attempts + 1;
    // Exponential, capped, and stored as a time rather than a sleep so it survives a restart.
    let backoff_seconds = f64::from(2_i32.saturating_pow(attempts.min(6).unsigned_abs())) * 5.0;
    let message = truncate(reason, MAX_ERROR_BYTES);
    // `attempts = $5` and `completed_at IS NULL` make this a compare-and-set against the row this
    // controller actually read. If another controller has completed or advanced the dispatch in
    // the meantime, nothing matches and their outcome stands untouched.
    let advanced = sqlx::query_scalar::<_, i32>(
        "UPDATE capacity_dispatches \
         SET attempts = $2, last_error = $3, next_attempt_at = now() + make_interval(secs => $4) \
         WHERE id = $1 AND completed_at IS NULL AND attempts = $5 \
         RETURNING attempts",
    )
    .bind(dispatch.id)
    .bind(attempts)
    .bind(&message)
    .bind(backoff_seconds)
    .bind(dispatch.attempts)
    .fetch_optional(pool)
    .await;
    let advanced = match advanced {
        Ok(value) => value,
        Err(error) => {
            warn!(dispatch = %dispatch.id, %error, "could not record capacity dispatch failure");
            return;
        }
    };
    if advanced.is_none() {
        // Somebody else moved this dispatch on. Recording anything further would be reporting on
        // a state that no longer exists.
        return;
    }

    let _ = sqlx::query(
        "UPDATE capacity_requests SET last_error = $2, updated_at = now() WHERE id = $1",
    )
    .bind(dispatch.request_id)
    .bind(&message)
    .execute(pool)
    .await;

    if attempts < MAX_DISPATCH_ATTEMPTS {
        return;
    }

    // Out of attempts. The due query will not select this dispatch again, so the provider stops
    // being called; what remains is making sure the right thing is left behind for a person.
    if dispatch.action == "release" {
        // A release obligation outlives its retries. Marking the request `failed` would drop it
        // out of the open-request index and so hide a machine that is very possibly still
        // running and still being paid for. It stays `releasing`, and `outstanding_releases`
        // is where it surfaces.
        warn!(
            dispatch = %dispatch.id,
            request = %dispatch.request_id,
            attempts,
            "capacity release exhausted its attempts; the machine may still exist"
        );
        return;
    }

    // A provision that never obtained a handle made nothing, so failing it strands nothing. One
    // that did obtain a handle holds a machine, and must stay visible rather than be marked
    // `failed` -- which is exactly the state the open-request index excludes.
    let _ = sqlx::query(
        "UPDATE capacity_requests SET status = 'failed', updated_at = now() \
         WHERE id = $1 AND status NOT IN ('released', 'failed') AND external_id IS NULL",
    )
    .bind(dispatch.request_id)
    .execute(pool)
    .await;
    warn!(dispatch = %dispatch.id, attempts, "capacity dispatch given up");
}

/// Requests whose release ran out of attempts and may still be holding a machine.
///
/// The operator surface for the one failure this boundary cannot resolve by itself: Kratos has
/// decided the capacity should go, the provider has refused often enough that asking again is no
/// longer useful, and the machine is presumed alive until a person says otherwise. Bounded and
/// cheap to poll, unlike the retries it replaces.
///
/// # Errors
/// Returns an error when the database cannot be reached.
pub async fn outstanding_releases(pool: &PgPool) -> Result<Vec<OutstandingRelease>, CapacityError> {
    sqlx::query_as::<_, OutstandingRelease>(
        "SELECT r.id AS request_id, r.attempt_id, r.project_id, r.provider, \
                r.external_id, d.attempts, d.last_error \
         FROM capacity_dispatches d JOIN capacity_requests r ON r.id = d.request_id \
         WHERE d.action = 'release' AND d.completed_at IS NULL AND d.attempts >= $1 \
           AND r.external_id IS NOT NULL AND r.released_at IS NULL \
         ORDER BY r.created_at",
    )
    .bind(MAX_DISPATCH_ATTEMPTS)
    .fetch_all(pool)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))
}

/// Capacity that Kratos has given up releasing and that a person now owns.
#[derive(Debug, sqlx::FromRow)]
pub struct OutstandingRelease {
    pub request_id: Uuid,
    pub attempt_id: Uuid,
    pub project_id: Uuid,
    pub provider: String,
    /// Never null here: the query only returns requests that still hold a handle.
    pub external_id: Option<String>,
    pub attempts: i32,
    pub last_error: Option<String>,
}

/// Whether the capacity feature is switched on.
///
/// Off unless explicitly enabled, and off is the state in which nothing can ever be spent.
#[must_use]
pub fn enabled() -> bool {
    enabled_from(std::env::var("KRATOS_CAPACITY_ENABLED").ok().as_deref())
}

/// The decision itself, separated from where the value comes from so it can be tested without
/// mutating the process environment -- which this workspace forbids, and which would make two
/// tests running at once interfere with each other anyway.
///
/// Anything other than an explicit "true" is off, including absent, empty and misspelt. A
/// feature that can spend money should not be switched on by a typo.
#[must_use]
pub fn enabled_from(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// When a request was last touched, for tests and for a future console view.
#[derive(Debug, sqlx::FromRow)]
pub struct RequestSnapshot {
    pub status: String,
    pub external_id: Option<String>,
    pub released_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// Read one attempt's request, if it has one.
///
/// # Errors
/// Returns an error when the database cannot be reached.
pub async fn snapshot(
    pool: &PgPool,
    attempt_id: Uuid,
) -> Result<Option<RequestSnapshot>, CapacityError> {
    sqlx::query_as::<_, RequestSnapshot>(
        "SELECT status, external_id, released_at, last_error FROM capacity_requests \
         WHERE attempt_id = $1",
    )
    .bind(attempt_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))
}

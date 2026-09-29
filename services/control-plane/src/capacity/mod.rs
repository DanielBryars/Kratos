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
use sqlx::{PgConnection, PgPool};
use tracing::{info, warn};
use uuid::Uuid;

pub mod fake;
pub(crate) mod operator_view;

#[cfg(test)]
mod tests;

/// The first half of the advisory-lock key, so these locks cannot collide with the projector's.
const CONTROLLER_LOCK_NAMESPACE: i32 = 0x4b52_4341;
/// How many times a dispatch is tried before it is left failed for a person to look at.
const MAX_DISPATCH_ATTEMPTS: i32 = 8;
/// Bounded so a pathological provider message cannot fill the column or the console.
const MAX_ERROR_BYTES: usize = 500;

/// Why a provider call did not succeed -- and, crucially, whether it might have had an effect
/// anyway.
///
/// The distinction is part of the provider contract, not an inference: `Refused` is a promise that
/// nothing was created, and an implementation must use `Unavailable` for anything it cannot
/// promise that about. A timeout, a dropped connection, a 500 and a lost response are all
/// `Unavailable`, because the machine may exist on the other side of them.
#[derive(Debug, thiserror::Error)]
pub enum CapacityError {
    #[error("the provider is unavailable")]
    Unavailable,
    #[error("the provider refused: {0}")]
    Refused(String),
    #[error("capacity state is inconsistent: {0}")]
    Inconsistent(String),
}

impl CapacityError {
    /// Whether the call may have had an effect despite failing.
    ///
    /// Only a positive refusal says nothing happened. Everything else has to be treated as though
    /// a machine might exist, because the alternative -- assuming it does not -- is how a running
    /// machine stops being anybody's problem.
    #[must_use]
    pub const fn may_have_created_capacity(&self) -> bool {
        match self {
            Self::Refused(_) => false,
            Self::Unavailable | Self::Inconsistent(_) => true,
        }
    }
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
///
/// Both must report failure honestly: `CapacityError::Refused` is a promise that nothing was
/// created or changed, and anything the implementation cannot promise that about — a timeout, a
/// dropped connection, an unparseable reply — must be `Unavailable`. Kratos decides whether a
/// machine might be running from that distinction alone.
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
    /// Asked for, never answered usably, and so of unknown effect. Stays open until the provider
    /// answers or a person settles it.
    Unreconciled,
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
            Self::Unreconciled => "unreconciled",
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
    if request.status == "released" {
        return Ok(());
    }

    // Record the intent first, and unconditionally. This is the fact that has to outlive every
    // uncertainty below: whether a handle is known, whether the provider ever answered, whether
    // this request is later reconciled by a controller or by a person. Without it, a machine that
    // turns up after the cancellation becomes ready capacity for a job that no longer exists.
    sqlx::query(
        "UPDATE capacity_requests \
         SET release_requested_at = COALESCE(release_requested_at, now()), updated_at = now() \
         WHERE id = $1",
    )
    .bind(request.id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;

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
    } else if matches!(request.status.as_str(), "requested" | "failed") {
        // The two states where a null handle really does mean no machine, and they are the only
        // ones. `requested` is what the claim in `provision` moves a request out of *before* the
        // provider is called, and this row is locked, so a request still sitting there has never
        // been asked for. `failed` is only ever reached from a positive refusal, which is the
        // provider promising it created nothing. Either way there is nothing to chase, so the
        // provision is cancelled and the request converges.
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
    // Otherwise the provider has been asked and has not given a usable answer, so whether a
    // machine exists is unknown. The intent recorded above is all that can be settled now: the
    // provision dispatch is deliberately left alone, because it is the only path back to the
    // handle. When it arrives the request converges to `releasing` rather than `ready`, and if
    // the dispatch has already exhausted its attempts the request is waiting in
    // `ambiguous_provisions` for `reconcile_provision` or `resolve_unreconciled`.
    //
    // Deleting the dispatch here -- which is what a null handle used to mean -- would throw away
    // the only way of ever learning the machine's name.

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
        record_failure(pool, dispatch, error).await;
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
    // The transition is the permission to call the provider, so its result is checked rather than
    // assumed. Between reading the request above and this statement the row may have gone, or
    // moved to a state that must not be provisioned; either way the update matches nothing, and
    // calling the provider anyway would build a machine with nowhere to record it.
    let claimed = sqlx::query(
        "UPDATE capacity_requests \
         SET status = CASE WHEN status = 'requested' THEN 'provisioning' ELSE status END, \
             updated_at = now() \
         WHERE id = $1 AND status IN ('requested', 'provisioning', 'unreconciled', 'ready')",
    )
    .bind(request.id)
    .execute(pool)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    if claimed.rows_affected() != 1 {
        warn!(
            request = %request.id,
            "capacity request vanished or changed state before provisioning; not calling provider"
        );
        return Ok(false);
    }

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
    // What the handle's arrival means depends on whether anyone still wants this capacity, and
    // that question is answered by `release_requested_at` rather than by the status. A request
    // cancelled while its outcome was unknown could not be moved to `releasing` at the time --
    // there was no handle to release -- so the status alone would say `provisioning` or
    // `unreconciled` and this machine would become ready capacity for a cancelled job.
    //
    // The new status is returned rather than the old one, because it is the decision itself.
    let settled: String = sqlx::query_scalar(
        "UPDATE capacity_requests \
         SET external_id = COALESCE(external_id, $2), \
             status = CASE \
                 WHEN status = 'failed' THEN 'failed' \
                 WHEN status = 'released' OR release_requested_at IS NOT NULL THEN 'releasing' \
                 ELSE 'ready' END, \
             released_at = CASE \
                 WHEN status = 'released' OR release_requested_at IS NOT NULL THEN NULL \
                 ELSE released_at END, \
             last_error = NULL, updated_at = now() \
         WHERE id = $1 \
         RETURNING status",
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
    // Release was asked for at some point -- while the provider was answering, or long before,
    // while nobody knew whether this machine existed. Either way it exists now and has to be
    // handed back rather than forgotten.
    if settled == "releasing" {
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
    sqlx::query(
        "UPDATE capacity_requests \
         SET status = 'released', released_at = now(), last_error = NULL, updated_at = now() \
         WHERE id = $1 AND status <> 'released'",
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
async fn record_failure(pool: &PgPool, dispatch: &DispatchRow, error: &CapacityError) {
    if let Err(failure) = write_failure(pool, dispatch, error).await {
        // Nothing was written, so the dispatch stays exactly as it was and the next pass will
        // find it again. Losing the bookkeeping is recoverable; a half-written give-up is not,
        // which is why it is one transaction.
        warn!(dispatch = %dispatch.id, %failure, "could not record capacity dispatch failure");
    }
}

/// The whole give-up, in one transaction.
///
/// The compare-and-set, the request's error and the terminal transition have to commit together.
/// Split across autocommit statements, a crash after the dispatch reaches its last attempt leaves
/// it permanently excluded from the due query while the request still looks live and appears on no
/// operator surface — stuck in a way nothing would ever notice.
async fn write_failure(
    pool: &PgPool,
    dispatch: &DispatchRow,
    error: &CapacityError,
) -> Result<(), sqlx::Error> {
    let attempts = dispatch.attempts + 1;
    // Exponential, capped, and stored as a time rather than a sleep so it survives a restart.
    let backoff_seconds = f64::from(2_i32.saturating_pow(attempts.min(6).unsigned_abs())) * 5.0;
    let message = truncate(&error.to_string(), MAX_ERROR_BYTES);
    let mut transaction = pool.begin().await?;

    // One lock order everywhere in this module: the request row first, then its dispatches.
    // `release_capacity` takes the request with `FOR UPDATE` and then touches the outbox, and a
    // controller recording a provider error at the moment a job is cancelled is an overlap to
    // expect rather than to hope against. Taking them in the opposite order here would deadlock
    // and abort one of the two -- either losing the failure bookkeeping or, worse, the
    // cancellation.
    let present =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM capacity_requests WHERE id = $1 FOR UPDATE")
            .bind(dispatch.request_id)
            .fetch_optional(&mut *transaction)
            .await?;
    if present.is_none() {
        // The request has been deleted, so there is nothing left to record against.
        return transaction.rollback().await;
    }

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
    .fetch_optional(&mut *transaction)
    .await?;
    if advanced.is_none() {
        // Somebody else moved this dispatch on. Recording anything further would be reporting on
        // a state that no longer exists.
        return transaction.rollback().await;
    }

    sqlx::query("UPDATE capacity_requests SET last_error = $2, updated_at = now() WHERE id = $1")
        .bind(dispatch.request_id)
        .bind(&message)
        .execute(&mut *transaction)
        .await?;

    if attempts >= MAX_DISPATCH_ATTEMPTS {
        // Out of attempts. The due query will not select this dispatch again, so the provider
        // stops being called; what remains is leaving the right thing behind for a person.
        terminal_transition(&mut transaction, dispatch, error, attempts).await?;
    }

    transaction.commit().await
}

/// What an exhausted dispatch leaves behind.
async fn terminal_transition(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch: &DispatchRow,
    error: &CapacityError,
    attempts: i32,
) -> Result<(), sqlx::Error> {
    if dispatch.action == "release" {
        // A release obligation outlives its retries. The machine is presumed alive, so the
        // request stays `releasing` and surfaces through `outstanding_releases`.
        warn!(
            dispatch = %dispatch.id,
            request = %dispatch.request_id,
            attempts,
            "capacity release exhausted its attempts; the machine may still exist"
        );
        return Ok(());
    }

    // The question is not whether provisioning succeeded -- it plainly did not -- but whether a
    // machine exists anyway. Only a positive refusal answers that, and everything else leaves it
    // open, so only a refusal may be treated as knowledge.
    if error.may_have_created_capacity() {
        warn!(
            dispatch = %dispatch.id,
            request = %dispatch.request_id,
            attempts,
            "capacity provision exhausted with an unknown outcome; a machine may exist"
        );
        // A request that obtained a handle holds a machine, whatever this dispatch thinks, so it
        // is left alone rather than moved to a terminal state.
        sqlx::query(
            "UPDATE capacity_requests SET status = $2, updated_at = now() \
             WHERE id = $1 AND status NOT IN ('released', 'releasing', 'failed') \
               AND external_id IS NULL",
        )
        .bind(dispatch.request_id)
        .bind(RequestStatus::Unreconciled.as_str())
        .execute(&mut **transaction)
        .await?;
        return Ok(());
    }

    // A positive refusal establishes that no machine exists. If cancellation was already asked
    // for, both halves of the question are now answered -- there is nothing to release, and
    // nobody wants it -- so the request settles here. Left as `failed` it would sit in neither
    // operator view, with `released_at` null and the delete guard blocking cleanup, converging
    // only if some external caller happened to ask for release a second time.
    //
    // This is in the same transaction as the dispatch's final attempt, so the request settling
    // and the dispatch being spent commit together.
    let settled = sqlx::query_scalar::<_, bool>(
        "UPDATE capacity_requests \
         SET status = CASE WHEN release_requested_at IS NOT NULL THEN 'released' \
                           ELSE 'failed' END, \
             released_at = CASE WHEN release_requested_at IS NOT NULL THEN now() \
                                ELSE released_at END, \
             updated_at = now() \
         WHERE id = $1 AND status NOT IN ('released', 'releasing', 'failed') \
           AND external_id IS NULL \
         RETURNING released_at IS NOT NULL",
    )
    .bind(dispatch.request_id)
    .fetch_optional(&mut **transaction)
    .await?;

    if settled == Some(true) {
        // Nothing is outstanding, so no dispatch should look due either.
        sqlx::query(
            "UPDATE capacity_dispatches SET completed_at = now() \
             WHERE request_id = $1 AND completed_at IS NULL",
        )
        .bind(dispatch.request_id)
        .execute(&mut **transaction)
        .await?;
        info!(
            dispatch = %dispatch.id,
            request = %dispatch.request_id,
            attempts,
            "capacity provision refused after cancellation; settled as released"
        );
    } else {
        warn!(dispatch = %dispatch.id, attempts, "capacity provision refused; giving up");
    }
    Ok(())
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
    let mut connection = pool
        .acquire()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    fetch_outstanding_releases(&mut connection, None).await
}

/// `projects` of `None` means every project. One query for the function and the console, so what
/// counts as outstanding cannot drift between them.
async fn fetch_outstanding_releases(
    connection: &mut PgConnection,
    projects: Option<&[Uuid]>,
) -> Result<Vec<OutstandingRelease>, CapacityError> {
    sqlx::query_as::<_, OutstandingRelease>(
        "SELECT r.id AS request_id, r.attempt_id, r.project_id, r.provider, \
                r.external_id, d.attempts, d.last_error \
         FROM capacity_dispatches d JOIN capacity_requests r ON r.id = d.request_id \
         WHERE d.action = 'release' AND d.completed_at IS NULL AND d.attempts >= $1 \
           AND r.external_id IS NOT NULL AND r.released_at IS NULL \
           AND ($2::uuid[] IS NULL OR r.project_id = ANY($2)) \
         ORDER BY r.created_at",
    )
    .bind(MAX_DISPATCH_ATTEMPTS)
    .bind(projects)
    .fetch_all(connection)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))
}

/// Requests that were asked for and never usably answered, which may or may not have a machine.
///
/// The other half of the operator surface. These are not failures — a failure would be a claim
/// that nothing exists — they are open questions, and they stay open until the provider answers
/// one way or the other or a person settles them.
///
/// # Errors
/// Returns an error when the database cannot be reached.
pub async fn ambiguous_provisions(pool: &PgPool) -> Result<Vec<AmbiguousProvision>, CapacityError> {
    let mut connection = pool
        .acquire()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    fetch_ambiguous_provisions(&mut connection, None).await
}

/// `projects` of `None` means every project.
async fn fetch_ambiguous_provisions(
    connection: &mut PgConnection,
    projects: Option<&[Uuid]>,
) -> Result<Vec<AmbiguousProvision>, CapacityError> {
    sqlx::query_as::<_, AmbiguousProvision>(
        "SELECT id AS request_id, attempt_id, project_id, provider, idempotency_key, \
                release_requested_at, last_error \
         FROM capacity_requests WHERE status = 'unreconciled' \
           AND ($1::uuid[] IS NULL OR project_id = ANY($1)) \
         ORDER BY created_at",
    )
    .bind(projects)
    .fetch_all(connection)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))
}

/// Both operator views for `projects`, read from one snapshot so a request moving between them
/// mid-read cannot appear in both or in neither.
///
/// # Errors
/// Returns an error when the database cannot be reached.
pub(crate) async fn needing_attention(
    pool: &PgPool,
    projects: &[Uuid],
) -> Result<(Vec<AmbiguousProvision>, Vec<OutstandingRelease>), CapacityError> {
    let inconsistent = |error: sqlx::Error| CapacityError::Inconsistent(error.to_string());
    let mut transaction = pool.begin().await.map_err(inconsistent)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *transaction)
        .await
        .map_err(inconsistent)?;
    let ambiguous = fetch_ambiguous_provisions(&mut transaction, Some(projects)).await?;
    let outstanding = fetch_outstanding_releases(&mut transaction, Some(projects)).await?;
    transaction.commit().await.map_err(inconsistent)?;
    Ok((ambiguous, outstanding))
}

/// Ask the provider again about a request whose outcome was never established.
///
/// This is the cheap resolution, and the reason the idempotency key is derived rather than random:
/// the same question gets the same answer, so a provider that did build a machine hands back the
/// handle that was lost and the request becomes ordinary `ready` capacity that can then be
/// released properly.
///
/// # Errors
/// Returns an error when the database cannot be reached.
pub async fn reconcile_provision(pool: &PgPool, request_id: Uuid) -> Result<bool, CapacityError> {
    let reset = sqlx::query(
        "UPDATE capacity_dispatches SET attempts = 0, next_attempt_at = now() \
         WHERE request_id = $1 AND action = 'provision' AND completed_at IS NULL",
    )
    .bind(request_id)
    .execute(pool)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    Ok(reset.rows_affected() > 0)
}

/// Record that a person has established there is no machine behind an unreconciled request.
///
/// The expensive resolution, and deliberately explicit: it asserts something Kratos could not
/// determine, so it is never reached by any automatic path and the reason is kept.
///
/// # Errors
/// Returns an error when the database cannot be reached.
pub async fn resolve_unreconciled(
    pool: &PgPool,
    request_id: Uuid,
    note: &str,
) -> Result<bool, CapacityError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    let resolved = sqlx::query(
        "UPDATE capacity_requests \
         SET status = 'released', released_at = now(), last_error = $2, updated_at = now() \
         WHERE id = $1 AND status = 'unreconciled'",
    )
    .bind(request_id)
    .bind(truncate(note, MAX_ERROR_BYTES))
    .execute(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    if resolved.rows_affected() == 0 {
        transaction
            .rollback()
            .await
            .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE capacity_dispatches SET completed_at = now() \
         WHERE request_id = $1 AND completed_at IS NULL",
    )
    .bind(request_id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    transaction
        .commit()
        .await
        .map_err(|error| CapacityError::Inconsistent(error.to_string()))?;
    Ok(true)
}

/// A request whose provider outcome was never established.
#[derive(Debug, sqlx::FromRow, serde::Serialize, utoipa::ToSchema)]
pub struct AmbiguousProvision {
    pub request_id: Uuid,
    pub attempt_id: Uuid,
    pub project_id: Uuid,
    pub provider: String,
    /// What to ask the provider about, if anyone goes looking by hand.
    pub idempotency_key: String,
    /// Set when the job has already been cancelled. Such a request must converge to a release
    /// once its handle is recovered, never to ready capacity.
    pub release_requested_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// Capacity that Kratos has given up releasing and that a person now owns.
#[derive(Debug, sqlx::FromRow, serde::Serialize, utoipa::ToSchema)]
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
    /// That somebody asked for this capacity to go, whether or not it could be acted on yet.
    pub release_requested_at: Option<DateTime<Utc>>,
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
        "SELECT status, external_id, release_requested_at, released_at, last_error \
         FROM capacity_requests WHERE attempt_id = $1",
    )
    .bind(attempt_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| CapacityError::Inconsistent(error.to_string()))
}

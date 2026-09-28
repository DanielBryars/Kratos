//! The four properties the capacity boundary exists to guarantee.
//!
//! Each is proved against a real database and the fake provider. The double-dispatch test forces
//! its overlap rather than hoping for it, because joined futures do not overlap against a local
//! `PostgreSQL` — a lesson this project has now paid for more than once.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use super::fake::{Call, FakeProvider};
use super::*;

/// A job with one attempt, which is the only thing a capacity request can be derived from.
async fn seed_attempt(pool: &PgPool) -> Uuid {
    let owner_id = Uuid::new_v4();
    let worker_id = Uuid::new_v4();
    let job_id = Uuid::new_v4();
    let attempt_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
         VALUES ($1, 'test', $2, 'Owner')",
    )
    .bind(owner_id)
    .bind(owner_id.to_string())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workers \
         (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, \
          capabilities, project_id) \
         VALUES ($1, $2, $3, 'Worker', '1.3', 'busy', '{}'::jsonb, $4)",
    )
    .bind(worker_id)
    .bind(owner_id)
    .bind(Uuid::new_v4())
    .bind(crate::projects::DEFAULT_PROJECT_ID)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs \
         (id, owner_identity_id, name, image_reference, timeout_seconds, status, \
          assigned_worker_id, project_id) \
         VALUES ($1, $2, 'Training', $3, 120, 'running', $4, $5)",
    )
    .bind(job_id)
    .bind(owner_id)
    .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
    .bind(worker_id)
    .bind(crate::projects::DEFAULT_PROJECT_ID)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO job_attempts (id, job_id, attempt_number, worker_id, lease_expires_at) \
         VALUES ($1, $2, 1, $3, now() + interval '5 minutes')",
    )
    .bind(attempt_id)
    .bind(job_id)
    .bind(worker_id)
    .execute(pool)
    .await
    .unwrap();
    attempt_id
}

/// A queued job that never got an attempt, which is the state a cancellation usually finds.
async fn seed_queued_job(pool: &PgPool) -> Uuid {
    let owner_id = Uuid::new_v4();
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
         VALUES ($1, 'test', $2, 'Owner')",
    )
    .bind(owner_id)
    .bind(owner_id.to_string())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs \
         (id, owner_identity_id, name, image_reference, timeout_seconds, project_id) \
         VALUES ($1, $2, 'Queued', $3, 120, $4)",
    )
    .bind(job_id)
    .bind(owner_id)
    .bind(format!("example.test/work@sha256:{}", "b".repeat(64)))
    .bind(crate::projects::DEFAULT_PROJECT_ID)
    .execute(pool)
    .await
    .unwrap();
    job_id
}

// ---------------------------------------------------------------------------------------------
// 1. Two controllers cannot double-dispatch
// ---------------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn two_controllers_cannot_double_dispatch(pool: PgPool) {
    // The control plane runs as more than one instance. Two of them provisioning one request
    // would buy two machines and strand one, since only one external id can be stored.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = Arc::new(FakeProvider::new());

    // Hold the request's advisory lock from another connection, as a second controller would
    // while it is mid-dispatch. Forcing the overlap rather than hoping two futures interleave.
    let request_id: Uuid =
        sqlx::query_scalar("SELECT id FROM capacity_requests WHERE attempt_id = $1")
            .bind(attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut holder = pool.acquire().await.unwrap();
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, hashtext($2))")
        .bind(CONTROLLER_LOCK_NAMESPACE)
        .bind(request_id.to_string())
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    assert!(held);

    let blocked = dispatch_once(&pool, provider.as_ref()).await;
    assert_eq!(blocked, 0, "a held request must be skipped, not duplicated");
    assert!(
        provider.calls().is_empty(),
        "the provider must not be asked at all while another controller holds the request"
    );

    // Once released, exactly one provision happens.
    sqlx::query("SELECT pg_advisory_unlock($1, hashtext($2))")
        .bind(CONTROLLER_LOCK_NAMESPACE)
        .bind(request_id.to_string())
        .execute(&mut *holder)
        .await
        .unwrap();
    assert_eq!(dispatch_once(&pool, provider.as_ref()).await, 1);
    assert_eq!(provider.machine_count(), 1);
    // And a further pass asks for nothing, because the dispatch is complete.
    assert_eq!(dispatch_once(&pool, provider.as_ref()).await, 0);
    assert_eq!(
        provider.calls().len(),
        1,
        "one provision, however many controllers pass over it"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn one_attempt_yields_one_request_however_often_it_is_asked(pool: PgPool) {
    let attempt_id = seed_attempt(&pool).await;
    let first = request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let second = request_capacity(&pool, attempt_id, "fake").await.unwrap();

    assert_eq!(
        first, second,
        "asking twice must not create a second request"
    );
    let dispatches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM capacity_dispatches WHERE request_id = $1 AND action = 'provision'",
    )
    .bind(first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dispatches, 1, "nor a second thing to tell the provider");
}

// ---------------------------------------------------------------------------------------------
// 2. Queued cancellation creates no request
// ---------------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn cancelling_a_queued_job_creates_no_capacity_request(pool: PgPool) {
    // A job cancelled before it was ever assigned has no attempt, so there is nothing to derive
    // a request from. The foreign key decides this rather than any controller logic, which is
    // why it cannot be got wrong later by a change of mind in the scheduler.
    let job_id = seed_queued_job(&pool).await;
    // The jobs table ties a cancelled status to when cancellation was asked for, so a fixture
    // that sets only the status describes a state the schema will not hold.
    sqlx::query(
        "UPDATE jobs SET status = 'cancelled', cancel_requested_at = now(), finished_at = now() \
         WHERE id = $1",
    )
    .bind(job_id)
    .execute(&pool)
    .await
    .unwrap();

    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM capacity_requests WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(requests, 0);

    // And releasing an attempt that never existed is quietly fine rather than an error.
    release_capacity(&pool, Uuid::new_v4()).await.unwrap();
    let provider = FakeProvider::new();
    assert_eq!(dispatch_once(&pool, &provider).await, 0);
    assert!(provider.calls().is_empty(), "nothing to tell anyone about");
}

// ---------------------------------------------------------------------------------------------
// 3. Provisioning cancellation converges to released
// ---------------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn cancelling_while_provisioning_converges_to_released(pool: PgPool) {
    // The dangerous case: the provider is mid-answer when the job is cancelled. The machine will
    // exist, so forgetting the request would leave capacity running that nobody is watching.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();

    // Cancelled after the request was made but before the provider answered.
    release_capacity(&pool, attempt_id).await.unwrap();
    let snapshot = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(
        snapshot.status, "released",
        "nothing was made, so it converges immediately"
    );

    let provider = FakeProvider::new();
    assert_eq!(dispatch_once(&pool, &provider).await, 0);
    assert!(
        provider.calls().is_empty(),
        "a machine that was never asked for must not be created by a late dispatch"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn cancelling_after_the_machine_exists_releases_it(pool: PgPool) {
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();
    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    let ready = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(ready.status, "ready");
    let external_id = ready.external_id.clone().unwrap();

    release_capacity(&pool, attempt_id).await.unwrap();
    assert_eq!(
        snapshot(&pool, attempt_id).await.unwrap().unwrap().status,
        "releasing",
        "it cannot be released until the provider has been told"
    );

    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    let released = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(released.status, "released");
    assert!(released.released_at.is_some());
    assert!(provider.is_released(&external_id));
    assert_eq!(
        provider.calls(),
        vec![
            Call::Provision(idempotency_key(attempt_id)),
            Call::Release(external_id),
        ]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_machine_that_arrives_after_cancellation_is_still_released(pool: PgPool) {
    // Cancelled *while the provider was answering*, which is the only way a machine can exist for
    // a request nobody wants. Checking the status before dispatching covers the ordinary case;
    // this is the window that check cannot cover, and forgetting it leaves capacity running with
    // nothing pointing at it.
    //
    // The race is forced rather than hoped for: the fake holds its provision open until the test
    // has released the request.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();

    let provider = Arc::new(FakeProvider::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    provider.hold_next_provision(gate.clone());
    let started = provider.provision_started();
    let waiting = started.notified();

    let dispatching = {
        let pool = pool.clone();
        let provider = provider.clone();
        tokio::spawn(async move { dispatch_once(&pool, provider.as_ref()).await })
    };

    // Once the provider has been entered, cancel. The machine is already being made.
    waiting.await;
    release_capacity(&pool, attempt_id).await.unwrap();
    gate.notify_waiters();
    dispatching.await.unwrap();

    // The answer arrived for a request that had already converged to released. It must not be
    // resurrected as ready, and the machine must be handed back rather than forgotten.
    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(after.status, "releasing", "{after:?}");
    let external_id = after
        .external_id
        .expect("the machine exists and must be tracked");

    assert_eq!(dispatch_once(&pool, provider.as_ref()).await, 1);
    let released = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(released.status, "released");
    assert!(
        provider.is_released(&external_id),
        "capacity created during a cancellation must still be released"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. Restart replay reuses the same external id
// ---------------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_replayed_dispatch_reuses_the_same_machine(pool: PgPool) {
    // The crash this design is shaped around: the provider made a machine and the controller
    // died before recording it. On restart the dispatch is still pending, so it is sent again —
    // and because the key is derived from the attempt, the provider answers with the machine it
    // already has rather than building a second one nobody will ever release.
    let attempt_id = seed_attempt(&pool).await;
    let request_id = request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();

    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    let first = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    let external_id = first.external_id.clone().unwrap();

    // Undo only what the controller records, leaving the provider's world untouched: exactly the
    // state a crash between the provider answering and the write landing would leave.
    sqlx::query(
        "UPDATE capacity_dispatches SET completed_at = NULL WHERE request_id = $1 \
         AND action = 'provision'",
    )
    .bind(request_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE capacity_requests SET external_id = NULL, status = 'requested' WHERE id = $1",
    )
    .bind(request_id)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    let second = snapshot(&pool, attempt_id).await.unwrap().unwrap();

    assert_eq!(
        second.external_id.as_deref(),
        Some(external_id.as_str()),
        "a replay must be answered with the same machine"
    );
    assert_eq!(
        provider.machine_count(),
        1,
        "and must not have caused a second one to exist"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_external_id_is_never_overwritten(pool: PgPool) {
    // It is the only handle by which the machine can be released. A second value would strand
    // the first, and stranded capacity is the one failure here that costs money.
    let attempt_id = seed_attempt(&pool).await;
    let request_id = request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();
    dispatch_once(&pool, &provider).await;
    let original = snapshot(&pool, attempt_id)
        .await
        .unwrap()
        .unwrap()
        .external_id
        .unwrap();

    // Replay the dispatch without clearing the stored id, as a duplicated pass would.
    sqlx::query(
        "UPDATE capacity_dispatches SET completed_at = NULL WHERE request_id = $1 \
         AND action = 'provision'",
    )
    .bind(request_id)
    .execute(&pool)
    .await
    .unwrap();
    dispatch_once(&pool, &provider).await;

    assert_eq!(
        snapshot(&pool, attempt_id)
            .await
            .unwrap()
            .unwrap()
            .external_id,
        Some(original)
    );
}

// ---------------------------------------------------------------------------------------------
// Failure handling
// ---------------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_refused_provision_is_retried_later_and_recorded(pool: PgPool) {
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();
    provider.fail_next_provision("no capacity in this region");

    assert_eq!(dispatch_once(&pool, &provider).await, 0);
    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert!(
        after
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("no capacity")),
        "{after:?}"
    );
    assert_ne!(after.status, "failed", "one refusal is not giving up");

    // Backoff is a stored time rather than a sleep, so it survives a restart; bring it forward.
    sqlx::query(
        "UPDATE capacity_dispatches SET next_attempt_at = now() WHERE completed_at IS NULL",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    assert_eq!(
        snapshot(&pool, attempt_id).await.unwrap().unwrap().status,
        "ready"
    );
}

/// Reads one dispatch, as a controller does before it starts work.
async fn read_dispatch(pool: &PgPool, request_id: Uuid, action: &str) -> DispatchRow {
    sqlx::query_as::<_, DispatchRow>(
        "SELECT id, request_id, action, attempts FROM capacity_dispatches \
         WHERE request_id = $1 AND action = $2",
    )
    .bind(request_id)
    .bind(action)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn request_id_of(pool: &PgPool, attempt_id: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM capacity_requests WHERE attempt_id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_stale_give_up_cannot_overwrite_another_controllers_success(pool: PgPool) {
    // The overlap that hides a live machine: controller A reads a due dispatch, is slow talking to
    // the provider, and fails. Meanwhile controller B provisions successfully. If A's give-up is
    // then written, the request is marked `failed` -- which the open-request index excludes -- and
    // a machine nobody is pointing at runs on being paid for.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let request_id = request_id_of(&pool, attempt_id).await;

    // What the slow controller read before it started, near the end of its patience.
    let stale = read_dispatch(&pool, request_id, "provision").await;
    let stale = DispatchRow {
        attempts: MAX_DISPATCH_ATTEMPTS - 1,
        ..stale
    };

    // The other controller gets there first.
    let provider = FakeProvider::new();
    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    let won = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(won.status, "ready");
    let machine = won.external_id.clone().unwrap();

    // Now the slow one gives up, against the state it read a moment ago.
    record_failure(&pool, &stale, &CapacityError::Unavailable).await;

    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "ready",
        "a stale give-up must not overwrite a successful provision: {after:?}"
    );
    assert_eq!(after.external_id.as_deref(), Some(machine.as_str()));
    assert_eq!(
        after.last_error, None,
        "nor report a failure against a request that succeeded"
    );
    let dispatch: (Option<DateTime<Utc>>, i32) =
        sqlx::query_as("SELECT completed_at, attempts FROM capacity_dispatches WHERE id = $1")
            .bind(stale.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dispatch.0.is_some(), "the dispatch stays completed");
    assert_eq!(dispatch.1, 0, "and its attempts are not rewritten");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_request_holding_a_machine_is_never_marked_failed(pool: PgPool) {
    // The second guard, for the case where the compare-and-set does match: whatever the attempt
    // count says, a request with an external id has a machine, and `failed` would hide it.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let request_id = request_id_of(&pool, attempt_id).await;

    // A machine exists, but the dispatch that recorded it is still open and out of patience.
    sqlx::query(
        "UPDATE capacity_requests SET status = 'ready', external_id = 'fake-held' WHERE id = $1",
    )
    .bind(request_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE capacity_dispatches SET attempts = $2 WHERE request_id = $1")
        .bind(request_id)
        .bind(MAX_DISPATCH_ATTEMPTS - 1)
        .execute(&pool)
        .await
        .unwrap();

    let dispatch = read_dispatch(&pool, request_id, "provision").await;
    record_failure(&pool, &dispatch, &CapacityError::Unavailable).await;

    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_ne!(
        after.status, "failed",
        "capacity that exists must stay visible, not be filed as failed: {after:?}"
    );
    assert_eq!(after.external_id.as_deref(), Some("fake-held"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_release_stops_calling_the_provider_but_stays_outstanding(pool: PgPool) {
    // A release that keeps failing must stop somewhere: the stated limit is eight attempts, and
    // before this the due query had no cutoff, so the provider was called forever. It must also
    // not be marked `failed`, because the machine is presumed alive and `failed` is exactly the
    // state the open-request index leaves out.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();
    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    release_capacity(&pool, attempt_id).await.unwrap();

    // Refuse every release, bringing the stored backoff forward each time rather than sleeping.
    for _ in 0..MAX_DISPATCH_ATTEMPTS + 4 {
        provider.fail_next_release("the provider is having a bad day");
        sqlx::query(
            "UPDATE capacity_dispatches SET next_attempt_at = now() WHERE completed_at IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        dispatch_once(&pool, &provider).await;
    }

    let releases = provider
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::Release(_)))
        .count();
    assert_eq!(
        releases,
        usize::try_from(MAX_DISPATCH_ATTEMPTS).unwrap(),
        "the provider must be asked exactly as many times as the limit says, then left alone"
    );

    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "releasing",
        "an unreleased machine stays open, not filed as failed: {after:?}"
    );
    assert!(after.external_id.is_some(), "the handle is kept");

    // And it is surfaced for a person, which is the whole point of not retrying forever.
    let outstanding = outstanding_releases(&pool).await.unwrap();
    assert_eq!(outstanding.len(), 1, "{outstanding:?}");
    assert_eq!(outstanding[0].attempt_id, attempt_id);
    assert_eq!(outstanding[0].external_id, after.external_id);
    assert_eq!(outstanding[0].provider, "fake");
}

/// A provider that is never meant to be asked anything, because it did not make the machine.
struct WrongProvider;

#[async_trait]
impl CapacityProvider for WrongProvider {
    fn name(&self) -> &'static str {
        "not-fake"
    }

    async fn provision(&self, _spec: &CapacitySpec) -> Result<ProvisionedCapacity, CapacityError> {
        panic!("a request recorded against another provider must never reach this one");
    }

    async fn release(&self, _external_id: &str) -> Result<(), CapacityError> {
        panic!("a request recorded against another provider must never reach this one");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_dispatch_only_goes_to_the_provider_that_owns_it(pool: PgPool) {
    // `capacity_requests.provider` is the authority, not whichever provider object a caller
    // happened to construct. An external id means nothing to a provider that did not issue it,
    // and asking the wrong one to release it either errors or succeeds against something else.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();

    assert_eq!(
        dispatch_once(&pool, &WrongProvider).await,
        0,
        "a provider that did not own this request has nothing to do"
    );
    let untouched = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(untouched.status, "requested");

    // The owner still finds it, so the dispatch was skipped rather than consumed.
    let provider = FakeProvider::new();
    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    assert_eq!(
        snapshot(&pool, attempt_id).await.unwrap().unwrap().status,
        "ready"
    );
}

// ---------------------------------------------------------------------------------------------
// What the schema refuses
// ---------------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_request_cannot_be_filed_against_another_jobs_attempt(pool: PgPool) {
    // Three independent foreign keys would each be satisfied by an attempt from one job and a job
    // from another project, which is capacity billed to a project that never asked for it.
    let attempt_id = seed_attempt(&pool).await;
    let other_job_id = seed_queued_job(&pool).await;

    let mismatched = sqlx::query(
        "INSERT INTO capacity_requests \
         (id, attempt_id, job_id, project_id, provider, idempotency_key) \
         VALUES ($1, $2, $3, $4, 'fake', 'kratos-mismatched')",
    )
    .bind(Uuid::new_v4())
    .bind(attempt_id)
    .bind(other_job_id)
    .bind(crate::projects::DEFAULT_PROJECT_ID)
    .execute(&pool)
    .await;
    assert!(
        mismatched.is_err(),
        "an attempt belonging to another job must be refused"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_job_holding_capacity_cannot_be_deleted(pool: PgPool) {
    // Cascading the parent away would take `external_id` -- the only handle the machine can ever
    // be released by -- and the pending release dispatch with it, leaving a paid machine running
    // with nothing in the database pointing at it.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();
    assert_eq!(dispatch_once(&pool, &provider).await, 1);

    let job_id: Uuid =
        sqlx::query_scalar("SELECT job_id FROM capacity_requests WHERE attempt_id = $1")
            .bind(attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let blocked = sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(job_id)
        .execute(&pool)
        .await;
    assert!(
        blocked.is_err(),
        "deleting a job that still holds capacity must be refused"
    );

    // Once the machine is genuinely gone, the row is settled and no longer stands in the way.
    release_capacity(&pool, attempt_id).await.unwrap();
    assert_eq!(dispatch_once(&pool, &provider).await, 1);
    assert_eq!(
        snapshot(&pool, attempt_id).await.unwrap().unwrap().status,
        "released"
    );
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(job_id)
        .execute(&pool)
        .await
        .expect("a released request must not block ordinary cleanup");
}

// ---------------------------------------------------------------------------------------------
// Capacity that may exist without Kratos knowing
// ---------------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_request_being_provisioned_cannot_be_deleted(pool: PgPool) {
    // A request with no handle yet is the most dangerous thing to delete, not the safest: the
    // provider call is in flight this instant, and cascading the row away means its answer
    // arrives with nowhere to record the machine it describes.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let job_id: Uuid =
        sqlx::query_scalar("SELECT job_id FROM capacity_requests WHERE attempt_id = $1")
            .bind(attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    // Hold the provider mid-call, which is exactly when the row has a null handle and a machine
    // is being built. Forcing the window rather than hoping to land in it.
    let provider = Arc::new(FakeProvider::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    provider.hold_next_provision(gate.clone());
    let started = provider.provision_started();
    let dispatching = tokio::spawn({
        let pool = pool.clone();
        let provider = provider.clone();
        async move { dispatch_once(&pool, provider.as_ref()).await }
    });
    started.notified().await;

    let blocked = sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(job_id)
        .execute(&pool)
        .await;
    assert!(
        blocked.is_err(),
        "a request with a provider call in flight must not be deleted"
    );

    gate.notify_waiters();
    assert_eq!(dispatching.await.unwrap(), 1);
    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(after.status, "ready");
    assert!(
        after.external_id.is_some(),
        "the machine still had somewhere to be recorded: {after:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_vanished_request_is_never_provisioned(pool: PgPool) {
    // The narrower window: the request is read, and is gone by the time the dispatch claims it.
    // The claim is the permission to call the provider, so a claim that matches nothing must stop
    // the dispatch rather than be assumed to have worked.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let request_id = request_id_of(&pool, attempt_id).await;
    let dispatch = read_dispatch(&pool, request_id, "provision").await;
    let stale = sqlx::query_as::<_, RequestRow>(
        "SELECT id, attempt_id, job_id, status, external_id, idempotency_key, provider \
         FROM capacity_requests WHERE id = $1",
    )
    .bind(request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stale.status, "requested");

    // Settle and remove it, as a cancellation followed by retention would.
    release_capacity(&pool, attempt_id).await.unwrap();
    sqlx::query("DELETE FROM capacity_requests WHERE id = $1")
        .bind(request_id)
        .execute(&pool)
        .await
        .unwrap();

    // The controller still holds the read it took before any of that happened.
    let provider = FakeProvider::new();
    let outcome = provision(&pool, &provider, &dispatch, &stale)
        .await
        .unwrap();

    assert!(!outcome, "a vanished request completes nothing");
    assert!(
        provider.calls().is_empty(),
        "the provider must not be asked for a machine nothing can record: {:?}",
        provider.calls()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_provision_that_loses_its_answer_stays_reconcilable(pool: PgPool) {
    // The failure a boundary cannot see through. `Unavailable` promises nothing about side
    // effects: the provider may have built the machine and lost the reply. Treating that as
    // "nothing was made" is how a running machine stops being anybody's problem.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();

    for _ in 0..MAX_DISPATCH_ATTEMPTS {
        provider.lose_next_provision_response();
        sqlx::query(
            "UPDATE capacity_dispatches SET next_attempt_at = now() WHERE completed_at IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        dispatch_once(&pool, &provider).await;
    }

    // A machine exists. Kratos has never been told its name.
    assert_eq!(provider.machine_count(), 1);
    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "unreconciled",
        "an unknown outcome must not be recorded as a failure: {after:?}"
    );
    assert_eq!(after.external_id, None);
    assert_eq!(
        after.released_at, None,
        "and must stay in the open-request index, where a person can see it"
    );

    let ambiguous = ambiguous_provisions(&pool).await.unwrap();
    assert_eq!(ambiguous.len(), 1, "{ambiguous:?}");
    assert_eq!(ambiguous[0].attempt_id, attempt_id);
    assert_eq!(ambiguous[0].idempotency_key, idempotency_key(attempt_id));

    // Asking the same question again is what recovers the lost handle -- the whole reason the
    // idempotency key is derived rather than random.
    assert!(
        reconcile_provision(&pool, ambiguous[0].request_id)
            .await
            .unwrap()
    );
    assert_eq!(dispatch_once(&pool, &provider).await, 1);

    let reconciled = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(reconciled.status, "ready");
    assert!(reconciled.external_id.is_some());
    assert_eq!(
        provider.machine_count(),
        1,
        "reconciling must find the machine, not build another"
    );
    assert!(ambiguous_provisions(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_refused_provision_is_still_recorded_as_failed(pool: PgPool) {
    // The other side of the distinction, so that "we cannot be sure" does not swallow the case
    // where we can be. A positive refusal is a promise that nothing was created.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();

    for _ in 0..MAX_DISPATCH_ATTEMPTS {
        provider.fail_next_provision("no such machine type");
        sqlx::query(
            "UPDATE capacity_dispatches SET next_attempt_at = now() WHERE completed_at IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        dispatch_once(&pool, &provider).await;
    }

    assert_eq!(provider.machine_count(), 0);
    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "failed",
        "a refusal is knowledge, and must not be filed as an open question: {after:?}"
    );
    assert!(ambiguous_provisions(&pool).await.unwrap().is_empty());

    // And a failure that genuinely made nothing can still be settled and tidied away.
    release_capacity(&pool, attempt_id).await.unwrap();
    assert_eq!(
        snapshot(&pool, attempt_id).await.unwrap().unwrap().status,
        "released"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unreconciled_request_is_not_quietly_settled(pool: PgPool) {
    // Cancelling the job must not convert "we do not know" into "nothing is running". Only a
    // person can assert that, and the assertion is kept.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let provider = FakeProvider::new();
    for _ in 0..MAX_DISPATCH_ATTEMPTS {
        provider.lose_next_provision_response();
        sqlx::query(
            "UPDATE capacity_dispatches SET next_attempt_at = now() WHERE completed_at IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        dispatch_once(&pool, &provider).await;
    }

    release_capacity(&pool, attempt_id).await.unwrap();
    let after = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "unreconciled",
        "cancelling must not assert that an unknown machine does not exist: {after:?}"
    );

    let request_id = request_id_of(&pool, attempt_id).await;
    let job_id: Uuid = sqlx::query_scalar("SELECT job_id FROM capacity_requests WHERE id = $1")
        .bind(request_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("DELETE FROM jobs WHERE id = $1")
            .bind(job_id)
            .execute(&pool)
            .await
            .is_err(),
        "nor let it be deleted while the question is open"
    );

    // A person looks, finds nothing, and says so.
    assert!(
        resolve_unreconciled(
            &pool,
            request_id,
            "checked the console by hand; no such machine"
        )
        .await
        .unwrap()
    );
    let settled = snapshot(&pool, attempt_id).await.unwrap().unwrap();
    assert_eq!(settled.status, "released");
    assert_eq!(
        settled.last_error.as_deref(),
        Some("checked the console by hand; no such machine"),
        "the assertion is kept, because it was a judgement rather than an observation"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_last_attempt_commits_as_one_transaction(pool: PgPool) {
    // The crash boundary at attempt eight. Advancing the dispatch and moving the request are one
    // decision: apply the first alone and the dispatch is permanently past the due query's cutoff
    // while the request still looks live and appears on no operator surface -- stuck in a way
    // nothing would ever notice.
    let attempt_id = seed_attempt(&pool).await;
    request_capacity(&pool, attempt_id, "fake").await.unwrap();
    let request_id = request_id_of(&pool, attempt_id).await;
    sqlx::query("UPDATE capacity_dispatches SET attempts = $2 WHERE request_id = $1")
        .bind(request_id)
        .bind(MAX_DISPATCH_ATTEMPTS - 1)
        .execute(&pool)
        .await
        .unwrap();

    // Make the terminal statement -- and only the terminal statement -- fail, which is the crash
    // window in the middle of the give-up.
    sqlx::query(
        "ALTER TABLE capacity_requests \
         ADD CONSTRAINT no_unreconciled CHECK (status <> 'unreconciled') NOT VALID",
    )
    .execute(&pool)
    .await
    .unwrap();

    let dispatch = read_dispatch(&pool, request_id, "provision").await;
    record_failure(&pool, &dispatch, &CapacityError::Unavailable).await;

    let (attempts, last_error): (i32, Option<String>) =
        sqlx::query_as("SELECT attempts, last_error FROM capacity_dispatches WHERE id = $1")
            .bind(dispatch.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        attempts,
        MAX_DISPATCH_ATTEMPTS - 1,
        "a give-up that could not be completed must leave the dispatch retryable, not spent"
    );
    assert_eq!(last_error, None);
    assert_eq!(
        snapshot(&pool, attempt_id)
            .await
            .unwrap()
            .unwrap()
            .last_error,
        None,
        "and must write nothing about the request either"
    );

    // With the obstruction gone the same give-up applies completely, both parts together.
    sqlx::query("ALTER TABLE capacity_requests DROP CONSTRAINT no_unreconciled")
        .execute(&pool)
        .await
        .unwrap();
    record_failure(&pool, &dispatch, &CapacityError::Unavailable).await;
    let spent: i32 = sqlx::query_scalar("SELECT attempts FROM capacity_dispatches WHERE id = $1")
        .bind(dispatch.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(spent, MAX_DISPATCH_ATTEMPTS);
    assert_eq!(
        snapshot(&pool, attempt_id).await.unwrap().unwrap().status,
        "unreconciled",
        "the dispatch reaching its limit and the request moving are one decision"
    );
}

#[test]
fn the_feature_is_off_unless_it_is_switched_on() {
    // Off is the state in which nothing can ever be spent, so absent means off and only an
    // explicit "true" means on. A feature that can spend money should not be switched on by a
    // typo, an empty string, or a leftover "false".
    assert!(!enabled_from(None));
    assert!(!enabled_from(Some("")));
    assert!(!enabled_from(Some("false")));
    assert!(!enabled_from(Some("1")));
    assert!(!enabled_from(Some("yes")));
    assert!(!enabled_from(Some("truthy")));
    assert!(enabled_from(Some("true")));
    assert!(enabled_from(Some("TRUE")));
    assert!(enabled_from(Some("  true  ")));
}

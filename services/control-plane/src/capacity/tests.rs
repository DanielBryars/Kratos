//! The four properties the capacity boundary exists to guarantee.
//!
//! Each is proved against a real database and the fake provider. The double-dispatch test forces
//! its overlap rather than hoping for it, because joined futures do not overlap against a local
//! `PostgreSQL` — a lesson this project has now paid for more than once.

use std::sync::Arc;

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

//! Project concurrency limits, against a real `PostgreSQL` database.
//!
//! `tokio::join!` gives no overlap against a local database, so the concurrent cases hold the
//! project row in a separate transaction and wait until both schedulers are observed blocked on a
//! lock before releasing it. Slots are released through the real result endpoint, not by editing
//! job rows, so the lifecycle that frees a slot is the one production uses.

use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode, header::AUTHORIZATION},
};
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use super::{ACTIVE_PROJECT_JOBS, current_or_assign_job};
use crate::{
    app,
    credentials::{self, CredentialKind},
    projects::DEFAULT_PROJECT_ID,
};

struct TestWorker {
    id: Uuid,
    token: String,
}

async fn insert_owner(pool: &PgPool) -> Uuid {
    let owner_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
         VALUES ($1, 'test', $2, 'Owner')",
    )
    .bind(owner_id)
    .bind(Uuid::new_v4().to_string())
    .execute(pool)
    .await
    .unwrap();
    owner_id
}

/// An idle, credentialed worker speaking protocol 1.1, so it may take jobs with declared outputs.
async fn insert_worker(pool: &PgPool, owner_id: Uuid) -> TestWorker {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workers \
         (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities, project_id) \
         VALUES ($1, $2, $3, 'GPU worker', '1.1', 'idle', '{}'::jsonb, $4)",
    )
    .bind(id)
    .bind(owner_id)
    .bind(Uuid::new_v4())
    .bind(DEFAULT_PROJECT_ID)
    .execute(pool)
    .await
    .unwrap();
    let credential = credentials::issue(CredentialKind::Worker).unwrap();
    sqlx::query(
        "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, $3)",
    )
    .bind(credential.id)
    .bind(id)
    .bind(&credential.verifier)
    .execute(pool)
    .await
    .unwrap();
    TestWorker {
        id,
        token: credential.plaintext.expose().to_owned(),
    }
}

async fn insert_job(pool: &PgPool, owner_id: Uuid) -> Uuid {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, owner_identity_id, name, image_reference, timeout_seconds, project_id) \
         VALUES ($1, $2, 'Matrix check', $3, 120, $4)",
    )
    .bind(job_id)
    .bind(owner_id)
    .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
    .bind(DEFAULT_PROJECT_ID)
    .execute(pool)
    .await
    .unwrap();
    job_id
}

async fn set_limit(pool: &PgPool, limit: Option<i32>) {
    sqlx::query("UPDATE projects SET max_concurrent_jobs = $2 WHERE id = $1")
        .bind(DEFAULT_PROJECT_ID)
        .bind(limit)
        .execute(pool)
        .await
        .unwrap();
}

async fn active_jobs(pool: &PgPool) -> i64 {
    sqlx::query_scalar(ACTIVE_PROJECT_JOBS)
        .bind(DEFAULT_PROJECT_ID)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn job_status(pool: &PgPool, job_id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Wait until `expected` sessions in this test database are blocked on a lock.
async fn wait_for_lock_waiters(pool: &PgPool, expected: i64) {
    for _ in 0..400 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("expected {expected} schedulers to be waiting on the project row");
}

/// Report a result through the worker API, as a legacy worker without execution timing would.
async fn report_result(pool: &PgPool, worker: &TestWorker, attempt_id: Uuid) -> StatusCode {
    let worker_id = worker.id;
    app(None, Some(pool.clone()))
        .oneshot(
            Request::put(format!(
                "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/result"
            ))
            .header(AUTHORIZATION, format!("Bearer {}", worker.token))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "exit_code": 0,
                    "timed_out": false,
                    "stdout": "",
                    "stderr": "",
                    "failure_message": null
                })
                .to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[sqlx::test(migrations = "./migrations")]
async fn limit_of_one_never_oversubscribes_two_concurrent_workers(pool: PgPool) {
    let owner = insert_owner(&pool).await;
    let first = insert_worker(&pool, owner).await;
    let second = insert_worker(&pool, owner).await;
    insert_job(&pool, owner).await;
    insert_job(&pool, owner).await;

    // An uncommitted policy change holds the project row, so both schedulers are provably queued
    // behind it and must each decide against the limit it commits.
    let mut policy = pool.begin().await.unwrap();
    sqlx::query("UPDATE projects SET max_concurrent_jobs = 1 WHERE id = $1")
        .bind(DEFAULT_PROJECT_ID)
        .execute(&mut *policy)
        .await
        .unwrap();
    let schedulers = [first.id, second.id].map(|worker_id| {
        let pool = pool.clone();
        tokio::spawn(async move { current_or_assign_job(&pool, worker_id, true, None).await })
    });
    wait_for_lock_waiters(&pool, 2).await;
    policy.commit().await.unwrap();

    let mut assigned = 0;
    for scheduler in schedulers {
        if scheduler.await.unwrap().unwrap().is_some() {
            assigned += 1;
        }
    }
    assert_eq!(assigned, 1);
    let (attempts, queued, busy): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM job_attempts), \
                (SELECT count(*) FROM jobs WHERE status = 'queued'), \
                (SELECT count(*) FROM workers WHERE status = 'busy')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((attempts, queued, busy), (1, 1, 1));
    assert_eq!(active_jobs(&pool).await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn unlimited_project_assigns_both_concurrent_workers(pool: PgPool) {
    let owner = insert_owner(&pool).await;
    let first = insert_worker(&pool, owner).await;
    let second = insert_worker(&pool, owner).await;
    insert_job(&pool, owner).await;
    insert_job(&pool, owner).await;

    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR NO KEY UPDATE")
        .bind(DEFAULT_PROJECT_ID)
        .execute(&mut *holder)
        .await
        .unwrap();
    let schedulers = [first.id, second.id].map(|worker_id| {
        let pool = pool.clone();
        tokio::spawn(async move { current_or_assign_job(&pool, worker_id, true, None).await })
    });
    wait_for_lock_waiters(&pool, 2).await;
    holder.commit().await.unwrap();

    let mut jobs = Vec::new();
    for scheduler in schedulers {
        jobs.push(scheduler.await.unwrap().unwrap().unwrap().job_id);
    }
    assert_ne!(jobs[0], jobs[1]);
    assert_eq!(active_jobs(&pool).await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn cancelling_job_holds_its_slot_until_acknowledged(pool: PgPool) {
    let owner = insert_owner(&pool).await;
    let first = insert_worker(&pool, owner).await;
    let second = insert_worker(&pool, owner).await;
    let cancelled_job = insert_job(&pool, owner).await;
    let waiting_job = insert_job(&pool, owner).await;
    set_limit(&pool, Some(1)).await;

    let assignment = current_or_assign_job(&pool, first.id, true, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(assignment.job_id, cancelled_job);
    sqlx::query(
        "UPDATE jobs SET status = 'cancelling', cancel_requested_at = now() \
         WHERE id = $1 AND status IN ('assigned', 'running')",
    )
    .bind(cancelled_job)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        current_or_assign_job(&pool, second.id, true, None)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(job_status(&pool, waiting_job).await, "queued");

    assert_eq!(
        report_result(&pool, &first, assignment.attempt_id).await,
        StatusCode::OK
    );
    assert_eq!(job_status(&pool, cancelled_job).await, "cancelled");
    let resumed = current_or_assign_job(&pool, second.id, true, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.job_id, waiting_job);
}

#[sqlx::test(migrations = "./migrations")]
async fn cap_keeps_replay_and_resumes_a_due_job_after_release(pool: PgPool) {
    let owner = insert_owner(&pool).await;
    let first = insert_worker(&pool, owner).await;
    let second = insert_worker(&pool, owner).await;
    let running_job = insert_job(&pool, owner).await;
    let due_job = insert_job(&pool, owner).await;
    sqlx::query("UPDATE jobs SET earliest_start_at = now() - interval '1 second' WHERE id = $1")
        .bind(due_job)
        .execute(&pool)
        .await
        .unwrap();
    set_limit(&pool, Some(1)).await;

    let assignment = current_or_assign_job(&pool, first.id, true, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(assignment.job_id, running_job);
    // At capacity, the worker that holds the slot still gets its own assignment back.
    let replay = current_or_assign_job(&pool, first.id, true, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replay.attempt_id, assignment.attempt_id);
    assert!(
        current_or_assign_job(&pool, second.id, true, None)
            .await
            .unwrap()
            .is_none()
    );
    let due_attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM job_attempts WHERE job_id = $1")
            .bind(due_job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(due_attempts, 0);

    assert_eq!(
        report_result(&pool, &first, assignment.attempt_id).await,
        StatusCode::OK
    );
    assert_eq!(active_jobs(&pool).await, 0);
    let resumed = current_or_assign_job(&pool, second.id, true, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.job_id, due_job);
}

#[sqlx::test(migrations = "./migrations")]
async fn pending_output_delivery_holds_its_slot(pool: PgPool) {
    let owner = insert_owner(&pool).await;
    let first = insert_worker(&pool, owner).await;
    let second = insert_worker(&pool, owner).await;
    let delivering_job = insert_job(&pool, owner).await;
    insert_job(&pool, owner).await;
    sqlx::query(
        "INSERT INTO job_output_requirements \
         (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
         VALUES ($1, $2, 'model.pt', 'checkpoint', 'application/octet-stream', true, 1024)",
    )
    .bind(Uuid::new_v4())
    .bind(delivering_job)
    .execute(&pool)
    .await
    .unwrap();
    set_limit(&pool, Some(1)).await;

    let assignment = current_or_assign_job(&pool, first.id, true, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(assignment.job_id, delivering_job);
    // Success without the verified output is refused, so the job is not terminal yet.
    assert_eq!(
        report_result(&pool, &first, assignment.attempt_id).await,
        StatusCode::CONFLICT
    );
    assert_eq!(job_status(&pool, delivering_job).await, "assigned");
    assert!(
        current_or_assign_job(&pool, second.id, true, None)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn lowering_the_limit_does_not_preempt_active_jobs(pool: PgPool) {
    let owner = insert_owner(&pool).await;
    let first = insert_worker(&pool, owner).await;
    let second = insert_worker(&pool, owner).await;
    let third = insert_worker(&pool, owner).await;
    let first_job = insert_job(&pool, owner).await;
    let second_job = insert_job(&pool, owner).await;
    let third_job = insert_job(&pool, owner).await;

    let first_assignment = current_or_assign_job(&pool, first.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let second_assignment = current_or_assign_job(&pool, second.id, true, None)
        .await
        .unwrap()
        .unwrap();
    set_limit(&pool, Some(1)).await;

    assert_eq!(job_status(&pool, first_job).await, "assigned");
    assert_eq!(job_status(&pool, second_job).await, "assigned");
    assert!(
        current_or_assign_job(&pool, third.id, true, None)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        report_result(&pool, &first, first_assignment.attempt_id).await,
        StatusCode::OK
    );
    // One slot freed, but one job is still active against a limit of one.
    assert!(
        current_or_assign_job(&pool, third.id, true, None)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        report_result(&pool, &second, second_assignment.attempt_id).await,
        StatusCode::OK
    );
    let resumed = current_or_assign_job(&pool, third.id, true, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.job_id, third_job);
}

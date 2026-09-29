//! Last-slot fairness uses real transactions and independently credentialed peer fixtures.
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::super::current_or_assign_job;

struct Fixture {
    owner: Uuid,
    project: Uuid,
    legacy: Uuid,
    peer: Uuid,
}

fn capabilities(protocol: &str) -> Value {
    json!({
        "protocol_version": protocol, "collected_at": Utc::now(),
        "hostname": "fairness-worker", "operating_system": "linux",
        "operating_system_version": "6.8", "architecture": "x86_64",
        "logical_cpu_count": 8, "memory_total_bytes": 16_000_000_000_u64,
        "storage_available_bytes": 100_000_000_000_u64, "python_version": "3.12",
        "gpus": [{"index": 0, "name": "Test GPU", "memory_total_bytes": 8_000_000_000_u64,
                  "driver_version": "560.35"}],
        "gpu_health": {"status": "healthy", "detail": "Computation passed", "evidence": {
            "schema_version": "1.0", "status": "healthy", "checked_at": Utc::now(),
            "image_reference": format!("example.test/health@sha256:{}", "a".repeat(64)),
            "device_index": 0, "device_name": "Test GPU", "operation": "matrix multiplication",
            "matrix_size": 512, "max_absolute_error": 0.0, "duration_ms": 10.0,
            "cuda_driver_api_version": "13.3", "cuda_runtime_version": "12.9"
        }}
    })
}

async fn worker(pool: &PgPool, owner: Uuid, project: Uuid, protocol: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workers (id, owner_identity_id, agent_instance_id, display_name, \
        protocol_version, status, capabilities, project_id, last_seen_at) \
        VALUES ($1, $2, $3, 'Fairness worker', $4, 'idle', $5, $6, now())",
    )
    .bind(id)
    .bind(owner)
    .bind(Uuid::new_v4())
    .bind(protocol)
    .bind(capabilities(protocol))
    .bind(project)
    .execute(pool)
    .await
    .unwrap();
    // Scheduling reads credential validity, never a plaintext token or its verifier.
    sqlx::query(
        "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, 'fixture')",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn fixture(pool: &PgPool, limit: Option<i32>, protocol: &str) -> Fixture {
    let owner = Uuid::new_v4();
    let project = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
        VALUES ($1, 'test', $2, 'Fairness owner')",
    )
    .bind(owner)
    .bind(owner.to_string())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO projects (id, name, max_concurrent_jobs) VALUES ($1, 'Fairness', $2)")
        .bind(project)
        .bind(limit)
        .execute(pool)
        .await
        .unwrap();
    let legacy = worker(pool, owner, project, "1.0").await;
    let peer = worker(pool, owner, project, protocol).await;
    Fixture {
        owner,
        project,
        legacy,
        peer,
    }
}

async fn job(pool: &PgPool, f: &Fixture, age: i32, required: u8) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, owner_identity_id, name, image_reference, timeout_seconds, \
        project_id, submitted_at) VALUES ($1, $2, 'Fairness job', $3, 120, $4, \
        statement_timestamp() - make_interval(secs => $5))",
    )
    .bind(id)
    .bind(f.owner)
    .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
    .bind(f.project)
    .bind(f64::from(age))
    .execute(pool)
    .await
    .unwrap();
    if required == 1 {
        sqlx::query(
            "INSERT INTO job_output_requirements \
            (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
            VALUES ($1, $2, 'model.pt', 'checkpoint', 'application/octet-stream', true, 1024)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    } else if required == 3 {
        let dataset = Uuid::new_v4();
        let version = Uuid::new_v4();
        sqlx::query("INSERT INTO datasets (id, project_id, name, created_by_identity_id) VALUES ($1, $2, $3, $4)")
            .bind(dataset).bind(f.project).bind(dataset.to_string()).bind(f.owner).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO dataset_versions (id, dataset_id, project_id, version_number, source_kind, \
            status, manifest_sha256, info_json, total_episodes, total_frames, fps, created_by_identity_id, ready_at) \
            VALUES ($1, $2, $3, 1, 'upload', 'ready', $4, '{}'::jsonb, 1, 10, 10, $5, now())")
            .bind(version).bind(dataset).bind(f.project).bind("b".repeat(64)).bind(f.owner)
            .execute(pool).await.unwrap();
        sqlx::query("INSERT INTO job_dataset_inputs (id, job_id, alias, dataset_id, dataset_version_id, \
            source_kind, dataset_manifest_sha256) VALUES ($1, $2, 'training_data', $3, $4, 'upload', $5)")
            .bind(Uuid::new_v4()).bind(id).bind(dataset).bind(version).bind("b".repeat(64))
            .execute(pool).await.unwrap();
    }
    id
}

async fn assigned(pool: &PgPool, worker_id: Uuid) -> Option<Uuid> {
    current_or_assign_job(pool, worker_id, true, None)
        .await
        .unwrap()
        .map(|a| a.job_id)
}

#[sqlx::test(migrations = "./migrations")]
async fn repeated_legacy_requests_preserve_old_output_and_dataset_jobs(pool: PgPool) {
    for (required, protocol) in [(1, "1.1"), (3, "1.3")] {
        let f = fixture(&pool, Some(1), protocol).await;
        let oldest = job(&pool, &f, 100, required).await;
        let newer = job(&pool, &f, 10, 0).await;
        for _ in 0..5 {
            assert_eq!(assigned(&pool, f.legacy).await, None);
        }
        assert_eq!(assigned(&pool, f.peer).await, Some(oldest));
        assert_eq!(assigned(&pool, f.legacy).await, None);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM job_attempts WHERE job_id = $1")
            .bind(newer)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn unusable_peers_do_not_reserve_the_last_slot(pool: PgPool) {
    let mutations = [
        "UPDATE workers SET last_seen_at = now() - interval '91 seconds' WHERE id = $1",
        "UPDATE workers SET last_seen_at = NULL WHERE id = $1",
        "UPDATE workers SET last_seen_at = now() + interval '1 hour' WHERE id = $1",
        "UPDATE workers SET status = 'revoked' WHERE id = $1",
        "UPDATE workers SET status = 'unapproved' WHERE id = $1",
        "UPDATE workers SET status = 'busy' WHERE id = $1",
        "UPDATE workers SET status = 'draining' WHERE id = $1",
        "UPDATE workers SET status = 'quarantined' WHERE id = $1",
        "UPDATE workers SET capabilities = jsonb_set(capabilities, '{gpu_health}', \
            '{\"status\":\"unverified\",\"detail\":\"No evidence\",\"evidence\":null}'::jsonb) WHERE id = $1",
        "UPDATE workers SET capabilities = jsonb_set(capabilities, '{gpu_health,status}', '\"unhealthy\"'::jsonb) WHERE id = $1",
        "UPDATE workers SET capabilities = jsonb_set(capabilities, '{gpus}', '[]'::jsonb) WHERE id = $1",
        "UPDATE workers SET capabilities = '{}'::jsonb WHERE id = $1",
        "UPDATE workers SET capabilities = jsonb_set(capabilities, '{logical_cpu_count}', '0'::jsonb) WHERE id = $1",
        "UPDATE workers SET protocol_version = '1.0' WHERE id = $1",
        "UPDATE worker_credentials SET revoked_at = now() WHERE worker_id = $1",
        "UPDATE worker_credentials SET created_at = now() - interval '2 hours', \
            expires_at = now() - interval '1 hour' WHERE worker_id = $1",
        "DELETE FROM worker_credentials WHERE worker_id = $1",
    ];
    for mutation in mutations {
        let f = fixture(&pool, Some(1), "1.3").await;
        job(&pool, &f, 100, 3).await;
        let newer = job(&pool, &f, 10, 0).await;
        sqlx::query(mutation)
            .bind(f.peer)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(assigned(&pool, f.legacy).await, Some(newer), "{mutation}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn foreign_project_and_insufficient_protocol_peers_do_not_defer(pool: PgPool) {
    for protocol in ["1.0", "1.1", "1.2", "1.3"] {
        let f = fixture(&pool, Some(1), protocol).await;
        job(&pool, &f, 100, 3).await;
        let newer = job(&pool, &f, 10, 0).await;
        if protocol == "1.3" {
            let foreign = fixture(&pool, Some(1), "1.0").await;
            sqlx::query("UPDATE workers SET project_id = $2 WHERE id = $1")
                .bind(f.peer)
                .bind(foreign.project)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(assigned(&pool, f.legacy).await, Some(newer));
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn idle_peer_with_active_attempt_is_not_available(pool: PgPool) {
    let f = fixture(&pool, Some(2), "1.3").await;
    let busy = job(&pool, &f, 200, 0).await;
    assert_eq!(assigned(&pool, f.peer).await, Some(busy));
    sqlx::query("UPDATE workers SET status = 'idle' WHERE id = $1")
        .bind(f.peer)
        .execute(&pool)
        .await
        .unwrap();
    job(&pool, &f, 100, 3).await;
    let newer = job(&pool, &f, 10, 0).await;
    assert_eq!(assigned(&pool, f.legacy).await, Some(newer));
}

#[sqlx::test(migrations = "./migrations")]
async fn disconnected_peer_reservation_expires_without_refreshing_its_timestamp(pool: PgPool) {
    let f = fixture(&pool, Some(1), "1.3").await;
    job(&pool, &f, 100, 3).await;
    let newer = job(&pool, &f, 10, 0).await;
    assert_eq!(assigned(&pool, f.legacy).await, None);
    sqlx::query("UPDATE workers SET last_seen_at = now() - interval '91 seconds' WHERE id = $1")
        .bind(f.peer)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(assigned(&pool, f.legacy).await, Some(newer));
}

#[sqlx::test(migrations = "./migrations")]
async fn future_cancelled_and_exhausted_jobs_do_not_hold_a_slot(pool: PgPool) {
    for state in ["future", "cancelled", "exhausted"] {
        let f = fixture(&pool, Some(1), "1.3").await;
        let oldest = job(&pool, &f, 100, 3).await;
        let newer = job(&pool, &f, 10, 0).await;
        match state {
            "future" => {
                sqlx::query(
                    "UPDATE jobs SET earliest_start_at = now() + interval '1 hour' WHERE id = $1",
                )
                .bind(oldest)
                .execute(&pool)
                .await
                .unwrap();
            }
            "cancelled" => {
                sqlx::query(
                    "UPDATE jobs SET status = 'cancelled', cancel_requested_at = now(), \
                     finished_at = now() WHERE id = $1",
                )
                .bind(oldest)
                .execute(&pool)
                .await
                .unwrap();
            }
            _ => {
                sqlx::query("UPDATE jobs SET max_attempts = 1 WHERE id = $1")
                    .bind(oldest)
                    .execute(&pool)
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO job_attempts (id, job_id, attempt_number, worker_id, status, lease_expires_at) \
                    VALUES ($1, $2, 1, $3, 'failed', now() - interval '1 hour')")
                    .bind(Uuid::new_v4()).bind(oldest).bind(f.peer).execute(&pool).await.unwrap();
            }
        }
        assert_eq!(assigned(&pool, f.legacy).await, Some(newer), "{state}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn unlimited_or_multiple_free_slots_allow_compatible_backfill(pool: PgPool) {
    for limit in [None, Some(2)] {
        let f = fixture(&pool, limit, "1.3").await;
        let oldest = job(&pool, &f, 100, 3).await;
        let newer = job(&pool, &f, 10, 0).await;
        assert_eq!(assigned(&pool, f.legacy).await, Some(newer));
        assert_eq!(assigned(&pool, f.peer).await, Some(oldest));
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn one_peer_does_not_reserve_all_slots_for_many_old_jobs(pool: PgPool) {
    let f = fixture(&pool, Some(2), "1.3").await;
    let oldest = job(&pool, &f, 300, 3).await;
    job(&pool, &f, 200, 3).await;
    job(&pool, &f, 100, 3).await;
    let newer = job(&pool, &f, 10, 0).await;
    assert_eq!(assigned(&pool, f.legacy).await, Some(newer));
    assert_eq!(assigned(&pool, f.peer).await, Some(oldest));
}

#[sqlx::test(migrations = "./migrations")]
async fn existing_assignment_replays_even_when_project_is_full(pool: PgPool) {
    let f = fixture(&pool, Some(1), "1.3").await;
    let oldest = job(&pool, &f, 100, 3).await;
    let first = current_or_assign_job(&pool, f.peer, true, None)
        .await
        .unwrap()
        .unwrap();
    job(&pool, &f, 10, 0).await;
    let replay = current_or_assign_job(&pool, f.peer, true, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.job_id, oldest);
    assert_eq!(first.attempt_id, replay.attempt_id);
    assert_eq!(assigned(&pool, f.legacy).await, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn simultaneous_schedulers_do_not_lock_each_other_and_oldest_wins(pool: PgPool) {
    let f = fixture(&pool, Some(1), "1.3").await;
    let oldest = job(&pool, &f, 100, 3).await;
    job(&pool, &f, 10, 0).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR NO KEY UPDATE")
        .bind(f.project)
        .execute(&mut *holder)
        .await
        .unwrap();
    let tasks = [f.legacy, f.peer].map(|worker_id| {
        let pool = pool.clone();
        tokio::spawn(async move { assigned(&pool, worker_id).await })
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE '%SELECT p.id, p.max_concurrent_jobs%'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if count >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("both schedulers must contend on the held project lock");
    holder.commit().await.unwrap();
    let [legacy, peer] = tasks;
    let outcomes = tokio::time::timeout(Duration::from_secs(10), async {
        (legacy.await.unwrap(), peer.await.unwrap())
    })
    .await
    .expect("peer inspection must not deadlock worker locks");
    assert_eq!(outcomes, (None, Some(oldest)));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM job_attempts a JOIN jobs j ON j.id = a.job_id WHERE j.project_id = $1")
        .bind(f.project).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn submission_order_and_uuid_tie_break_are_stable(pool: PgPool) {
    for equal_time in [false, true] {
        let f = fixture(&pool, Some(1), "1.3").await;
        let first = job(&pool, &f, 100, 0).await;
        let second = job(&pool, &f, 10, 0).await;
        if equal_time {
            sqlx::query(
                "UPDATE jobs SET submitted_at = '2026-01-01T00:00:00Z' WHERE project_id = $1",
            )
            .bind(f.project)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(
            assigned(&pool, f.legacy).await,
            Some(if equal_time { first.min(second) } else { first })
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn peer_worker_row_lock_does_not_block_fairness_decision(pool: PgPool) {
    let f = fixture(&pool, Some(1), "1.3").await;
    let oldest = job(&pool, &f, 100, 3).await;
    job(&pool, &f, 10, 0).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workers WHERE id = $1 FOR UPDATE")
        .bind(f.peer)
        .execute(&mut *holder)
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), assigned(&pool, f.legacy))
        .await
        .expect("fairness must read the committed peer without acquiring its row lock");
    assert_eq!(result, None);
    holder.commit().await.unwrap();
    assert_eq!(assigned(&pool, f.peer).await, Some(oldest));
}

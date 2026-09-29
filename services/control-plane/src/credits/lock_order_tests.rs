//! Credit writes must use the identity → project order used by membership operations.
use std::time::Duration;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use super::{EntryRequest, record_entry};

#[sqlx::test(migrations = "./migrations")]
async fn membership_operation_and_credit_write_do_not_invert_identity_project_locks(pool: PgPool) {
    let identity = Uuid::new_v4();
    let project = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name, role) \
         VALUES ($1, 'test', $2, 'Credit manager', 'operator')",
    )
    .bind(identity)
    .bind(identity.to_string())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO projects (id, name) VALUES ($1, 'Lock order')")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_memberships (project_id, identity_id) VALUES ($1, $2)")
        .bind(project)
        .bind(identity)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_credit_managers (project_id, identity_id) VALUES ($1, $2)")
        .bind(project)
        .bind(identity)
        .execute(&pool)
        .await
        .unwrap();

    // Invitation/member handlers retain the authenticated identity lock while taking the
    // project lock. Pause that real lock sequence between its two locks.
    let mut membership = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM human_identities WHERE id = $1 FOR UPDATE")
        .bind(identity)
        .execute(&mut *membership)
        .await
        .unwrap();
    let writer_pool = pool.clone();
    let writer = tokio::spawn(async move {
        record_entry(
            &writer_pool,
            identity,
            project,
            Uuid::new_v4(),
            &EntryRequest::Grant {
                amount: 1,
                reason: "Lock test".to_owned(),
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock' \
                   AND (query LIKE 'SELECT id FROM human_identities%' \
                        OR query LIKE 'SELECT m.identity_id FROM project_credit_managers%'))",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("credit writer must reach the held identity lock");
    // If the writer already holds the project lock, these transactions deadlock. Correct
    // ordering leaves this lock free and lets the membership operation finish first.
    tokio::time::timeout(
        Duration::from_secs(5),
        sqlx::query("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
            .bind(project)
            .execute(&mut *membership),
    )
    .await
    .expect("membership must not wait for a credit writer holding its project")
    .unwrap();
    membership.commit().await.unwrap();
    let (status, _) = tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::CREATED);
}

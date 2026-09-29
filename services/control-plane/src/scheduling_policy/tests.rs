//! Scheduling policy API against a real `PostgreSQL` database: payload semantics, authorisation,
//! project scoping, audit, and the membership recheck under the project row lock.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header::AUTHORIZATION},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::{
    app_with_human_auth,
    human_auth::{ClientAuthConfig, HumanAuth, HumanIdentity, IdentityVerifier, VerifyError},
    projects::DEFAULT_PROJECT_ID,
};

const PATH: &str = "/api/v1/operator/scheduling-policy";

struct SingleToken {
    token: &'static str,
    identity: HumanIdentity,
}

#[async_trait]
impl IdentityVerifier for SingleToken {
    async fn verify(&self, id_token: &str) -> Result<HumanIdentity, VerifyError> {
        if id_token == self.token {
            Ok(self.identity.clone())
        } else {
            Err(VerifyError::Rejected)
        }
    }
}

/// A router authenticating one subject with `token`. Nobody is the bootstrap operator, so a
/// subject must already exist to be authorised.
fn router_for(pool: &PgPool, subject: &str, token: &'static str) -> axum::Router {
    let identity = HumanIdentity {
        subject: subject.to_owned(),
        email: format!("{subject}@example.com"),
        display_name: subject.to_owned(),
    };
    let auth = HumanAuth::new(
        Arc::new(SingleToken { token, identity }),
        "nobody@example.com",
        ClientAuthConfig {
            api_key: "test-api-key".to_owned(),
            auth_domain: "example.test".to_owned(),
            project_id: "test-project".to_owned(),
        },
    );
    app_with_human_auth(None, Some(pool.clone()), Some(auth))
}

async fn call(
    router: &axum::Router,
    method: Method,
    token: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(PATH);
    if let Some(token) = token {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_owned()));
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Existing operator identity with an active membership of `project_id`.
async fn member(pool: &PgPool, subject: &str, project_id: Uuid) -> Uuid {
    let identity_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name, role) \
         VALUES ($1, 'identity-platform', $2, $2, 'operator')",
    )
    .bind(identity_id)
    .bind(subject)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO project_memberships (project_id, identity_id) VALUES ($1, $2)")
        .bind(project_id)
        .bind(identity_id)
        .execute(pool)
        .await
        .unwrap();
    identity_id
}

async fn second_project(pool: &PgPool) -> Uuid {
    let project_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects (id, name) VALUES ($1, 'Second project')")
        .bind(project_id)
        .execute(pool)
        .await
        .unwrap();
    project_id
}

async fn insert_job(pool: &PgPool, owner: Uuid, project_id: Uuid, status: &str) {
    sqlx::query(
        "INSERT INTO jobs \
         (id, owner_identity_id, name, image_reference, timeout_seconds, project_id, status, \
          cancel_requested_at, finished_at) \
         VALUES ($1, $2, 'Job', $3, 120, $4, $5, \
                 CASE WHEN $5 IN ('cancelling', 'cancelled') THEN now() END, \
                 CASE WHEN $5 IN ('succeeded', 'failed', 'cancelled') THEN now() END)",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
    .bind(project_id)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn stored_limit(pool: &PgPool, project_id: Uuid) -> Option<i32> {
    sqlx::query_scalar("SELECT max_concurrent_jobs FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn policy_audits(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT detail FROM audit_events \
         WHERE action = 'project.scheduling_policy.updated' ORDER BY occurred_at, id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn policy_defaults_to_unlimited_and_sets_and_clears_explicitly(pool: PgPool) {
    let owner = member(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let router = router_for(&pool, "owner", "owner-token");
    insert_job(&pool, owner, DEFAULT_PROJECT_ID, "queued").await;
    insert_job(&pool, owner, DEFAULT_PROJECT_ID, "assigned").await;
    insert_job(&pool, owner, DEFAULT_PROJECT_ID, "running").await;
    insert_job(&pool, owner, DEFAULT_PROJECT_ID, "cancelling").await;
    insert_job(&pool, owner, DEFAULT_PROJECT_ID, "succeeded").await;
    insert_job(&pool, owner, DEFAULT_PROJECT_ID, "cancelled").await;

    let (status, body) = call(&router, Method::GET, Some("owner-token"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "project_id": DEFAULT_PROJECT_ID,
            "max_concurrent_jobs": null,
            "active_jobs": 3
        })
    );

    let (status, body) = call(
        &router,
        Method::PUT,
        Some("owner-token"),
        Some(r#"{"max_concurrent_jobs":2}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max_concurrent_jobs"], 2);
    assert_eq!(body["active_jobs"], 3);
    assert_eq!(stored_limit(&pool, DEFAULT_PROJECT_ID).await, Some(2));

    let (status, body) = call(
        &router,
        Method::PUT,
        Some("owner-token"),
        Some(r#"{"max_concurrent_jobs":null}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max_concurrent_jobs"], Value::Null);
    assert_eq!(stored_limit(&pool, DEFAULT_PROJECT_ID).await, None);

    let audits = policy_audits(&pool).await;
    assert_eq!(
        audits,
        vec![
            json!({"previous_max_concurrent_jobs": null, "max_concurrent_jobs": 2}),
            json!({"previous_max_concurrent_jobs": 2, "max_concurrent_jobs": null}),
        ]
    );
    let actor: Uuid = sqlx::query_scalar(
        "SELECT actor_id FROM audit_events WHERE action = 'project.scheduling_policy.updated' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actor, owner);
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_or_missing_limits_are_rejected_without_clearing(pool: PgPool) {
    member(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let router = router_for(&pool, "owner", "owner-token");
    let (status, _) = call(
        &router,
        Method::PUT,
        Some("owner-token"),
        Some(r#"{"max_concurrent_jobs":3}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for body in [
        "{}",
        r#"{"max_concurrent_jobs":0}"#,
        r#"{"max_concurrent_jobs":-1}"#,
        r#"{"max_concurrent_jobs":1.5}"#,
        r#"{"max_concurrent_jobs":"2"}"#,
        r#"{"max_concurrent_jobs":3000000000}"#,
        r#"{"max_concurrent_jobs":2,"fair_share":true}"#,
        "not json",
    ] {
        let (status, reply) = call(&router, Method::PUT, Some("owner-token"), Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(reply["code"], "invalid_request", "{body}");
    }
    assert_eq!(stored_limit(&pool, DEFAULT_PROJECT_ID).await, Some(3));
    assert_eq!(policy_audits(&pool).await.len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn policy_requires_an_authenticated_project_member(pool: PgPool) {
    member(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let owner_router = router_for(&pool, "owner", "owner-token");
    let limit = Some(r#"{"max_concurrent_jobs":1}"#);

    for method in [Method::GET, Method::PUT] {
        let body = (method == Method::PUT).then_some(r#"{"max_concurrent_jobs":1}"#);
        assert_eq!(
            call(&owner_router, method.clone(), None, body).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&owner_router, method, Some("wrong-token"), body)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    // Authentication is checked before the body, so an invalid body still answers 401.
    assert_eq!(
        call(&owner_router, Method::PUT, None, Some("{}")).await.0,
        StatusCode::UNAUTHORIZED
    );

    // Unknown to Kratos and not the bootstrap address.
    let stranger = router_for(&pool, "stranger", "stranger-token");
    assert_eq!(
        call(&stranger, Method::PUT, Some("stranger-token"), limit)
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    // Known, but removed from the project.
    let former = member(&pool, "former", DEFAULT_PROJECT_ID).await;
    sqlx::query(
        "UPDATE project_memberships SET revoked_at = now(), revoked_by_identity_id = $1 \
         WHERE identity_id = $1",
    )
    .bind(former)
    .execute(&pool)
    .await
    .unwrap();
    let former_router = router_for(&pool, "former", "former-token");
    for method in [Method::GET, Method::PUT] {
        let body = (method == Method::PUT).then_some(r#"{"max_concurrent_jobs":1}"#);
        assert_eq!(
            call(&former_router, method, Some("former-token"), body)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }

    assert_eq!(stored_limit(&pool, DEFAULT_PROJECT_ID).await, None);
    assert!(policy_audits(&pool).await.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn policy_is_scoped_to_the_callers_project(pool: PgPool) {
    let other_project = second_project(&pool).await;
    let outsider = member(&pool, "outsider", other_project).await;
    insert_job(&pool, outsider, other_project, "running").await;
    insert_job(&pool, outsider, DEFAULT_PROJECT_ID, "running").await;
    insert_job(&pool, outsider, DEFAULT_PROJECT_ID, "assigned").await;
    let router = router_for(&pool, "outsider", "outsider-token");

    let (status, body) = call(&router, Method::GET, Some("outsider-token"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["project_id"], other_project.to_string());
    assert_eq!(body["active_jobs"], 1);

    let (status, _) = call(
        &router,
        Method::PUT,
        Some("outsider-token"),
        Some(r#"{"max_concurrent_jobs":4}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored_limit(&pool, other_project).await, Some(4));
    assert_eq!(stored_limit(&pool, DEFAULT_PROJECT_ID).await, None);

    // A caller in two projects must not have one guessed for them.
    sqlx::query("INSERT INTO project_memberships (project_id, identity_id) VALUES ($1, $2)")
        .bind(DEFAULT_PROJECT_ID)
        .bind(outsider)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = call(
        &router,
        Method::PUT,
        Some("outsider-token"),
        Some(r#"{"max_concurrent_jobs":1}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "project_required");
    assert_eq!(
        call(&router, Method::GET, Some("outsider-token"), None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(stored_limit(&pool, other_project).await, Some(4));
    assert_eq!(stored_limit(&pool, DEFAULT_PROJECT_ID).await, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_rechecks_membership_after_a_concurrent_revocation(pool: PgPool) {
    let owner = member(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let co_owner = member(&pool, "co-owner", DEFAULT_PROJECT_ID).await;
    let router = router_for(&pool, "owner", "owner-token");

    // Revoke as the other owner: using the caller as the revoker would take a foreign-key
    // lock on their identity and block authentication before it reaches the policy recheck.
    let mut revocation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
        .bind(DEFAULT_PROJECT_ID)
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE project_memberships SET revoked_at = now(), revoked_by_identity_id = $2 \
         WHERE identity_id = $1",
    )
    .bind(owner)
    .bind(co_owner)
    .execute(&mut *revocation)
    .await
    .unwrap();

    let update = tokio::spawn(async move {
        call(
            &router,
            Method::PUT,
            Some("owner-token"),
            Some(r#"{"max_concurrent_jobs":1}"#),
        )
        .await
    });
    // Authentication reads the still-committed membership, then the update blocks on the row.
    let mut blocked = false;
    for _ in 0..400 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
               AND query LIKE '%FROM projects WHERE id = $1 FOR NO KEY UPDATE%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting >= 1 {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(blocked, "the policy update should wait on the project row");
    revocation.commit().await.unwrap();

    let (status, _) = update.await.unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(stored_limit(&pool, DEFAULT_PROJECT_ID).await, None);
    assert!(policy_audits(&pool).await.is_empty());
}

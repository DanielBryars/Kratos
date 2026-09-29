//! The operator capacity view against a real `PostgreSQL` database: who may read it, and that a
//! project member sees their own project's unsettled capacity and nobody else's.

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::AUTHORIZATION},
    routing::get,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use super::capacity_attention;
use crate::{
    AppState, ExternalLinksResponse,
    capacity::{ambiguous_provisions, outstanding_releases},
    human_auth::{ClientAuthConfig, HumanAuth, HumanIdentity, IdentityVerifier, VerifyError},
    projects::DEFAULT_PROJECT_ID,
    registry::VerificationGate,
};

const ATTENTION: &str = "/api/v1/operator/capacity/attention";

/// Accepts `token-<subject>` for any subject. Nobody is the bootstrap operator, so a subject must
/// already exist to be authorised.
struct SubjectTokens;

#[async_trait]
impl IdentityVerifier for SubjectTokens {
    async fn verify(&self, id_token: &str) -> Result<HumanIdentity, VerifyError> {
        let Some(subject) = id_token.strip_prefix("token-") else {
            return Err(VerifyError::Rejected);
        };
        Ok(HumanIdentity {
            subject: subject.to_owned(),
            email: format!("{subject}@example.com"),
            display_name: subject.to_owned(),
        })
    }
}

fn router(pool: &PgPool) -> Router {
    let auth = HumanAuth::new(
        Arc::new(SubjectTokens),
        "bootstrap-nobody@example.invalid",
        ClientAuthConfig {
            api_key: "test-api-key".to_owned(),
            auth_domain: "example.test".to_owned(),
            project_id: "test-project".to_owned(),
        },
    );
    Router::new()
        .route(ATTENTION, get(capacity_attention))
        .with_state(AppState {
            database: Some(pool.clone()),
            human_auth: Some(auth),
            artifact_storage: None,
            verification_gate: VerificationGate::default(),
            external_links: ExternalLinksResponse {
                grafana_url: None,
                mlflow_url: None,
            },
        })
}

async fn read(router: &Router, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::get(ATTENTION);
    if let Some(bearer) = bearer {
        request = request.header(AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
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
async fn member(pool: &PgPool, subject: &str, project_id: Uuid) {
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

/// A running job with one attempt in `project_id`, returning `(job_id, attempt_id)`.
async fn seed_attempt(pool: &PgPool, project_id: Uuid) -> (Uuid, Uuid) {
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
    .bind(project_id)
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
    .bind(project_id)
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
    (job_id, attempt_id)
}

/// A request in `status` for a fresh attempt in `project_id`. Written directly: the paths that
/// reach these states are proved in `capacity::tests`, and this module is only about reading them.
async fn seed_request(
    pool: &PgPool,
    project_id: Uuid,
    status: &str,
    external_id: Option<&str>,
) -> Uuid {
    let (job_id, attempt_id) = seed_attempt(pool, project_id).await;
    let request_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO capacity_requests \
         (id, attempt_id, job_id, project_id, provider, idempotency_key, status, external_id, \
          last_error) \
         VALUES ($1, $2, $3, $4, 'fake', $5, $6, $7, 'provider unreachable')",
    )
    .bind(request_id)
    .bind(attempt_id)
    .bind(job_id)
    .bind(project_id)
    .bind(crate::capacity::idempotency_key(attempt_id))
    .bind(status)
    .bind(external_id)
    .execute(pool)
    .await
    .unwrap();
    request_id
}

/// An unreconciled provision in `project_id`.
async fn seed_ambiguous(pool: &PgPool, project_id: Uuid) -> Uuid {
    seed_request(pool, project_id, "unreconciled", None).await
}

/// A release that has used `attempts` of its dispatch attempts, in `project_id`.
async fn seed_release(pool: &PgPool, project_id: Uuid, attempts: i32) -> Uuid {
    let request_id = seed_request(pool, project_id, "releasing", Some("fake-held")).await;
    sqlx::query(
        "INSERT INTO capacity_dispatches (id, request_id, action, attempts, last_error) \
         VALUES ($1, $2, 'release', $3, 'provider unreachable')",
    )
    .bind(Uuid::new_v4())
    .bind(request_id)
    .bind(attempts)
    .execute(pool)
    .await
    .unwrap();
    request_id
}

fn request_ids(body: &Value, list: &str) -> Vec<String> {
    body[list]
        .as_array()
        .unwrap_or_else(|| panic!("{list} must be an array: {body}"))
        .iter()
        .map(|row| row["request_id"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrations = "./migrations")]
async fn capacity_view_requires_an_operator(pool: PgPool) {
    let router = router(&pool);
    let (status, _) = read(&router, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A verified identity that is not already an operator, and is not the bootstrap operator.
    let (status, _) = read(&router, Some("token-stranger")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn capacity_view_shows_only_the_callers_projects(pool: PgPool) {
    // Unsettled capacity in two projects. Each member must see their own and never the other's:
    // membership is broad access within a project, not sight of anyone else's machines.
    let other = second_project(&pool).await;
    member(&pool, "home", DEFAULT_PROJECT_ID).await;
    member(&pool, "away", other).await;
    let home_ambiguous = seed_ambiguous(&pool, DEFAULT_PROJECT_ID).await;
    let home_release = seed_release(&pool, DEFAULT_PROJECT_ID, 8).await;
    let away_ambiguous = seed_ambiguous(&pool, other).await;
    let away_release = seed_release(&pool, other, 8).await;
    let router = router(&pool);

    let (status, home) = read(&router, Some("token-home")).await;
    assert_eq!(status, StatusCode::OK, "{home}");
    assert_eq!(
        request_ids(&home, "ambiguous_provisions"),
        [home_ambiguous.to_string()],
        "a member must see their own project's unknown outcomes and no other project's"
    );
    assert_eq!(
        request_ids(&home, "outstanding_releases"),
        [home_release.to_string()],
        "a member must see their own project's abandoned releases and no other project's"
    );
    assert!(home["provider_enabled"].is_boolean());
    let row = &home["outstanding_releases"][0];
    assert_eq!(row["external_id"], "fake-held");
    assert_eq!(row["attempts"], 8);
    let key = home["ambiguous_provisions"][0]["idempotency_key"].as_str();
    assert!(key.unwrap().starts_with("kratos-attempt-"));

    let (status, away) = read(&router, Some("token-away")).await;
    assert_eq!(status, StatusCode::OK, "{away}");
    assert_eq!(
        request_ids(&away, "ambiguous_provisions"),
        [away_ambiguous.to_string()]
    );
    assert_eq!(
        request_ids(&away, "outstanding_releases"),
        [away_release.to_string()]
    );

    // The unscoped functions share the queries and still see every project.
    assert_eq!(ambiguous_provisions(&pool).await.unwrap().len(), 2);
    assert_eq!(outstanding_releases(&pool).await.unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_release_still_being_retried_is_not_yet_a_persons_problem(pool: PgPool) {
    // The view shares its definition with `outstanding_releases`: a release with attempts left is
    // still Kratos's to finish, so listing it would cry wolf.
    member(&pool, "home", DEFAULT_PROJECT_ID).await;
    seed_release(&pool, DEFAULT_PROJECT_ID, 7).await;
    let (status, body) = read(&router(&pool), Some("token-home")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(request_ids(&body, "outstanding_releases").is_empty());
    assert!(request_ids(&body, "ambiguous_provisions").is_empty());
}

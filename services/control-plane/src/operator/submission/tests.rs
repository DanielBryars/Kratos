//! Job submission idempotency (TRN-011), against a real `PostgreSQL` database.
//!
//! `tokio::join!` gives no overlap against a local database, so the concurrent case holds an
//! uncommitted job with the same key in a separate transaction and waits until both submissions are
//! observed blocked on it inside `INSERT INTO jobs` before rolling it back. They are then racing
//! for the same unique-index entry, not merely running near each other.
//!
//! Every other case goes through the HTTP handler, so authentication, project membership and the
//! header parsing are exercised exactly as a client meets them.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header::AUTHORIZATION},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use super::{IDEMPOTENCY_KEY_HEADER, fingerprint, idempotency_key};
use crate::{
    app_with_human_auth,
    human_auth::{ClientAuthConfig, HumanAuth, HumanIdentity, IdentityVerifier, VerifyError},
    operator::{Caller, CreateJobRequest, queue_job},
    projects::DEFAULT_PROJECT_ID,
};

const TOKEN: &str = "valid-token";

struct FakeVerifier {
    identity: HumanIdentity,
}

#[async_trait]
impl IdentityVerifier for FakeVerifier {
    async fn verify(&self, id_token: &str) -> Result<HumanIdentity, VerifyError> {
        if id_token == TOKEN {
            Ok(self.identity.clone())
        } else {
            Err(VerifyError::Rejected)
        }
    }
}

/// A router whose only valid token authenticates as `subject`.
fn router(pool: &PgPool, subject: &str) -> Router {
    let auth = HumanAuth::new(
        Arc::new(FakeVerifier {
            identity: HumanIdentity {
                subject: subject.to_owned(),
                email: format!("{subject}@example.com"),
                display_name: subject.to_owned(),
            },
        }),
        "bootstrap@example.com",
        ClientAuthConfig {
            api_key: "test-api-key".to_owned(),
            auth_domain: "example.test".to_owned(),
            project_id: "test-project".to_owned(),
        },
    );
    app_with_human_auth(None, Some(pool.clone()), Some(auth))
}

/// An operator identity with the membership bootstrap or an invitation claim would have given it.
async fn insert_operator(pool: &PgPool, subject: &str, project_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities \
         (id, provider, provider_subject, display_name, email, role) \
         VALUES ($1, 'identity-platform', $2, $2, $3, 'operator')",
    )
    .bind(id)
    .bind(subject)
    .bind(format!("{subject}@example.com"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO project_memberships (project_id, identity_id) VALUES ($1, $2)")
        .bind(project_id)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn insert_project(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects (id, name) VALUES ($1, 'Another project')")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn revoke_membership(pool: &PgPool, identity_id: Uuid, project_id: Uuid) {
    sqlx::query(
        "UPDATE project_memberships SET revoked_at = now(), revoked_by_identity_id = $1 \
         WHERE identity_id = $1 AND project_id = $2",
    )
    .bind(identity_id)
    .bind(project_id)
    .execute(pool)
    .await
    .unwrap();
}

/// A ready upload dataset version in the default project, with the id the caller chose.
async fn insert_ready_dataset_version(pool: &PgPool, owner_id: Uuid, version_id: Uuid) {
    let dataset_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO datasets (id, project_id, name, created_by_identity_id) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(dataset_id)
    .bind(DEFAULT_PROJECT_ID)
    .bind(format!("Dataset {dataset_id}"))
    .bind(owner_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO dataset_versions \
         (id, dataset_id, project_id, version_number, source_kind, status, \
          manifest_sha256, info_json, total_episodes, total_frames, fps, \
          created_by_identity_id, ready_at) \
         VALUES ($1, $2, $3, 1, 'upload', 'ready', $4, '{}'::jsonb, 3, 30, 10, $5, now())",
    )
    .bind(version_id)
    .bind(dataset_id)
    .bind(DEFAULT_PROJECT_ID)
    .bind("b".repeat(64))
    .bind(owner_id)
    .execute(pool)
    .await
    .unwrap();
}

fn image(digit: char) -> String {
    format!("example.test/work@sha256:{}", digit.to_string().repeat(64))
}

fn outputs() -> Value {
    json!([
        {
            "logical_path": "outputs/model.bin",
            "role": "model",
            "media_type": "application/octet-stream",
            "mandatory": true,
            "max_bytes": 2048
        },
        {
            "logical_path": "outputs/metrics.json",
            "role": "metrics",
            "media_type": "application/json",
            "mandatory": false,
            "max_bytes": 1024
        }
    ])
}

/// A submission without dataset inputs, for the cases that are not about lineage.
fn simple_body() -> Value {
    json!({
        "name": "Idempotent training",
        "image_reference": image('b'),
        "timeout_seconds": 600,
        "output_requirements": outputs()
    })
}

/// A submission exercising every field the fingerprint covers.
fn full_body(dataset_version_id: Uuid) -> Value {
    json!({
        "name": "Idempotent training",
        "image_reference": image('b'),
        "timeout_seconds": 600,
        "earliest_start_at": "2099-10-01T09:30:00Z",
        "output_requirements": outputs(),
        "dataset_inputs": [{ "alias": "training_data", "dataset_version_id": dataset_version_id }]
    })
}

/// The same request as `full_body`, spelled differently in every way that means nothing.
fn equivalent_full_body(dataset_version_id: Uuid) -> Value {
    let mut body = full_body(dataset_version_id);
    body["name"] = json!("  Idempotent training \n");
    body["earliest_start_at"] = json!("2099-10-01T10:30:00.000000+01:00");
    let outputs = outputs();
    body["output_requirements"] = json!([outputs[1].clone(), outputs[0].clone()]);
    body
}

/// One valid change to each field of `full_body`, every one of which would queue a different job.
fn meaningful_changes(dataset_version_id: Uuid) -> Vec<(&'static str, Value)> {
    let outputs = outputs();
    let mut larger_model = outputs[0].clone();
    larger_model["max_bytes"] = json!(4096);
    let mut optional_model = outputs[0].clone();
    optional_model["mandatory"] = json!(false);
    vec![
        ("name", json!("Different training")),
        ("image_reference", json!(image('c'))),
        ("timeout_seconds", json!(900)),
        ("earliest_start_at", json!("2099-10-01T09:30:01Z")),
        ("earliest_start_at", Value::Null),
        ("output_requirements", json!([outputs[0].clone()])),
        (
            "output_requirements",
            json!([larger_model, outputs[1].clone()]),
        ),
        (
            "output_requirements",
            json!([optional_model, outputs[1].clone()]),
        ),
        ("dataset_inputs", json!([])),
        (
            "dataset_inputs",
            json!([{ "alias": "other_data", "dataset_version_id": dataset_version_id }]),
        ),
    ]
}

/// Submit a job, sending one `Idempotency-Key` header per entry in `keys`.
async fn submit(router: &Router, token: &str, keys: &[&str], body: &Value) -> (StatusCode, Value) {
    let mut request = Request::post("/api/v1/operator/jobs")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json");
    for key in keys {
        request = request.header(IDEMPOTENCY_KEY_HEADER, *key);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn count(pool: &PgPool, query: &str) -> i64 {
    sqlx::query_scalar(query).fetch_one(pool).await.unwrap()
}

async fn queued_audit_events(pool: &PgPool) -> i64 {
    count(
        pool,
        "SELECT count(*) FROM audit_events WHERE action = 'job.queued'",
    )
    .await
}

/// Wait until `expected` sessions are blocked on a lock while inserting a job.
async fn wait_for_blocked_job_inserts(pool: &PgPool, expected: i64) {
    for _ in 0..400 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
               AND query LIKE 'INSERT INTO jobs%'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("expected {expected} submissions to be waiting on the same idempotency key");
}

fn request(body: Value) -> CreateJobRequest {
    serde_json::from_value(body).unwrap()
}

#[test]
fn idempotency_key_header_is_optional_but_strict() {
    let key = Uuid::new_v4();
    let mut headers = HeaderMap::new();
    assert_eq!(idempotency_key(&headers).unwrap(), None);

    for spelling in [key.to_string(), key.to_string().to_uppercase()] {
        let value = HeaderValue::from_str(&spelling).unwrap();
        headers.insert(IDEMPOTENCY_KEY_HEADER, value);
        assert_eq!(idempotency_key(&headers).unwrap(), Some(key));
    }

    let simple = key.simple().to_string();
    let braced = key.braced().to_string();
    let urn = key.urn().to_string();
    let truncated = key.to_string()[..35].to_owned();
    let not_hex = format!("{}g", &key.to_string()[..35]);
    for invalid in [
        "",
        "not-a-uuid",
        simple.as_str(),
        braced.as_str(),
        urn.as_str(),
        truncated.as_str(),
        not_hex.as_str(),
    ] {
        let value = HeaderValue::from_str(invalid).unwrap();
        headers.insert(IDEMPOTENCY_KEY_HEADER, value);
        assert!(idempotency_key(&headers).is_err(), "{invalid:?}");
    }

    let valid = HeaderValue::from_str(&key.to_string()).unwrap();
    headers.insert(IDEMPOTENCY_KEY_HEADER, valid.clone());
    headers.append(IDEMPOTENCY_KEY_HEADER, valid);
    assert!(idempotency_key(&headers).is_err());
}

#[test]
fn fingerprint_ignores_spelling_but_not_meaning() {
    let version = Uuid::new_v4();
    let original = fingerprint(&request(full_body(version))).unwrap();
    assert_eq!(original.len(), 64);
    assert!(
        original
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(
        fingerprint(&request(equivalent_full_body(version))).unwrap(),
        original
    );

    // Omitted collections and an omitted start time are their empty and null forms.
    let minimal = json!({
        "name": "Minimal",
        "image_reference": image('a'),
        "timeout_seconds": 120
    });
    let explicit = json!({
        "name": "Minimal",
        "image_reference": image('a'),
        "timeout_seconds": 120,
        "earliest_start_at": null,
        "output_requirements": [],
        "dataset_inputs": []
    });
    assert_eq!(
        fingerprint(&request(minimal)).unwrap(),
        fingerprint(&request(explicit)).unwrap()
    );

    // Dataset inputs are a set keyed by alias, so their order is not part of the request.
    let second_version = Uuid::new_v4();
    let mut two_inputs = full_body(version);
    two_inputs["dataset_inputs"] = json!([
        { "alias": "training_data", "dataset_version_id": version },
        { "alias": "validation_data", "dataset_version_id": second_version }
    ]);
    let mut reordered = two_inputs.clone();
    reordered["dataset_inputs"] = json!([
        { "alias": "validation_data", "dataset_version_id": second_version },
        { "alias": "training_data", "dataset_version_id": version }
    ]);
    assert_eq!(
        fingerprint(&request(two_inputs)).unwrap(),
        fingerprint(&request(reordered)).unwrap()
    );

    for (field, value) in meaningful_changes(version) {
        let mut changed = full_body(version);
        changed[field] = value;
        let changed = fingerprint(&request(changed)).unwrap();
        assert_ne!(changed, original, "{field}");
    }
    let mut viewed = full_body(version);
    viewed["dataset_inputs"][0]["dataset_view_id"] = json!(Uuid::new_v4());
    assert_ne!(fingerprint(&request(viewed)).unwrap(), original);
}

#[test]
fn fingerprint_preserves_submicrosecond_timestamp_changes() {
    for year in [1999, 2099] {
        let mut first = simple_body();
        first["earliest_start_at"] = json!(format!("{year}-10-01T09:30:00.000000000Z"));
        let mut changed = first.clone();
        changed["earliest_start_at"] = json!(format!("{year}-10-01T09:30:00.000000100Z"));
        assert_ne!(
            fingerprint(&request(first)).unwrap(),
            fingerprint(&request(changed)).unwrap()
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn identical_replay_returns_the_original_job_and_queues_nothing(pool: PgPool) {
    let owner = insert_operator(&pool, "submitter", DEFAULT_PROJECT_ID).await;
    let version = Uuid::new_v4();
    insert_ready_dataset_version(&pool, owner, version).await;
    let router = router(&pool, "submitter");
    let key = Uuid::new_v4().to_string();

    let (status, created) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, replayed) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed["job_id"], created["job_id"]);
    assert_eq!(replayed["submitted_at"], created["submitted_at"]);
    assert_eq!(replayed["status"], "queued");
    assert_eq!(replayed["earliest_start_at"], "2099-10-01T09:30:00Z");
    // Read back from storage, so ordered by logical path whatever order they were sent in.
    assert_eq!(
        replayed["output_requirements"][0]["logical_path"],
        "outputs/metrics.json"
    );
    assert_eq!(
        replayed["output_requirements"][1]["logical_path"],
        "outputs/model.bin"
    );
    assert_eq!(replayed["dataset_inputs"][0]["alias"], "training_data");
    assert_eq!(
        replayed["dataset_inputs"][0]["dataset_version_id"],
        version.to_string()
    );
    assert_eq!(
        replayed["dataset_inputs"][0]["dataset_manifest_sha256"],
        "b".repeat(64)
    );

    // A retry is not obliged to reproduce the original bytes, only the original request.
    let uppercase = key.to_uppercase();
    let body = equivalent_full_body(version);
    let (status, equivalent) = submit(&router, TOKEN, &[uppercase.as_str()], &body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(equivalent["job_id"], created["job_id"]);

    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 1);
    assert_eq!(queued_audit_events(&pool).await, 1);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM job_output_requirements").await,
        2
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM job_dataset_inputs").await,
        1
    );
    let stored: Option<Uuid> = sqlx::query_scalar("SELECT submission_idempotency_key FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, Some(Uuid::parse_str(&key).unwrap()));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_used_key_rejects_any_different_request(pool: PgPool) {
    let owner = insert_operator(&pool, "submitter", DEFAULT_PROJECT_ID).await;
    let version = Uuid::new_v4();
    insert_ready_dataset_version(&pool, owner, version).await;
    let router = router(&pool, "submitter");
    let key = Uuid::new_v4().to_string();
    let (status, created) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::CREATED);
    let job_id = created["job_id"].as_str().unwrap().to_owned();

    for (field, value) in meaningful_changes(version) {
        let mut changed = full_body(version);
        changed[field] = value;
        // Each change is itself a valid request: with a fresh key it would be queued, so the
        // conflict below is about the key and nothing else.
        let (status, error) = submit(&router, TOKEN, &[key.as_str()], &changed).await;
        assert_eq!(status, StatusCode::CONFLICT, "{field}");
        assert_eq!(error["code"], "idempotency_key_reused", "{field}");
        assert!(!error.to_string().contains(&job_id), "{field}");
    }

    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 1);
    assert_eq!(queued_audit_events(&pool).await, 1);
    let (status, replayed) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed["job_id"].as_str(), Some(job_id.as_str()));

    // The changed requests were rejected for their key alone.
    let fresh = Uuid::new_v4().to_string();
    let mut changed = full_body(version);
    changed["name"] = json!("Different training");
    let (status, _) = submit(&router, TOKEN, &[fresh.as_str()], &changed).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrations = "./migrations")]
async fn simultaneous_submissions_with_one_key_create_exactly_one_job(pool: PgPool) {
    let owner = insert_operator(&pool, "submitter", DEFAULT_PROJECT_ID).await;
    let key = Uuid::new_v4();

    // An uncommitted job holding the key: both submissions must reach the unique index and wait on
    // it, so when it rolls back they are contending for the same entry at the same moment.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO jobs \
         (id, owner_identity_id, project_id, name, image_reference, timeout_seconds, \
          submission_idempotency_key, submission_fingerprint) \
         VALUES ($1, $2, $3, 'Placeholder', $4, 120, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(DEFAULT_PROJECT_ID)
    .bind(image('a'))
    .bind(key)
    .bind("0".repeat(64))
    .execute(&mut *holder)
    .await
    .unwrap();

    // Below the handler, as an authorised caller: see `queue_job` for why HTTP cannot hold two
    // submissions at the insert together.
    let caller = Caller {
        identity_id: owner,
        project_ids: vec![DEFAULT_PROJECT_ID],
    };
    let submissions: Vec<_> = (0..2)
        .map(|_| {
            let pool = pool.clone();
            let caller = caller.clone();
            tokio::spawn(async move {
                queue_job(
                    &pool,
                    &caller,
                    DEFAULT_PROJECT_ID,
                    Some(key),
                    request(simple_body()),
                )
                .await
                .unwrap()
            })
        })
        .collect();
    wait_for_blocked_job_inserts(&pool, 2).await;
    holder.rollback().await.unwrap();

    let mut statuses = Vec::new();
    let mut job_ids = Vec::new();
    for submission in submissions {
        let (status, job) = submission.await.unwrap();
        // The loser's answer is the winner's job, outputs and all, not an empty shell.
        assert_eq!(job.output_requirements.len(), 2);
        statuses.push(status.as_u16());
        job_ids.push(job.job_id);
    }
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 201]);
    assert_eq!(job_ids[0], job_ids[1]);
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 1);
    assert_eq!(queued_audit_events(&pool).await, 1);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM job_output_requirements").await,
        2
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn keys_are_scoped_to_the_submitter_and_the_project(pool: PgPool) {
    let first = insert_operator(&pool, "first-submitter", DEFAULT_PROJECT_ID).await;
    insert_operator(&pool, "second-submitter", DEFAULT_PROJECT_ID).await;
    let first_router = router(&pool, "first-submitter");
    let second_router = router(&pool, "second-submitter");
    let key = Uuid::new_v4().to_string();

    let (status, mine) = submit(&first_router, TOKEN, &[key.as_str()], &simple_body()).await;
    assert_eq!(status, StatusCode::CREATED);
    // A colleague who happens to choose the same key queues their own job; they are never handed
    // someone else's, and cannot use a key to learn that it is taken.
    let (status, theirs) = submit(&second_router, TOKEN, &[key.as_str()], &simple_body()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(mine["job_id"], theirs["job_id"]);
    let (status, replay) = submit(&second_router, TOKEN, &[key.as_str()], &simple_body()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["job_id"], theirs["job_id"]);

    // The same person in a different project starts a separate key space.
    let other_project = insert_project(&pool).await;
    revoke_membership(&pool, first, DEFAULT_PROJECT_ID).await;
    sqlx::query("INSERT INTO project_memberships (project_id, identity_id) VALUES ($1, $2)")
        .bind(other_project)
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    let (status, moved) = submit(&first_router, TOKEN, &[key.as_str()], &simple_body()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(moved["job_id"], mine["job_id"]);
    let moved_id = Uuid::parse_str(moved["job_id"].as_str().unwrap()).unwrap();
    let project: Uuid = sqlx::query_scalar("SELECT project_id FROM jobs WHERE id = $1")
        .bind(moved_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(project, other_project);

    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 3);
    assert_eq!(queued_audit_events(&pool).await, 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_keys_are_rejected_before_anything_is_written(pool: PgPool) {
    insert_operator(&pool, "submitter", DEFAULT_PROJECT_ID).await;
    let router = router(&pool, "submitter");
    let key = Uuid::new_v4();
    let simple = key.simple().to_string();
    let braced = key.braced().to_string();
    let first = key.to_string();
    let second = Uuid::new_v4().to_string();

    for keys in [
        vec!["not-a-uuid"],
        vec![""],
        vec![simple.as_str()],
        vec![braced.as_str()],
        // Two keys, even two valid ones, are refused rather than one being picked.
        vec![first.as_str(), second.as_str()],
    ] {
        let (status, error) = submit(&router, TOKEN, &keys, &simple_body()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{keys:?}");
        assert_eq!(error["code"], "invalid_idempotency_key", "{keys:?}");
    }
    // Authentication is decided first, so a stranger learns nothing from the header.
    let (status, _) = submit(&router, "wrong-token", &["not-a-uuid"], &simple_body()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 0);
    assert_eq!(queued_audit_events(&pool).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failed_submission_does_not_consume_its_key(pool: PgPool) {
    let owner = insert_operator(&pool, "submitter", DEFAULT_PROJECT_ID).await;
    let router = router(&pool, "submitter");
    let key = Uuid::new_v4().to_string();
    // Not yet in the catalogue, so the dataset input fails *after* the job row was inserted in the
    // same transaction. The key must leave with it.
    let version = Uuid::new_v4();

    let (status, error) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["code"], "invalid_request");
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 0);
    assert_eq!(queued_audit_events(&pool).await, 0);

    insert_ready_dataset_version(&pool, owner, version).await;
    let (status, created) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, replayed) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed["job_id"], created["job_id"]);
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn replay_reports_current_state_without_revalidating_datasets(pool: PgPool) {
    let owner = insert_operator(&pool, "submitter", DEFAULT_PROJECT_ID).await;
    let version = Uuid::new_v4();
    insert_ready_dataset_version(&pool, owner, version).await;
    let router = router(&pool, "submitter");
    let key = Uuid::new_v4().to_string();
    let (status, created) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::CREATED);
    let job_id = Uuid::parse_str(created["job_id"].as_str().unwrap()).unwrap();

    // The job has since been picked up by a worker...
    let worker_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workers \
         (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, \
          capabilities, project_id) \
         VALUES ($1, $2, $3, 'GPU worker', '1.1', 'busy', '{}'::jsonb, $4)",
    )
    .bind(worker_id)
    .bind(owner)
    .bind(Uuid::new_v4())
    .bind(DEFAULT_PROJECT_ID)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE jobs SET status = 'running', assigned_worker_id = $2, started_at = now() \
         WHERE id = $1",
    )
    .bind(job_id)
    .bind(worker_id)
    .execute(&pool)
    .await
    .unwrap();
    let attempt_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job_attempts \
         (id, job_id, attempt_number, worker_id, status, started_at, lease_expires_at) \
         VALUES ($1, $2, 1, $3, 'running', now(), now() + interval '5 minutes')",
    )
    .bind(attempt_id)
    .bind(job_id)
    .bind(worker_id)
    .execute(&pool)
    .await
    .unwrap();
    // ...and its dataset version is no longer one a new job could select.
    sqlx::query("UPDATE dataset_versions SET status = 'failed', ready_at = NULL WHERE id = $1")
        .bind(version)
        .execute(&pool)
        .await
        .unwrap();

    let (status, replayed) = submit(&router, TOKEN, &[key.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed["job_id"], created["job_id"]);
    assert_eq!(replayed["status"], "running");
    assert_eq!(replayed["assigned_worker_id"], worker_id.to_string());
    assert_eq!(
        replayed["current_attempt"]["attempt_id"],
        attempt_id.to_string()
    );
    assert_eq!(replayed["current_attempt"]["attempt_number"], 1);
    assert_eq!(replayed["artifacts"], json!([]));
    assert_eq!(
        replayed["output_requirements"][1]["logical_path"],
        "outputs/model.bin"
    );
    assert_eq!(
        replayed["dataset_inputs"][0]["dataset_manifest_sha256"],
        "b".repeat(64)
    );

    // The same request under a new key is a new submission, and is refused the version: the
    // replay above succeeded because it skipped that check, not because the check passes.
    let fresh = Uuid::new_v4().to_string();
    let (status, _) = submit(&router, TOKEN, &[fresh.as_str()], &full_body(version)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 1);
    assert_eq!(queued_audit_events(&pool).await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn replay_still_requires_authentication_and_membership(pool: PgPool) {
    let owner = insert_operator(&pool, "submitter", DEFAULT_PROJECT_ID).await;
    let router = router(&pool, "submitter");
    let key = Uuid::new_v4().to_string();
    let (status, created) = submit(&router, TOKEN, &[key.as_str()], &simple_body()).await;
    assert_eq!(status, StatusCode::CREATED);
    let job_id = created["job_id"].as_str().unwrap().to_owned();

    let (status, error) = submit(&router, "wrong-token", &[key.as_str()], &simple_body()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!error.to_string().contains(&job_id));

    revoke_membership(&pool, owner, DEFAULT_PROJECT_ID).await;
    let (status, error) = submit(&router, TOKEN, &[key.as_str()], &simple_body()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!error.to_string().contains(&job_id));

    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 1);
    assert_eq!(queued_audit_events(&pool).await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn submissions_without_a_key_are_never_deduplicated(pool: PgPool) {
    insert_operator(&pool, "submitter", DEFAULT_PROJECT_ID).await;
    let router = router(&pool, "submitter");

    let (first_status, first) = submit(&router, TOKEN, &[], &simple_body()).await;
    let (second_status, second) = submit(&router, TOKEN, &[], &simple_body()).await;
    assert_eq!(
        (first_status, second_status),
        (StatusCode::CREATED, StatusCode::CREATED)
    );
    assert_ne!(first["job_id"], second["job_id"]);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM jobs \
             WHERE submission_idempotency_key IS NULL AND submission_fingerprint IS NULL",
        )
        .await,
        2
    );
    assert_eq!(queued_audit_events(&pool).await, 2);
}

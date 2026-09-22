//! The dataset catalogue against a real `PostgreSQL` database.
//!
//! The handover asks for five properties to be proved rather than argued: project isolation,
//! immutable version numbering, duplicate-name handling, upload integrity rejection, and curated
//! view snapshots. Each is a place where reading the code and running it have already disagreed
//! once on this project, so these go through the HTTP surface and then check what the database
//! actually holds.
//!
//! The Hugging Face import is not exercised here: it resolves a revision over the network before
//! it touches the database, so it cannot run in a test that must work offline. Everything it
//! shares with the upload path -- naming, scoping, version numbering -- is covered through the
//! upload path instead.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::AUTHORIZATION},
};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

use crate::{
    app_with_dependencies,
    artifact_storage::{
        ArtifactStorage, ArtifactStorageClient, ArtifactStorageError, ResumableUploadSession,
        StoredObjectMetadata,
    },
    human_auth::{ClientAuthConfig, HumanAuth, HumanIdentity, IdentityVerifier, VerifyError},
    projects::DEFAULT_PROJECT_ID,
};

/// Storage that reports whatever the test tells it to.
///
/// `object_metadata` is the authority the control plane trusts over the uploader's claim, so the
/// only way to test that integrity check is to be able to disagree with the declaration.
struct FakeStorage {
    bucket: String,
    byte_length: i64,
    sha256: String,
}

#[async_trait]
impl ArtifactStorage for FakeStorage {
    async fn initiate_resumable_upload(
        &self,
        object_key: &str,
        _media_type: &str,
        _byte_length: u64,
        _sha256: &str,
        issued_at: DateTime<Utc>,
    ) -> Result<ResumableUploadSession, ArtifactStorageError> {
        Ok(ResumableUploadSession {
            uri: format!("https://storage.example.test/upload/{object_key}"),
            method: "PUT".to_owned(),
            expires_at: issued_at + TimeDelta::days(1),
        })
    }

    async fn object_metadata(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<StoredObjectMetadata, ArtifactStorageError> {
        Ok(StoredObjectMetadata {
            bucket: bucket.to_owned(),
            object_key: object_key.to_owned(),
            generation,
            byte_length: self.byte_length,
            crc32c: "AAAAAA==".to_owned(),
            sha256: self.sha256.clone(),
        })
    }

    async fn protect_verified_object(
        &self,
        _bucket: &str,
        _object_key: &str,
        _generation: i64,
    ) -> Result<(), ArtifactStorageError> {
        Ok(())
    }

    async fn delete_object(
        &self,
        _bucket: &str,
        _object_key: &str,
        _generation: i64,
    ) -> Result<(), ArtifactStorageError> {
        Ok(())
    }

    async fn cancel_resumable_upload(
        &self,
        _session_uri: &str,
    ) -> Result<(), ArtifactStorageError> {
        Ok(())
    }

    async fn signed_read_url(
        &self,
        bucket: &str,
        object_key: &str,
        lifetime_seconds: u32,
        _issued_at: DateTime<Utc>,
    ) -> Result<String, ArtifactStorageError> {
        Ok(format!(
            "https://storage.example.test/read/{bucket}/{object_key}?expires_in={lifetime_seconds}"
        ))
    }

    fn bucket(&self) -> &str {
        &self.bucket
    }
}

/// The SHA-256 a declaration claims, which the honest fake storage agrees with.
fn declared_sha() -> String {
    "c".repeat(64)
}

const DECLARED_BYTES: i64 = 512;

fn storage_agreeing() -> ArtifactStorageClient {
    ArtifactStorageClient::new(FakeStorage {
        bucket: "kratos-datasets-test".to_owned(),
        byte_length: DECLARED_BYTES,
        sha256: declared_sha(),
    })
}

/// Storage that reports different bytes from the ones declared, as a truncated upload would.
fn storage_disagreeing() -> ArtifactStorageClient {
    ArtifactStorageClient::new(FakeStorage {
        bucket: "kratos-datasets-test".to_owned(),
        byte_length: DECLARED_BYTES - 1,
        sha256: declared_sha(),
    })
}

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

fn router_for(
    pool: &PgPool,
    subject: &str,
    email: &str,
    token: &'static str,
    bootstrap: bool,
    storage: ArtifactStorageClient,
) -> axum::Router {
    let identity = HumanIdentity {
        subject: subject.to_owned(),
        email: email.to_owned(),
        display_name: subject.to_owned(),
    };
    let bootstrap_email = if bootstrap {
        email
    } else {
        "nobody@example.com"
    };
    let auth = HumanAuth::new(
        Arc::new(SingleToken { token, identity }),
        bootstrap_email,
        ClientAuthConfig {
            api_key: "test-api-key".to_owned(),
            auth_domain: "example.test".to_owned(),
            project_id: "test-project".to_owned(),
        },
    );
    app_with_dependencies(None, Some(pool.clone()), Some(auth), Some(storage))
}

async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get(router: &axum::Router, path: &str, token: &str) -> (StatusCode, Value) {
    send(
        router,
        Request::get(path)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn post(router: &axum::Router, path: &str, token: &str, body: &Value) -> (StatusCode, Value) {
    send(
        router,
        Request::post(path)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
    )
    .await
}

async fn put(router: &axum::Router, path: &str, token: &str, body: &Value) -> (StatusCode, Value) {
    send(
        router,
        Request::put(path)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
    )
    .await
}

/// Sign in as the founding operator, which creates the identity and its default membership.
async fn found(pool: &PgPool, storage: ArtifactStorageClient) -> (axum::Router, Uuid) {
    let router = router_for(
        pool,
        "founder",
        "founder@example.com",
        "founder-token",
        true,
        storage,
    );
    let (status, _) = get(&router, "/api/v1/operator/datasets", "founder-token").await;
    assert_eq!(status, StatusCode::OK, "founding sign-in should succeed");
    let (id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM human_identities WHERE provider_subject = 'founder'")
            .fetch_one(pool)
            .await
            .unwrap();
    (router, id)
}

/// The smallest `LeRobot` metadata `validate_info` accepts.
fn lerobot_info(total_episodes: i64) -> Value {
    json!({
        "codebase_version": "v2.1",
        "robot_type": "so101",
        "total_episodes": total_episodes,
        "total_frames": total_episodes * 100,
        "fps": 30,
        "features": {
            "observation.state": { "dtype": "float32", "shape": [6] },
            "action": { "dtype": "float32", "shape": [6] },
        },
    })
}

fn upload_request(name: &str, total_episodes: i64) -> Value {
    json!({
        "name": name,
        "description": "An uploaded LeRobot dataset",
        "info": lerobot_info(total_episodes),
        "files": [{
            "logical_path": "meta/info.json",
            "media_type": "application/json",
            "byte_length": DECLARED_BYTES,
            "sha256": declared_sha(),
        }],
    })
}

/// Declare an upload dataset and return its dataset id, version id and first file id.
async fn declare_upload(
    router: &axum::Router,
    token: &str,
    name: &str,
    total_episodes: i64,
) -> (Uuid, Uuid, Uuid) {
    let (status, body) = post(
        router,
        "/api/v1/operator/datasets/upload",
        token,
        &upload_request(name, total_episodes),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "declaring an upload should succeed, got {body}"
    );
    let dataset_id = body["id"].as_str().unwrap().parse().unwrap();
    let version = &body["versions"][0];
    let version_id = version["id"].as_str().unwrap().parse().unwrap();
    let file_id = version["files"][0]["id"].as_str().unwrap().parse().unwrap();
    (dataset_id, version_id, file_id)
}

/// Put a second project and a second operator in it, so isolation has two sides.
async fn second_project(pool: &PgPool, storage: ArtifactStorageClient) -> axum::Router {
    let project_id = Uuid::new_v4();
    let identity_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects (id, name) VALUES ($1, 'Second project')")
        .bind(project_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO human_identities \
         (id, provider, provider_subject, display_name, email, role) \
         VALUES ($1, 'identity-platform', 'outsider', 'Outsider', 'outsider@example.com', 'operator')",
    )
    .bind(identity_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO project_memberships (project_id, identity_id) VALUES ($1, $2)")
        .bind(project_id)
        .bind(identity_id)
        .execute(pool)
        .await
        .unwrap();
    router_for(
        pool,
        "outsider",
        "outsider@example.com",
        "outsider-token",
        false,
        storage,
    )
}

// ---------------------------------------------------------------------------------------------
// Project isolation
// ---------------------------------------------------------------------------------------------

/// A dataset belongs to one project, and nobody outside it can see or touch any part of it.
///
/// Checked at every level the API exposes -- the catalogue, the version, the declared file and
/// the curation endpoint -- because each one takes a different identifier and could have been
/// scoped independently, or not at all.
#[sqlx::test(migrations = "./migrations")]
async fn a_dataset_is_invisible_outside_its_project(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_agreeing()).await;
    let (dataset_id, version_id, file_id) =
        declare_upload(&founder, "founder-token", "Pick and place", 4).await;

    let outsider = second_project(&pool, storage_agreeing()).await;

    let (status, body) = get(&outsider, "/api/v1/operator/datasets", "outsider-token").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]), "another project's catalogue is empty");

    // Each identifier, tried directly. A real id must answer exactly as an invented one does.
    let invented = Uuid::new_v4();
    for (path, invented_path) in [
        (
            format!("/api/v1/operator/dataset-files/{file_id}/upload"),
            format!("/api/v1/operator/dataset-files/{invented}/upload"),
        ),
        (
            format!("/api/v1/operator/dataset-files/{file_id}/complete"),
            format!("/api/v1/operator/dataset-files/{invented}/complete"),
        ),
    ] {
        let real = put(
            &outsider,
            &path,
            "outsider-token",
            &json!({ "generation": "1" }),
        )
        .await;
        let fake = put(
            &outsider,
            &invented_path,
            "outsider-token",
            &json!({ "generation": "1" }),
        )
        .await;
        assert_eq!(
            real.0,
            StatusCode::NOT_FOUND,
            "another project's file must not be reachable at {path}"
        );
        assert_eq!(
            real.0, fake.0,
            "a real id and an invented one must be indistinguishable at {path}"
        );
    }

    let curation = put(
        &outsider,
        &format!("/api/v1/operator/dataset-versions/{version_id}/episodes/0/curation"),
        "outsider-token",
        &json!({ "decision": "included" }),
    )
    .await;
    assert_eq!(
        curation.0,
        StatusCode::NOT_FOUND,
        "another project's version must not be curatable"
    );

    let view = post(
        &outsider,
        &format!("/api/v1/operator/dataset-versions/{version_id}/views"),
        "outsider-token",
        &json!({ "name": "stolen" }),
    )
    .await;
    assert_eq!(
        view.0,
        StatusCode::NOT_FOUND,
        "another project's version must not be publishable"
    );

    // Nothing above may have written anything.
    let leaked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dataset_episode_curations WHERE version_id = $1")
            .bind(version_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        leaked, 0,
        "a refused request must not leave a curation behind"
    );

    let still_ours: Uuid = sqlx::query_scalar("SELECT project_id FROM datasets WHERE id = $1")
        .bind(dataset_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still_ours, DEFAULT_PROJECT_ID);
}

// ---------------------------------------------------------------------------------------------
// Duplicate names
// ---------------------------------------------------------------------------------------------

/// One name per project, and the same name free in another.
#[sqlx::test(migrations = "./migrations")]
async fn a_dataset_name_is_unique_within_a_project_and_free_across_them(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_agreeing()).await;
    declare_upload(&founder, "founder-token", "Pick and place", 4).await;

    let (status, _) = post(
        &founder,
        "/api/v1/operator/datasets/upload",
        "founder-token",
        &upload_request("Pick and place", 4),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a repeated name in one project is a conflict"
    );

    // Trimming means "  Pick and place  " is the same name, not a second one.
    let (status, _) = post(
        &founder,
        "/api/v1/operator/datasets/upload",
        "founder-token",
        &upload_request("  Pick and place  ", 4),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a name differing only by surrounding space is the same name"
    );

    let outsider = second_project(&pool, storage_agreeing()).await;
    let (status, _) = post(
        &outsider,
        "/api/v1/operator/datasets/upload",
        "outsider-token",
        &upload_request("Pick and place", 4),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the same name in another project is a different dataset"
    );

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM datasets WHERE name = 'Pick and place'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 2, "one per project, and no more");
}

// ---------------------------------------------------------------------------------------------
// Upload integrity
// ---------------------------------------------------------------------------------------------

/// Storage is the authority, not the uploader's declaration.
///
/// A declaration is a claim made before the bytes exist. If the object that arrives does not
/// match it, the file is rejected and the version fails -- a half-verified version that still
/// looked ready would be the worst outcome, because a training run would then select it.
#[sqlx::test(migrations = "./migrations")]
async fn an_upload_that_does_not_match_its_declaration_is_rejected(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_disagreeing()).await;
    let (_dataset_id, version_id, file_id) =
        declare_upload(&founder, "founder-token", "Truncated", 2).await;

    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/upload"),
        "founder-token",
        &json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "beginning the upload should succeed"
    );

    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/complete"),
        "founder-token",
        &json!({ "generation": "1" }),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "an object that does not match its declaration must not verify"
    );

    let (file_status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status, rejection_reason FROM dataset_files WHERE id = $1")
            .bind(file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(file_status, "rejected");
    assert!(reason.is_some(), "a rejection should record why");

    let version_status: String =
        sqlx::query_scalar("SELECT status FROM dataset_versions WHERE id = $1")
            .bind(version_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        version_status, "failed",
        "a version with a rejected file must never be selectable for training"
    );
}

/// The happy path, and the credential's disappearance.
///
/// ADR-018 requires the resumable-session credential to be deleted once the object is verified:
/// it is a write capability against the dataset bucket, and it has no reason to outlive the
/// upload it was minted for.
#[sqlx::test(migrations = "./migrations")]
async fn a_verified_upload_publishes_the_version_and_discards_the_session(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_agreeing()).await;
    let (_dataset_id, version_id, file_id) =
        declare_upload(&founder, "founder-token", "Complete upload", 2).await;

    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/upload"),
        "founder-token",
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dataset_file_uploads WHERE file_id = $1")
            .bind(file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sessions, 1, "beginning an upload records its session");

    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/complete"),
        "founder-token",
        &json!({ "generation": "7" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a matching object should verify");

    let (file_status, generation): (String, Option<i64>) =
        sqlx::query_as("SELECT status, storage_generation FROM dataset_files WHERE id = $1")
            .bind(file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(file_status, "verified");
    assert_eq!(
        generation,
        Some(7),
        "the verified generation pins which object was checked"
    );

    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dataset_file_uploads WHERE file_id = $1")
            .bind(file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        sessions, 0,
        "the resumable-session credential must not outlive the upload"
    );

    let version_status: String =
        sqlx::query_scalar("SELECT status FROM dataset_versions WHERE id = $1")
            .bind(version_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        version_status, "ready",
        "a version whose every file verified should be ready"
    );
}

// ---------------------------------------------------------------------------------------------
// Curated views
// ---------------------------------------------------------------------------------------------

/// A published view is a snapshot, and later curation does not reach back into it.
///
/// ADR-018: "Subsequent curation creates a new view." A view that drifted with the curation it
/// was taken from would make a training run unreproducible without anything appearing to change.
#[sqlx::test(migrations = "./migrations")]
async fn a_published_view_does_not_move_when_curation_changes(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_agreeing()).await;
    let (_dataset_id, version_id, file_id) =
        declare_upload(&founder, "founder-token", "Curated", 3).await;

    // A view can only be taken from a ready version, so complete the upload first.
    put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/upload"),
        "founder-token",
        &json!({}),
    )
    .await;
    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/complete"),
        "founder-token",
        &json!({ "generation": "1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Episodes 0 and 2 in, episode 1 out.
    for (episode, decision) in [(0, "included"), (1, "excluded"), (2, "included")] {
        let (status, _) = put(
            &founder,
            &format!("/api/v1/operator/dataset-versions/{version_id}/episodes/{episode}/curation"),
            "founder-token",
            &json!({ "decision": decision }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "curating episode {episode}");
    }

    let (status, view) = post(
        &founder,
        &format!("/api/v1/operator/dataset-versions/{version_id}/views"),
        "founder-token",
        &json!({ "name": "First cut" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "publishing a view");
    let view_id: Uuid = view["id"].as_str().unwrap().parse().unwrap();

    let snapshot: Vec<(i32,)> = sqlx::query_as(
        "SELECT episode_index FROM dataset_view_episodes WHERE view_id = $1 ORDER BY episode_index",
    )
    .bind(view_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let included: Vec<i32> = snapshot.into_iter().map(|row| row.0).collect();
    assert_eq!(included, vec![0, 2], "the view holds what was included");

    // Change the curation afterwards, in both directions.
    for (episode, decision) in [(0, "excluded"), (1, "included")] {
        let (status, _) = put(
            &founder,
            &format!("/api/v1/operator/dataset-versions/{version_id}/episodes/{episode}/curation"),
            "founder-token",
            &json!({ "decision": decision }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let after: Vec<(i32,)> = sqlx::query_as(
        "SELECT episode_index FROM dataset_view_episodes WHERE view_id = $1 ORDER BY episode_index",
    )
    .bind(view_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let after: Vec<i32> = after.into_iter().map(|row| row.0).collect();
    assert_eq!(
        after,
        vec![0, 2],
        "a published view must not move when curation changes underneath it"
    );

    // And a second view taken now reflects the new decisions, which is how curation is kept.
    let (status, second) = post(
        &founder,
        &format!("/api/v1/operator/dataset-versions/{version_id}/views"),
        "founder-token",
        &json!({ "name": "Second cut" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let second_id: Uuid = second["id"].as_str().unwrap().parse().unwrap();
    let second_episodes: Vec<(i32,)> = sqlx::query_as(
        "SELECT episode_index FROM dataset_view_episodes WHERE view_id = $1 ORDER BY episode_index",
    )
    .bind(second_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let second_episodes: Vec<i32> = second_episodes.into_iter().map(|row| row.0).collect();
    assert_eq!(
        second_episodes,
        vec![1, 2],
        "a new view reflects the curation as it stands now"
    );
}

// ---------------------------------------------------------------------------------------------
// Version numbering
// ---------------------------------------------------------------------------------------------

/// ADR-018: "A `ready` version is immutable. Replacing any file, changing a Hugging Face
/// revision, or changing generated metadata creates another version."
///
/// `POST /datasets/{dataset_id}/versions` is what makes that true. It is separate from the
/// upload endpoint on purpose: that one must keep refusing a duplicate name, so it cannot also
/// treat a repeated name as a request for the next version.
#[sqlx::test(migrations = "./migrations")]
async fn adding_contents_to_a_dataset_creates_a_further_version(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_agreeing()).await;
    let (dataset_id, first_version_id, file_id) =
        declare_upload(&founder, "founder-token", "Versioned", 2).await;

    put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/upload"),
        "founder-token",
        &json!({}),
    )
    .await;
    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/complete"),
        "founder-token",
        &json!({ "generation": "1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post(
        &founder,
        &format!("/api/v1/operator/datasets/{dataset_id}/versions"),
        "founder-token",
        &json!({
            "info": lerobot_info(5),
            "files": [{
                "logical_path": "meta/info.json",
                "media_type": "application/json",
                "byte_length": DECLARED_BYTES,
                "sha256": declared_sha(),
            }],
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a further set of contents should be a new version, got {body}"
    );

    let numbers: Vec<(i32, String)> = sqlx::query_as(
        "SELECT version_number, status FROM dataset_versions          WHERE dataset_id = $1 ORDER BY version_number",
    )
    .bind(dataset_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        numbers,
        vec![(1, "ready".to_owned()), (2, "uploading".to_owned())],
        "version 1 must be left exactly as it was, and version 2 begins its own upload"
    );

    // Version 1 keeps its own files. A job that selected it still means what it meant.
    let first_files: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dataset_files WHERE version_id = $1")
            .bind(first_version_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(first_files, 1);

    let verified: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dataset_files WHERE version_id = $1 AND status = 'verified'",
    )
    .bind(first_version_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(verified, 1, "the earlier version's file stays verified");

    // The catalogue now reports both.
    let (status, listed) = get(&founder, "/api/v1/operator/datasets", "founder-token").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed[0]["versions"].as_array().map(Vec::len),
        Some(2),
        "the catalogue should show both versions"
    );
}

/// A dataset in another project cannot be given a version.
#[sqlx::test(migrations = "./migrations")]
async fn a_version_cannot_be_added_to_another_projects_dataset(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_agreeing()).await;
    let (dataset_id, _version_id, _file_id) =
        declare_upload(&founder, "founder-token", "Private", 1).await;

    let outsider = second_project(&pool, storage_agreeing()).await;
    let body = json!({
        "info": lerobot_info(1),
        "files": [{
            "logical_path": "meta/info.json",
            "media_type": "application/json",
            "byte_length": DECLARED_BYTES,
            "sha256": declared_sha(),
        }],
    });
    let real = post(
        &outsider,
        &format!("/api/v1/operator/datasets/{dataset_id}/versions"),
        "outsider-token",
        &body,
    )
    .await;
    let invented = post(
        &outsider,
        &format!("/api/v1/operator/datasets/{}/versions", Uuid::new_v4()),
        "outsider-token",
        &body,
    )
    .await;
    assert_eq!(real.0, StatusCode::NOT_FOUND);
    assert_eq!(
        real.0, invented.0,
        "a real dataset and an invented one must be indistinguishable"
    );

    let versions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dataset_versions WHERE dataset_id = $1")
            .bind(dataset_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(versions, 1, "nothing may have been added");
}

// ---------------------------------------------------------------------------------------------
// Audit trail
// ---------------------------------------------------------------------------------------------

/// Every dataset action that changes something leaves a trail entry, in the same transaction.
///
/// Same transaction rather than after it, so the trail cannot disagree with the catalogue: an
/// event without the change it describes is worse than no event, because it gets read as proof.
///
/// The detail is checked for what it must *not* contain as well. A storage object key or a
/// resumable-session URI in the audit table would turn a record of what happened into a way of
/// reaching the bytes, which is the one thing the guardrails single out.
#[sqlx::test(migrations = "./migrations")]
async fn every_dataset_action_is_audited_without_leaking_storage_locations(pool: PgPool) {
    let (founder, founder_id) = found(&pool, storage_agreeing()).await;
    let (dataset_id, version_id, file_id) =
        declare_upload(&founder, "founder-token", "Audited", 2).await;

    put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/upload"),
        "founder-token",
        &json!({}),
    )
    .await;
    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/complete"),
        "founder-token",
        &json!({ "generation": "3" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-versions/{version_id}/episodes/0/curation"),
        "founder-token",
        &json!({ "decision": "included" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = post(
        &founder,
        &format!("/api/v1/operator/dataset-versions/{version_id}/views"),
        "founder-token",
        &json!({ "name": "Audited cut" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let actions: Vec<(String, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT action, outcome, actor_id FROM audit_events \
         WHERE action LIKE 'dataset.%' ORDER BY occurred_at, action",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let names: Vec<&str> = actions.iter().map(|row| row.0.as_str()).collect();
    for expected in [
        "dataset.version.declared",
        "dataset.file.verified",
        "dataset.curation.updated",
        "dataset.view.published",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} should be on the audit trail, found {names:?}"
        );
    }
    for (action, outcome, actor) in &actions {
        assert_eq!(outcome, "succeeded", "{action} should have succeeded");
        assert_eq!(*actor, Some(founder_id), "{action} should name who did it");
    }

    // Whatever else the detail carries, it must not be a way to reach the object.
    let details: Vec<(Value,)> =
        sqlx::query_as("SELECT detail FROM audit_events WHERE action LIKE 'dataset.%'")
            .fetch_all(&pool)
            .await
            .unwrap();
    for (detail,) in &details {
        let text = serde_json::to_string(detail).unwrap();
        assert!(
            !text.contains("v1/projects/"),
            "an audit detail must not carry a storage object key: {text}"
        );
        assert!(
            !text.contains("storage.example.test") && !text.contains("upload/session"),
            "an audit detail must not carry a resumable-session URI: {text}"
        );
    }

    let _ = dataset_id;
}

/// A rejected upload is recorded as a failure, not omitted.
///
/// The trail has to answer "did anything go wrong with this dataset", and a rejection that left
/// no entry would make a failed version look like one that was simply never finished.
#[sqlx::test(migrations = "./migrations")]
async fn a_rejected_upload_is_recorded_as_a_failure(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_disagreeing()).await;
    let (_dataset_id, _version_id, file_id) =
        declare_upload(&founder, "founder-token", "Rejected", 1).await;

    put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/upload"),
        "founder-token",
        &json!({}),
    )
    .await;
    let (status, _) = put(
        &founder,
        &format!("/api/v1/operator/dataset-files/{file_id}/complete"),
        "founder-token",
        &json!({ "generation": "1" }),
    )
    .await;
    assert_ne!(status, StatusCode::OK);

    let (outcome, detail): (String, Value) = sqlx::query_as(
        "SELECT outcome, detail FROM audit_events \
         WHERE action = 'dataset.file.rejected' AND target_id = $1",
    )
    .bind(file_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outcome, "failed", "a rejection is a failure, not a success");
    assert_eq!(detail["reason"], "integrity mismatch");
    assert!(
        detail["declared_byte_length"] != detail["stored_byte_length"],
        "the trail should record what disagreed"
    );
}

/// The next version number is read behind a lock on the dataset row.
///
/// Two people adding a version at the same moment would otherwise both read the same maximum and
/// both claim the same number, and one would lose to `UNIQUE (dataset_id, version_number)` and be
/// told the dataset was in a state it was not.
///
/// The interleaving is forced rather than hoped for: joined futures do not overlap against a
/// local database, which answers inside a single poll. A connection outside the pool holds the
/// dataset row, the request blocks against it, a version is written and committed underneath, and
/// the request must then number itself 3 rather than 2.
#[sqlx::test(migrations = "./migrations")]
async fn the_next_version_number_is_read_under_the_dataset_lock(pool: PgPool) {
    let (founder, _founder_id) = found(&pool, storage_agreeing()).await;
    let (dataset_id, _version_id, _file_id) =
        declare_upload(&founder, "founder-token", "Raced", 1).await;

    // A separate identity for the outside writer: crediting the founder would deadlock against
    // the human_identities row the blocked request already holds from authorize_operator.
    let colleague_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities \
         (id, provider, provider_subject, display_name, email, role) \
         VALUES ($1, 'identity-platform', 'colleague', 'Colleague', 'colleague@example.com', 'operator')",
    )
    .bind(colleague_id)
    .execute(&pool)
    .await
    .unwrap();

    let mut outside = PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    let mut blocker = outside.begin().await.unwrap();
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM datasets WHERE id = $1 FOR UPDATE")
        .bind(dataset_id)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let body = json!({
        "info": lerobot_info(1),
        "files": [{
            "logical_path": "meta/info.json",
            "media_type": "application/json",
            "byte_length": DECLARED_BYTES,
            "sha256": declared_sha(),
        }],
    });
    let versions_path = format!("/api/v1/operator/datasets/{dataset_id}/versions");
    let (attempt, ()) = tokio::join!(
        post(&founder, &versions_path, "founder-token", &body),
        async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            sqlx::query(
                "INSERT INTO dataset_versions \
                 (id, dataset_id, project_id, version_number, source_kind, status, info_json, \
                  validation_json, total_episodes, total_frames, fps, created_by_identity_id) \
                 VALUES ($1, $2, $3, 2, 'upload', 'uploading', '{}'::jsonb, '{}'::jsonb, 1, 1, 30, $4)",
            )
            .bind(Uuid::new_v4())
            .bind(dataset_id)
            .bind(DEFAULT_PROJECT_ID)
            .bind(colleague_id)
            .execute(&mut *blocker)
            .await
            .unwrap();
            blocker.commit().await.unwrap();
        },
    );

    assert_eq!(
        attempt.0,
        StatusCode::CREATED,
        "the waiting request should succeed once the lock is released, got {}",
        attempt.1
    );

    let numbers: Vec<(i32,)> = sqlx::query_as(
        "SELECT version_number FROM dataset_versions WHERE dataset_id = $1 ORDER BY version_number",
    )
    .bind(dataset_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let numbers: Vec<i32> = numbers.into_iter().map(|row| row.0).collect();
    assert_eq!(
        numbers,
        vec![1, 2, 3],
        "the request must number itself after the version committed while it waited"
    );
}

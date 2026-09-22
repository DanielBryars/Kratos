use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    artifact_storage::ResumableUploadSession,
    operator::{OperatorError, authenticate_operator},
};

const DEFAULT_HF_REVISION: &str = "main";
const MAX_UPLOAD_FILES: usize = 10_000;
const MAX_NOTE_CHARS: usize = 2_000;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportHuggingFaceDatasetRequest {
    name: String,
    description: Option<String>,
    repository: String,
    revision: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateUploadDatasetRequest {
    name: String,
    description: Option<String>,
    info: Value,
    files: Vec<DatasetFileDeclaration>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateDatasetVersionRequest {
    info: Value,
    files: Vec<DatasetFileDeclaration>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatasetFileDeclaration {
    logical_path: String,
    media_type: String,
    byte_length: i64,
    sha256: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DatasetFileResponse {
    id: Uuid,
    logical_path: String,
    media_type: String,
    byte_length: i64,
    sha256: String,
    status: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DatasetVersionResponse {
    id: Uuid,
    version_number: i32,
    source_kind: String,
    status: String,
    source_repository: Option<String>,
    requested_revision: Option<String>,
    resolved_revision: Option<String>,
    manifest_sha256: Option<String>,
    info: Value,
    total_episodes: i32,
    total_frames: i64,
    fps: f64,
    created_at: DateTime<Utc>,
    ready_at: Option<DateTime<Utc>>,
    files: Vec<DatasetFileResponse>,
    curations: BTreeMap<i32, String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DatasetResponse {
    id: Uuid,
    project_id: Uuid,
    name: String,
    description: Option<String>,
    created_at: DateTime<Utc>,
    versions: Vec<DatasetVersionResponse>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompleteDatasetFileUploadRequest {
    generation: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct BeginDatasetFileUploadResponse {
    file_id: Uuid,
    upload: ResumableUploadSession,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EpisodeCurationRequest {
    decision: String,
    note: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct EpisodeCurationResponse {
    version_id: Uuid,
    episode_index: i32,
    decision: String,
    note: Option<String>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateDatasetViewRequest {
    name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DatasetViewResponse {
    id: Uuid,
    version_id: Uuid,
    name: String,
    manifest_sha256: String,
    included_episode_count: i32,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct HuggingFaceRevision {
    sha: String,
}

#[derive(Debug)]
struct DatasetSummary {
    total_episodes: i32,
    total_frames: i64,
    fps: f64,
}

#[utoipa::path(
    get,
    path = "/api/v1/operator/datasets",
    tag = "operator",
    security(("human_bearer" = [])),
    responses((status = 200, description = "Project datasets and immutable versions", body = [DatasetResponse]))
)]
pub(crate) async fn list_datasets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DatasetResponse>>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let dataset_rows = sqlx::query_as::<_, (Uuid, Uuid, String, Option<String>, DateTime<Utc>)>(
        "SELECT id, project_id, name, description, created_at FROM datasets \
         WHERE project_id = ANY($1) AND archived_at IS NULL ORDER BY created_at DESC",
    )
    .bind(&caller.project_ids)
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;

    let mut responses = Vec::with_capacity(dataset_rows.len());
    for (id, project_id, name, description, created_at) in dataset_rows {
        let version_rows = sqlx::query_as::<_, VersionRow>(
            "SELECT id, version_number, source_kind, status, source_repository, requested_revision, \
                    resolved_revision, manifest_sha256, info_json, total_episodes, total_frames, \
                    fps, created_at, ready_at FROM dataset_versions \
             WHERE dataset_id = $1 ORDER BY version_number DESC",
        )
        .bind(id)
        .fetch_all(database)
        .await
        .map_err(|_| OperatorError::internal())?;
        let mut versions = Vec::with_capacity(version_rows.len());
        for version in version_rows {
            versions.push(version_response(database, version).await?);
        }
        responses.push(DatasetResponse {
            id,
            project_id,
            name,
            description,
            created_at,
            versions,
        });
    }
    Ok(Json(responses))
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/datasets/import-hugging-face",
    tag = "operator",
    security(("human_bearer" = [])),
    request_body = ImportHuggingFaceDatasetRequest,
    responses((status = 201, description = "Immutable Hugging Face dataset imported", body = DatasetResponse))
)]
pub(crate) async fn import_hugging_face_dataset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ImportHuggingFaceDatasetRequest>,
) -> Result<(StatusCode, Json<DatasetResponse>), OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let project_id = caller.sole_project()?;
    validate_dataset_name(&request.name, request.description.as_deref())?;
    let repository_parts = validate_hf_repository(&request.repository)?;
    let requested_revision = request
        .revision
        .as_deref()
        .unwrap_or(DEFAULT_HF_REVISION)
        .trim();
    if requested_revision.is_empty() || requested_revision.len() > 200 {
        return Err(OperatorError::invalid_request());
    }

    let client = Client::new();
    let resolved_revision =
        resolve_hf_revision(&client, repository_parts, requested_revision).await?;
    let info = fetch_hf_info(&client, repository_parts, &resolved_revision).await?;
    let summary = validate_info(&info)?;
    let manifest_sha256 = hash_json(&json!({
        "source": "hugging_face",
        "repository": request.repository,
        "revision": resolved_revision,
    }))?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let dataset_id = Uuid::new_v4();
    let version_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO datasets \
         (id, project_id, name, description, created_by_identity_id) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(dataset_id)
    .bind(project_id)
    .bind(request.name.trim())
    .bind(request.description.as_deref().map(str::trim))
    .bind(caller.identity_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::dataset_conflict())?;
    sqlx::query(
        "INSERT INTO dataset_versions \
         (id, dataset_id, project_id, version_number, source_kind, status, source_repository, \
          requested_revision, resolved_revision, manifest_sha256, info_json, validation_json, \
          total_episodes, total_frames, fps, created_by_identity_id, ready_at) \
         VALUES ($1, $2, $3, 1, 'hugging_face', 'ready', $4, $5, $6, $7, $8, $9, \
                 $10, $11, $12, $13, now())",
    )
    .bind(version_id)
    .bind(dataset_id)
    .bind(project_id)
    .bind(request.repository.trim())
    .bind(requested_revision)
    .bind(&resolved_revision)
    .bind(&manifest_sha256)
    .bind(&info)
    .bind(json!({ "lerobot_info": "passed", "source_revision": "resolved" }))
    .bind(summary.total_episodes)
    .bind(summary.total_frames)
    .bind(summary.fps)
    .bind(caller.identity_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    record_dataset_audit(
        &mut transaction,
        caller.identity_id,
        "dataset.version.imported",
        "dataset_version",
        version_id,
        "succeeded",
        json!({ "dataset_id": dataset_id, "source_kind": "hugging_face", "resolved_revision": resolved_revision }),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;

    let created = load_dataset(database, dataset_id, &caller.project_ids).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/datasets/upload",
    tag = "operator",
    security(("human_bearer" = [])),
    request_body = CreateUploadDatasetRequest,
    responses((status = 201, description = "Dataset upload declared", body = DatasetResponse))
)]
pub(crate) async fn create_upload_dataset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateUploadDatasetRequest>,
) -> Result<(StatusCode, Json<DatasetResponse>), OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let project_id = caller.sole_project()?;
    validate_dataset_name(&request.name, request.description.as_deref())?;
    let summary = validate_info(&request.info)?;
    validate_file_declarations(&request.files)?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let storage = state
        .artifact_storage
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let dataset_id = Uuid::new_v4();
    let version_id = Uuid::new_v4();
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    sqlx::query(
        "INSERT INTO datasets \
         (id, project_id, name, description, created_by_identity_id) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(dataset_id)
    .bind(project_id)
    .bind(request.name.trim())
    .bind(request.description.as_deref().map(str::trim))
    .bind(caller.identity_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::dataset_conflict())?;
    sqlx::query(
        "INSERT INTO dataset_versions \
         (id, dataset_id, project_id, version_number, source_kind, status, info_json, \
          validation_json, total_episodes, total_frames, fps, created_by_identity_id) \
         VALUES ($1, $2, $3, 1, 'upload', 'uploading', $4, $5, $6, $7, $8, $9)",
    )
    .bind(version_id)
    .bind(dataset_id)
    .bind(project_id)
    .bind(&request.info)
    .bind(json!({ "lerobot_info": "passed", "files": "pending" }))
    .bind(summary.total_episodes)
    .bind(summary.total_frames)
    .bind(summary.fps)
    .bind(caller.identity_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    declare_version_files(
        &mut transaction,
        project_id,
        dataset_id,
        version_id,
        storage.bucket(),
        &request.files,
    )
    .await?;
    record_dataset_audit(
        &mut transaction,
        caller.identity_id,
        "dataset.version.declared",
        "dataset_version",
        version_id,
        "succeeded",
        json!({ "dataset_id": dataset_id, "source_kind": "upload", "declared_files": request.files.len() }),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    let created = load_dataset(database, dataset_id, &caller.project_ids).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/datasets/{dataset_id}/versions",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("dataset_id" = Uuid, Path, description = "Existing catalogue entry")),
    request_body = CreateDatasetVersionRequest,
    responses(
        (status = 201, description = "A further immutable version declared", body = DatasetResponse),
        (status = 404, description = "No such dataset in the caller's projects"),
        (status = 422, description = "Invalid metadata or file declarations"),
    )
)]
/// Add a version to an existing dataset.
///
/// ADR-018: "A `ready` version is immutable. Replacing any file, changing a Hugging Face
/// revision, or changing generated metadata creates another version." This is the endpoint that
/// makes that true. It is deliberately separate from `create_upload_dataset`, which must keep
/// refusing a duplicate name -- creating a catalogue entry and adding contents to one are
/// different intentions, and one endpoint cannot both reject a repeated name and treat it as a
/// request for the next version.
///
/// Earlier versions are not touched. That is the whole point: a job that selected version 1
/// keeps meaning exactly what it meant.
pub(crate) async fn create_dataset_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(dataset_id): Path<Uuid>,
    Json(request): Json<CreateDatasetVersionRequest>,
) -> Result<(StatusCode, Json<DatasetResponse>), OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let summary = validate_info(&request.info)?;
    validate_file_declarations(&request.files)?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let storage = state
        .artifact_storage
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;

    // The dataset row is the lock, and the next version number is read behind it. Two people
    // adding a version at the same moment would otherwise both read the same maximum and both
    // claim the same number; one would lose to the unique constraint and be told the dataset was
    // in a state it was not. Locking here also scopes the request: a dataset in another project
    // is simply not found.
    let project_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT project_id FROM datasets \
         WHERE id = $1 AND project_id = ANY($2) AND archived_at IS NULL FOR UPDATE",
    )
    .bind(dataset_id)
    .bind(&caller.project_ids)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?
    .ok_or_else(OperatorError::dataset_not_found)?;

    let next_number = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT max(version_number) FROM dataset_versions WHERE dataset_id = $1",
    )
    .bind(dataset_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?
    .unwrap_or(0)
    .checked_add(1)
    .ok_or_else(OperatorError::internal)?;

    let version_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO dataset_versions \
         (id, dataset_id, project_id, version_number, source_kind, status, info_json, \
          validation_json, total_episodes, total_frames, fps, created_by_identity_id) \
         VALUES ($1, $2, $3, $4, 'upload', 'uploading', $5, $6, $7, $8, $9, $10)",
    )
    .bind(version_id)
    .bind(dataset_id)
    .bind(project_id)
    .bind(next_number)
    .bind(&request.info)
    .bind(json!({ "lerobot_info": "passed", "files": "pending" }))
    .bind(summary.total_episodes)
    .bind(summary.total_frames)
    .bind(summary.fps)
    .bind(caller.identity_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;

    declare_version_files(
        &mut transaction,
        project_id,
        dataset_id,
        version_id,
        storage.bucket(),
        &request.files,
    )
    .await?;

    record_dataset_audit(
        &mut transaction,
        caller.identity_id,
        "dataset.version.declared",
        "dataset_version",
        version_id,
        "succeeded",
        json!({
            "dataset_id": dataset_id,
            "source_kind": "upload",
            "version_number": next_number,
            "declared_files": request.files.len(),
        }),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;

    let created = load_dataset(database, dataset_id, &caller.project_ids).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

#[utoipa::path(
    put,
    path = "/api/v1/operator/dataset-files/{file_id}/upload",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("file_id" = Uuid, Path, description = "Declared dataset file")),
    responses((status = 200, description = "Replay-safe resumable upload session", body = BeginDatasetFileUploadResponse))
)]
pub(crate) async fn begin_dataset_file_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(file_id): Path<Uuid>,
) -> Result<Json<BeginDatasetFileUploadResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let storage = state
        .artifact_storage
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let row = sqlx::query_as::<_, UploadFileRow>(
        "SELECT df.storage_object_key, df.media_type, df.byte_length, df.sha256, df.status, \
                u.session_uri, u.expires_at \
         FROM dataset_files df \
         JOIN dataset_versions dv ON dv.id = df.version_id \
         LEFT JOIN dataset_file_uploads u ON u.file_id = df.id \
         WHERE df.id = $1 AND df.project_id = ANY($2) AND dv.status = 'uploading' FOR UPDATE OF df",
    )
    .bind(file_id)
    .bind(&caller.project_ids)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?
    .ok_or_else(OperatorError::dataset_not_found)?;
    if row.status == "verified" || row.status == "rejected" {
        return Err(OperatorError::dataset_conflict());
    }
    if let (Some(uri), Some(expires_at)) = (row.session_uri, row.expires_at)
        && expires_at > Utc::now()
    {
        transaction
            .commit()
            .await
            .map_err(|_| OperatorError::internal())?;
        return Ok(Json(BeginDatasetFileUploadResponse {
            file_id,
            upload: ResumableUploadSession {
                uri,
                method: "PUT".to_owned(),
                expires_at,
            },
        }));
    }
    let issued_at = Utc::now();
    let upload = storage
        .initiate_resumable_upload(
            &row.storage_object_key,
            &row.media_type,
            u64::try_from(row.byte_length).map_err(|_| OperatorError::invalid_request())?,
            &row.sha256,
            issued_at,
        )
        .await
        .map_err(|_| OperatorError::unavailable())?;
    sqlx::query(
        "INSERT INTO dataset_file_uploads (file_id, session_uri, issued_at, expires_at) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (file_id) DO UPDATE SET \
         session_uri = EXCLUDED.session_uri, issued_at = EXCLUDED.issued_at, \
         expires_at = EXCLUDED.expires_at, completed_at = NULL",
    )
    .bind(file_id)
    .bind(&upload.uri)
    .bind(issued_at)
    .bind(upload.expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    sqlx::query("UPDATE dataset_files SET status = 'uploading' WHERE id = $1")
        .bind(file_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok(Json(BeginDatasetFileUploadResponse { file_id, upload }))
}

#[utoipa::path(
    put,
    path = "/api/v1/operator/dataset-files/{file_id}/complete",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("file_id" = Uuid, Path, description = "Uploaded dataset file")),
    request_body = CompleteDatasetFileUploadRequest,
    responses((status = 200, description = "File verified and version published when complete", body = DatasetFileResponse))
)]
pub(crate) async fn complete_dataset_file_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(file_id): Path<Uuid>,
    Json(request): Json<CompleteDatasetFileUploadRequest>,
) -> Result<Json<DatasetFileResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let generation = request
        .generation
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(OperatorError::invalid_request)?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let storage = state
        .artifact_storage
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let row = sqlx::query_as::<_, CompleteFileRow>(
        "SELECT df.version_id, df.logical_path, df.media_type, df.byte_length, df.sha256, \
                df.storage_bucket, df.storage_object_key \
         FROM dataset_files df JOIN dataset_versions dv ON dv.id = df.version_id \
         WHERE df.id = $1 AND df.project_id = ANY($2) AND dv.status = 'uploading'",
    )
    .bind(file_id)
    .bind(&caller.project_ids)
    .fetch_optional(database)
    .await
    .map_err(|_| OperatorError::internal())?
    .ok_or_else(OperatorError::dataset_not_found)?;
    let metadata = storage
        .object_metadata(&row.storage_bucket, &row.storage_object_key, generation)
        .await
        .map_err(|_| OperatorError::unavailable())?;
    if metadata.byte_length != row.byte_length || metadata.sha256 != row.sha256 {
        reject_uploaded_file(database, &caller, file_id, &row, metadata.byte_length).await?;
        return Err(OperatorError::dataset_integrity());
    }
    storage
        .protect_verified_object(&row.storage_bucket, &row.storage_object_key, generation)
        .await
        .map_err(|_| OperatorError::unavailable())?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    sqlx::query(
        "UPDATE dataset_files SET status = 'verified', storage_generation = $2, verified_at = now(), \
         rejection_reason = NULL WHERE id = $1",
    )
    .bind(file_id)
    .bind(generation)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    sqlx::query("DELETE FROM dataset_file_uploads WHERE file_id = $1")
        .bind(file_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
    publish_version_if_complete(&mut transaction, row.version_id).await?;
    record_dataset_audit(
        &mut transaction,
        caller.identity_id,
        "dataset.file.verified",
        "dataset_file",
        file_id,
        "succeeded",
        json!({
            "version_id": row.version_id,
            "logical_path": row.logical_path,
            "byte_length": row.byte_length,
            "storage_generation": generation,
        }),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok(Json(DatasetFileResponse {
        id: file_id,
        logical_path: row.logical_path,
        media_type: row.media_type,
        byte_length: row.byte_length,
        sha256: row.sha256,
        status: "verified".to_owned(),
    }))
}

#[utoipa::path(
    put,
    path = "/api/v1/operator/dataset-versions/{version_id}/episodes/{episode_index}/curation",
    tag = "operator",
    security(("human_bearer" = [])),
    params(
        ("version_id" = Uuid, Path, description = "Immutable dataset version"),
        ("episode_index" = i32, Path, description = "Zero-based episode index")
    ),
    request_body = EpisodeCurationRequest,
    responses((status = 200, description = "Episode decision saved", body = EpisodeCurationResponse))
)]
pub(crate) async fn upsert_episode_curation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((version_id, episode_index)): Path<(Uuid, i32)>,
    Json(request): Json<EpisodeCurationRequest>,
) -> Result<Json<EpisodeCurationResponse>, OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    if episode_index < 0
        || !matches!(
            request.decision.as_str(),
            "included" | "excluded" | "needs_review"
        )
        || request
            .note
            .as_ref()
            .is_some_and(|note| note.chars().count() > MAX_NOTE_CHARS)
    {
        return Err(OperatorError::invalid_request());
    }
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let row = sqlx::query_as::<_, (Uuid, i32, String, Option<String>, DateTime<Utc>)>(
        "INSERT INTO dataset_episode_curations \
         (version_id, project_id, episode_index, decision, note, updated_by_identity_id) \
         SELECT dv.id, dv.project_id, $2, $3, $4, $5 FROM dataset_versions dv \
         WHERE dv.id = $1 AND dv.project_id = ANY($6) AND dv.status = 'ready' \
           AND $2 >= 0 AND $2 < dv.total_episodes \
         ON CONFLICT (version_id, episode_index) DO UPDATE SET \
           decision = EXCLUDED.decision, note = EXCLUDED.note, \
           updated_by_identity_id = EXCLUDED.updated_by_identity_id, updated_at = now() \
         RETURNING version_id, episode_index, decision, note, updated_at",
    )
    .bind(version_id)
    .bind(episode_index)
    .bind(&request.decision)
    .bind(
        request
            .note
            .as_deref()
            .map(str::trim)
            .filter(|note| !note.is_empty()),
    )
    .bind(caller.identity_id)
    .bind(&caller.project_ids)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?
    .ok_or_else(OperatorError::dataset_not_found)?;
    // The decision and the record of who made it commit together. A curation the audit trail
    // does not know about is how "who excluded this episode, and why" stops being answerable.
    record_dataset_audit(
        &mut transaction,
        caller.identity_id,
        "dataset.curation.updated",
        "dataset_version",
        row.0,
        "succeeded",
        json!({ "episode_index": row.1, "decision": row.2 }),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok(Json(EpisodeCurationResponse {
        version_id: row.0,
        episode_index: row.1,
        decision: row.2,
        note: row.3,
        updated_at: row.4,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/operator/dataset-versions/{version_id}/views",
    tag = "operator",
    security(("human_bearer" = [])),
    params(("version_id" = Uuid, Path, description = "Curated dataset version")),
    request_body = CreateDatasetViewRequest,
    responses((status = 201, description = "Immutable curated view published", body = DatasetViewResponse))
)]
pub(crate) async fn create_dataset_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(version_id): Path<Uuid>,
    Json(request): Json<CreateDatasetViewRequest>,
) -> Result<(StatusCode, Json<DatasetViewResponse>), OperatorError> {
    let caller = authenticate_operator(&state, &headers).await?;
    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(OperatorError::invalid_request());
    }
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    let version = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT dataset_id, id, project_id FROM dataset_versions \
         WHERE id = $1 AND project_id = ANY($2) AND status = 'ready' FOR SHARE",
    )
    .bind(version_id)
    .bind(&caller.project_ids)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?
    .ok_or_else(OperatorError::dataset_not_found)?;
    let episodes = sqlx::query_scalar::<_, i32>(
        "SELECT episode_index FROM dataset_episode_curations \
         WHERE version_id = $1 AND decision = 'included' ORDER BY episode_index",
    )
    .bind(version_id)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    if episodes.is_empty() {
        return Err(OperatorError::invalid_request());
    }
    let manifest_sha256 = hash_json(&json!({
        "version_id": version_id,
        "included_episodes": &episodes,
    }))?;
    let view_id = Uuid::new_v4();
    let created_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        "INSERT INTO dataset_views \
         (id, dataset_id, version_id, project_id, name, manifest_sha256, included_episode_count, \
          created_by_identity_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING created_at",
    )
    .bind(view_id)
    .bind(version.0)
    .bind(version_id)
    .bind(version.2)
    .bind(name)
    .bind(&manifest_sha256)
    .bind(i32::try_from(episodes.len()).map_err(|_| OperatorError::invalid_request())?)
    .bind(caller.identity_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| OperatorError::dataset_conflict())?;
    sqlx::query(
        "INSERT INTO dataset_view_episodes (view_id, episode_index) \
         SELECT $1, unnest($2::integer[])",
    )
    .bind(view_id)
    .bind(&episodes)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    record_dataset_audit(
        &mut transaction,
        caller.identity_id,
        "dataset.view.published",
        "dataset_view",
        view_id,
        "succeeded",
        json!({
            "version_id": version_id,
            "included_episode_count": episodes.len(),
        }),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok((
        StatusCode::CREATED,
        Json(DatasetViewResponse {
            id: view_id,
            version_id,
            name: name.to_owned(),
            manifest_sha256,
            included_episode_count: i32::try_from(episodes.len())
                .map_err(|_| OperatorError::invalid_request())?,
            created_at,
        }),
    ))
}

#[derive(sqlx::FromRow)]
struct VersionRow {
    id: Uuid,
    version_number: i32,
    source_kind: String,
    status: String,
    source_repository: Option<String>,
    requested_revision: Option<String>,
    resolved_revision: Option<String>,
    manifest_sha256: Option<String>,
    info_json: Value,
    total_episodes: i32,
    total_frames: i64,
    fps: f64,
    created_at: DateTime<Utc>,
    ready_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct UploadFileRow {
    storage_object_key: String,
    media_type: String,
    byte_length: i64,
    sha256: String,
    status: String,
    session_uri: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct CompleteFileRow {
    version_id: Uuid,
    logical_path: String,
    media_type: String,
    byte_length: i64,
    sha256: String,
    storage_bucket: String,
    storage_object_key: String,
}

/// Write the declared files of a version, each with the object key it will occupy.
///
/// Shared by the first version of a dataset and every later one, so the key shape cannot drift
/// between the two paths -- which is the sort of difference nothing would notice until an
/// artefact could not be found.
async fn declare_version_files(
    transaction: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    dataset_id: Uuid,
    version_id: Uuid,
    bucket: &str,
    files: &[DatasetFileDeclaration],
) -> Result<(), OperatorError> {
    for file in files {
        let file_id = Uuid::new_v4();
        let object_key = format!(
            "v1/projects/{project_id}/datasets/{dataset_id}/versions/{version_id}/files/{file_id}"
        );
        sqlx::query(
            "INSERT INTO dataset_files \
             (id, version_id, project_id, logical_path, media_type, byte_length, sha256, \
              storage_bucket, storage_object_key, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'declared')",
        )
        .bind(file_id)
        .bind(version_id)
        .bind(project_id)
        .bind(&file.logical_path)
        .bind(&file.media_type)
        .bind(file.byte_length)
        .bind(&file.sha256)
        .bind(bucket)
        .bind(object_key)
        .execute(&mut **transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
    }
    Ok(())
}

/// Refuse an uploaded object that does not match what was declared, and fail its version.
///
/// The version fails rather than merely losing one file. A version that stayed selectable with a
/// rejected file in it is the dangerous outcome: a training run would pick it and quietly train
/// on an incomplete dataset.
async fn reject_uploaded_file(
    database: &sqlx::PgPool,
    caller: &crate::operator::Caller,
    file_id: Uuid,
    row: &CompleteFileRow,
    stored_byte_length: i64,
) -> Result<(), OperatorError> {
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| OperatorError::internal())?;
    sqlx::query(
        "UPDATE dataset_files SET status = 'rejected', rejection_reason = 'integrity mismatch'          WHERE id = $1",
    )
    .bind(file_id)
    .execute(&mut *transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    sqlx::query("UPDATE dataset_versions SET status = 'failed' WHERE id = $1")
        .bind(row.version_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
    record_dataset_audit(
        &mut transaction,
        caller.identity_id,
        "dataset.file.rejected",
        "dataset_file",
        file_id,
        "failed",
        json!({
            "version_id": row.version_id,
            "logical_path": row.logical_path,
            "reason": "integrity mismatch",
            "declared_byte_length": row.byte_length,
            "stored_byte_length": stored_byte_length,
        }),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|_| OperatorError::internal())?;
    Ok(())
}

/// Record a dataset action on the audit trail, inside the caller's transaction.
///
/// Inside, rather than after, so the trail cannot disagree with the catalogue: an event without
/// the change it describes is worse than no event, because it is read as proof.
///
/// `detail` carries identifiers and the operator's own words -- a logical path, an episode
/// index, a rejection reason. It never carries a storage object key, a resumable-session URI or
/// anything else that would turn the audit table into a way of reaching the bytes.
async fn record_dataset_audit(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: Uuid,
    action: &str,
    target_type: &str,
    target_id: Uuid,
    outcome: &str,
    detail: Value,
) -> Result<(), OperatorError> {
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'human', $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::new_v4())
    .bind(actor_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(outcome)
    .bind(detail)
    .execute(&mut **transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    Ok(())
}

async fn load_dataset(
    database: &sqlx::PgPool,
    dataset_id: Uuid,
    project_ids: &[Uuid],
) -> Result<DatasetResponse, OperatorError> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, String, Option<String>, DateTime<Utc>)>(
        "SELECT id, project_id, name, description, created_at FROM datasets \
         WHERE id = $1 AND project_id = ANY($2) AND archived_at IS NULL",
    )
    .bind(dataset_id)
    .bind(project_ids)
    .fetch_optional(database)
    .await
    .map_err(|_| OperatorError::internal())?
    .ok_or_else(OperatorError::dataset_not_found)?;
    let version_rows = sqlx::query_as::<_, VersionRow>(
        "SELECT id, version_number, source_kind, status, source_repository, requested_revision, \
                resolved_revision, manifest_sha256, info_json, total_episodes, total_frames, fps, \
                created_at, ready_at FROM dataset_versions WHERE dataset_id = $1 \
         ORDER BY version_number DESC",
    )
    .bind(dataset_id)
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    let mut versions = Vec::with_capacity(version_rows.len());
    for version in version_rows {
        versions.push(version_response(database, version).await?);
    }
    Ok(DatasetResponse {
        id: row.0,
        project_id: row.1,
        name: row.2,
        description: row.3,
        created_at: row.4,
        versions,
    })
}

async fn version_response(
    database: &sqlx::PgPool,
    version: VersionRow,
) -> Result<DatasetVersionResponse, OperatorError> {
    let files = sqlx::query_as::<_, (Uuid, String, String, i64, String, String)>(
        "SELECT id, logical_path, media_type, byte_length, sha256, status FROM dataset_files \
         WHERE version_id = $1 ORDER BY logical_path",
    )
    .bind(version.id)
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?
    .into_iter()
    .map(
        |(id, logical_path, media_type, byte_length, sha256, status)| DatasetFileResponse {
            id,
            logical_path,
            media_type,
            byte_length,
            sha256,
            status,
        },
    )
    .collect();
    let curations = sqlx::query_as::<_, (i32, String)>(
        "SELECT episode_index, decision FROM dataset_episode_curations \
         WHERE version_id = $1 ORDER BY episode_index",
    )
    .bind(version.id)
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?
    .into_iter()
    .collect();
    Ok(DatasetVersionResponse {
        id: version.id,
        version_number: version.version_number,
        source_kind: version.source_kind,
        status: version.status,
        source_repository: version.source_repository,
        requested_revision: version.requested_revision,
        resolved_revision: version.resolved_revision,
        manifest_sha256: version.manifest_sha256,
        info: version.info_json,
        total_episodes: version.total_episodes,
        total_frames: version.total_frames,
        fps: version.fps,
        created_at: version.created_at,
        ready_at: version.ready_at,
        files,
        curations,
    })
}

async fn publish_version_if_complete(
    transaction: &mut Transaction<'_, Postgres>,
    version_id: Uuid,
) -> Result<(), OperatorError> {
    let remaining = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM dataset_files WHERE version_id = $1 AND status <> 'verified'",
    )
    .bind(version_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    if remaining != 0 {
        return Ok(());
    }
    let files = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT logical_path, byte_length, sha256 FROM dataset_files \
         WHERE version_id = $1 ORDER BY logical_path",
    )
    .bind(version_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    let manifest_sha256 = hash_json(&files)?;
    sqlx::query(
        "UPDATE dataset_versions SET status = 'ready', manifest_sha256 = $2, ready_at = now(), \
         validation_json = validation_json || '{\"files\":\"passed\"}'::jsonb \
         WHERE id = $1 AND status = 'uploading'",
    )
    .bind(version_id)
    .bind(manifest_sha256)
    .execute(&mut **transaction)
    .await
    .map_err(|_| OperatorError::internal())?;
    Ok(())
}

fn validate_dataset_name(name: &str, description: Option<&str>) -> Result<(), OperatorError> {
    if name.trim().is_empty()
        || name.chars().count() > 200
        || description.is_some_and(|value| value.chars().count() > 2_000)
    {
        return Err(OperatorError::invalid_request());
    }
    Ok(())
}

fn validate_hf_repository(repository: &str) -> Result<[&str; 2], OperatorError> {
    let mut parts = repository.trim().split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty()
        || name.is_empty()
        || parts.next().is_some()
        || !owner
            .chars()
            .chain(name.chars())
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        return Err(OperatorError::invalid_request());
    }
    Ok([owner, name])
}

fn validate_info(info: &Value) -> Result<DatasetSummary, OperatorError> {
    let total_episodes = info
        .get("total_episodes")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value >= 0)
        .ok_or_else(OperatorError::invalid_request)?;
    let total_frames = info
        .get("total_frames")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or_else(OperatorError::invalid_request)?;
    let fps = info
        .get("fps")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(OperatorError::invalid_request)?;
    let version = info
        .get("codebase_version")
        .and_then(Value::as_str)
        .filter(|value| value.starts_with('2') || value.starts_with('3') || value.starts_with('v'))
        .ok_or_else(OperatorError::invalid_request)?;
    if version.len() > 20 || !info.get("features").is_some_and(Value::is_object) {
        return Err(OperatorError::invalid_request());
    }
    Ok(DatasetSummary {
        total_episodes,
        total_frames,
        fps,
    })
}

fn validate_file_declarations(files: &[DatasetFileDeclaration]) -> Result<(), OperatorError> {
    if files.is_empty() || files.len() > MAX_UPLOAD_FILES {
        return Err(OperatorError::invalid_request());
    }
    let mut paths = std::collections::BTreeSet::new();
    for file in files {
        let valid_path = !file.logical_path.is_empty()
            && file.logical_path.len() <= 512
            && !file.logical_path.starts_with('/')
            && !file.logical_path.contains('\\')
            && file
                .logical_path
                .split('/')
                .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
        if !valid_path
            || !paths.insert(file.logical_path.as_str())
            || file.byte_length <= 0
            || file.media_type.is_empty()
            || file.media_type.len() > 200
            || file.sha256.len() != 64
            || !file
                .sha256
                .chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
        {
            return Err(OperatorError::invalid_request());
        }
    }
    if !paths.contains("meta/info.json") {
        return Err(OperatorError::invalid_request());
    }
    Ok(())
}

async fn resolve_hf_revision(
    client: &Client,
    repository: [&str; 2],
    revision: &str,
) -> Result<String, OperatorError> {
    let mut url = Url::parse("https://huggingface.co").expect("static URL is valid");
    url.path_segments_mut()
        .expect("HTTPS URL accepts path segments")
        .extend([
            "api",
            "datasets",
            repository[0],
            repository[1],
            "revision",
            revision,
        ]);
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| OperatorError::dataset_source_unavailable())?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(OperatorError::dataset_not_found());
    }
    let revision = response
        .error_for_status()
        .map_err(|_| OperatorError::dataset_source_unavailable())?
        .json::<HuggingFaceRevision>()
        .await
        .map_err(|_| OperatorError::dataset_source_unavailable())?;
    if revision.sha.len() != 40
        || !revision
            .sha
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(OperatorError::dataset_source_unavailable());
    }
    Ok(revision.sha.to_ascii_lowercase())
}

async fn fetch_hf_info(
    client: &Client,
    repository: [&str; 2],
    revision: &str,
) -> Result<Value, OperatorError> {
    let mut url = Url::parse("https://huggingface.co").expect("static URL is valid");
    url.path_segments_mut()
        .expect("HTTPS URL accepts path segments")
        .extend([
            "datasets",
            repository[0],
            repository[1],
            "resolve",
            revision,
            "meta",
            "info.json",
        ]);
    client
        .get(url)
        .send()
        .await
        .map_err(|_| OperatorError::dataset_source_unavailable())?
        .error_for_status()
        .map_err(|_| OperatorError::dataset_source_unavailable())?
        .json()
        .await
        .map_err(|_| OperatorError::invalid_request())
}

fn hash_json(value: &impl Serialize) -> Result<String, OperatorError> {
    let bytes = serde_json::to_vec(value).map_err(|_| OperatorError::internal())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_upload_paths() {
        let files = [DatasetFileDeclaration {
            logical_path: "meta/../secret".to_owned(),
            media_type: "application/octet-stream".to_owned(),
            byte_length: 1,
            sha256: "a".repeat(64),
        }];
        assert!(validate_file_declarations(&files).is_err());
    }

    #[test]
    fn requires_lerobot_info_file() {
        let files = [DatasetFileDeclaration {
            logical_path: "data/episode.parquet".to_owned(),
            media_type: "application/vnd.apache.parquet".to_owned(),
            byte_length: 1,
            sha256: "a".repeat(64),
        }];
        assert!(validate_file_declarations(&files).is_err());
    }

    #[test]
    fn accepts_safe_upload_manifest() {
        let files = [DatasetFileDeclaration {
            logical_path: "meta/info.json".to_owned(),
            media_type: "application/json".to_owned(),
            byte_length: 42,
            sha256: "a".repeat(64),
        }];
        assert!(validate_file_declarations(&files).is_ok());
    }
}

#[cfg(test)]
mod acceptance;

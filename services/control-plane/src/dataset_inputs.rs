use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    operator::OperatorError,
    registry::{
        ApiError, ErrorResponse, authenticate_worker, database_error,
        lock_current_worker_authorization,
    },
};

const DOWNLOAD_SECONDS: u32 = 3_600;
pub(crate) const MAX_JOB_DATASET_INPUTS: usize = 8;
const MAX_DATASET_INPUT_FILES: i64 = 10_000;
const MAX_DATASET_INPUT_FILE_BYTES: i64 = 20 * 1024 * 1024 * 1024;
const MAX_DATASET_INPUT_TOTAL_BYTES: i64 = 200 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobDatasetInputRequest {
    pub alias: String,
    pub dataset_version_id: Uuid,
    #[serde(default)]
    pub dataset_view_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct JobDatasetInputResponse {
    pub alias: String,
    pub dataset_id: Uuid,
    pub dataset_name: String,
    pub dataset_version_id: Uuid,
    pub version_number: i32,
    pub source_kind: String,
    pub source_repository: Option<String>,
    pub resolved_revision: Option<String>,
    pub dataset_manifest_sha256: String,
    pub dataset_view_id: Option<Uuid>,
    pub dataset_view_name: Option<String>,
    pub dataset_view_manifest_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct JobDatasetInputAssignment {
    pub alias: String,
    pub dataset_version_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset_view_id: Option<Uuid>,
    pub manifest_sha256: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DatasetInputFile {
    pub path: String,
    pub media_type: String,
    pub byte_length: u64,
    pub sha256: String,
    pub download_url: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DatasetInputManifestResponse {
    pub alias: String,
    pub dataset_id: Uuid,
    pub dataset_name: String,
    pub dataset_version_id: Uuid,
    pub version_number: i32,
    pub source_kind: String,
    pub source_repository: Option<String>,
    pub resolved_revision: Option<String>,
    pub manifest_sha256: String,
    pub dataset_view_id: Option<Uuid>,
    pub dataset_view_name: Option<String>,
    pub dataset_view_manifest_sha256: Option<String>,
    pub included_episodes: Vec<i32>,
    pub files: Vec<DatasetInputFile>,
}

#[derive(sqlx::FromRow)]
struct DatasetInputFileRecord {
    logical_path: String,
    media_type: String,
    byte_length: i64,
    sha256: String,
    storage_bucket: String,
    storage_object_key: String,
}

pub(crate) fn valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

pub(crate) async fn insert_job_dataset_inputs(
    transaction: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    project_id: Uuid,
    inputs: &[JobDatasetInputRequest],
) -> Result<(), OperatorError> {
    if inputs.len() > MAX_JOB_DATASET_INPUTS
        || inputs.iter().any(|input| !valid_alias(&input.alias))
    {
        return Err(OperatorError::invalid_request());
    }
    let mut aliases = std::collections::HashSet::new();
    if inputs.iter().any(|input| !aliases.insert(&input.alias)) {
        return Err(OperatorError::invalid_request());
    }
    for input in inputs {
        let inserted = sqlx::query(
            "INSERT INTO job_dataset_inputs \
             (id, job_id, alias, dataset_id, dataset_version_id, dataset_view_id, \
              source_kind, source_repository, resolved_revision, dataset_manifest_sha256, \
              dataset_view_manifest_sha256) \
             SELECT $1, $2, $3, dv.dataset_id, dv.id, view.id, dv.source_kind, \
                    dv.source_repository, dv.resolved_revision, dv.manifest_sha256, \
                    view.manifest_sha256 \
             FROM dataset_versions dv \
             LEFT JOIN dataset_views view ON view.version_id = dv.id AND view.id = $6 \
             WHERE dv.id = $4 AND dv.project_id = $5 AND dv.status = 'ready' \
               AND ($6::uuid IS NULL OR view.id IS NOT NULL) \
               AND (SELECT count(*) FROM dataset_files file \
                    WHERE file.version_id = dv.id AND file.status = 'verified') <= $7 \
               AND NOT EXISTS (SELECT 1 FROM dataset_files file \
                    WHERE file.version_id = dv.id AND file.status = 'verified' \
                      AND file.byte_length > $8) \
               AND COALESCE((SELECT sum(file.byte_length) FROM dataset_files file \
                    WHERE file.version_id = dv.id AND file.status = 'verified'), 0) <= $9",
        )
        .bind(Uuid::new_v4())
        .bind(job_id)
        .bind(&input.alias)
        .bind(input.dataset_version_id)
        .bind(project_id)
        .bind(input.dataset_view_id)
        .bind(MAX_DATASET_INPUT_FILES)
        .bind(MAX_DATASET_INPUT_FILE_BYTES)
        .bind(MAX_DATASET_INPUT_TOTAL_BYTES)
        .execute(&mut **transaction)
        .await
        .map_err(|_| OperatorError::internal())?;
        if inserted.rows_affected() != 1 {
            return Err(OperatorError::invalid_request());
        }
    }
    Ok(())
}

pub(crate) async fn load_job_dataset_inputs(
    database: &PgPool,
    job_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<JobDatasetInputResponse>>, OperatorError> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Uuid,
            String,
            Uuid,
            i32,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<Uuid>,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT input.job_id, input.alias, input.dataset_id, dataset.name, \
                input.dataset_version_id, version.version_number, input.source_kind, \
                input.source_repository, input.resolved_revision, \
                input.dataset_manifest_sha256, input.dataset_view_id, view.name, \
                input.dataset_view_manifest_sha256 \
         FROM job_dataset_inputs input \
         JOIN datasets dataset ON dataset.id = input.dataset_id \
         JOIN dataset_versions version ON version.id = input.dataset_version_id \
         LEFT JOIN dataset_views view ON view.id = input.dataset_view_id \
         WHERE input.job_id = ANY($1) ORDER BY input.alias",
    )
    .bind(job_ids)
    .fetch_all(database)
    .await
    .map_err(|_| OperatorError::internal())?;
    let mut by_job = std::collections::HashMap::new();
    for (
        job_id,
        alias,
        dataset_id,
        dataset_name,
        dataset_version_id,
        version_number,
        source_kind,
        source_repository,
        resolved_revision,
        dataset_manifest_sha256,
        dataset_view_id,
        dataset_view_name,
        dataset_view_manifest_sha256,
    ) in rows
    {
        by_job
            .entry(job_id)
            .or_insert_with(Vec::new)
            .push(JobDatasetInputResponse {
                alias,
                dataset_id,
                dataset_name,
                dataset_version_id,
                version_number,
                source_kind,
                source_repository,
                resolved_revision,
                dataset_manifest_sha256,
                dataset_view_id,
                dataset_view_name,
                dataset_view_manifest_sha256,
            });
    }
    Ok(by_job)
}

pub(crate) async fn load_assignment_dataset_inputs(
    database: &PgPool,
    job_id: Uuid,
) -> Result<Vec<JobDatasetInputAssignment>, ApiError> {
    let rows = sqlx::query_as::<_, (String, Uuid, Option<Uuid>, String)>(
        "SELECT alias, dataset_version_id, dataset_view_id, dataset_manifest_sha256 \
         FROM job_dataset_inputs WHERE job_id = $1 ORDER BY alias",
    )
    .bind(job_id)
    .fetch_all(database)
    .await
    .map_err(|error| database_error(&error, "load job dataset inputs"))?;
    Ok(rows
        .into_iter()
        .map(
            |(alias, dataset_version_id, dataset_view_id, manifest_sha256)| {
                JobDatasetInputAssignment {
                    alias,
                    dataset_version_id,
                    dataset_view_id,
                    manifest_sha256,
                }
            },
        )
        .collect())
}

#[utoipa::path(
    get,
    path = "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/dataset-inputs/{alias}",
    tag = "workers",
    security(("bearer_credential" = [])),
    params(
        ("worker_id" = Uuid, Path, description = "Worker identifier"),
        ("attempt_id" = Uuid, Path, description = "Current job attempt"),
        ("alias" = String, Path, description = "Input mount alias")
    ),
    responses(
        (status = 200, description = "Immutable dataset download manifest", body = DatasetInputManifestResponse),
        (status = 401, description = "Credential or attempt rejected", body = ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn read_job_dataset_input(
    State(state): State<AppState>,
    Path((worker_id, attempt_id, alias)): Path<(Uuid, Uuid, String)>,
    headers: HeaderMap,
) -> Result<Json<DatasetInputManifestResponse>, ApiError> {
    if !valid_alias(&alias) {
        return Err(ApiError::invalid_request());
    }
    let credential_id = authenticate_worker(
        &state,
        &headers,
        worker_id,
        "authenticate dataset input download",
    )
    .await?;
    let database = state.database.as_ref().ok_or_else(ApiError::unavailable)?;
    let storage = state
        .artifact_storage
        .as_ref()
        .ok_or_else(ApiError::artifact_storage_unavailable)?;
    let mut transaction = database
        .begin()
        .await
        .map_err(|error| database_error(&error, "begin dataset input download"))?;
    lock_current_worker_authorization(&mut transaction, credential_id, worker_id).await?;
    let identity = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Uuid,
            i32,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<Uuid>,
            Option<String>,
            Option<String>,
            i32,
        ),
    >(
        "SELECT input.dataset_id, dataset.name, input.dataset_version_id, version.version_number, \
                input.source_kind, input.source_repository, input.resolved_revision, \
                input.dataset_manifest_sha256, input.dataset_view_id, view.name, \
                input.dataset_view_manifest_sha256, version.total_episodes \
         FROM job_attempts attempt \
         JOIN jobs job ON job.id = attempt.job_id \
         JOIN job_dataset_inputs input ON input.job_id = job.id AND input.alias = $3 \
         JOIN datasets dataset ON dataset.id = input.dataset_id \
         JOIN dataset_versions version ON version.id = input.dataset_version_id \
         LEFT JOIN dataset_views view ON view.id = input.dataset_view_id \
         WHERE attempt.id = $1 AND attempt.worker_id = $2 \
           AND attempt.status IN ('assigned', 'running') \
           AND job.status IN ('assigned', 'running') \
           AND attempt.lease_expires_at > now()",
    )
    .bind(attempt_id)
    .bind(worker_id)
    .bind(&alias)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "authorize dataset input download"))?
    .ok_or_else(ApiError::unauthorized)?;
    let file_rows = sqlx::query_as::<_, DatasetInputFileRecord>(
        "SELECT logical_path, media_type, byte_length, sha256, storage_bucket, storage_object_key \
         FROM dataset_files WHERE version_id = $1 AND status = 'verified' ORDER BY logical_path",
    )
    .bind(identity.2)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "load dataset input files"))?;
    let included_episodes = if let Some(view_id) = identity.8 {
        sqlx::query_scalar::<_, i32>(
            "SELECT episode_index FROM dataset_view_episodes WHERE view_id = $1 ORDER BY episode_index",
        )
        .bind(view_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| database_error(&error, "load dataset input view"))?
    } else {
        (0..identity.11).collect()
    };
    transaction
        .commit()
        .await
        .map_err(|error| database_error(&error, "commit dataset input download"))?;
    let mut files = Vec::with_capacity(file_rows.len());
    for file in file_rows {
        let download_url = storage
            .signed_read_url(
                &file.storage_bucket,
                &file.storage_object_key,
                DOWNLOAD_SECONDS,
                Utc::now(),
            )
            .await
            .map_err(|_| ApiError::artifact_storage_unavailable())?;
        files.push(DatasetInputFile {
            path: file.logical_path,
            media_type: file.media_type,
            byte_length: u64::try_from(file.byte_length).map_err(|_| ApiError::internal())?,
            sha256: file.sha256,
            download_url,
        });
    }
    Ok(Json(DatasetInputManifestResponse {
        alias,
        dataset_id: identity.0,
        dataset_name: identity.1,
        dataset_version_id: identity.2,
        version_number: identity.3,
        source_kind: identity.4,
        source_repository: identity.5,
        resolved_revision: identity.6,
        manifest_sha256: identity.7,
        dataset_view_id: identity.8,
        dataset_view_name: identity.9,
        dataset_view_manifest_sha256: identity.10,
        included_episodes,
        files,
    }))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::{
        JobDatasetInputRequest, MAX_DATASET_INPUT_FILE_BYTES, insert_job_dataset_inputs,
        load_job_dataset_inputs, valid_alias,
    };
    use crate::projects::DEFAULT_PROJECT_ID;

    #[test]
    fn aliases_are_safe_mount_directory_names() {
        for alias in ["dataset", "training_data", "input-2", "a"] {
            assert!(valid_alias(alias));
        }
        for alias in ["", "Dataset", "2data", "../data", "data/path", "data data"] {
            assert!(!valid_alias(alias));
        }
        assert!(!valid_alias(&"a".repeat(33)));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn job_input_freezes_version_and_view_hashes(pool: PgPool) {
        let owner_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let dataset_id = Uuid::new_v4();
        let version_id = Uuid::new_v4();
        let view_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
             VALUES ($1, 'test', $2, 'Owner')",
        )
        .bind(owner_id)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, owner_identity_id, project_id, name, image_reference, timeout_seconds) \
             VALUES ($1, $2, $3, 'Training', $4, 120)",
        )
        .bind(job_id)
        .bind(owner_id)
        .bind(DEFAULT_PROJECT_ID)
        .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO datasets (id, project_id, name, created_by_identity_id) \
             VALUES ($1, $2, 'SVLA', $3)",
        )
        .bind(dataset_id)
        .bind(DEFAULT_PROJECT_ID)
        .bind(owner_id)
        .execute(&pool)
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
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO dataset_files \
             (id, version_id, project_id, logical_path, media_type, byte_length, sha256, \
              storage_bucket, storage_object_key, storage_generation, status, verified_at) \
             VALUES ($1, $2, $3, 'meta/info.json', 'application/json', 1, $4, \
                     'datasets', 'svla/meta/info.json', 1, 'verified', now())",
        )
        .bind(Uuid::new_v4())
        .bind(version_id)
        .bind(DEFAULT_PROJECT_ID)
        .bind("e".repeat(64))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO dataset_views \
             (id, dataset_id, version_id, project_id, name, manifest_sha256, \
              included_episode_count, created_by_identity_id) \
             VALUES ($1, $2, $3, $4, 'Accepted', $5, 2, $6)",
        )
        .bind(view_id)
        .bind(dataset_id)
        .bind(version_id)
        .bind(DEFAULT_PROJECT_ID)
        .bind("c".repeat(64))
        .bind(owner_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut transaction = pool.begin().await.unwrap();
        insert_job_dataset_inputs(
            &mut transaction,
            job_id,
            DEFAULT_PROJECT_ID,
            &[JobDatasetInputRequest {
                alias: "training_data".to_owned(),
                dataset_version_id: version_id,
                dataset_view_id: Some(view_id),
            }],
        )
        .await
        .unwrap();
        transaction.commit().await.unwrap();

        // The source catalogue is immutable through the API. Even a direct administrative update
        // cannot change the hashes that were captured in the queued job specification.
        sqlx::query("UPDATE dataset_versions SET manifest_sha256 = $2 WHERE id = $1")
            .bind(version_id)
            .bind("d".repeat(64))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE dataset_views SET manifest_sha256 = $2 WHERE id = $1")
            .bind(view_id)
            .bind("d".repeat(64))
            .execute(&pool)
            .await
            .unwrap();
        let input = load_job_dataset_inputs(&pool, &[job_id])
            .await
            .unwrap()
            .remove(&job_id)
            .unwrap()
            .remove(0);
        assert_eq!(input.alias, "training_data");
        assert_eq!(input.dataset_manifest_sha256, "b".repeat(64));
        assert_eq!(input.dataset_view_manifest_sha256, Some("c".repeat(64)));

        // A catalogue version may exist for review or archival even when it is too large for the
        // first worker execution profile. Reject it at job creation so a worker never receives an
        // assignment its strict protocol model cannot parse.
        let oversized_job_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO jobs (id, owner_identity_id, project_id, name, image_reference, timeout_seconds) \
             VALUES ($1, $2, $3, 'Oversized training', $4, 120)",
        )
        .bind(oversized_job_id)
        .bind(owner_id)
        .bind(DEFAULT_PROJECT_ID)
        .bind(format!("example.test/work@sha256:{}", "f".repeat(64)))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE dataset_files SET byte_length = $2 WHERE version_id = $1")
            .bind(version_id)
            .bind(MAX_DATASET_INPUT_FILE_BYTES + 1)
            .execute(&pool)
            .await
            .unwrap();
        let mut transaction = pool.begin().await.unwrap();
        let rejected = insert_job_dataset_inputs(
            &mut transaction,
            oversized_job_id,
            DEFAULT_PROJECT_ID,
            &[JobDatasetInputRequest {
                alias: "training_data".to_owned(),
                dataset_version_id: version_id,
                dataset_view_id: None,
            }],
        )
        .await;
        assert!(rejected.is_err());
    }
}

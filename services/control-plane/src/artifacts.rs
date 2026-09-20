use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::HeaderMap,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    registry::{ApiError, authenticate_worker, validate_protocol},
};

const MAX_FILES: usize = 100;
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobOutputRequirement {
    pub logical_path: String,
    pub role: String,
    pub media_type: String,
    pub mandatory: bool,
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifestFile {
    pub logical_path: String,
    pub byte_length: u64,
    pub sha256: String,
    pub crc32c: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclareArtifactManifestRequest {
    pub protocol_version: String,
    pub manifest_id: Uuid,
    pub files: Vec<ArtifactManifestFile>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BeginArtifactUploadRequest {
    pub protocol_version: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteArtifactUploadRequest {
    pub protocol_version: String,
    pub storage_generation: i64,
    pub byte_length: u64,
    pub sha256: String,
    pub crc32c: String,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ArtifactResponse {
    pub artifact_id: Uuid,
    pub logical_path: String,
    pub role: String,
    pub media_type: String,
    pub mandatory: bool,
    pub byte_length: u64,
    pub sha256: String,
    pub crc32c: String,
    pub object_key: String,
    pub status: String,
    pub storage_generation: Option<i64>,
    pub upload_started_at: Option<DateTime<Utc>>,
    pub upload_completed_at: Option<DateTime<Utc>>,
    pub verification_pending: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactManifestResponse {
    pub manifest_id: Uuid,
    pub attempt_id: Uuid,
    pub artifacts: Vec<ArtifactResponse>,
}

#[derive(FromRow)]
struct AttemptRecord {
    job_id: Uuid,
    owner_identity_id: Uuid,
    status: String,
}

#[derive(FromRow)]
struct RequirementRecord {
    id: Uuid,
    logical_path: String,
    role: String,
    media_type: String,
    mandatory: bool,
    max_bytes: i64,
}

#[derive(FromRow)]
struct ArtifactRecord {
    id: Uuid,
    logical_path: String,
    role: String,
    media_type: String,
    mandatory: bool,
    byte_length: i64,
    sha256: String,
    crc32c: String,
    object_key: String,
    status: String,
    storage_generation: Option<i64>,
    uploaded_byte_length: Option<i64>,
    uploaded_crc32c: Option<String>,
    upload_started_at: Option<DateTime<Utc>>,
    upload_completed_at: Option<DateTime<Utc>>,
}

impl TryFrom<ArtifactRecord> for ArtifactResponse {
    type Error = ApiError;

    fn try_from(record: ArtifactRecord) -> Result<Self, Self::Error> {
        let byte_length = u64::try_from(record.byte_length).map_err(|_| ApiError::internal())?;
        Ok(Self {
            artifact_id: record.id,
            logical_path: record.logical_path,
            role: record.role,
            media_type: record.media_type,
            mandatory: record.mandatory,
            byte_length,
            sha256: record.sha256,
            crc32c: record.crc32c,
            object_key: record.object_key,
            verification_pending: record.status == "uploading"
                && record.upload_completed_at.is_some(),
            status: record.status,
            storage_generation: record.storage_generation,
            upload_started_at: record.upload_started_at,
            upload_completed_at: record.upload_completed_at,
        })
    }
}

pub(crate) fn validate_output_requirements(
    requirements: &[JobOutputRequirement],
) -> Result<(), ApiError> {
    if requirements.len() > MAX_FILES {
        return Err(ApiError::invalid_request());
    }
    let mut paths = HashSet::with_capacity(requirements.len());
    let mut total = 0_u64;
    for requirement in requirements {
        if !valid_logical_path(&requirement.logical_path)
            || !paths.insert(&requirement.logical_path)
            || !valid_role(&requirement.role)
            || !valid_media_type(&requirement.media_type)
            || !(1..=MAX_FILE_BYTES).contains(&requirement.max_bytes)
        {
            return Err(ApiError::invalid_request());
        }
        total = total
            .checked_add(requirement.max_bytes)
            .ok_or_else(ApiError::invalid_request)?;
        if total > MAX_TOTAL_BYTES {
            return Err(ApiError::invalid_request());
        }
    }
    Ok(())
}

fn valid_logical_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 240
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn valid_role(role: &str) -> bool {
    let mut bytes = role.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && role.len() <= 32
        && bytes
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

fn valid_media_type(media_type: &str) -> bool {
    fn valid_part(part: &str) -> bool {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
    }

    media_type.len() <= 127
        && media_type
            .split_once('/')
            .is_some_and(|(kind, subtype)| valid_part(kind) && valid_part(subtype))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_crc32c(value: &str) -> bool {
    STANDARD
        .decode(value)
        .is_ok_and(|decoded| decoded.len() == 4 && STANDARD.encode(decoded) == value)
}

fn validate_artifact_protocol(version: &str) -> Result<(), ApiError> {
    validate_protocol(version)?;
    let minor = version
        .split_once('.')
        .and_then(|(_, minor)| minor.parse::<u32>().ok())
        .ok_or_else(ApiError::invalid_request)?;
    if minor < 1 {
        return Err(ApiError::conflict(
            "artifact_protocol_required",
            "Worker protocol 1.1 or newer is required for artefact transfer.",
        ));
    }
    Ok(())
}

fn database(state: &AppState) -> Result<&sqlx::PgPool, ApiError> {
    state.database.as_ref().ok_or_else(ApiError::unavailable)
}

async fn attempt_for_worker(
    transaction: &mut Transaction<'_, Postgres>,
    attempt_id: Uuid,
    worker_id: Uuid,
) -> Result<AttemptRecord, ApiError> {
    let record = sqlx::query_as::<_, AttemptRecord>(
        "SELECT a.job_id, j.owner_identity_id, a.status \
         FROM job_attempts a JOIN jobs j ON j.id = a.job_id \
         WHERE a.id = $1 AND a.worker_id = $2 FOR UPDATE OF a",
    )
    .bind(attempt_id)
    .bind(worker_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| ApiError::internal())?
    .ok_or_else(|| ApiError::conflict("attempt_unavailable", "The job attempt is unavailable."))?;
    if !matches!(record.status.as_str(), "assigned" | "running") {
        return Err(ApiError::conflict(
            "attempt_not_active",
            "The job attempt no longer accepts artefact changes.",
        ));
    }
    Ok(record)
}

async fn load_artifacts(
    transaction: &mut Transaction<'_, Postgres>,
    manifest_id: Uuid,
) -> Result<Vec<ArtifactRecord>, ApiError> {
    sqlx::query_as::<_, ArtifactRecord>(
        "SELECT id, logical_path, role, media_type, mandatory, byte_length, sha256, crc32c, \
                object_key, status, storage_generation, uploaded_byte_length, uploaded_crc32c, \
                upload_started_at, upload_completed_at \
         FROM job_artifacts WHERE manifest_id = $1 ORDER BY logical_path",
    )
    .bind(manifest_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|_| ApiError::internal())
}

fn stored_manifest_matches(records: &[ArtifactRecord], requested: &[ArtifactManifestFile]) -> bool {
    records.len() == requested.len()
        && records.iter().zip(requested).all(|(stored, supplied)| {
            stored.logical_path == supplied.logical_path
                && u64::try_from(stored.byte_length).ok() == Some(supplied.byte_length)
                && stored.sha256 == supplied.sha256
                && stored.crc32c == supplied.crc32c
        })
}

#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/artifact-manifest",
    tag = "workers",
    security(("bearer_credential" = [])),
    params(
        ("worker_id" = Uuid, Path, description = "Worker identifier"),
        ("attempt_id" = Uuid, Path, description = "Job attempt identifier")
    ),
    request_body = DeclareArtifactManifestRequest,
    responses(
        (status = 200, description = "Manifest accepted or replayed", body = ArtifactManifestResponse),
        (status = 401, description = "Credential rejected", body = crate::registry::ErrorResponse),
        (status = 409, description = "Attempt or manifest conflicts", body = crate::registry::ErrorResponse),
        (status = 422, description = "Manifest is invalid", body = crate::registry::ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn declare_manifest(
    State(state): State<AppState>,
    Path((worker_id, attempt_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<DeclareArtifactManifestRequest>, JsonRejection>,
) -> Result<Json<ArtifactManifestResponse>, ApiError> {
    authenticate_worker(
        &state,
        &headers,
        worker_id,
        "authenticate artefact manifest",
    )
    .await?;
    let Json(mut request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_artifact_protocol(&request.protocol_version)?;
    if request.files.len() > MAX_FILES {
        return Err(ApiError::invalid_request());
    }
    request
        .files
        .sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    let mut paths = HashSet::with_capacity(request.files.len());
    let mut total = 0_u64;
    for file in &request.files {
        if !valid_logical_path(&file.logical_path)
            || !paths.insert(&file.logical_path)
            || file.byte_length > MAX_FILE_BYTES
            || !valid_sha256(&file.sha256)
            || !valid_crc32c(&file.crc32c)
        {
            return Err(ApiError::invalid_request());
        }
        total = total
            .checked_add(file.byte_length)
            .ok_or_else(ApiError::invalid_request)?;
        if total > MAX_TOTAL_BYTES {
            return Err(ApiError::invalid_request());
        }
    }

    let pool = database(&state)?;
    let mut transaction = pool.begin().await.map_err(|_| ApiError::internal())?;
    let attempt = attempt_for_worker(&mut transaction, attempt_id, worker_id).await?;
    let existing_manifest =
        sqlx::query_as::<_, (Uuid,)>("SELECT id FROM job_artifact_manifests WHERE attempt_id = $1")
            .bind(attempt_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| ApiError::internal())?;
    if let Some((manifest_id,)) = existing_manifest {
        if manifest_id != request.manifest_id {
            return Err(ApiError::conflict(
                "manifest_already_declared",
                "This attempt already has an output manifest.",
            ));
        }
        let records = load_artifacts(&mut transaction, manifest_id).await?;
        if !stored_manifest_matches(&records, &request.files) {
            return Err(ApiError::conflict(
                "manifest_replay_mismatch",
                "The manifest identifier was replayed with different content.",
            ));
        }
        transaction
            .commit()
            .await
            .map_err(|_| ApiError::internal())?;
        return Ok(Json(ArtifactManifestResponse {
            manifest_id,
            attempt_id,
            artifacts: records
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        }));
    }

    let requirements = sqlx::query_as::<_, RequirementRecord>(
        "SELECT id, logical_path, role, media_type, mandatory, max_bytes \
         FROM job_output_requirements WHERE job_id = $1 ORDER BY logical_path",
    )
    .bind(attempt.job_id)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|_| ApiError::internal())?;
    let by_path: HashMap<&str, &RequirementRecord> = requirements
        .iter()
        .map(|requirement| (requirement.logical_path.as_str(), requirement))
        .collect();
    if requirements
        .iter()
        .any(|required| required.mandatory && !paths.contains(&required.logical_path))
        || request.files.iter().any(|file| {
            by_path
                .get(file.logical_path.as_str())
                .is_none_or(|requirement| {
                    i64::try_from(file.byte_length)
                        .map_or(true, |length| length > requirement.max_bytes)
                })
        })
    {
        return Err(ApiError::invalid_request());
    }

    sqlx::query("INSERT INTO job_artifact_manifests (id, attempt_id, job_id) VALUES ($1, $2, $3)")
        .bind(request.manifest_id)
        .bind(attempt_id)
        .bind(attempt.job_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApiError::internal())?;
    for file in &request.files {
        let requirement = by_path
            .get(file.logical_path.as_str())
            .ok_or_else(ApiError::invalid_request)?;
        let artifact_id = Uuid::new_v4();
        let object_key = format!(
            "v1/owners/{}/jobs/{}/attempts/{attempt_id}/artifacts/{artifact_id}",
            attempt.owner_identity_id, attempt.job_id
        );
        sqlx::query(
            "INSERT INTO job_artifacts \
             (id, manifest_id, attempt_id, job_id, output_requirement_id, logical_path, role, \
              media_type, mandatory, byte_length, sha256, crc32c, object_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(artifact_id)
        .bind(request.manifest_id)
        .bind(attempt_id)
        .bind(attempt.job_id)
        .bind(requirement.id)
        .bind(&file.logical_path)
        .bind(&requirement.role)
        .bind(&requirement.media_type)
        .bind(requirement.mandatory)
        .bind(i64::try_from(file.byte_length).map_err(|_| ApiError::invalid_request())?)
        .bind(&file.sha256)
        .bind(&file.crc32c)
        .bind(object_key)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApiError::internal())?;
    }
    let records = load_artifacts(&mut transaction, request.manifest_id).await?;
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(ArtifactManifestResponse {
        manifest_id: request.manifest_id,
        attempt_id,
        artifacts: records
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()?,
    }))
}

async fn artifact_for_worker(
    transaction: &mut Transaction<'_, Postgres>,
    attempt_id: Uuid,
    artifact_id: Uuid,
    worker_id: Uuid,
) -> Result<ArtifactRecord, ApiError> {
    sqlx::query_as::<_, ArtifactRecord>(
        "SELECT ar.id, ar.logical_path, ar.role, ar.media_type, ar.mandatory, ar.byte_length, \
                ar.sha256, ar.crc32c, ar.object_key, ar.status, ar.storage_generation, \
                ar.uploaded_byte_length, ar.uploaded_crc32c, ar.upload_started_at, \
                ar.upload_completed_at \
         FROM job_artifacts ar JOIN job_attempts a ON a.id = ar.attempt_id \
         WHERE ar.id = $1 AND ar.attempt_id = $2 AND a.worker_id = $3 FOR UPDATE OF ar",
    )
    .bind(artifact_id)
    .bind(attempt_id)
    .bind(worker_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| ApiError::internal())?
    .ok_or_else(|| ApiError::conflict("artifact_unavailable", "The artefact is unavailable."))
}

#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/artifacts/{artifact_id}/upload",
    tag = "workers",
    security(("bearer_credential" = [])),
    request_body = BeginArtifactUploadRequest,
    responses((status = 200, description = "Upload state started or replayed", body = ArtifactResponse))
)]
pub(crate) async fn begin_upload(
    State(state): State<AppState>,
    Path((worker_id, attempt_id, artifact_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<BeginArtifactUploadRequest>, JsonRejection>,
) -> Result<Json<ArtifactResponse>, ApiError> {
    authenticate_worker(&state, &headers, worker_id, "authenticate artefact upload").await?;
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_artifact_protocol(&request.protocol_version)?;
    let pool = database(&state)?;
    let mut transaction = pool.begin().await.map_err(|_| ApiError::internal())?;
    attempt_for_worker(&mut transaction, attempt_id, worker_id).await?;
    let artifact =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    let artifact = match artifact.status.as_str() {
        "declared" => {
            sqlx::query(
                "UPDATE job_artifacts SET status = 'uploading', upload_started_at = now() \
                 WHERE id = $1",
            )
            .bind(artifact_id)
            .execute(&mut *transaction)
            .await
            .map_err(|_| ApiError::internal())?;
            artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?
        }
        "uploading" | "verified" => artifact,
        _ => {
            return Err(ApiError::conflict(
                "artifact_not_uploadable",
                "The artefact no longer accepts an upload.",
            ));
        }
    };
    let artifact = artifact.try_into()?;
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(artifact))
}

#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/artifacts/{artifact_id}/complete-upload",
    tag = "workers",
    security(("bearer_credential" = [])),
    request_body = CompleteArtifactUploadRequest,
    responses(
        (status = 200, description = "Upload completion recorded idempotently; storage verification remains pending", body = ArtifactResponse),
        (status = 409, description = "Completion conflicts with stored evidence", body = crate::registry::ErrorResponse),
        (status = 422, description = "Completion evidence is invalid", body = crate::registry::ErrorResponse)
    )
)]
pub(crate) async fn complete_upload(
    State(state): State<AppState>,
    Path((worker_id, attempt_id, artifact_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<CompleteArtifactUploadRequest>, JsonRejection>,
) -> Result<Json<ArtifactResponse>, ApiError> {
    authenticate_worker(
        &state,
        &headers,
        worker_id,
        "authenticate artefact completion",
    )
    .await?;
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_artifact_protocol(&request.protocol_version)?;
    if request.storage_generation <= 0
        || !valid_sha256(&request.sha256)
        || !valid_crc32c(&request.crc32c)
    {
        return Err(ApiError::invalid_request());
    }
    let pool = database(&state)?;
    let mut transaction = pool.begin().await.map_err(|_| ApiError::internal())?;
    let artifact =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    if request.byte_length
        != u64::try_from(artifact.byte_length).map_err(|_| ApiError::internal())?
        || request.sha256 != artifact.sha256
        || request.crc32c != artifact.crc32c
    {
        return Err(ApiError::invalid_request());
    }
    if artifact.upload_completed_at.is_some() {
        let matches = artifact.storage_generation == Some(request.storage_generation)
            && artifact.uploaded_byte_length == i64::try_from(request.byte_length).ok()
            && artifact.uploaded_crc32c.as_deref() == Some(request.crc32c.as_str());
        if !matches {
            return Err(ApiError::conflict(
                "upload_completion_mismatch",
                "The upload was already completed with different evidence.",
            ));
        }
        transaction
            .commit()
            .await
            .map_err(|_| ApiError::internal())?;
        return Ok(Json(artifact.try_into()?));
    }
    attempt_for_worker(&mut transaction, attempt_id, worker_id).await?;
    if artifact.status != "uploading" {
        return Err(ApiError::conflict(
            "artifact_not_uploading",
            "The artefact upload has not been started.",
        ));
    }
    sqlx::query(
        "UPDATE job_artifacts SET storage_generation = $2, uploaded_byte_length = $3, \
                uploaded_crc32c = $4, upload_completed_at = now() WHERE id = $1",
    )
    .bind(artifact_id)
    .bind(request.storage_generation)
    .bind(i64::try_from(request.byte_length).map_err(|_| ApiError::invalid_request())?)
    .bind(&request.crc32c)
    .execute(&mut *transaction)
    .await
    .map_err(|_| ApiError::internal())?;
    let completed =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(completed.try_into()?))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::AUTHORIZATION},
    };
    use chrono::{TimeDelta, Utc};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::{
        app,
        credentials::{self, CredentialKind},
    };

    use super::{
        JobOutputRequirement, valid_crc32c, valid_logical_path, validate_output_requirements,
    };

    struct Fixture {
        worker_id: Uuid,
        attempt_id: Uuid,
        credential: String,
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        let owner_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let attempt_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
             VALUES ($1, 'test', $2, 'Owner')",
        )
        .bind(owner_id)
        .bind(Uuid::new_v4().to_string())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities) \
             VALUES ($1, $2, $3, 'GPU worker', '1.1', 'busy', '{}'::jsonb)",
        )
        .bind(worker_id)
        .bind(owner_id)
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
        let credential = credentials::issue(CredentialKind::Worker).unwrap();
        sqlx::query(
            "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, $3)",
        )
        .bind(credential.id)
        .bind(worker_id)
        .bind(&credential.verifier)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO jobs \
             (id, owner_identity_id, name, image_reference, timeout_seconds, status, assigned_worker_id) \
             VALUES ($1, $2, 'Training', $3, 120, 'assigned', $4)",
        )
        .bind(job_id)
        .bind(owner_id)
        .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
        .bind(worker_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, lease_expires_at) \
             VALUES ($1, $2, 1, $3, $4)",
        )
        .bind(attempt_id)
        .bind(job_id)
        .bind(worker_id)
        .bind(Utc::now() + TimeDelta::minutes(10))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_output_requirements \
             (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
             VALUES ($1, $2, 'model.pt', 'checkpoint', 'application/octet-stream', true, 1024)",
        )
        .bind(Uuid::new_v4())
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
        Fixture {
            worker_id,
            attempt_id,
            credential: credential.plaintext.into_string(),
        }
    }

    async fn response_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn manifest_body(manifest_id: Uuid) -> Value {
        json!({
            "protocol_version": "1.1",
            "manifest_id": manifest_id,
            "files": [{
                "logical_path": "model.pt",
                "byte_length": 512,
                "sha256": "b".repeat(64),
                "crc32c": "ImIEBA=="
            }]
        })
    }

    #[test]
    fn paths_cannot_escape_the_output_root() {
        assert!(valid_logical_path("models/checkpoint.pt"));
        for path in ["", "/root", "../secret", "models/../secret", "a//b", "a\\b"] {
            assert!(!valid_logical_path(path), "accepted {path}");
        }
    }

    #[test]
    fn crc32c_is_canonical_base64_for_four_bytes() {
        assert!(valid_crc32c("ImIEBA=="));
        assert!(!valid_crc32c("not-a-checksum"));
        assert!(!valid_crc32c("aGVsbG8="));
    }

    #[test]
    fn duplicate_contract_paths_are_rejected() {
        let output = JobOutputRequirement {
            logical_path: "model.pt".to_owned(),
            role: "checkpoint".to_owned(),
            media_type: "application/octet-stream".to_owned(),
            mandatory: true,
            max_bytes: 1024,
        };
        assert!(validate_output_requirements(std::slice::from_ref(&output)).is_ok());
        assert!(validate_output_requirements(&[output.clone(), output]).is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn manifest_declaration_is_immutable_and_idempotent(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let router = app(None, Some(pool.clone()));
        let manifest_id = Uuid::new_v4();
        let uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
            fixture.worker_id, fixture.attempt_id
        );
        let mut first_body = None;
        for _ in 0..2 {
            let response = router
                .clone()
                .oneshot(
                    Request::put(&uri)
                        .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                        .header("content-type", "application/json")
                        .body(Body::from(manifest_body(manifest_id).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response_json(response).await;
            if let Some(first) = &first_body {
                assert_eq!(&body, first);
            } else {
                first_body = Some(body);
            }
        }

        let mut changed = manifest_body(manifest_id);
        changed["files"][0]["sha256"] = Value::String("c".repeat(64));
        let response = router
            .oneshot(
                Request::put(&uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(changed.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM job_artifacts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn database_rejects_cross_job_artifact_links(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let manifest_id = Uuid::new_v4();
        let uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
            fixture.worker_id, fixture.attempt_id
        );
        let declared = app(None, Some(pool.clone()))
            .oneshot(
                Request::put(uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(manifest_body(manifest_id).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(declared.status(), StatusCode::OK);

        let (first_job_id, owner_id, first_requirement_id): (Uuid, Uuid, Uuid) = sqlx::query_as(
            "SELECT a.job_id, j.owner_identity_id, r.id \
             FROM job_attempts a JOIN jobs j ON j.id = a.job_id \
             JOIN job_output_requirements r ON r.job_id = j.id WHERE a.id = $1",
        )
        .bind(fixture.attempt_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let second_job_id = Uuid::new_v4();
        let second_attempt_id = Uuid::new_v4();
        let second_requirement_id = Uuid::new_v4();
        let second_manifest_id = Uuid::new_v4();
        sqlx::query("UPDATE job_attempts SET status = 'succeeded' WHERE id = $1")
            .bind(fixture.attempt_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO jobs \
             (id, owner_identity_id, name, image_reference, timeout_seconds, status, assigned_worker_id) \
             VALUES ($1, $2, 'Second training', $3, 120, 'assigned', $4)",
        )
        .bind(second_job_id)
        .bind(owner_id)
        .bind(format!("example.test/work@sha256:{}", "c".repeat(64)))
        .bind(fixture.worker_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, lease_expires_at) \
             VALUES ($1, $2, 1, $3, $4)",
        )
        .bind(second_attempt_id)
        .bind(second_job_id)
        .bind(fixture.worker_id)
        .bind(Utc::now() + TimeDelta::minutes(10))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_output_requirements \
             (id, job_id, logical_path, role, media_type, mandatory, max_bytes) \
             VALUES ($1, $2, 'other.pt', 'checkpoint', 'application/octet-stream', true, 1024)",
        )
        .bind(second_requirement_id)
        .bind(second_job_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO job_artifact_manifests (id, attempt_id, job_id) VALUES ($1, $2, $3)",
        )
        .bind(second_manifest_id)
        .bind(second_attempt_id)
        .bind(second_job_id)
        .execute(&pool)
        .await
        .unwrap();

        let cross_manifest = sqlx::query(
            "INSERT INTO job_artifacts \
             (id, manifest_id, attempt_id, job_id, output_requirement_id, logical_path, role, \
              media_type, mandatory, byte_length, sha256, crc32c, object_key) \
             VALUES ($1, $2, $3, $4, $5, 'model.pt', 'checkpoint', \
                     'application/octet-stream', true, 512, $6, 'ImIEBA==', $7)",
        )
        .bind(Uuid::new_v4())
        .bind(second_manifest_id)
        .bind(fixture.attempt_id)
        .bind(first_job_id)
        .bind(first_requirement_id)
        .bind("d".repeat(64))
        .bind(format!("cross-manifest-{}", Uuid::new_v4()))
        .execute(&pool)
        .await;
        assert!(cross_manifest.is_err());

        let cross_requirement = sqlx::query(
            "INSERT INTO job_artifacts \
             (id, manifest_id, attempt_id, job_id, output_requirement_id, logical_path, role, \
              media_type, mandatory, byte_length, sha256, crc32c, object_key) \
             VALUES ($1, $2, $3, $4, $5, 'other.pt', 'checkpoint', \
                     'application/octet-stream', true, 512, $6, 'ImIEBA==', $7)",
        )
        .bind(Uuid::new_v4())
        .bind(manifest_id)
        .bind(fixture.attempt_id)
        .bind(first_job_id)
        .bind(second_requirement_id)
        .bind("e".repeat(64))
        .bind(format!("cross-requirement-{}", Uuid::new_v4()))
        .execute(&pool)
        .await;
        assert!(cross_requirement.is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upload_completion_replays_and_does_not_claim_verification(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let router = app(None, Some(pool.clone()));
        let manifest_uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
            fixture.worker_id, fixture.attempt_id
        );
        let declared = router
            .clone()
            .oneshot(
                Request::put(&manifest_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(manifest_body(Uuid::new_v4()).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(declared.status(), StatusCode::OK);
        let declared = response_json(declared).await;
        let artifact_id =
            Uuid::parse_str(declared["artifacts"][0]["artifact_id"].as_str().unwrap()).unwrap();
        let upload_uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifacts/{artifact_id}/upload",
            fixture.worker_id, fixture.attempt_id
        );
        let started = router
            .clone()
            .oneshot(
                Request::put(&upload_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "protocol_version": "1.1" }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::OK);

        let completion_uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifacts/{artifact_id}/complete-upload",
            fixture.worker_id, fixture.attempt_id
        );
        let completion = json!({
            "protocol_version": "1.1",
            "storage_generation": 42,
            "byte_length": 512,
            "sha256": "b".repeat(64),
            "crc32c": "ImIEBA=="
        });
        let mut first_body = None;
        for _ in 0..2 {
            let response = router
                .clone()
                .oneshot(
                    Request::put(&completion_uri)
                        .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                        .header("content-type", "application/json")
                        .body(Body::from(completion.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response_json(response).await;
            assert_eq!(body["status"], "uploading");
            assert_eq!(body["verification_pending"], true);
            if let Some(first) = &first_body {
                assert_eq!(&body, first);
            } else {
                first_body = Some(body);
            }
        }

        let mut conflicting = completion.clone();
        conflicting["storage_generation"] = json!(43);
        let response = router
            .clone()
            .oneshot(
                Request::put(&completion_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(conflicting.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let result_uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/result",
            fixture.worker_id, fixture.attempt_id
        );
        let result = json!({
            "exit_code": 0,
            "timed_out": false,
            "stdout": "",
            "stderr": "",
            "failure_message": null
        });
        let blocked = router
            .clone()
            .oneshot(
                Request::put(&result_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(result.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::CONFLICT);
        let blocked = response_json(blocked).await;
        assert_eq!(blocked["code"], "artifacts_incomplete");

        let unverifiable = sqlx::query(
            "UPDATE job_artifacts SET status = 'verified', verified_at = now() WHERE id = $1",
        )
        .bind(artifact_id)
        .execute(&pool)
        .await;
        assert!(unverifiable.is_err());
        sqlx::query(
            "UPDATE job_artifacts SET status = 'verified', verified_at = now(), \
                    verified_storage_generation = storage_generation, \
                    verified_byte_length = uploaded_byte_length, \
                    verified_crc32c = uploaded_crc32c, verification_source = 'gcs_metadata' \
             WHERE id = $1",
        )
        .bind(artifact_id)
        .execute(&pool)
        .await
        .unwrap();
        let accepted = router
            .clone()
            .oneshot(
                Request::put(result_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(result.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let accepted = response_json(accepted).await;
        assert_eq!(accepted["status"], "succeeded");

        let replayed_after_terminal = router
            .oneshot(
                Request::put(&completion_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(completion.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replayed_after_terminal.status(), StatusCode::OK);
        let replayed_after_terminal = response_json(replayed_after_terminal).await;
        assert_eq!(replayed_after_terminal["status"], "verified");
    }
}

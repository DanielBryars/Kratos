use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, header},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    artifact_storage::{ArtifactStorageError, ResumableUploadSession},
    registry::{
        ApiError, authenticate_worker, lock_current_worker_authorization, validate_protocol,
    },
};

const MAX_FILES: usize = 100;
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MIN_DELIVERY_SECONDS: i64 = 15 * 60;
const MAX_DELIVERY_SECONDS: i64 = 24 * 60 * 60;
const ASSUMED_MIN_UPLOAD_BYTES_PER_SECOND: u64 = 128 * 1024;
const VERIFICATION_ALLOWANCE_SECONDS: u64 = 5 * 60;

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
pub struct AbandonArtifactUploadRequest {
    /// Worker protocol version. This operation was introduced in protocol 1.1.
    pub protocol_version: String,
    /// SHA-256 of the exact UTF-8 session URI bytes, encoded as 64 lowercase hexadecimal digits.
    pub session_uri_sha256: String,
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

#[derive(Debug, Serialize, ToSchema)]
pub struct BeginArtifactUploadResponse {
    pub artifact: ArtifactResponse,
    pub session: ResumableUploadSession,
}

#[derive(FromRow)]
struct AttemptRecord {
    job_id: Uuid,
    owner_identity_id: Uuid,
    status: String,
    job_status: String,
    lease_expires_at: DateTime<Utc>,
    artifact_delivery_expires_at: Option<DateTime<Utc>>,
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
    storage_bucket: Option<String>,
    status: String,
    storage_generation: Option<i64>,
    uploaded_byte_length: Option<i64>,
    uploaded_crc32c: Option<String>,
    verified_storage_generation: Option<i64>,
    verified_byte_length: Option<i64>,
    verified_crc32c: Option<String>,
    verified_sha256: Option<String>,
    protection_pending: bool,
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
            verification_pending: record.protection_pending
                || (record.status == "uploading" && record.upload_completed_at.is_some()),
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
        "SELECT a.job_id, j.owner_identity_id, a.status, j.status AS job_status, \
                a.lease_expires_at, a.artifact_delivery_expires_at \
         FROM job_attempts a JOIN jobs j ON j.id = a.job_id \
         WHERE a.id = $1 AND a.worker_id = $2 FOR UPDATE OF a, j",
    )
    .bind(attempt_id)
    .bind(worker_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| ApiError::internal())?
    .ok_or_else(|| ApiError::conflict("attempt_unavailable", "The job attempt is unavailable."))?;
    if !matches!(record.status.as_str(), "assigned" | "running")
        || !matches!(record.job_status.as_str(), "assigned" | "running")
        || record
            .artifact_delivery_expires_at
            .unwrap_or(record.lease_expires_at)
            <= Utc::now()
    {
        return Err(ApiError::conflict(
            "attempt_not_active",
            "The job attempt no longer accepts artefact changes.",
        ));
    }
    Ok(record)
}

pub(crate) fn delivery_window(total_bytes: u64) -> TimeDelta {
    let transfer_seconds =
        total_bytes.div_ceil(ASSUMED_MIN_UPLOAD_BYTES_PER_SECOND) + VERIFICATION_ALLOWANCE_SECONDS;
    let seconds = i64::try_from(transfer_seconds)
        .unwrap_or(MAX_DELIVERY_SECONDS)
        .clamp(MIN_DELIVERY_SECONDS, MAX_DELIVERY_SECONDS);
    TimeDelta::seconds(seconds)
}

async fn load_artifacts(
    transaction: &mut Transaction<'_, Postgres>,
    manifest_id: Uuid,
) -> Result<Vec<ArtifactRecord>, ApiError> {
    sqlx::query_as::<_, ArtifactRecord>(
        "SELECT id, logical_path, role, media_type, mandatory, byte_length, sha256, crc32c, \
                object_key, status, storage_generation, uploaded_byte_length, uploaded_crc32c, \
                upload_started_at, upload_completed_at, storage_bucket, verified_storage_generation, \
                verified_byte_length, verified_crc32c, verified_sha256, protection_pending \
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
    let proposed_delivery_deadline = Utc::now()
        .checked_add_signed(delivery_window(total))
        .ok_or_else(ApiError::internal)?;
    let delivery_deadline = proposed_delivery_deadline.max(attempt.lease_expires_at);
    sqlx::query(
        "UPDATE job_attempts SET artifact_delivery_expires_at = $2 \
         WHERE id = $1 AND artifact_delivery_expires_at IS NULL",
    )
    .bind(attempt_id)
    .bind(delivery_deadline)
    .execute(&mut *transaction)
    .await
    .map_err(|_| ApiError::internal())?;
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
                ar.upload_completed_at, ar.storage_bucket, ar.verified_storage_generation, \
                ar.verified_byte_length, ar.verified_crc32c, ar.verified_sha256, ar.protection_pending \
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
    responses(
        (status = 200, description = "One control-plane-created resumable upload session returned", body = BeginArtifactUploadResponse),
        (status = 503, description = "Artifact storage is unavailable", body = crate::registry::ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn begin_upload(
    State(state): State<AppState>,
    Path((worker_id, attempt_id, artifact_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<BeginArtifactUploadRequest>, JsonRejection>,
) -> Result<(HeaderMap, Json<BeginArtifactUploadResponse>), ApiError> {
    let credential_id =
        authenticate_worker(&state, &headers, worker_id, "authenticate artefact upload").await?;
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_artifact_protocol(&request.protocol_version)?;
    let storage = state
        .artifact_storage
        .clone()
        .ok_or_else(ApiError::artifact_storage_unavailable)?;
    let pool = database(&state)?;
    let mut transaction = pool.begin().await.map_err(|_| ApiError::internal())?;
    lock_current_worker_authorization(&mut transaction, credential_id, worker_id).await?;
    attempt_for_worker(&mut transaction, attempt_id, worker_id).await?;
    let artifact =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    if !matches!(artifact.status.as_str(), "declared" | "uploading")
        || artifact.upload_completed_at.is_some()
    {
        return Err(ApiError::conflict(
            "artifact_not_uploadable",
            "The artefact no longer accepts an upload.",
        ));
    }
    let object_key = artifact.object_key.clone();
    let media_type = artifact.media_type.clone();
    let byte_length = u64::try_from(artifact.byte_length).map_err(|_| ApiError::internal())?;
    let sha256 = artifact.sha256.clone();
    let issued_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .ok_or_else(ApiError::internal)?;
    let existing = sqlx::query_as::<_, (String, Uuid, Option<String>, DateTime<Utc>, Option<String>)>(
        "SELECT state, initiation_id, session_uri, expires_at, cancellation_reason FROM artifact_upload_grants \
         WHERE artifact_id = $1 FOR UPDATE",
    )
    .bind(artifact_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| ApiError::internal())?;
    if let Some((grant_state, _, session_uri, expires_at, cancellation_reason)) = &existing {
        if grant_state == "active" && *expires_at > issued_at {
            let session = ResumableUploadSession {
                uri: session_uri.clone().ok_or_else(ApiError::internal)?,
                method: "PUT".to_owned(),
                expires_at: *expires_at,
            };
            transaction
                .commit()
                .await
                .map_err(|_| ApiError::internal())?;
            return upload_session_response(artifact, session);
        }
        if grant_state == "active" {
            let session_uri_sha256 = session_uri
                .as_deref()
                .map(session_uri_fingerprint)
                .ok_or_else(ApiError::internal)?;
            sqlx::query(
                "UPDATE artifact_upload_grants SET state = 'cancel_pending', \
                        session_uri_sha256 = $2, cancel_requested_at = now(), \
                        cancellation_reason = 'session_expired' \
                 WHERE artifact_id = $1 AND state = 'active'",
            )
            .bind(artifact_id)
            .bind(session_uri_sha256)
            .execute(&mut *transaction)
            .await
            .map_err(|_| ApiError::internal())?;
            transaction
                .commit()
                .await
                .map_err(|_| ApiError::internal())?;
            let _ = reconcile_pending_session_cancellations(pool, &storage).await;
            return Err(ApiError::artifact_storage_unavailable());
        }
        if grant_state == "initiating" && *expires_at > issued_at {
            return Err(ApiError::artifact_storage_unavailable());
        }
        if grant_state == "cancel_pending" {
            transaction
                .commit()
                .await
                .map_err(|_| ApiError::internal())?;
            let _ = reconcile_pending_session_cancellations(pool, &storage).await;
            return Err(ApiError::artifact_storage_unavailable());
        }
        if grant_state == "cancelled"
            && !matches!(
                cancellation_reason.as_deref(),
                Some("session_expired" | "worker_reported_session_unusable")
            )
        {
            return Err(ApiError::conflict(
                "artifact_not_uploadable",
                "The artefact upload session was cancelled.",
            ));
        }
    }
    let initiation_id = Uuid::new_v4();
    let initiation_deadline = issued_at + chrono::TimeDelta::minutes(15);
    if existing.is_some() {
        sqlx::query(
            "UPDATE artifact_upload_grants SET state = 'initiating', initiation_id = $2, \
                    initiation_attempts = initiation_attempts + 1, session_uri = NULL, \
                    session_uri_sha256 = NULL, \
                    issued_at = $3, expires_at = $4, activated_at = NULL, \
                    cancel_requested_at = NULL, cancelled_at = NULL, cancellation_reason = NULL \
             WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .bind(initiation_id)
        .bind(issued_at)
        .bind(initiation_deadline)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApiError::internal())?;
    } else {
        sqlx::query(
            "INSERT INTO artifact_upload_grants \
             (id, artifact_id, worker_id, bucket_name, object_key, state, initiation_id, \
              issued_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5, 'initiating', $6, $7, $8)",
        )
        .bind(Uuid::new_v4())
        .bind(artifact_id)
        .bind(worker_id)
        .bind(storage.bucket())
        .bind(&object_key)
        .bind(initiation_id)
        .bind(issued_at)
        .bind(initiation_deadline)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApiError::internal())?;
    }
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;

    let session = storage
        .initiate_resumable_upload(&object_key, &media_type, byte_length, &sha256, issued_at)
        .await
        .map_err(|_| ApiError::artifact_storage_unavailable())?;

    let mut transaction = pool.begin().await.map_err(|_| ApiError::internal())?;
    let authorization =
        match lock_current_worker_authorization(&mut transaction, credential_id, worker_id).await {
            Ok(()) => attempt_for_worker(&mut transaction, attempt_id, worker_id)
                .await
                .map(|_| ()),
            Err(error) => Err(error),
        };
    if let Err(error) = authorization {
        mark_initiated_session_for_cancellation(
            &mut transaction,
            artifact_id,
            initiation_id,
            &session,
            "authority_ended_during_initiation",
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|_| ApiError::internal())?;
        let _ = storage.cancel_resumable_upload(&session.uri).await;
        let _ = reconcile_pending_session_cancellations(pool, &storage).await;
        return Err(error);
    }
    artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    let activated = sqlx::query(
        "UPDATE artifact_upload_grants SET state = 'active', session_uri = $3, \
                session_uri_sha256 = $4, expires_at = $5, activated_at = now() \
         WHERE artifact_id = $1 AND state = 'initiating' AND initiation_id = $2",
    )
    .bind(artifact_id)
    .bind(initiation_id)
    .bind(&session.uri)
    .bind(session_uri_fingerprint(&session.uri))
    .bind(session.expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(|_| ApiError::internal())?
    .rows_affected();
    if activated != 1 {
        transaction
            .rollback()
            .await
            .map_err(|_| ApiError::internal())?;
        let _ = storage.cancel_resumable_upload(&session.uri).await;
        return Err(ApiError::conflict(
            "upload_initiation_superseded",
            "The upload initiation was superseded.",
        ));
    }
    sqlx::query(
        "UPDATE job_artifacts SET status = 'uploading', storage_bucket = $2, \
                upload_started_at = COALESCE(upload_started_at, $3) WHERE id = $1",
    )
    .bind(artifact_id)
    .bind(storage.bucket())
    .bind(issued_at)
    .execute(&mut *transaction)
    .await
    .map_err(|_| ApiError::internal())?;
    let artifact =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;
    upload_session_response(artifact, session)
}

#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/artifacts/{artifact_id}/abandon-upload",
    tag = "workers",
    security(("bearer_credential" = [])),
    request_body = AbandonArtifactUploadRequest,
    responses(
        (status = 204, description = "The matching unusable upload session was abandoned"),
        (status = 409, description = "The session is unavailable or has already been replaced", body = crate::registry::ErrorResponse),
        (status = 503, description = "Artifact storage cancellation is pending", body = crate::registry::ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn abandon_upload(
    State(state): State<AppState>,
    Path((worker_id, attempt_id, artifact_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<AbandonArtifactUploadRequest>, JsonRejection>,
) -> Result<axum::http::StatusCode, ApiError> {
    let credential_id =
        authenticate_worker(&state, &headers, worker_id, "abandon artefact upload").await?;
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_artifact_protocol(&request.protocol_version)?;
    if request.session_uri_sha256.len() != 64
        || !request
            .session_uri_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ApiError::invalid_request());
    }
    let storage = state
        .artifact_storage
        .clone()
        .ok_or_else(ApiError::artifact_storage_unavailable)?;
    let pool = database(&state)?;
    let mut transaction = pool.begin().await.map_err(|_| ApiError::internal())?;
    lock_current_worker_authorization(&mut transaction, credential_id, worker_id).await?;
    attempt_for_worker(&mut transaction, attempt_id, worker_id).await?;
    let artifact =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    if !matches!(artifact.status.as_str(), "declared" | "uploading")
        || artifact.upload_completed_at.is_some()
    {
        return Err(ApiError::conflict(
            "artifact_not_uploadable",
            "The artefact no longer accepts an upload.",
        ));
    }
    let grant = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
        "SELECT state, session_uri, session_uri_sha256, cancellation_reason FROM artifact_upload_grants \
         WHERE artifact_id = $1 FOR UPDATE",
    )
    .bind(artifact_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| ApiError::internal())?
    .ok_or_else(|| {
        ApiError::conflict(
            "upload_session_unavailable",
            "The upload session is unavailable.",
        )
    })?;
    match grant {
        (state, Some(session_uri), stored_fingerprint, _)
            if matches!(state.as_str(), "active" | "cancel_pending") =>
        {
            let fingerprint =
                stored_fingerprint.unwrap_or_else(|| session_uri_fingerprint(&session_uri));
            if fingerprint != request.session_uri_sha256 {
                return Err(ApiError::conflict(
                    "upload_session_changed",
                    "The upload session has already changed.",
                ));
            }
            if state == "active" {
                sqlx::query(
                    "UPDATE artifact_upload_grants SET state = 'cancel_pending', \
                            session_uri_sha256 = $3, cancel_requested_at = now(), \
                            cancellation_reason = 'worker_reported_session_unusable' \
                     WHERE artifact_id = $1 AND state = 'active' AND session_uri = $2",
                )
                .bind(artifact_id)
                .bind(&session_uri)
                .bind(&request.session_uri_sha256)
                .execute(&mut *transaction)
                .await
                .map_err(|_| ApiError::internal())?;
            }
        }
        (state, None, Some(fingerprint), Some(reason))
            if state == "cancelled"
                && reason == "worker_reported_session_unusable"
                && fingerprint == request.session_uri_sha256 => {}
        (state, None, _, Some(reason))
            if state == "cancelled" && reason == "worker_reported_session_unusable" =>
        {
            return Err(ApiError::conflict(
                "upload_session_changed",
                "The upload session has already changed.",
            ));
        }
        _ => {
            return Err(ApiError::conflict(
                "upload_session_unavailable",
                "The upload session is unavailable.",
            ));
        }
    }
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;
    let _ = reconcile_pending_session_cancellations(pool, &storage).await;
    let still_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM artifact_upload_grants \
         WHERE artifact_id = $1 AND state = 'cancel_pending')",
    )
    .bind(artifact_id)
    .fetch_one(pool)
    .await
    .map_err(|_| ApiError::internal())?;
    if still_pending {
        Err(ApiError::artifact_storage_unavailable())
    } else {
        Ok(axum::http::StatusCode::NO_CONTENT)
    }
}

fn session_uri_fingerprint(session_uri: &str) -> String {
    format!("{:x}", Sha256::digest(session_uri.as_bytes()))
}

fn upload_session_response(
    artifact: ArtifactRecord,
    session: ResumableUploadSession,
) -> Result<(HeaderMap, Json<BeginArtifactUploadResponse>), ApiError> {
    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response_headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    Ok((
        response_headers,
        Json(BeginArtifactUploadResponse {
            artifact: artifact.try_into()?,
            session,
        }),
    ))
}

async fn mark_initiated_session_for_cancellation(
    transaction: &mut Transaction<'_, Postgres>,
    artifact_id: Uuid,
    initiation_id: Uuid,
    session: &ResumableUploadSession,
    reason: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE artifact_upload_grants SET state = 'cancel_pending', session_uri = $3, \
                session_uri_sha256 = $4, expires_at = $5, cancel_requested_at = now(), \
                cancellation_reason = $6 \
         WHERE artifact_id = $1 AND state = 'initiating' AND initiation_id = $2",
    )
    .bind(artifact_id)
    .bind(initiation_id)
    .bind(&session.uri)
    .bind(session_uri_fingerprint(&session.uri))
    .bind(session.expires_at)
    .bind(reason)
    .execute(&mut **transaction)
    .await
    .map_err(|_| ApiError::internal())?;
    Ok(())
}

#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/job-attempts/{attempt_id}/artifacts/{artifact_id}/complete-upload",
    tag = "workers",
    security(("bearer_credential" = [])),
    request_body = CompleteArtifactUploadRequest,
    responses(
        (status = 200, description = "Upload metadata independently verified and artifact finalized", body = ArtifactResponse),
        (status = 409, description = "Completion conflicts with stored evidence", body = crate::registry::ErrorResponse),
        (status = 422, description = "Completion evidence is invalid", body = crate::registry::ErrorResponse),
        (status = 503, description = "Artifact storage verification is unavailable", body = crate::registry::ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
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
    let storage = state
        .artifact_storage
        .clone()
        .ok_or_else(ApiError::artifact_storage_unavailable)?;
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
        if artifact.status == "verified" {
            consume_completed_upload_session(&mut transaction, artifact_id)
                .await
                .map_err(|_| ApiError::internal())?;
            transaction
                .commit()
                .await
                .map_err(|_| ApiError::internal())?;
            return Ok(Json(artifact.try_into()?));
        }
        if artifact.status != "uploading" {
            return Err(ApiError::conflict(
                "artifact_verification_failed",
                "The stored object did not match the declared artefact.",
            ));
        }
    } else {
        attempt_for_worker(&mut transaction, attempt_id, worker_id).await?;
    }
    if artifact.status != "uploading" || artifact.storage_bucket.is_none() {
        return Err(ApiError::conflict(
            "artifact_not_uploading",
            "The artefact upload has not been started.",
        ));
    }
    if artifact.upload_completed_at.is_none() {
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
    }
    let bucket = artifact
        .storage_bucket
        .clone()
        .ok_or_else(ApiError::internal)?;
    let object_key = artifact.object_key.clone();
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;

    let observed = storage
        .object_metadata(&bucket, &object_key, request.storage_generation)
        .await
        .map_err(|error| match error {
            ArtifactStorageError::Unavailable
            | ArtifactStorageError::NotFound
            | ArtifactStorageError::InvalidResponse => ApiError::artifact_storage_unavailable(),
        })?;
    let expected_length =
        i64::try_from(request.byte_length).map_err(|_| ApiError::invalid_request())?;
    let verified = observed.bucket == bucket
        && observed.object_key == object_key
        && observed.generation == request.storage_generation
        && observed.byte_length == expected_length
        && observed.crc32c == request.crc32c
        && observed.sha256 == request.sha256;

    let mut transaction = pool.begin().await.map_err(|_| ApiError::internal())?;
    let artifact =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    let evidence_unchanged = artifact.storage_bucket.as_deref() == Some(bucket.as_str())
        && artifact.object_key == object_key
        && artifact.storage_generation == Some(request.storage_generation)
        && artifact.uploaded_byte_length == Some(expected_length)
        && artifact.uploaded_crc32c.as_deref() == Some(request.crc32c.as_str());
    if !evidence_unchanged {
        return Err(ApiError::conflict(
            "upload_completion_mismatch",
            "The upload was already completed with different evidence.",
        ));
    }
    if artifact.status == "verified" {
        consume_completed_upload_session(&mut transaction, artifact_id)
            .await
            .map_err(|_| ApiError::internal())?;
        transaction
            .commit()
            .await
            .map_err(|_| ApiError::internal())?;
        return Ok(Json(artifact.try_into()?));
    }
    if artifact.status != "uploading" {
        return Err(ApiError::conflict(
            "artifact_verification_failed",
            "The stored object did not match the declared artefact.",
        ));
    }
    if !verified {
        sqlx::query(
            "UPDATE job_artifacts SET status = 'rejected', rejected_at = now(), \
                    state_reason = 'gcs_metadata_mismatch', protection_pending = false, \
                    verified_storage_generation = NULL, verified_byte_length = NULL, \
                    verified_crc32c = NULL, verified_sha256 = NULL, verification_source = NULL, \
                    verified_at = NULL WHERE id = $1",
        )
        .bind(artifact_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApiError::internal())?;
        cancel_artifact_upload_session(&mut transaction, artifact_id, "artifact_metadata_rejected")
            .await
            .map_err(|_| ApiError::internal())?;
        transaction
            .commit()
            .await
            .map_err(|_| ApiError::internal())?;
        let _ = storage
            .delete_object(&bucket, &object_key, request.storage_generation)
            .await;
        reconcile_pending_session_cancellations(pool, &storage).await;
        return Err(ApiError::conflict(
            "artifact_verification_failed",
            "The stored object did not match the declared artefact.",
        ));
    }
    let proof_matches = artifact.verified_storage_generation == Some(observed.generation)
        && artifact.verified_byte_length == Some(observed.byte_length)
        && artifact.verified_crc32c.as_deref() == Some(observed.crc32c.as_str())
        && artifact.verified_sha256.as_deref() == Some(observed.sha256.as_str());
    if artifact.protection_pending && !proof_matches {
        return Err(ApiError::conflict(
            "upload_completion_mismatch",
            "The upload was already verified with different evidence.",
        ));
    }
    sqlx::query(
        "UPDATE job_artifacts SET verified_storage_generation = $2, \
                verified_byte_length = $3, verified_crc32c = $4, verified_sha256 = $5, \
                verification_source = 'gcs_metadata', verified_at = COALESCE(verified_at, now()), \
                protection_pending = true, state_reason = 'gcs_protection_pending' \
         WHERE id = $1",
    )
    .bind(artifact_id)
    .bind(observed.generation)
    .bind(observed.byte_length)
    .bind(&observed.crc32c)
    .bind(&observed.sha256)
    .execute(&mut *transaction)
    .await
    .map_err(|_| ApiError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;

    storage
        .protect_verified_object(&bucket, &object_key, request.storage_generation)
        .await
        .map_err(|_| ApiError::artifact_storage_unavailable())?;

    let mut transaction = pool.begin().await.map_err(|_| ApiError::internal())?;
    let artifact =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    if artifact.status != "verified" {
        if !artifact.protection_pending
            || artifact.verified_storage_generation != Some(request.storage_generation)
        {
            return Err(ApiError::conflict(
                "upload_completion_mismatch",
                "The upload protection state changed.",
            ));
        }
        sqlx::query(
            "UPDATE job_artifacts SET status = 'verified', protection_pending = false, \
                    state_reason = NULL WHERE id = $1",
        )
        .bind(artifact_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ApiError::internal())?;
        consume_completed_upload_session(&mut transaction, artifact_id)
            .await
            .map_err(|_| ApiError::internal())?;
    }
    let verified_artifact =
        artifact_for_worker(&mut transaction, attempt_id, artifact_id, worker_id).await?;
    transaction
        .commit()
        .await
        .map_err(|_| ApiError::internal())?;
    Ok(Json(verified_artifact.try_into()?))
}

pub(crate) async fn reconcile_pending_protections(
    pool: &sqlx::PgPool,
    storage: &crate::artifact_storage::ArtifactStorageClient,
) -> usize {
    let pending = match sqlx::query_as::<_, (Uuid, String, String, i64)>(
        "SELECT id, storage_bucket, object_key, verified_storage_generation \
         FROM job_artifacts WHERE protection_pending = true AND status = 'uploading' \
         ORDER BY verified_at LIMIT 100",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "could not load pending artifact protections");
            return 0;
        }
    };
    let mut completed = 0;
    for (artifact_id, bucket, object_key, generation) in pending {
        if storage
            .protect_verified_object(&bucket, &object_key, generation)
            .await
            .is_err()
        {
            continue;
        }
        let mut transaction = match pool.begin().await {
            Ok(transaction) => transaction,
            Err(error) => {
                tracing::warn!(%error, %artifact_id, "could not begin artifact publication");
                continue;
            }
        };
        let published = match sqlx::query(
            "UPDATE job_artifacts SET status = 'verified', protection_pending = false, \
                    state_reason = NULL WHERE id = $1 AND status = 'uploading' \
                    AND protection_pending = true AND verified_storage_generation = $2",
        )
        .bind(artifact_id)
        .bind(generation)
        .execute(&mut *transaction)
        .await
        {
            Ok(result) => result.rows_affected(),
            Err(error) => {
                tracing::warn!(%error, %artifact_id, "could not publish protected artifact");
                continue;
            }
        };
        if published == 1
            && let Err(error) =
                consume_completed_upload_session(&mut transaction, artifact_id).await
        {
            tracing::warn!(%error, %artifact_id, "could not consume completed upload session");
            continue;
        }
        match transaction.commit().await {
            Ok(()) => completed += usize::try_from(published).unwrap_or(0),
            Err(error) => {
                tracing::warn!(%error, %artifact_id, "could not commit artifact publication");
            }
        }
    }
    completed
}

async fn mark_sessions_for_cancellation(
    transaction: &mut Transaction<'_, Postgres>,
    predicate: &str,
    subject_id: Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    let query = format!(
        "UPDATE artifact_upload_grants g SET \
             state = CASE WHEN g.state = 'active' THEN 'cancel_pending' ELSE 'cancelled' END, \
             cancel_requested_at = now(), cancelled_at = CASE WHEN g.state = 'initiating' THEN now() ELSE NULL END, \
             cancellation_reason = $2 \
         FROM job_artifacts a WHERE g.artifact_id = a.id AND {predicate} \
           AND g.state IN ('initiating', 'active')"
    );
    sqlx::query(&query)
        .bind(subject_id)
        .bind(reason)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn cancel_artifact_upload_session(
    transaction: &mut Transaction<'_, Postgres>,
    artifact_id: Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    mark_sessions_for_cancellation(transaction, "g.artifact_id = $1", artifact_id, reason).await
}

async fn consume_completed_upload_session(
    transaction: &mut Transaction<'_, Postgres>,
    artifact_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE artifact_upload_grants SET state = 'cancelled', session_uri = NULL, \
                cancel_requested_at = COALESCE(cancel_requested_at, now()), \
                cancelled_at = now(), cancellation_reason = 'upload_completed' \
         WHERE artifact_id = $1 AND state = 'active'",
    )
    .bind(artifact_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn cancel_worker_upload_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    worker_id: Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    mark_sessions_for_cancellation(transaction, "g.worker_id = $1", worker_id, reason).await
}

pub(crate) async fn cancel_job_upload_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    mark_sessions_for_cancellation(transaction, "a.job_id = $1", job_id, reason).await
}

pub(crate) async fn cancel_attempt_upload_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    attempt_id: Uuid,
    reason: &str,
) -> Result<(), sqlx::Error> {
    mark_sessions_for_cancellation(transaction, "a.attempt_id = $1", attempt_id, reason).await
}

pub(crate) async fn reconcile_pending_session_cancellations(
    pool: &sqlx::PgPool,
    storage: &crate::artifact_storage::ArtifactStorageClient,
) -> usize {
    let pending = match sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, session_uri FROM artifact_upload_grants \
         WHERE state = 'cancel_pending' ORDER BY cancel_requested_at LIMIT 100",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "could not load upload sessions pending cancellation");
            return 0;
        }
    };
    let mut completed = 0;
    for (grant_id, session_uri) in pending {
        if storage.cancel_resumable_upload(&session_uri).await.is_err() {
            continue;
        }
        match sqlx::query(
            "UPDATE artifact_upload_grants SET state = 'cancelled', session_uri = NULL, \
                    session_uri_sha256 = COALESCE(session_uri_sha256, $3), \
                    cancelled_at = now() WHERE id = $1 AND state = 'cancel_pending' \
                    AND session_uri = $2",
        )
        .bind(grant_id)
        .bind(&session_uri)
        .bind(session_uri_fingerprint(&session_uri))
        .execute(pool)
        .await
        {
            Ok(result) => completed += usize::try_from(result.rows_affected()).unwrap_or(0),
            Err(error) => {
                tracing::warn!(%error, %grant_id, "could not consume cancelled session URI");
            }
        }
    }
    completed
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::AUTHORIZATION},
    };
    use chrono::{DateTime, TimeDelta, Utc};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use sqlx::PgPool;
    use tokio::sync::Notify;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::projects::DEFAULT_PROJECT_ID;
    use crate::{
        app, app_with_dependencies,
        artifact_storage::{
            ArtifactStorage, ArtifactStorageClient, ArtifactStorageError, ResumableUploadSession,
            StoredObjectMetadata,
        },
        credentials::{self, CredentialKind},
    };

    use super::{
        JobOutputRequirement, MAX_TOTAL_BYTES, cancel_attempt_upload_sessions,
        cancel_job_upload_sessions, cancel_worker_upload_sessions, delivery_window,
        reconcile_pending_protections, reconcile_pending_session_cancellations,
        session_uri_fingerprint, valid_crc32c, valid_logical_path, validate_output_requirements,
    };

    struct Fixture {
        worker_id: Uuid,
        job_id: Uuid,
        attempt_id: Uuid,
        credential: String,
    }

    #[derive(Default)]
    struct StorageCalls {
        initiated: AtomicUsize,
        protected: AtomicUsize,
        deleted: AtomicUsize,
        cancelled: AtomicUsize,
        fail_next_cancellation: AtomicBool,
        block_next_initiation: AtomicBool,
        initiation_started: Notify,
        release_initiation: Notify,
    }

    struct FakeArtifactStorage {
        crc32c: &'static str,
        bucket: String,
        calls: Arc<StorageCalls>,
    }

    #[async_trait]
    impl ArtifactStorage for FakeArtifactStorage {
        async fn initiate_resumable_upload(
            &self,
            object_key: &str,
            media_type: &str,
            byte_length: u64,
            sha256: &str,
            issued_at: chrono::DateTime<Utc>,
        ) -> Result<ResumableUploadSession, ArtifactStorageError> {
            let sequence = self.calls.initiated.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = (media_type, byte_length, sha256);
            if self
                .calls
                .block_next_initiation
                .swap(false, Ordering::SeqCst)
            {
                self.calls.initiation_started.notify_one();
                self.calls.release_initiation.notified().await;
            }
            Ok(ResumableUploadSession {
                uri: format!(
                    "https://storage.googleapis.com/upload/session/{sequence}/{object_key}"
                ),
                method: "PUT".to_owned(),
                expires_at: issued_at + TimeDelta::days(7),
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
                byte_length: 512,
                crc32c: self.crc32c.to_owned(),
                sha256: "b".repeat(64),
            })
        }

        fn bucket(&self) -> &str {
            &self.bucket
        }

        async fn protect_verified_object(
            &self,
            _bucket: &str,
            _object_key: &str,
            _generation: i64,
        ) -> Result<(), ArtifactStorageError> {
            self.calls.protected.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn delete_object(
            &self,
            _bucket: &str,
            _object_key: &str,
            _generation: i64,
        ) -> Result<(), ArtifactStorageError> {
            self.calls.deleted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn cancel_resumable_upload(
            &self,
            _session_uri: &str,
        ) -> Result<(), ArtifactStorageError> {
            self.calls.cancelled.fetch_add(1, Ordering::SeqCst);
            if self
                .calls
                .fail_next_cancellation
                .swap(false, Ordering::SeqCst)
            {
                return Err(ArtifactStorageError::Unavailable);
            }
            Ok(())
        }
    }

    fn app_with_storage(pool: PgPool, crc32c: &'static str) -> (axum::Router, Arc<StorageCalls>) {
        let calls = Arc::new(StorageCalls::default());
        let router = app_with_dependencies(
            None,
            Some(pool),
            None,
            Some(ArtifactStorageClient::new(FakeArtifactStorage {
                crc32c,
                bucket: "test-artifacts".to_owned(),
                calls: Arc::clone(&calls),
            })),
        );
        (router, calls)
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
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities, project_id) \
             VALUES ($1, $2, $3, 'GPU worker', '1.1', 'busy', '{}'::jsonb, $4)",
        )
        .bind(worker_id)
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
        .bind(worker_id)
        .bind(&credential.verifier)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO jobs \
             (id, owner_identity_id, name, image_reference, timeout_seconds, status, assigned_worker_id, project_id) \
             VALUES ($1, $2, 'Training', $3, 120, 'assigned', $4, $5)",
        )
        .bind(job_id)
        .bind(owner_id)
        .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
        .bind(worker_id)
        .bind(DEFAULT_PROJECT_ID)
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
            job_id,
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

    #[test]
    fn delivery_window_is_bounded_and_scales_with_bytes() {
        assert_eq!(delivery_window(512), TimeDelta::minutes(15));
        assert!(delivery_window(MAX_TOTAL_BYTES) > TimeDelta::hours(22));
        assert!(delivery_window(MAX_TOTAL_BYTES) <= TimeDelta::hours(24));
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
        let mut first_deadline = None;
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
            let (lease, delivery): (DateTime<Utc>, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT lease_expires_at, artifact_delivery_expires_at \
                 FROM job_attempts WHERE id = $1",
            )
            .bind(fixture.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
            let delivery = delivery.expect("manifest must create a delivery deadline");
            assert!(delivery > lease);
            if let Some(first) = first_deadline {
                assert_eq!(
                    delivery, first,
                    "manifest replay must not slide the deadline"
                );
            } else {
                first_deadline = Some(delivery);
            }
        }

        let mut changed = manifest_body(manifest_id);
        changed["files"][0]["sha256"] = Value::String("c".repeat(64));
        let response = router
            .clone()
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

        sqlx::query(
            "UPDATE job_attempts SET lease_expires_at = now() - interval '1 second' \
             WHERE id = $1",
        )
        .bind(fixture.attempt_id)
        .execute(&pool)
        .await
        .unwrap();
        let result = router
            .oneshot(
                Request::put(format!(
                    "/api/v1/workers/{}/job-attempts/{}/result",
                    fixture.worker_id, fixture.attempt_id
                ))
                .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "exit_code": 125,
                        "timed_out": false,
                        "stdout": "",
                        "stderr": "",
                        "failure_message": "output delivery failed"
                    })
                    .to_string(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::OK);
        assert_eq!(response_json(result).await["status"], "failed");
        let attempts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM job_attempts WHERE job_id = $1")
                .bind(fixture.job_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            attempts, 1,
            "delivery failure must not become an execution retry"
        );
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
             (id, owner_identity_id, name, image_reference, timeout_seconds, status, assigned_worker_id, project_id) \
             VALUES ($1, $2, 'Second training', $3, 120, 'assigned', $4, $5)",
        )
        .bind(second_job_id)
        .bind(owner_id)
        .bind(format!("example.test/work@sha256:{}", "c".repeat(64)))
        .bind(fixture.worker_id)
        .bind(DEFAULT_PROJECT_ID)
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
    async fn upload_authority_and_verified_completion_are_replay_safe(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let (router, storage_calls) = app_with_storage(pool.clone(), "ImIEBA==");
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
        storage_calls
            .block_next_initiation
            .store(true, Ordering::SeqCst);
        let first = tokio::spawn(
            router.clone().oneshot(
                Request::put(&upload_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "protocol_version": "1.1" }).to_string()))
                    .unwrap(),
            ),
        );
        storage_calls.initiation_started.notified().await;
        let pending = router
            .clone()
            .oneshot(
                Request::put(&upload_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pending.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(storage_calls.initiated.load(Ordering::SeqCst), 1);
        storage_calls.release_initiation.notify_one();
        let started = first.await.unwrap().unwrap();
        assert_eq!(started.status(), StatusCode::OK);
        assert_eq!(started.headers()["cache-control"], "no-store");
        let started = response_json(started).await;
        assert_eq!(started["artifact"]["status"], "uploading");
        assert_eq!(started["session"]["method"], "PUT");
        assert!(
            started["session"]["uri"]
                .as_str()
                .unwrap()
                .contains("/upload/session/")
        );
        let replayed = router
            .clone()
            .oneshot(
                Request::put(&upload_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replayed.status(), StatusCode::OK);
        let replayed = response_json(replayed).await;
        assert_eq!(replayed["session"], started["session"]);
        let grant_count: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact_upload_grants")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(grant_count, 1);
        assert_eq!(storage_calls.initiated.load(Ordering::SeqCst), 1);

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
            assert_eq!(body["status"], "verified");
            assert_eq!(body["verification_pending"], false);
            if let Some(first) = &first_body {
                assert_eq!(&body, first);
            } else {
                first_body = Some(body);
            }
        }

        // A crash after durable verification but before publishing the row leaves recoverable work,
        // including the still-reachable upload session.
        let mut recovery = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE job_artifacts SET status = 'uploading', protection_pending = true, \
                    state_reason = 'gcs_protection_pending' WHERE id = $1",
        )
        .bind(artifact_id)
        .execute(&mut *recovery)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE artifact_upload_grants SET state = 'active', session_uri = $2, \
                    activated_at = now(), cancel_requested_at = NULL, cancelled_at = NULL, \
                    cancellation_reason = NULL WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .bind("https://storage.googleapis.com/upload/session/protection-recovery")
        .execute(&mut *recovery)
        .await
        .unwrap();
        recovery.commit().await.unwrap();
        let recovery_storage = ArtifactStorageClient::new(FakeArtifactStorage {
            crc32c: "ImIEBA==",
            bucket: "test-artifacts".to_owned(),
            calls: storage_calls.clone(),
        });
        assert_eq!(
            reconcile_pending_protections(&pool, &recovery_storage).await,
            1
        );
        let recovered: (String, bool, String, Option<String>) = sqlx::query_as(
            "SELECT a.status, a.protection_pending, g.state, g.session_uri \
             FROM job_artifacts a JOIN artifact_upload_grants g ON g.artifact_id = a.id \
             WHERE a.id = $1",
        )
        .bind(artifact_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            recovered,
            ("verified".to_owned(), false, "cancelled".to_owned(), None)
        );

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

        // A replay also repairs a legacy/partial verified row with a reachable session.
        sqlx::query(
            "UPDATE artifact_upload_grants SET state = 'active', session_uri = $2, \
                    activated_at = now(), cancel_requested_at = NULL, cancelled_at = NULL, \
                    cancellation_reason = NULL WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .bind("https://storage.googleapis.com/upload/session/replay-recovery")
        .execute(&pool)
        .await
        .unwrap();

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
        let replay_grant: (String, Option<String>) = sqlx::query_as(
            "SELECT state, session_uri FROM artifact_upload_grants WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(replay_grant, ("cancelled".to_owned(), None));
        assert_eq!(storage_calls.protected.load(Ordering::SeqCst), 2);
        assert_eq!(storage_calls.deleted.load(Ordering::SeqCst), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unusable_upload_session_is_abandoned_and_safely_replaced(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let (router, calls) = app_with_storage(pool.clone(), "ImIEBA==");
        let declared = router
            .clone()
            .oneshot(
                Request::put(format!(
                    "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
                    fixture.worker_id, fixture.attempt_id
                ))
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
        let base_uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifacts/{artifact_id}",
            fixture.worker_id, fixture.attempt_id
        );
        let started = router
            .clone()
            .oneshot(
                Request::put(format!("{base_uri}/upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::OK);
        let started = response_json(started).await;
        let first_uri = started["session"]["uri"].as_str().unwrap();
        let first_fingerprint = format!("{:x}", Sha256::digest(first_uri.as_bytes()));

        let wrong_session = router
            .clone()
            .oneshot(
                Request::put(format!("{base_uri}/abandon-upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "protocol_version": "1.1",
                            "session_uri_sha256": "0".repeat(64)
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_session.status(), StatusCode::CONFLICT);
        assert_eq!(calls.cancelled.load(Ordering::SeqCst), 0);

        let abandon_body = json!({
            "protocol_version": "1.1",
            "session_uri_sha256": first_fingerprint
        });
        let abandoned = router
            .clone()
            .oneshot(
                Request::put(format!("{base_uri}/abandon-upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(abandon_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(abandoned.status(), StatusCode::NO_CONTENT);
        assert_eq!(calls.cancelled.load(Ordering::SeqCst), 1);

        let replayed_abandon = router
            .clone()
            .oneshot(
                Request::put(format!("{base_uri}/abandon-upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(abandon_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replayed_abandon.status(), StatusCode::NO_CONTENT);

        let unrelated_replay = router
            .clone()
            .oneshot(
                Request::put(format!("{base_uri}/abandon-upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "protocol_version": "1.1",
                            "session_uri_sha256": "f".repeat(64)
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unrelated_replay.status(), StatusCode::CONFLICT);
        let unrelated_replay = response_json(unrelated_replay).await;
        assert_eq!(unrelated_replay["code"], "upload_session_changed");

        let replacement = router
            .clone()
            .oneshot(
                Request::put(format!("{base_uri}/upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replacement.status(), StatusCode::OK);
        let replacement = response_json(replacement).await;
        assert_ne!(replacement["session"]["uri"], started["session"]["uri"]);
        assert_eq!(calls.initiated.load(Ordering::SeqCst), 2);

        let stale_abandon = router
            .clone()
            .oneshot(
                Request::put(format!("{base_uri}/abandon-upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(abandon_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale_abandon.status(), StatusCode::CONFLICT);
        assert_eq!(calls.cancelled.load(Ordering::SeqCst), 1);

        let replacement_uri = replacement["session"]["uri"].as_str().unwrap();
        let replacement_fingerprint = session_uri_fingerprint(replacement_uri);
        sqlx::query(
            "UPDATE artifact_upload_grants SET state = 'cancel_pending', \
                    session_uri_sha256 = NULL, cancel_requested_at = now(), \
                    cancellation_reason = 'worker_reported_session_unusable' \
             WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .execute(&pool)
        .await
        .unwrap();
        let replacement_abandon_body = json!({
            "protocol_version": "1.1",
            "session_uri_sha256": replacement_fingerprint
        });
        let legacy_pending = router
            .clone()
            .oneshot(
                Request::put(format!("{base_uri}/abandon-upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(replacement_abandon_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy_pending.status(), StatusCode::NO_CONTENT);
        let retained_fingerprint: Option<String> = sqlx::query_scalar(
            "SELECT session_uri_sha256 FROM artifact_upload_grants WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            retained_fingerprint.as_deref(),
            Some(replacement_fingerprint.as_str())
        );
        let legacy_replay = router
            .oneshot(
                Request::put(format!("{base_uri}/abandon-upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(replacement_abandon_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy_replay.status(), StatusCode::NO_CONTENT);
        assert_eq!(calls.cancelled.load(Ordering::SeqCst), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn expired_upload_session_is_cancelled_before_replacement(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let (router, calls) = app_with_storage(pool.clone(), "ImIEBA==");
        let declared = router
            .clone()
            .oneshot(
                Request::put(format!(
                    "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
                    fixture.worker_id, fixture.attempt_id
                ))
                .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                .header("content-type", "application/json")
                .body(Body::from(manifest_body(Uuid::new_v4()).to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
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
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::OK);
        let past = Utc::now() - TimeDelta::minutes(1);
        sqlx::query(
            "UPDATE artifact_upload_grants SET issued_at = $2, expires_at = $3 \
             WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .bind(past - TimeDelta::days(7))
        .bind(past)
        .execute(&pool)
        .await
        .unwrap();
        calls.fail_next_cancellation.store(true, Ordering::SeqCst);

        let expired = router
            .clone()
            .oneshot(
                Request::put(&upload_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(expired.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(calls.cancelled.load(Ordering::SeqCst), 1);

        let cancellation_retry = router
            .clone()
            .oneshot(
                Request::put(&upload_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancellation_retry.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(calls.cancelled.load(Ordering::SeqCst), 2);

        let replacement = router
            .oneshot(
                Request::put(&upload_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replacement.status(), StatusCode::OK);
        assert_eq!(calls.initiated.load(Ordering::SeqCst), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upload_session_denies_revoked_workers_and_closed_jobs(pool: PgPool) {
        for cancel_job in [false, true] {
            let fixture = fixture(&pool).await;
            let (router, storage_calls) = app_with_storage(pool.clone(), "ImIEBA==");
            let declared = router
                .clone()
                .oneshot(
                    Request::put(format!(
                        "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
                        fixture.worker_id, fixture.attempt_id
                    ))
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

            if cancel_job {
                sqlx::query(
                    "UPDATE jobs SET status = 'cancelled', cancel_requested_at = now(), \
                            finished_at = now() WHERE id = $1",
                )
                .bind(fixture.job_id)
                .execute(&pool)
                .await
                .unwrap();
            } else {
                sqlx::query(
                    "UPDATE worker_credentials SET revoked_at = now() WHERE worker_id = $1",
                )
                .bind(fixture.worker_id)
                .execute(&pool)
                .await
                .unwrap();
            }

            let denied = router
                .oneshot(
                    Request::put(format!(
                        "/api/v1/workers/{}/job-attempts/{}/artifacts/{artifact_id}/upload",
                        fixture.worker_id, fixture.attempt_id
                    ))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                denied.status(),
                if cancel_job {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
            assert_eq!(storage_calls.initiated.load(Ordering::SeqCst), 0);
        }
        let grant_count: i64 = sqlx::query_scalar("SELECT count(*) FROM artifact_upload_grants")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(grant_count, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn crash_before_session_commit_does_not_fan_out_initiation(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let calls = Arc::new(StorageCalls::default());
        let storage = ArtifactStorageClient::new(FakeArtifactStorage {
            crc32c: "ImIEBA==",
            bucket: "test-artifacts".to_owned(),
            calls: calls.clone(),
        });
        let router = app_with_dependencies(None, Some(pool.clone()), None, Some(storage.clone()));
        let declared = router
            .clone()
            .oneshot(
                Request::put(format!(
                    "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
                    fixture.worker_id, fixture.attempt_id
                ))
                .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                .header("content-type", "application/json")
                .body(Body::from(manifest_body(Uuid::new_v4()).to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        let declared = response_json(declared).await;
        let artifact_id =
            Uuid::parse_str(declared["artifacts"][0]["artifact_id"].as_str().unwrap()).unwrap();
        let object_key = declared["artifacts"][0]["object_key"].as_str().unwrap();
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO artifact_upload_grants \
             (id, artifact_id, worker_id, bucket_name, object_key, state, initiation_id, issued_at, expires_at) \
             VALUES ($1, $2, $3, 'test-artifacts', $4, 'initiating', $5, $6, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(artifact_id)
        .bind(fixture.worker_id)
        .bind(object_key)
        .bind(Uuid::new_v4())
        .bind(now)
        .bind(now + TimeDelta::minutes(15))
        .execute(&pool)
        .await
        .unwrap();
        // GCS returned a session, then the process died before recording it. The durable initiating
        // row makes the URI unreachable to a worker and suppresses retries until the grace expires.
        let _orphaned = storage
            .initiate_resumable_upload(
                object_key,
                "application/octet-stream",
                512,
                &"b".repeat(64),
                now,
            )
            .await
            .unwrap();
        let upload_uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifacts/{artifact_id}/upload",
            fixture.worker_id, fixture.attempt_id
        );
        for _ in 0..2 {
            let response = router
                .clone()
                .oneshot(
                    Request::put(&upload_uri)
                        .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        assert_eq!(calls.initiated.load(Ordering::SeqCst), 1);
        let state: (String, Option<String>) = sqlx::query_as(
            "SELECT state, session_uri FROM artifact_upload_grants WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, ("initiating".to_owned(), None));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn authority_endings_cancel_and_consume_reachable_sessions(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let (router, calls) = app_with_storage(pool.clone(), "ImIEBA==");
        let declared = router
            .clone()
            .oneshot(
                Request::put(format!(
                    "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
                    fixture.worker_id, fixture.attempt_id
                ))
                .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                .header("content-type", "application/json")
                .body(Body::from(manifest_body(Uuid::new_v4()).to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        let declared = response_json(declared).await;
        let artifact_id =
            Uuid::parse_str(declared["artifacts"][0]["artifact_id"].as_str().unwrap()).unwrap();
        let upload_uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifacts/{artifact_id}/upload",
            fixture.worker_id, fixture.attempt_id
        );
        let started = router
            .oneshot(
                Request::put(upload_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::OK);
        let storage = ArtifactStorageClient::new(FakeArtifactStorage {
            crc32c: "ImIEBA==",
            bucket: "test-artifacts".to_owned(),
            calls: calls.clone(),
        });
        for scope in 0..3 {
            if scope > 0 {
                sqlx::query(
                    "UPDATE artifact_upload_grants SET state = 'active', \
                            session_uri = $2, activated_at = now(), cancel_requested_at = NULL, \
                            cancelled_at = NULL, cancellation_reason = NULL WHERE artifact_id = $1",
                )
                .bind(artifact_id)
                .bind(format!(
                    "https://storage.googleapis.com/upload/session/retry-{scope}"
                ))
                .execute(&pool)
                .await
                .unwrap();
            }
            let mut transaction = pool.begin().await.unwrap();
            match scope {
                0 => cancel_worker_upload_sessions(&mut transaction, fixture.worker_id, "revoked")
                    .await
                    .unwrap(),
                1 => cancel_job_upload_sessions(&mut transaction, fixture.job_id, "cancelled")
                    .await
                    .unwrap(),
                _ => cancel_attempt_upload_sessions(
                    &mut transaction,
                    fixture.attempt_id,
                    "lease_expired",
                )
                .await
                .unwrap(),
            }
            transaction.commit().await.unwrap();
            assert_eq!(
                reconcile_pending_session_cancellations(&pool, &storage).await,
                1
            );
            let grant: (String, Option<String>) = sqlx::query_as(
                "SELECT state, session_uri FROM artifact_upload_grants WHERE artifact_id = $1",
            )
            .bind(artifact_id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(grant, ("cancelled".to_owned(), None));
        }
        assert_eq!(calls.cancelled.load(Ordering::SeqCst), 3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn authoritative_metadata_mismatch_rejects_artifact(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let (router, storage_calls) = app_with_storage(pool.clone(), "AAAAAA==");
        let manifest_uri = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifact-manifest",
            fixture.worker_id, fixture.attempt_id
        );
        let declared = router
            .clone()
            .oneshot(
                Request::put(manifest_uri)
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(manifest_body(Uuid::new_v4()).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let declared = response_json(declared).await;
        let artifact_id =
            Uuid::parse_str(declared["artifacts"][0]["artifact_id"].as_str().unwrap()).unwrap();
        let base = format!(
            "/api/v1/workers/{}/job-attempts/{}/artifacts/{artifact_id}",
            fixture.worker_id, fixture.attempt_id
        );
        let started = router
            .clone()
            .oneshot(
                Request::put(format!("{base}/upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"protocol_version":"1.1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::OK);

        let rejected = router
            .oneshot(
                Request::put(format!("{base}/complete-upload"))
                    .header(AUTHORIZATION, format!("Bearer {}", fixture.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "protocol_version": "1.1",
                            "storage_generation": 42,
                            "byte_length": 512,
                            "sha256": "b".repeat(64),
                            "crc32c": "ImIEBA=="
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::CONFLICT);
        let rejected = response_json(rejected).await;
        assert_eq!(rejected["code"], "artifact_verification_failed");
        let stored: (String, Option<String>) =
            sqlx::query_as("SELECT status, state_reason FROM job_artifacts WHERE id = $1")
                .bind(artifact_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.0, "rejected");
        assert_eq!(stored.1.as_deref(), Some("gcs_metadata_mismatch"));
        assert_eq!(storage_calls.protected.load(Ordering::SeqCst), 0);
        assert_eq!(storage_calls.deleted.load(Ordering::SeqCst), 1);
        assert_eq!(storage_calls.cancelled.load(Ordering::SeqCst), 1);
        let grant: (String, Option<String>) = sqlx::query_as(
            "SELECT state, session_uri FROM artifact_upload_grants WHERE artifact_id = $1",
        )
        .bind(artifact_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(grant, ("cancelled".to_owned(), None));
    }
}

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::HeaderMap,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    registry::{
        ApiError, ErrorResponse, authenticate_worker, database_error,
        lock_current_worker_authorization, protocol_minor, validate_protocol,
    },
};

const MAX_BATCH_RECORDS: usize = 100;
pub(crate) const MAX_BATCH_BYTES: usize = 256 * 1024;
const MAX_STEP: i64 = 1_i64 << 53;

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationRecord {
    pub sequence: i64,
    pub at: DateTime<Utc>,
    pub record: String,
    pub name: Option<String>,
    pub value: Option<Value>,
    pub step: Option<i64>,
    pub total_steps: Option<i64>,
    pub unit: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitObservationBatchRequest {
    pub protocol_version: String,
    pub first_sequence: i64,
    pub records: Vec<ObservationRecord>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ObservationBatchResponse {
    pub stream_id: Uuid,
    pub batch_id: Uuid,
    pub accepted_through_sequence: i64,
}

fn valid_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && value.len() <= 64
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'.'
        })
}

fn valid_unit(value: &str) -> bool {
    value.len() <= 32
}

fn valid_step(value: i64) -> bool {
    (0..MAX_STEP).contains(&value)
}

fn valid_param_value(value: &Value) -> bool {
    matches!(value, Value::Bool(_) | Value::String(_) | Value::Number(_))
        && serde_json::to_vec(value).is_ok_and(|encoded| encoded.len() <= 512)
}

fn validate_record(record: &ObservationRecord) -> bool {
    if record.sequence < 1 || record.unit.as_deref().is_some_and(|unit| !valid_unit(unit)) {
        return false;
    }
    match record.record.as_str() {
        "param" => {
            record.name.as_deref().is_some_and(valid_name)
                && record.value.as_ref().is_some_and(valid_param_value)
                && record.step.is_none()
                && record.total_steps.is_none()
        }
        "metric" => {
            record.name.as_deref().is_some_and(valid_name)
                && record.value.as_ref().is_some_and(Value::is_number)
                && record.step.is_some_and(valid_step)
                && record.total_steps.is_none()
        }
        "progress" => {
            record.name.is_none()
                && record.value.is_none()
                && record.step.is_some_and(valid_step)
                && record.total_steps.is_none_or(valid_step)
                && record
                    .total_steps
                    .zip(record.step)
                    .is_none_or(|(total, step)| total >= step)
        }
        _ => false,
    }
}

fn validate_request(request: &SubmitObservationBatchRequest) -> Result<(), ApiError> {
    validate_protocol(&request.protocol_version)?;
    if protocol_minor(&request.protocol_version).is_none_or(|minor| minor < 2)
        || request.first_sequence < 1
        || request.records.is_empty()
        || request.records.len() > MAX_BATCH_RECORDS
        || serde_json::to_vec(request).map_or(true, |body| body.len() > MAX_BATCH_BYTES)
    {
        return Err(ApiError::invalid_request());
    }
    let mut expected = request.first_sequence;
    for record in &request.records {
        if record.sequence != expected || !validate_record(record) {
            return Err(ApiError::invalid_request());
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(ApiError::invalid_request)?;
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[utoipa::path(
    put,
    path = "/api/v1/workers/{worker_id}/observation-streams/{stream_id}/batches/{batch_id}",
    tag = "workers",
    security(("bearer_credential" = [])),
    params(
        ("worker_id" = Uuid, Path, description = "Worker identifier"),
        ("stream_id" = Uuid, Path, description = "Attempt observation stream"),
        ("batch_id" = Uuid, Path, description = "Stable batch identifier")
    ),
    request_body = SubmitObservationBatchRequest,
    responses(
        (status = 200, description = "Observation batch accepted idempotently", body = ObservationBatchResponse),
        (status = 401, description = "Credential or stream rejected", body = ErrorResponse),
        (status = 409, description = "Batch or sequence content changed", body = ErrorResponse),
        (status = 422, description = "Batch is invalid or arrives with a sequence gap", body = ErrorResponse)
    )
)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn submit_batch(
    State(state): State<AppState>,
    Path((worker_id, stream_id, batch_id)): Path<(Uuid, Uuid, Uuid)>,
    headers: HeaderMap,
    payload: Result<Json<SubmitObservationBatchRequest>, JsonRejection>,
) -> Result<Json<ObservationBatchResponse>, ApiError> {
    let Json(request) = payload.map_err(|_| ApiError::invalid_request())?;
    validate_request(&request)?;
    let credential_id = authenticate_worker(
        &state,
        &headers,
        worker_id,
        "load observation worker credential",
    )
    .await?;
    let pool = state.database.as_ref().ok_or_else(ApiError::unavailable)?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| database_error(&error, "begin observation batch"))?;
    lock_current_worker_authorization(&mut transaction, credential_id, worker_id).await?;
    let accepted = sqlx::query_scalar::<_, i64>(
        "SELECT accepted_through_sequence FROM observation_streams \
         WHERE id = $1 AND worker_id = $2 FOR UPDATE",
    )
    .bind(stream_id)
    .bind(worker_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "lock observation stream"))?
    .ok_or_else(ApiError::unauthorized)?;

    let request_bytes = serde_json::to_vec(&request).map_err(|_| ApiError::invalid_request())?;
    let batch_hash = sha256_hex(&request_bytes);
    if let Some(existing_hash) = sqlx::query_scalar::<_, String>(
        "SELECT content_sha256 FROM observation_batches \
         WHERE stream_id = $1 AND batch_id = $2",
    )
    .bind(stream_id)
    .bind(batch_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "load observation batch replay"))?
    {
        if existing_hash != batch_hash {
            return Err(ApiError::conflict(
                "observation_batch_conflict",
                "The batch identifier was already used with different content.",
            ));
        }
        transaction
            .commit()
            .await
            .map_err(|error| database_error(&error, "commit observation batch replay"))?;
        return Ok(Json(ObservationBatchResponse {
            stream_id,
            batch_id,
            accepted_through_sequence: accepted,
        }));
    }

    if request.first_sequence > accepted + 1 {
        return Err(ApiError::invalid_request());
    }
    let last_sequence = request
        .records
        .last()
        .map(|record| record.sequence)
        .ok_or_else(ApiError::invalid_request)?;
    for record in &request.records {
        let content = serde_json::to_value(record).map_err(|_| ApiError::invalid_request())?;
        let content_hash =
            sha256_hex(&serde_json::to_vec(&content).map_err(|_| ApiError::invalid_request())?);
        if let Some(existing_hash) = sqlx::query_scalar::<_, String>(
            "SELECT content_sha256 FROM observations WHERE stream_id = $1 AND sequence = $2",
        )
        .bind(stream_id)
        .bind(record.sequence)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| database_error(&error, "load observation replay"))?
        {
            if existing_hash != content_hash {
                return Err(ApiError::conflict(
                    "observation_sequence_conflict",
                    "The observation sequence was already used with different content.",
                ));
            }
            continue;
        }
        sqlx::query(
            "INSERT INTO observations \
             (stream_id, sequence, first_batch_id, observed_at, record_type, content, content_sha256) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(stream_id)
        .bind(record.sequence)
        .bind(batch_id)
        .bind(record.at)
        .bind(&record.record)
        .bind(content)
        .bind(content_hash)
        .execute(&mut *transaction)
        .await
        .map_err(|error| database_error(&error, "store observation"))?;
    }
    sqlx::query(
        "INSERT INTO observation_batches \
         (stream_id, batch_id, first_sequence, last_sequence, content_sha256) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(stream_id)
    .bind(batch_id)
    .bind(request.first_sequence)
    .bind(last_sequence)
    .bind(batch_hash)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "store observation batch"))?;
    let accepted_through_sequence = accepted.max(last_sequence);
    sqlx::query(
        "UPDATE observation_streams SET accepted_through_sequence = $2, updated_at = now() \
         WHERE id = $1",
    )
    .bind(stream_id)
    .bind(accepted_through_sequence)
    .execute(&mut *transaction)
    .await
    .map_err(|error| database_error(&error, "advance observation stream"))?;
    transaction
        .commit()
        .await
        .map_err(|error| database_error(&error, "commit observation batch"))?;
    Ok(Json(ObservationBatchResponse {
        stream_id,
        batch_id,
        accepted_through_sequence,
    }))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header::AUTHORIZATION},
    };
    use chrono::Utc;
    use serde_json::json;
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::projects::DEFAULT_PROJECT_ID;
    use crate::{
        app,
        credentials::{self, CredentialKind},
    };

    use super::{ObservationRecord, SubmitObservationBatchRequest, validate_request};

    fn metric(sequence: i64) -> ObservationRecord {
        ObservationRecord {
            sequence,
            at: Utc::now(),
            record: "metric".to_owned(),
            name: Some("train.loss".to_owned()),
            value: Some(json!(0.25)),
            step: Some(sequence - 1),
            total_steps: None,
            unit: None,
        }
    }

    #[test]
    fn accepts_a_contiguous_protocol_1_2_batch() {
        let request = SubmitObservationBatchRequest {
            protocol_version: "1.2".to_owned(),
            first_sequence: 1,
            records: vec![metric(1), metric(2)],
        };
        assert!(validate_request(&request).is_ok());
    }

    #[test]
    fn rejects_a_sequence_gap() {
        let request = SubmitObservationBatchRequest {
            protocol_version: "1.2".to_owned(),
            first_sequence: 1,
            records: vec![metric(1), metric(3)],
        };
        assert!(validate_request(&request).is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn batch_replay_is_idempotent_by_sequence_even_after_the_attempt_finishes(pool: PgPool) {
        let owner_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
             VALUES ($1, 'test', $2, 'Owner')",
        )
        .bind(owner_id)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
        let worker_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workers \
             (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities, project_id) \
             VALUES ($1, $2, $3, 'GPU worker', '1.2', 'busy', '{}'::jsonb, $4)",
        )
        .bind(worker_id)
        .bind(owner_id)
        .bind(Uuid::new_v4())
        .bind(DEFAULT_PROJECT_ID)
        .execute(&pool)
        .await
        .unwrap();
        let credential = credentials::issue(CredentialKind::Worker).unwrap();
        sqlx::query(
            "INSERT INTO worker_credentials (id, worker_id, token_verifier) VALUES ($1, $2, $3)",
        )
        .bind(credential.id)
        .bind(worker_id)
        .bind(&credential.verifier)
        .execute(&pool)
        .await
        .unwrap();
        let job_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO jobs \
             (id, owner_identity_id, name, image_reference, timeout_seconds, status, assigned_worker_id, project_id) \
             VALUES ($1, $2, 'Observed job', $3, 120, 'assigned', $4, $5)",
        )
        .bind(job_id)
        .bind(owner_id)
        .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
        .bind(worker_id)
        .bind(DEFAULT_PROJECT_ID)
        .execute(&pool)
        .await
        .unwrap();
        let attempt_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_id, attempt_number, worker_id, lease_expires_at) \
             VALUES ($1, $2, 1, $3, now() + interval '5 minutes')",
        )
        .bind(attempt_id)
        .bind(job_id)
        .bind(worker_id)
        .execute(&pool)
        .await
        .unwrap();
        let stream_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO observation_streams (id, attempt_id, job_id, worker_id) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(stream_id)
        .bind(attempt_id)
        .bind(job_id)
        .bind(worker_id)
        .execute(&pool)
        .await
        .unwrap();

        let at = Utc::now();
        let body = json!({
            "protocol_version": "1.2",
            "first_sequence": 1,
            "records": [
                {"sequence": 1, "at": at, "record": "param", "name": "epochs", "value": 3},
                {"sequence": 2, "at": at, "record": "metric", "name": "train.loss", "value": 0.25, "step": 0}
            ]
        })
        .to_string();
        let endpoint = |batch_id: Uuid| {
            format!(
                "/api/v1/workers/{worker_id}/observation-streams/{stream_id}/batches/{batch_id}"
            )
        };
        let request = |batch_id: Uuid, body: String| {
            Request::put(endpoint(batch_id))
                .header(
                    AUTHORIZATION,
                    format!("Bearer {}", credential.plaintext.expose()),
                )
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let first_batch = Uuid::new_v4();
        let response = app(None, Some(pool.clone()))
            .oneshot(request(first_batch, body.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app(None, Some(pool.clone()))
            .oneshot(request(first_batch, body.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        sqlx::query(
            "UPDATE job_attempts SET status = 'succeeded', finished_at = now() WHERE id = $1",
        )
        .bind(attempt_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE jobs SET status = 'succeeded', finished_at = now() WHERE id = $1")
            .bind(job_id)
            .execute(&pool)
            .await
            .unwrap();
        let response = app(None, Some(pool.clone()))
            .oneshot(request(Uuid::new_v4(), body.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let changed = body.replace("0.25", "0.5");
        let response = app(None, Some(pool.clone()))
            .oneshot(request(Uuid::new_v4(), changed))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let counts: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM observations WHERE stream_id = $1), \
                    (SELECT count(*) FROM observation_batches WHERE stream_id = $1), \
                    (SELECT accepted_through_sequence FROM observation_streams WHERE id = $1)",
        )
        .bind(stream_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(counts, (2, 2, 2));
    }
}

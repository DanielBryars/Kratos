//! Projecting the observation outbox into `MLflow`.
//!
//! The agent's observations are already durable when they reach `observations`: the worker is
//! told they are accepted and stops holding them. `MLflow` is downstream of that promise, not part
//! of it, so this runs asynchronously and a tracking server that is slow, unreachable or briefly
//! broken must never cost a record or block an attempt.
//!
//! Restart safety comes from the database rather than from anything held in memory. A record is
//! marked `mlflow_applied_at` **only** after `MLflow` has accepted it, and `mlflow_run_id` is
//! written once and reused, so an interrupted pass resumes where it stopped and never creates a
//! second run for one stream.
//!
//! That ordering makes application *at least once*. A crash in the window between `MLflow`
//! accepting a record and this marking it will re-apply that record on the next pass. For a
//! parameter that is idempotent; for a metric `MLflow` appends, so the same point can appear twice.
//! Identical values at an identical step and timestamp are chosen deliberately for that reason:
//! a duplicate is indistinguishable from the original rather than a second, different reading.
//! The alternative — marking first — would silently lose records instead, which is worse.

use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

/// The identity endpoint, which returns a signed ID token rather than an access token. A
/// different path from the one `artifact_storage` uses, and the difference is the whole point:
/// IAP validates an ID token's audience, and an access token is refused.
const METADATA_IDENTITY_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/identity";

/// Renew this far before the token actually expires, so a request never carries one that dies in
/// flight.
const TOKEN_REFRESH_MARGIN: TimeDelta = TimeDelta::minutes(5);

/// How long to wait between passes when there is nothing to do.
const IDLE_INTERVAL_SECONDS: u64 = 15;
/// The longest a failing stream waits before being tried again.
const MAX_BACKOFF_SECONDS: u64 = 300;
/// `mlflow_last_error` is read by a person in the console, so it is bounded to stay readable and
/// to keep a pathological error from filling the column.
const MAX_ERROR_BYTES: usize = 500;
/// How many records one pass applies for a single stream before yielding to the next, so one very
/// long run cannot starve the others. It also bounds how long a stream's advisory lock is held,
/// since each record is a round trip to the tracking server.
const RECORDS_PER_PASS: i64 = 500;
/// The first half of the advisory-lock key, so these locks cannot collide with any other use of
/// the same mechanism elsewhere in the control plane.
const PROJECTOR_LOCK_NAMESPACE: i32 = 0x4b52_4d46;

#[derive(Debug, thiserror::Error)]
pub enum MlflowError {
    #[error("MLflow is unavailable")]
    Unavailable,
    #[error("MLflow refused the request: {0}")]
    Refused(String),
    #[error("MLflow returned something unusable: {0}")]
    InvalidResponse(String),
}

/// How a request proves who it is.
///
/// Absent means no header at all, which is what keeps a local or mocked `MLflow` simple to run.
/// Present means an IAP-validated ID token minted for exactly one audience. A static bearer token
/// is deliberately not an option: it would be a long-lived secret in configuration, and the
/// metadata server already provides something better.
#[derive(Clone, Debug)]
pub enum MlflowAuth {
    None,
    Iap { audience: String },
}

#[derive(Clone, Debug)]
struct CachedToken {
    value: String,
    expires_at: DateTime<Utc>,
}

/// Mints and caches the ID token `MLflow`'s IAP expects.
pub struct TokenSource {
    auth: MlflowAuth,
    client: Client,
    cached: Mutex<Option<CachedToken>>,
    metadata_url: String,
}

impl TokenSource {
    #[must_use]
    pub fn new(auth: MlflowAuth, client: Client) -> Self {
        Self {
            auth,
            client,
            cached: Mutex::new(None),
            metadata_url: METADATA_IDENTITY_URL.to_owned(),
        }
    }

    /// Point the source at a stand-in metadata server, for tests.
    #[must_use]
    pub fn with_metadata_url(mut self, url: String) -> Self {
        self.metadata_url = url;
        self
    }

    /// Forget any cached token, so the next call mints a fresh one.
    ///
    /// Called when IAP refuses: a 401 or 403 against a token we believed was valid means our
    /// belief is wrong, and retrying with the same token would fail identically.
    pub fn invalidate(&self) {
        if let Ok(mut cached) = self.cached.lock() {
            *cached = None;
        }
    }

    /// The `Authorization` value for a request, or None when no audience is configured.
    ///
    /// # Errors
    /// Returns an error when the metadata server is unreachable or returns an unusable token.
    pub async fn header(&self, now: DateTime<Utc>) -> Result<Option<String>, MlflowError> {
        let MlflowAuth::Iap { audience } = &self.auth else {
            return Ok(None);
        };
        if let Ok(cached) = self.cached.lock()
            && let Some(token) = cached.as_ref()
            && token.expires_at - TOKEN_REFRESH_MARGIN > now
        {
            return Ok(Some(format!("Bearer {}", token.value)));
        }
        let response = self
            .client
            .get(&self.metadata_url)
            .query(&[("audience", audience.as_str())])
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|_| MlflowError::Unavailable)?;
        if !response.status().is_success() {
            return Err(MlflowError::Refused(format!(
                "metadata server returned {}",
                response.status()
            )));
        }
        let token = response
            .text()
            .await
            .map_err(|_| MlflowError::Unavailable)?
            .trim()
            .to_owned();
        let expires_at = jwt_expiry(&token)?;
        if let Ok(mut cached) = self.cached.lock() {
            *cached = Some(CachedToken {
                value: token.clone(),
                expires_at,
            });
        }
        Ok(Some(format!("Bearer {token}")))
    }
}

/// Read `exp` from a JWT without verifying it.
///
/// Verification is Google's job and IAP's; this only needs to know when to stop reusing the
/// token. Refusing a token whose expiry cannot be read is deliberate — caching one forever
/// because its claims were unreadable is how an expired credential keeps being sent.
fn jwt_expiry(token: &str) -> Result<DateTime<Utc>, MlflowError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| MlflowError::InvalidResponse("identity token is not a JWT".to_owned()))?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).map_err(|_| {
        MlflowError::InvalidResponse("identity token payload is not base64".to_owned())
    })?;
    let claims: Value = serde_json::from_slice(&decoded).map_err(|_| {
        MlflowError::InvalidResponse("identity token payload is not JSON".to_owned())
    })?;
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or_else(|| MlflowError::InvalidResponse("identity token has no exp".to_owned()))?;
    DateTime::from_timestamp(exp, 0).ok_or_else(|| {
        MlflowError::InvalidResponse("identity token exp is out of range".to_owned())
    })
}

/// A thin `MLflow` REST client, holding only what the projector needs.
pub struct MlflowClient {
    base_url: String,
    client: Client,
    tokens: TokenSource,
}

#[derive(Debug, Deserialize)]
struct CreateRunResponse {
    run: RunEnvelope,
}

#[derive(Debug, Deserialize)]
struct RunEnvelope {
    info: RunInfo,
}

#[derive(Debug, Deserialize)]
struct RunInfo {
    run_id: String,
}

#[derive(Debug, Serialize)]
struct Tag {
    key: String,
    value: String,
}

impl MlflowClient {
    #[must_use]
    pub fn new(base_url: &str, client: Client, tokens: TokenSource) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client,
            tokens,
        }
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, MlflowError> {
        for attempt in 0..2 {
            let header = self.tokens.header(Utc::now()).await?;
            let mut request = self
                .client
                .post(format!("{}{path}", self.base_url))
                .json(body);
            if let Some(value) = header {
                request = request.header(reqwest::header::AUTHORIZATION, value);
            }
            let response = request.send().await.map_err(|_| MlflowError::Unavailable)?;
            let status = response.status();
            if status.is_success() {
                // A successful call with an unreadable body is still a success: `log-parameter`
                // returns `{}`, and the projector only needs the body when creating a run.
                return Ok(response.json::<Value>().await.unwrap_or(Value::Null));
            }
            // IAP refusing a token we believed good means the belief is stale, not that the
            // request is wrong. Mint a fresh one and try exactly once more.
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) && attempt == 0 {
                self.tokens.invalidate();
                continue;
            }
            let detail = response.text().await.unwrap_or_default();
            return Err(MlflowError::Refused(format!(
                "{status}: {}",
                truncate(&detail, 200)
            )));
        }
        Err(MlflowError::Unavailable)
    }

    /// Create the run for one observation stream and return its `MLflow` id.
    async fn create_run(
        &self,
        experiment_id: &str,
        stream: &StreamRecord,
        started_at: DateTime<Utc>,
    ) -> Result<String, MlflowError> {
        let mut tags = vec![
            tag("kratos.job_id", &stream.job_id.to_string()),
            tag("kratos.attempt_id", &stream.attempt_id.to_string()),
            tag("kratos.worker_id", &stream.worker_id.to_string()),
            tag("kratos.stream_id", &stream.id.to_string()),
        ];
        // The lineage the control plane recorded when the attempt started, never anything the
        // workload said about itself. That is what makes it trustworthy as provenance.
        if !stream.dataset_lineage.is_null() {
            tags.push(tag(
                "kratos.dataset_lineage",
                &serde_json::to_string(&stream.dataset_lineage).unwrap_or_else(|_| "[]".to_owned()),
            ));
        }
        let body = json!({
            "experiment_id": experiment_id,
            "start_time": started_at.timestamp_millis(),
            "tags": tags,
        });
        let response = self.post("/api/2.0/mlflow/runs/create", &body).await?;
        let parsed: CreateRunResponse = serde_json::from_value(response)
            .map_err(|error| MlflowError::InvalidResponse(error.to_string()))?;
        Ok(parsed.run.info.run_id)
    }

    async fn log_param(&self, run_id: &str, key: &str, value: &str) -> Result<(), MlflowError> {
        let body = json!({ "run_id": run_id, "key": key, "value": value });
        self.post("/api/2.0/mlflow/runs/log-parameter", &body)
            .await?;
        Ok(())
    }

    async fn log_metric(
        &self,
        run_id: &str,
        key: &str,
        value: f64,
        step: i64,
        at: DateTime<Utc>,
    ) -> Result<(), MlflowError> {
        // The observation's own timestamp and step, not the moment of projection, so a replay
        // after a restart writes the identical point rather than a second, later-looking one.
        let body = json!({
            "run_id": run_id,
            "key": key,
            "value": value,
            "timestamp": at.timestamp_millis(),
            "step": step,
        });
        self.post("/api/2.0/mlflow/runs/log-metric", &body).await?;
        Ok(())
    }
}

fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    value.chars().take(limit).collect()
}

#[derive(Debug, sqlx::FromRow)]
struct StreamRecord {
    id: Uuid,
    attempt_id: Uuid,
    job_id: Uuid,
    worker_id: Uuid,
    dataset_lineage: Value,
}

#[derive(Debug, sqlx::FromRow)]
struct PendingObservation {
    sequence: i64,
    observed_at: DateTime<Utc>,
    record_type: String,
    content: Value,
}

/// Project every stream that has records `MLflow` has not yet accepted.
///
/// Returns how many records were applied, which is what lets the caller idle when there is
/// nothing to do rather than poll a quiet database at full speed.
///
/// # Errors
/// Never returns an error: a stream that fails records its reason and is left for the next pass,
/// because one unreachable tracking server must not stop the others or the caller's loop.
pub async fn project_once(pool: &PgPool, client: &MlflowClient, experiment_id: &str) -> u64 {
    let streams = match sqlx::query_as::<_, StreamRecord>(
        "SELECT s.id, s.attempt_id, s.job_id, s.worker_id, s.dataset_lineage \
         FROM observation_streams s \
         WHERE EXISTS ( \
             SELECT 1 FROM observations o \
             WHERE o.stream_id = s.id AND o.mlflow_applied_at IS NULL \
         ) \
         ORDER BY s.created_at",
    )
    .fetch_all(pool)
    .await
    {
        Ok(streams) => streams,
        Err(error) => {
            warn!(%error, "could not list streams awaiting MLflow");
            return 0;
        }
    };

    let mut applied = 0;
    for stream in streams {
        match project_locked(pool, client, experiment_id, &stream).await {
            Ok(count) => {
                applied += count;
                if count > 0 {
                    // Clearing the error is part of succeeding: a stale message beside a healthy
                    // stream is read as a current fault.
                    let _ = sqlx::query(
                        "UPDATE observation_streams SET mlflow_last_error = NULL, updated_at = now() \
                         WHERE id = $1 AND mlflow_last_error IS NOT NULL",
                    )
                    .bind(stream.id)
                    .execute(pool)
                    .await;
                }
            }
            Err(error) => {
                warn!(stream_id = %stream.id, %error, "MLflow projection failed");
                let _ = sqlx::query(
                    "UPDATE observation_streams SET mlflow_last_error = $2, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(stream.id)
                .bind(truncate(&error.to_string(), MAX_ERROR_BYTES))
                .execute(pool)
                .await;
            }
        }
    }
    applied
}

/// Hold one stream's projector authority for the duration of a pass.
///
/// The control plane runs as more than one instance. Without this, two of them select the same
/// pending stream, each creates its own `MLflow` run, one loses the conditional write, and the
/// loser still applies records to the run it created: two runs, records split between them, and
/// every record applied twice. Restart safety says nothing about that, because nothing crashed.
///
/// An advisory lock rather than `SELECT ... FOR UPDATE` on the stream row, because a pass makes a
/// round trip to the tracking server for every record: holding a row lock across that would keep
/// a data lock open for as long as the network takes. This holds one pooled connection instead,
/// and the writes commit normally on other connections as they go.
///
/// A stream already being projected elsewhere is skipped, not waited for. The next pass will take
/// it if it is still pending, and waiting would serialise every instance behind the slowest.
async fn project_locked(
    pool: &PgPool,
    client: &MlflowClient,
    experiment_id: &str,
    stream: &StreamRecord,
) -> Result<u64, MlflowError> {
    let mut lock = pool
        .acquire()
        .await
        .map_err(|error| MlflowError::InvalidResponse(error.to_string()))?;
    let key = stream.id.to_string();
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, hashtext($2))")
        .bind(PROJECTOR_LOCK_NAMESPACE)
        .bind(&key)
        .fetch_one(&mut *lock)
        .await
        .map_err(|error| MlflowError::InvalidResponse(error.to_string()))?;
    if !held {
        return Ok(0);
    }

    let outcome = project_stream(pool, client, experiment_id, stream).await;

    // Released on every path. An advisory lock is held by the session, and this connection goes
    // back to the pool, so failing to release one would take a stream out of service until the
    // process restarted.
    let _ = sqlx::query("SELECT pg_advisory_unlock($1, hashtext($2))")
        .bind(PROJECTOR_LOCK_NAMESPACE)
        .bind(&key)
        .execute(&mut *lock)
        .await;
    outcome
}

async fn project_stream(
    pool: &PgPool,
    client: &MlflowClient,
    experiment_id: &str,
    stream: &StreamRecord,
) -> Result<u64, MlflowError> {
    // Re-read under the lock rather than trusting the row this pass was listed from: another
    // instance may have created the run between the listing and the lock being granted.
    let existing: Option<String> =
        sqlx::query_scalar("SELECT mlflow_run_id FROM observation_streams WHERE id = $1")
            .bind(stream.id)
            .fetch_one(pool)
            .await
            .map_err(|error| MlflowError::InvalidResponse(error.to_string()))?;

    let run_id = if let Some(existing) = existing {
        existing
    } else {
        let created = client.create_run(experiment_id, stream, Utc::now()).await?;
        // The database decides which run this stream has, not this function. If the conditional
        // write loses -- which the lock should prevent, and which a future refactor might
        // reintroduce -- the winner's id is adopted and the run just created is abandoned empty.
        // Using the local value instead is exactly how records end up split across two runs.
        let winner: Option<String> = sqlx::query_scalar(
            "UPDATE observation_streams \
             SET mlflow_run_id = $2, mlflow_created_at = now(), updated_at = now() \
             WHERE id = $1 AND mlflow_run_id IS NULL \
             RETURNING mlflow_run_id",
        )
        .bind(stream.id)
        .bind(&created)
        .fetch_optional(pool)
        .await
        .map_err(|error| MlflowError::InvalidResponse(error.to_string()))?
        .flatten();
        if let Some(id) = winner {
            info!(stream_id = %stream.id, run_id = %id, "created MLflow run");
            id
        } else {
            {
                let adopted: Option<String> = sqlx::query_scalar(
                    "SELECT mlflow_run_id FROM observation_streams WHERE id = $1",
                )
                .bind(stream.id)
                .fetch_one(pool)
                .await
                .map_err(|error| MlflowError::InvalidResponse(error.to_string()))?;
                let adopted = adopted.ok_or_else(|| {
                    MlflowError::InvalidResponse("stream lost its run id".to_owned())
                })?;
                warn!(
                    stream_id = %stream.id,
                    abandoned = %created,
                    adopted = %adopted,
                    "another projector created this stream's run first"
                );
                adopted
            }
        }
    };

    let pending = sqlx::query_as::<_, PendingObservation>(
        "SELECT sequence, observed_at, record_type, content FROM observations \
         WHERE stream_id = $1 AND mlflow_applied_at IS NULL \
         ORDER BY sequence LIMIT $2",
    )
    .bind(stream.id)
    .bind(RECORDS_PER_PASS)
    .fetch_all(pool)
    .await
    .map_err(|error| MlflowError::InvalidResponse(error.to_string()))?;

    let mut applied = 0;
    for observation in pending {
        apply(client, &run_id, &observation).await?;
        // Only now. A crash before this line re-applies the record; a crash after it cannot lose
        // one. At-least-once is the safe direction for evidence.
        sqlx::query(
            "UPDATE observations SET mlflow_applied_at = now() \
             WHERE stream_id = $1 AND sequence = $2",
        )
        .bind(stream.id)
        .bind(observation.sequence)
        .execute(pool)
        .await
        .map_err(|error| MlflowError::InvalidResponse(error.to_string()))?;
        applied += 1;
    }
    Ok(applied)
}

async fn apply(
    client: &MlflowClient,
    run_id: &str,
    observation: &PendingObservation,
) -> Result<(), MlflowError> {
    let name = observation
        .content
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match observation.record_type.as_str() {
        "param" => {
            let value = observation
                .content
                .get("value")
                .map_or_else(String::new, render_param);
            client.log_param(run_id, name, &value).await
        }
        "metric" => {
            let value = observation
                .content
                .get("value")
                .and_then(Value::as_f64)
                .ok_or_else(|| MlflowError::InvalidResponse("metric has no value".to_owned()))?;
            let step = observation
                .content
                .get("step")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            client
                .log_metric(run_id, name, value, step, observation.observed_at)
                .await
        }
        // A progress record is the workload saying where it is, not a measurement. MLflow has no
        // home for it that would not pretend otherwise, so it is marked applied without being
        // sent: the record stays in the outbox as evidence, and the run is not polluted.
        _ => Ok(()),
    }
}

/// `MLflow` parameter values are strings. A JSON string is passed through as itself rather than
/// re-encoded, so `"v2.1"` does not arrive as `"\"v2.1\""`.
fn render_param(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Run the projector until the process stops.
///
/// Backs off when a pass does nothing, so a quiet database is not polled hard, and resets as soon
/// as there is work again.
pub async fn run_projector(pool: PgPool, client: MlflowClient, experiment_id: String) {
    let mut backoff = IDLE_INTERVAL_SECONDS;
    loop {
        let applied = project_once(&pool, &client, &experiment_id).await;
        if applied > 0 {
            backoff = IDLE_INTERVAL_SECONDS;
        } else {
            backoff = (backoff * 2).min(MAX_BACKOFF_SECONDS);
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
    }
}

/// Build a projector from the environment, or None when `MLflow` is not configured.
///
/// Absent configuration is a supported state rather than an error: a deployment without `MLflow`
/// should run everything else unchanged.
#[must_use]
pub fn from_environment() -> Option<(MlflowClient, String)> {
    let base_url = std::env::var("KRATOS_MLFLOW_URL").ok()?;
    let auth = match std::env::var("KRATOS_MLFLOW_IAP_AUDIENCE") {
        Ok(audience) if !audience.trim().is_empty() => MlflowAuth::Iap { audience },
        _ => MlflowAuth::None,
    };
    let experiment_id =
        std::env::var("KRATOS_MLFLOW_EXPERIMENT_ID").unwrap_or_else(|_| "0".to_owned());
    let http = Client::new();
    let tokens = TokenSource::new(auth, http.clone());
    Some((MlflowClient::new(&base_url, http, tokens), experiment_id))
}

#[cfg(test)]
mod tests;

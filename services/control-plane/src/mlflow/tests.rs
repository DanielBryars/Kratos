//! The projector against a real HTTP server and a real database.
//!
//! The stand-in `MLflow` is an actual axum server on a loopback port rather than a mocked trait,
//! because the things most likely to be wrong here are the parts a trait would hide: whether the
//! `Authorization` header is attached at all, what happens on a 401, and whether the JSON shape
//! `MLflow` returns is parsed. A fake that returned `Ok(())` would prove none of them.

use std::sync::{Arc, Mutex};

use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use uuid::Uuid;

use super::*;

/// Every request the stand-in received, so a test can assert on what was actually sent.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, Value, Option<String>)>>,
    fail_next: Mutex<Option<StatusCode>>,
    created_runs: Mutex<u32>,
    /// When set, the first run creation waits here until a test releases it. That is what makes
    /// two projectors genuinely concurrent: without it they run one after the other and the test
    /// passes whether or not anything is serialised.
    hold_create: Mutex<Option<Arc<tokio::sync::Notify>>>,
    create_started: Arc<tokio::sync::Notify>,
}

impl Recorder {
    fn paths(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _, _)| path.clone())
            .collect()
    }

    fn bodies(&self, path: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen, _, _)| seen == path)
            .map(|(_, body, _)| body.clone())
            .collect()
    }

    fn auth_headers(&self) -> Vec<Option<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, auth)| auth.clone())
            .collect()
    }
}

async fn handle(
    State(recorder): State<Arc<Recorder>>,
    headers: HeaderMap,
    path: axum::extract::OriginalUri,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let auth = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    recorder
        .calls
        .lock()
        .unwrap()
        .push((path.0.path().to_owned(), body, auth));
    if let Some(status) = recorder.fail_next.lock().unwrap().take() {
        return (status, Json(json!({"error": "refused"})));
    }
    if path.0.path().ends_with("/runs/create") {
        let gate = recorder.hold_create.lock().unwrap().take();
        if let Some(gate) = gate {
            recorder.create_started.notify_waiters();
            gate.notified().await;
        }
        let run_id = {
            let mut created = recorder.created_runs.lock().unwrap();
            *created += 1;
            format!("run-{created}")
        };
        return (
            StatusCode::OK,
            Json(json!({"run": {"info": {"run_id": run_id}}})),
        );
    }
    (StatusCode::OK, Json(json!({})))
}

/// Start the stand-in and return its base URL.
async fn mlflow_server(recorder: Arc<Recorder>) -> String {
    let app = Router::new()
        .route("/api/2.0/mlflow/runs/create", post(handle))
        .route("/api/2.0/mlflow/runs/log-parameter", post(handle))
        .route("/api/2.0/mlflow/runs/log-metric", post(handle))
        .with_state(recorder);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

/// A JWT that is only ever read for its `exp`, which is all the token source does with it.
fn identity_token(expires_in_seconds: i64) -> String {
    let claims =
        json!({ "exp": (Utc::now() + TimeDelta::seconds(expires_in_seconds)).timestamp() });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    format!("header.{payload}.signature")
}

/// A stand-in metadata server that counts how often it was asked.
async fn metadata_server(token: String, hits: Arc<Mutex<u32>>) -> String {
    let app = Router::new().route(
        "/identity",
        axum::routing::get(move |headers: HeaderMap| {
            let token = token.clone();
            let hits = hits.clone();
            async move {
                // The metadata server requires this header; forgetting it is a real failure mode.
                assert_eq!(
                    headers.get("metadata-flavor").and_then(|v| v.to_str().ok()),
                    Some("Google"),
                    "the metadata server requires Metadata-Flavor: Google"
                );
                *hits.lock().unwrap() += 1;
                token
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/identity")
}

fn client_for(base_url: &str, auth: MlflowAuth, metadata_url: Option<String>) -> MlflowClient {
    let http = Client::new();
    let mut tokens = TokenSource::new(auth, http.clone());
    if let Some(url) = metadata_url {
        tokens = tokens.with_metadata_url(url);
    }
    MlflowClient::new(base_url, http, tokens)
}

// ---------------------------------------------------------------------------------------------
// The token source
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn no_audience_means_no_authorization_header() {
    // Local and mocked MLflow stay simple, which is the stated reason the audience is optional.
    let source = TokenSource::new(MlflowAuth::None, Client::new());
    assert!(source.header(Utc::now()).await.unwrap().is_none());
}

#[tokio::test]
async fn an_audience_mints_a_token_once_and_reuses_it() {
    let hits = Arc::new(Mutex::new(0));
    let url = metadata_server(identity_token(3600), hits.clone()).await;
    let source = TokenSource::new(
        MlflowAuth::Iap {
            audience: "client-id".to_owned(),
        },
        Client::new(),
    )
    .with_metadata_url(url);

    let first = source.header(Utc::now()).await.unwrap().unwrap();
    let second = source.header(Utc::now()).await.unwrap().unwrap();

    assert!(first.starts_with("Bearer "));
    assert_eq!(first, second);
    assert_eq!(*hits.lock().unwrap(), 1, "a cached token must be reused");
}

#[tokio::test]
async fn a_token_is_renewed_before_it_expires_rather_than_after() {
    // Renewing on expiry would send a token that dies in flight. The margin is the point.
    let hits = Arc::new(Mutex::new(0));
    let url = metadata_server(identity_token(600), hits.clone()).await;
    let source = TokenSource::new(
        MlflowAuth::Iap {
            audience: "client-id".to_owned(),
        },
        Client::new(),
    )
    .with_metadata_url(url);

    source.header(Utc::now()).await.unwrap();
    // Still valid, but inside the refresh margin.
    source
        .header(Utc::now() + TimeDelta::seconds(400))
        .await
        .unwrap();

    assert_eq!(
        *hits.lock().unwrap(),
        2,
        "a token near expiry must be replaced"
    );
}

#[tokio::test]
async fn an_unreadable_token_is_refused_rather_than_cached() {
    // Caching a token whose expiry cannot be read is how an expired credential keeps being sent.
    let hits = Arc::new(Mutex::new(0));
    let url = metadata_server("not-a-jwt".to_owned(), hits).await;
    let source = TokenSource::new(
        MlflowAuth::Iap {
            audience: "client-id".to_owned(),
        },
        Client::new(),
    )
    .with_metadata_url(url);

    assert!(source.header(Utc::now()).await.is_err());
}

#[tokio::test]
async fn the_token_is_attached_to_mlflow_requests() {
    let recorder = Arc::new(Recorder::default());
    let base = mlflow_server(recorder.clone()).await;
    let metadata = metadata_server(identity_token(3600), Arc::new(Mutex::new(0))).await;
    let client = client_for(
        &base,
        MlflowAuth::Iap {
            audience: "client-id".to_owned(),
        },
        Some(metadata),
    );

    client.log_param("run-1", "k", "v").await.unwrap();

    let headers = recorder.auth_headers();
    assert_eq!(headers.len(), 1);
    assert!(
        headers[0]
            .as_deref()
            .is_some_and(|v| v.starts_with("Bearer ")),
        "IAP needs the token on every call, got {headers:?}"
    );
}

#[tokio::test]
async fn a_refusal_mints_a_fresh_token_and_retries_exactly_once() {
    // A 403 against a token we believed good means the belief is stale, not the request wrong.
    let recorder = Arc::new(Recorder::default());
    *recorder.fail_next.lock().unwrap() = Some(StatusCode::FORBIDDEN);
    let base = mlflow_server(recorder.clone()).await;
    let hits = Arc::new(Mutex::new(0));
    let metadata = metadata_server(identity_token(3600), hits.clone()).await;
    let client = client_for(
        &base,
        MlflowAuth::Iap {
            audience: "client-id".to_owned(),
        },
        Some(metadata),
    );

    client.log_param("run-1", "k", "v").await.unwrap();

    assert_eq!(recorder.paths().len(), 2, "one refusal, one retry");
    assert_eq!(*hits.lock().unwrap(), 2, "the retry must use a fresh token");
}

#[tokio::test]
async fn a_second_refusal_is_reported_rather_than_retried_forever() {
    let recorder = Arc::new(Recorder::default());
    let base = mlflow_server(recorder.clone()).await;
    let metadata = metadata_server(identity_token(3600), Arc::new(Mutex::new(0))).await;
    let client = client_for(
        &base,
        MlflowAuth::Iap {
            audience: "client-id".to_owned(),
        },
        Some(metadata),
    );
    // Fail both the first call and the retry.
    *recorder.fail_next.lock().unwrap() = Some(StatusCode::FORBIDDEN);
    let first = client.log_param("run-1", "k", "v").await;
    assert!(
        first.is_ok(),
        "the first refusal is retried and then succeeds"
    );

    *recorder.fail_next.lock().unwrap() = Some(StatusCode::BAD_REQUEST);
    let refused = client.log_param("run-1", "k", "v").await;
    assert!(
        matches!(refused, Err(MlflowError::Refused(_))),
        "{refused:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The projector
// ---------------------------------------------------------------------------------------------

async fn seed_stream(pool: &PgPool, lineage: Value) -> Uuid {
    let owner_id = Uuid::new_v4();
    let worker_id = Uuid::new_v4();
    let job_id = Uuid::new_v4();
    let attempt_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();
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
    .bind(crate::projects::DEFAULT_PROJECT_ID)
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
    .bind(crate::projects::DEFAULT_PROJECT_ID)
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
    sqlx::query(
        "INSERT INTO observation_streams (id, attempt_id, job_id, worker_id, dataset_lineage) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(stream_id)
    .bind(attempt_id)
    .bind(job_id)
    .bind(worker_id)
    .bind(lineage)
    .execute(pool)
    .await
    .unwrap();
    stream_id
}

async fn seed_observation(pool: &PgPool, stream_id: Uuid, sequence: i64, content: Value) {
    let record_type = content
        .get("record")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();
    sqlx::query(
        "INSERT INTO observations \
         (stream_id, sequence, first_batch_id, observed_at, record_type, content, content_sha256) \
         VALUES ($1, $2, $3, now(), $4, $5, $6)",
    )
    .bind(stream_id)
    .bind(sequence)
    .bind(Uuid::new_v4())
    .bind(record_type)
    .bind(&content)
    .bind("a".repeat(64))
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn a_stream_gets_one_run_tagged_with_its_trusted_identities(pool: PgPool) {
    let lineage = json!([{ "alias": "training", "dataset_version_id": "v1" }]);
    let stream_id = seed_stream(&pool, lineage.clone()).await;
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "param", "name": "training.seed", "value": 7}),
    )
    .await;
    let recorder = Arc::new(Recorder::default());
    let base = mlflow_server(recorder.clone()).await;
    let client = client_for(&base, MlflowAuth::None, None);

    let applied = project_once(&pool, &client, "0").await;

    assert_eq!(applied, 1);
    let created = recorder.bodies("/api/2.0/mlflow/runs/create");
    assert_eq!(created.len(), 1);
    let tags: Vec<(String, String)> = created[0]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| {
            (
                tag["key"].as_str().unwrap().to_owned(),
                tag["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let keys: Vec<&str> = tags.iter().map(|(key, _)| key.as_str()).collect();
    assert!(keys.contains(&"kratos.job_id"), "{keys:?}");
    assert!(keys.contains(&"kratos.attempt_id"), "{keys:?}");
    assert!(keys.contains(&"kratos.worker_id"), "{keys:?}");
    // The lineage the control plane recorded, never what the workload said about itself.
    let (_, lineage_tag) = tags
        .iter()
        .find(|(key, _)| key == "kratos.dataset_lineage")
        .expect("lineage must be tagged");
    assert!(lineage_tag.contains("training"), "{lineage_tag}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_second_pass_reuses_the_run_and_does_not_reapply(pool: PgPool) {
    // Restart safety: the run id is written before any record is applied, so an interrupted pass
    // resumes rather than creating a second run for one stream.
    let stream_id = seed_stream(&pool, json!([])).await;
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "param", "name": "a", "value": "1"}),
    )
    .await;
    let recorder = Arc::new(Recorder::default());
    let base = mlflow_server(recorder.clone()).await;
    let client = client_for(&base, MlflowAuth::None, None);

    assert_eq!(project_once(&pool, &client, "0").await, 1);
    assert_eq!(
        project_once(&pool, &client, "0").await,
        0,
        "nothing left to apply"
    );

    assert_eq!(
        recorder.bodies("/api/2.0/mlflow/runs/create").len(),
        1,
        "one stream is one run, however many passes run"
    );
    assert_eq!(
        recorder.bodies("/api/2.0/mlflow/runs/log-parameter").len(),
        1
    );
    let run_id: Option<String> =
        sqlx::query_scalar("SELECT mlflow_run_id FROM observation_streams WHERE id = $1")
            .bind(stream_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(run_id.as_deref(), Some("run-1"));
}

#[sqlx::test(migrations = "./migrations")]
async fn records_are_applied_in_sequence_and_marked_only_after_acceptance(pool: PgPool) {
    let stream_id = seed_stream(&pool, json!([])).await;
    for (sequence, name) in [(1_i64, "first"), (2, "second"), (3, "third")] {
        seed_observation(
            &pool,
            stream_id,
            sequence,
            json!({"schema_version": "1.0", "record": "param", "name": name, "value": "x"}),
        )
        .await;
    }
    let recorder = Arc::new(Recorder::default());
    let base = mlflow_server(recorder.clone()).await;
    let client = client_for(&base, MlflowAuth::None, None);

    project_once(&pool, &client, "0").await;

    let names: Vec<String> = recorder
        .bodies("/api/2.0/mlflow/runs/log-parameter")
        .iter()
        .map(|body| body["key"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        vec!["first", "second", "third"],
        "order is the contract"
    );
    let unapplied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM observations WHERE stream_id = $1 AND mlflow_applied_at IS NULL",
    )
    .bind(stream_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unapplied, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failure_leaves_the_record_unapplied_and_records_why(pool: PgPool) {
    // The outbox is the safety net: a record MLflow did not accept must still be pending, or the
    // evidence is lost the moment the tracking server has a bad minute.
    let stream_id = seed_stream(&pool, json!([])).await;
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "param", "name": "a", "value": "1"}),
    )
    .await;
    let recorder = Arc::new(Recorder::default());
    let base = mlflow_server(recorder.clone()).await;
    let client = client_for(&base, MlflowAuth::None, None);
    // Refuse the run creation itself.
    *recorder.fail_next.lock().unwrap() = Some(StatusCode::BAD_REQUEST);

    assert_eq!(project_once(&pool, &client, "0").await, 0);

    let (unapplied, error): (i64, Option<String>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM observations \
                 WHERE stream_id = $1 AND mlflow_applied_at IS NULL), \
                s.mlflow_last_error \
         FROM observation_streams s WHERE s.id = $1",
    )
    .bind(stream_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unapplied, 1, "the record must still be pending");
    let error = error.expect("the reason must be recorded for the console");
    assert!(
        error.len() <= MAX_ERROR_BYTES,
        "the message must stay readable"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_recovered_stream_clears_its_error(pool: PgPool) {
    // A stale message beside a healthy stream is read as a current fault.
    let stream_id = seed_stream(&pool, json!([])).await;
    sqlx::query(
        "UPDATE observation_streams SET mlflow_last_error = 'an old failure' WHERE id = $1",
    )
    .bind(stream_id)
    .execute(&pool)
    .await
    .unwrap();
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "param", "name": "a", "value": "1"}),
    )
    .await;
    let client = client_for(
        &mlflow_server(Arc::new(Recorder::default())).await,
        MlflowAuth::None,
        None,
    );

    project_once(&pool, &client, "0").await;

    let error: Option<String> =
        sqlx::query_scalar("SELECT mlflow_last_error FROM observation_streams WHERE id = $1")
            .bind(stream_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(error, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_metric_carries_its_own_timestamp_and_step(pool: PgPool) {
    // So a replay after a crash writes the identical point rather than a second, later-looking
    // reading. At-least-once is only safe if the duplicate is indistinguishable.
    let stream_id = seed_stream(&pool, json!([])).await;
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "metric", "name": "train.loss",
               "value": 0.25, "step": 120}),
    )
    .await;
    let recorder = Arc::new(Recorder::default());
    let client = client_for(
        &mlflow_server(recorder.clone()).await,
        MlflowAuth::None,
        None,
    );

    project_once(&pool, &client, "0").await;

    let logged = recorder.bodies("/api/2.0/mlflow/runs/log-metric");
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0]["key"], "train.loss");
    assert!((logged[0]["value"].as_f64().unwrap() - 0.25).abs() < f64::EPSILON);
    assert_eq!(logged[0]["step"], 120);
    let observed_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT observed_at FROM observations WHERE stream_id = $1")
            .bind(stream_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        logged[0]["timestamp"].as_i64().unwrap(),
        observed_at.timestamp_millis(),
        "the observation's own time, not the moment of projection"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_string_parameter_is_not_double_encoded(pool: PgPool) {
    let stream_id = seed_stream(&pool, json!([])).await;
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "param", "name": "workload.version",
               "value": "1.2.0"}),
    )
    .await;
    let recorder = Arc::new(Recorder::default());
    let client = client_for(
        &mlflow_server(recorder.clone()).await,
        MlflowAuth::None,
        None,
    );

    project_once(&pool, &client, "0").await;

    let logged = recorder.bodies("/api/2.0/mlflow/runs/log-parameter");
    assert_eq!(logged[0]["value"], "1.2.0", "not \"\\\"1.2.0\\\"\"");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_progress_record_is_marked_without_being_sent(pool: PgPool) {
    // Progress is the workload saying where it is, not a measurement. MLflow has no home for it
    // that would not pretend otherwise, and leaving it pending forever would look like a stuck
    // projector.
    let stream_id = seed_stream(&pool, json!([])).await;
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "progress", "step": 3, "total_steps": 10}),
    )
    .await;
    let recorder = Arc::new(Recorder::default());
    let client = client_for(
        &mlflow_server(recorder.clone()).await,
        MlflowAuth::None,
        None,
    );

    assert_eq!(project_once(&pool, &client, "0").await, 1);

    assert!(
        recorder
            .bodies("/api/2.0/mlflow/runs/log-metric")
            .is_empty()
    );
    assert!(
        recorder
            .bodies("/api/2.0/mlflow/runs/log-parameter")
            .is_empty()
    );
    let unapplied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM observations WHERE stream_id = $1 AND mlflow_applied_at IS NULL",
    )
    .bind(stream_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unapplied, 0, "it must not sit pending forever");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_stream_with_nothing_pending_is_left_alone(pool: PgPool) {
    seed_stream(&pool, json!([])).await;
    let recorder = Arc::new(Recorder::default());
    let client = client_for(
        &mlflow_server(recorder.clone()).await,
        MlflowAuth::None,
        None,
    );

    assert_eq!(project_once(&pool, &client, "0").await, 0);

    assert!(
        recorder.paths().is_empty(),
        "a quiet stream must not create a run it will never use"
    );
}

// ---------------------------------------------------------------------------------------------
// Two instances
// ---------------------------------------------------------------------------------------------

/// The control plane runs as more than one instance, and restart safety says nothing about that
/// because nothing has crashed. Two projectors selecting one pending stream must not create two
/// runs, split records between them, or apply anything twice.
///
/// The overlap is forced rather than hoped for: the stand-in holds the first `runs/create` open
/// until the second projector has had its turn. Joined futures alone would run one after the
/// other and prove nothing, which is how the first version of this test would have passed
/// against the defect it exists to catch.
#[sqlx::test(migrations = "./migrations")]
async fn two_projectors_produce_one_run_and_apply_each_record_once(pool: PgPool) {
    let stream_id = seed_stream(&pool, json!([])).await;
    for sequence in 1..=4_i64 {
        seed_observation(
            &pool,
            stream_id,
            sequence,
            json!({"schema_version": "1.0", "record": "param",
                   "name": format!("p{sequence}"), "value": "v"}),
        )
        .await;
    }

    let recorder = Arc::new(Recorder::default());
    let gate = Arc::new(tokio::sync::Notify::new());
    *recorder.hold_create.lock().unwrap() = Some(gate.clone());
    let started = recorder.create_started.clone();
    let base = mlflow_server(recorder.clone()).await;

    let first = client_for(&base, MlflowAuth::None, None);
    let second = client_for(&base, MlflowAuth::None, None);
    let pool_a = pool.clone();
    let pool_b = pool.clone();

    let waiting = started.notified();
    let leader = tokio::spawn(async move { project_once(&pool_a, &first, "0").await });
    // Only once the leader is inside `runs/create`, holding the stream, does the second start.
    waiting.await;
    let follower = project_once(&pool_b, &second, "0").await;
    gate.notify_waiters();
    let led = leader.await.unwrap();

    assert_eq!(
        follower, 0,
        "the second projector must find the stream held and do nothing"
    );
    assert_eq!(led, 4, "the first must apply every record");
    assert_eq!(
        recorder.bodies("/api/2.0/mlflow/runs/create").len(),
        1,
        "two instances must not create two runs for one stream"
    );
    assert_eq!(
        recorder.bodies("/api/2.0/mlflow/runs/log-parameter").len(),
        4,
        "each record must be applied exactly once, not once per instance"
    );
    let unapplied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM observations WHERE stream_id = $1 AND mlflow_applied_at IS NULL",
    )
    .bind(stream_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unapplied, 0);
}

/// Waiting would serialise every instance behind the slowest stream. Skipping leaves it for the
/// next pass, seconds away.
#[sqlx::test(migrations = "./migrations")]
async fn a_stream_held_elsewhere_is_skipped_not_waited_for(pool: PgPool) {
    let stream_id = seed_stream(&pool, json!([])).await;
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "param", "name": "a", "value": "1"}),
    )
    .await;

    // Hold the same advisory lock from another connection, as a second instance would.
    let mut holder = pool.acquire().await.unwrap();
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, hashtext($2))")
        .bind(PROJECTOR_LOCK_NAMESPACE)
        .bind(stream_id.to_string())
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    assert!(held);

    let recorder = Arc::new(Recorder::default());
    let client = client_for(
        &mlflow_server(recorder.clone()).await,
        MlflowAuth::None,
        None,
    );
    assert_eq!(project_once(&pool, &client, "0").await, 0);
    assert!(
        recorder.paths().is_empty(),
        "a held stream must not even create its run"
    );

    sqlx::query("SELECT pg_advisory_unlock($1, hashtext($2))")
        .bind(PROJECTOR_LOCK_NAMESPACE)
        .bind(stream_id.to_string())
        .execute(&mut *holder)
        .await
        .unwrap();
    assert_eq!(project_once(&pool, &client, "0").await, 1);
}

/// Belt and braces for the defect this fix is about: if the conditional write ever loses, the run
/// this pass created must be abandoned and the stored one used. Applying to the local value is
/// exactly how records end up split across two runs.
#[sqlx::test(migrations = "./migrations")]
async fn a_loser_adopts_the_winners_run_rather_than_its_own(pool: PgPool) {
    let stream_id = seed_stream(&pool, json!([])).await;
    seed_observation(
        &pool,
        stream_id,
        1,
        json!({"schema_version": "1.0", "record": "param", "name": "a", "value": "1"}),
    )
    .await;
    // Another instance got there first.
    sqlx::query(
        "UPDATE observation_streams SET mlflow_run_id = 'run-from-elsewhere', \
         mlflow_created_at = now() WHERE id = $1",
    )
    .bind(stream_id)
    .execute(&pool)
    .await
    .unwrap();

    let recorder = Arc::new(Recorder::default());
    let client = client_for(
        &mlflow_server(recorder.clone()).await,
        MlflowAuth::None,
        None,
    );
    assert_eq!(project_once(&pool, &client, "0").await, 1);

    assert!(
        recorder.bodies("/api/2.0/mlflow/runs/create").is_empty(),
        "an existing run must not be recreated"
    );
    let logged = recorder.bodies("/api/2.0/mlflow/runs/log-parameter");
    assert_eq!(logged[0]["run_id"], "run-from-elsewhere");
}

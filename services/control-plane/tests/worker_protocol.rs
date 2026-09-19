use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use kratos_control_plane::{
    app,
    credentials::{CredentialKind, issue},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

fn capabilities() -> Value {
    json!({
        "protocol_version": "1.0",
        "collected_at": "2026-09-19T08:00:00Z",
        "hostname": "home-gpu-1",
        "operating_system": "linux",
        "operating_system_version": "6.8",
        "architecture": "x86_64",
        "logical_cpu_count": 16,
        "memory_total_bytes": 34_359_738_368_u64,
        "storage_available_bytes": 107_374_182_400_u64,
        "python_version": "3.12.7",
        "gpus": [{
            "index": 0,
            "name": "Test GPU",
            "memory_total_bytes": 8_589_934_592_u64,
            "driver_version": "560.35"
        }],
        "gpu_health": {
            "status": "healthy",
            "detail": "GPU computation passed on Test GPU in 10.000 ms",
            "evidence": {
                "schema_version": "1.0",
                "status": "healthy",
                "checked_at": "2026-09-19T07:59:59Z",
                "image_reference": concat!("example.test/health@sha256:",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                "device_index": 0,
                "device_name": "Test GPU",
                "operation": "matrix multiplication",
                "matrix_size": 512,
                "max_absolute_error": 0.0,
                "duration_ms": 10.0,
                "cuda_driver_api_version": "13.3",
                "cuda_runtime_version": "12.9"
            }
        }
    })
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[allow(clippy::too_many_lines)]
#[sqlx::test(migrations = "./migrations")]
async fn enrolment_and_heartbeat_are_transactional_and_replay_safe(pool: PgPool) {
    let owner_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name) \
         VALUES ($1, 'test', $2, 'Test Owner')",
    )
    .bind(owner_id)
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .unwrap();

    let enrolment = issue(CredentialKind::Enrolment).unwrap();
    sqlx::query(
        "INSERT INTO worker_enrolments (id, owner_identity_id, token_verifier, expires_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(enrolment.id)
    .bind(owner_id)
    .bind(&enrolment.verifier)
    .bind(Utc::now() + Duration::minutes(10))
    .execute(&pool)
    .await
    .unwrap();

    let agent_instance_id = Uuid::new_v4();
    let enrolment_body = json!({
        "protocol_version": "1.0",
        "agent_instance_id": agent_instance_id,
        "display_name": "Home GPU 1",
        "capabilities": capabilities()
    });
    let enrol = app(None, Some(pool.clone()))
        .oneshot(
            Request::post("/api/v1/worker-enrolments")
                .header(
                    "authorization",
                    format!("Bearer {}", enrolment.plaintext.expose()),
                )
                .header("content-type", "application/json")
                .body(Body::from(enrolment_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(enrol.status(), StatusCode::CREATED);
    let enrol_json = response_json(enrol).await;
    assert_eq!(enrol_json["state"], "unapproved");
    assert_eq!(enrol_json["heartbeat_interval_seconds"], 30);
    let worker_id = Uuid::parse_str(enrol_json["worker_id"].as_str().unwrap()).unwrap();
    let worker_credential = enrol_json["worker_credential"].as_str().unwrap().to_owned();
    assert!(worker_credential.starts_with("kwc_"));

    let stored: (bool, String) = sqlx::query_as(
        "SELECT e.consumed_at IS NOT NULL, c.token_verifier \
         FROM worker_enrolments e JOIN worker_credentials c \
           ON c.worker_id = e.consumed_by_worker_id WHERE e.id = $1",
    )
    .bind(enrolment.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stored.0);
    assert_ne!(stored.1, worker_credential);
    assert!(stored.1.starts_with("$argon2"));

    let repeated = app(None, Some(pool.clone()))
        .oneshot(
            Request::post("/api/v1/worker-enrolments")
                .header(
                    "authorization",
                    format!("Bearer {}", enrolment.plaintext.expose()),
                )
                .header("content-type", "application/json")
                .body(Body::from(enrolment_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repeated.status(), StatusCode::CONFLICT);

    for (sequence, expected) in [
        (1, StatusCode::OK),
        (1, StatusCode::OK),
        (0, StatusCode::CONFLICT),
    ] {
        let response = app(None, Some(pool.clone()))
            .oneshot(
                Request::put(format!("/api/v1/workers/{worker_id}/heartbeat"))
                    .header("authorization", format!("Bearer {worker_credential}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "protocol_version": "1.0",
                            "sequence": sequence,
                            "observed_at": Utc::now(),
                            "capabilities": capabilities()
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }

    let persisted: (i64, bool, bool) = sqlx::query_as(
        "SELECT heartbeat_sequence, last_seen_at IS NOT NULL, last_observed_at IS NOT NULL \
         FROM workers WHERE id = $1",
    )
    .bind(worker_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(persisted, (1, true, true));

    let wrong_worker = app(None, Some(pool.clone()))
        .oneshot(
            Request::put(format!("/api/v1/workers/{}/heartbeat", Uuid::new_v4()))
                .header("authorization", format!("Bearer {worker_credential}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "protocol_version": "1.0",
                        "sequence": 2,
                        "observed_at": Utc::now(),
                        "capabilities": capabilities()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_worker.status(), StatusCode::UNAUTHORIZED);

    sqlx::query("UPDATE worker_credentials SET revoked_at = now() WHERE worker_id = $1")
        .bind(worker_id)
        .execute(&pool)
        .await
        .unwrap();
    let revoked = app(None, Some(pool))
        .oneshot(
            Request::put(format!("/api/v1/workers/{worker_id}/heartbeat"))
                .header("authorization", format!("Bearer {worker_credential}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "protocol_version": "1.0",
                        "sequence": 2,
                        "observed_at": Utc::now(),
                        "capabilities": capabilities()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn approved_radio_in_requires_device_key_signature(pool: PgPool) {
    let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
    let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes());
    let agent_instance_id = Uuid::new_v4();
    let response = app(None, Some(pool.clone()))
        .oneshot(
            Request::post("/api/v1/worker-registration-requests")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "protocol_version": "1.0",
                        "agent_instance_id": agent_instance_id,
                        "display_name": "Rented GPU",
                        "public_key": public_key,
                        "capabilities": capabilities()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = response_json(response).await;
    let registration_id = Uuid::parse_str(created["registration_id"].as_str().unwrap()).unwrap();
    assert_eq!(created["confirmation_code"].as_str().unwrap().len(), 9);

    let operator_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name, role) \
         VALUES ($1, 'test', $2, 'Operator', 'operator')",
    )
    .bind(operator_id)
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .unwrap();
    let challenge = [9_u8; 32];
    sqlx::query(
        "UPDATE worker_registration_requests SET approved_at = now(), \
         approved_by_identity_id = $2, claim_challenge = $3 WHERE id = $1",
    )
    .bind(registration_id)
    .bind(operator_id)
    .bind(challenge.as_slice())
    .execute(&pool)
    .await
    .unwrap();

    let challenge_encoded = URL_SAFE_NO_PAD.encode(challenge);
    let message = format!("kratos-worker-claim-v1\n{registration_id}\n{challenge_encoded}");
    let signature = URL_SAFE_NO_PAD.encode(signing_key.sign(message.as_bytes()).to_bytes());
    let claimed = app(None, Some(pool.clone()))
        .oneshot(
            Request::post(format!(
                "/api/v1/worker-registration-requests/{registration_id}/claim"
            ))
            .header("content-type", "application/json")
            .body(Body::from(json!({ "signature": signature }).to_string()))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(claimed.status(), StatusCode::CREATED);
    let body = response_json(claimed).await;
    assert_eq!(body["state"], "idle");
    let worker_credential = body["worker_credential"].as_str().unwrap().to_owned();
    assert!(worker_credential.starts_with("kwc_"));

    let retried = app(None, Some(pool.clone()))
        .oneshot(
            Request::post(format!(
                "/api/v1/worker-registration-requests/{registration_id}/claim"
            ))
            .header("content-type", "application/json")
            .body(Body::from(json!({ "signature": signature }).to_string()))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retried.status(), StatusCode::OK);
    let retried_body = response_json(retried).await;
    assert_eq!(retried_body["worker_credential"], worker_credential);

    let status: String = sqlx::query_scalar(
        "SELECT w.status FROM workers w JOIN worker_registration_requests r \
         ON r.worker_id = w.id WHERE r.id = $1",
    )
    .bind(registration_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "idle");
}

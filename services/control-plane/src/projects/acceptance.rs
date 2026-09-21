//! The ADR-017 acceptance conditions, run against a real `PostgreSQL` database.
//!
//! These are deliberately written against the HTTP surface rather than against the functions
//! underneath it. Every condition here is about what one person can see of another's work, and
//! that answer is assembled from a predicate, a transaction and a status code together. A unit
//! test of any one of those three can pass while the answer is wrong.
//!
//! The concurrency cases matter most. `co_owner_parity` would survive a careless rewrite; two
//! simultaneous claims of one invitation, or two owners removing each other at the same instant,
//! are where "argued correct" and "actually correct" part company, so those are asserted against
//! the database's own state after the race rather than against the responses alone.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::AUTHORIZATION},
};
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

use crate::{
    app_with_human_auth,
    human_auth::{ClientAuthConfig, HumanAuth, HumanIdentity, IdentityVerifier, VerifyError},
    operator::MAX_PENDING_INVITATIONS,
    projects::DEFAULT_PROJECT_ID,
};

/// Accepts exactly one token, so each signed-in person in a test needs their own router.
///
/// That mirrors the real thing more closely than a shared verifier would: two people are two
/// callers with two tokens, and a test that shared one could not tell their answers apart.
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

fn client_config() -> ClientAuthConfig {
    ClientAuthConfig {
        api_key: "test-api-key".to_owned(),
        auth_domain: "example.test".to_owned(),
        project_id: "test-project".to_owned(),
    }
}

/// A router that authenticates one person, where `bootstrap` decides whether that person is
/// allowed to create themselves on first sign-in.
fn router_for(
    pool: &PgPool,
    subject: &str,
    email: &str,
    token: &'static str,
    bootstrap: bool,
) -> axum::Router {
    let identity = HumanIdentity {
        subject: subject.to_owned(),
        email: email.to_owned(),
        display_name: subject.to_owned(),
    };
    // The bootstrap address is compared against the caller's email, so naming a different one is
    // what makes an identity non-bootstrapping.
    let bootstrap_email = if bootstrap {
        email
    } else {
        "nobody@example.com"
    };
    let auth = HumanAuth::new(
        Arc::new(SingleToken { token, identity }),
        bootstrap_email,
        client_config(),
    );
    app_with_human_auth(None, Some(pool.clone()), Some(auth))
}

async fn get(router: &axum::Router, path: &str, token: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::get(path)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, value)
}

async fn post(router: &axum::Router, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::post(path)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Sign in as the founding operator, which creates the identity and its membership.
async fn found(pool: &PgPool) -> (axum::Router, Uuid) {
    let router = router_for(
        pool,
        "founder",
        "founder@example.com",
        "founder-token",
        true,
    );
    let (status, _) = get(&router, "/api/v1/operator/workers", "founder-token").await;
    assert_eq!(status, StatusCode::OK, "founding sign-in should succeed");
    let (id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM human_identities WHERE provider_subject = 'founder'")
            .fetch_one(pool)
            .await
            .unwrap();
    (router, id)
}

/// A job and a worker in the founder's project, so "sees the same set" has something to compare.
async fn seed_resources(pool: &PgPool, owner: Uuid) -> (Uuid, Uuid) {
    let job_id = Uuid::new_v4();
    let worker_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, owner_identity_id, name, image_reference, timeout_seconds, project_id) \
         VALUES ($1, $2, 'Shared job', $3, 120, $4)",
    )
    .bind(job_id)
    .bind(owner)
    .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
    .bind(DEFAULT_PROJECT_ID)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workers \
         (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, \
          capabilities, project_id) \
         VALUES ($1, $2, $3, 'Shared worker', '1.2', 'idle', $4, $5)",
    )
    .bind(worker_id)
    .bind(owner)
    .bind(Uuid::new_v4())
    .bind(worker_capabilities())
    .bind(DEFAULT_PROJECT_ID)
    .execute(pool)
    .await
    .unwrap();
    (job_id, worker_id)
}

/// Issue an invitation as the founder and return its single-use credential.
async fn invite(router: &axum::Router) -> String {
    let (status, body) = post(
        router,
        "/api/v1/operator/project-invitations",
        "founder-token",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "invitation should be created");
    body["invitation_credential"].as_str().unwrap().to_owned()
}

/// A capabilities blob the worker response can actually deserialise.
///
/// `'{}'::jsonb` is enough to satisfy the column and not enough to satisfy the reader: listing a
/// worker with empty capabilities fails while deserialising and surfaces as a 500, which is worth
/// knowing but is not what these tests are about.
fn worker_capabilities() -> Value {
    json!({
        "protocol_version": "1.2",
        "collected_at": "2026-09-21T08:00:00Z",
        "hostname": "shared-worker",
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
                "checked_at": "2026-09-21T07:59:59Z",
                "image_reference": concat!(
                    "example.test/health@sha256:",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                ),
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

/// Strip the fields that describe the viewer rather than the resource.
///
/// `is_self` marks the caller's own membership so the console can refuse to offer "remove me". It
/// is meant to differ between two people looking at the same list, so comparing it would make
/// parity impossible to hold rather than proving it. Everything else must match exactly.
fn without_viewer_fields(body: Value) -> Value {
    match body {
        Value::Array(items) => Value::Array(items.into_iter().map(without_viewer_fields).collect()),
        Value::Object(mut fields) => {
            fields.remove("is_self");
            Value::Object(fields)
        }
        other => other,
    }
}

async fn active_members(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM project_memberships WHERE project_id = $1 AND revoked_at IS NULL",
    )
    .bind(DEFAULT_PROJECT_ID)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------------------------
// Co-owner parity
// ---------------------------------------------------------------------------------------------

/// "A co-owner sees exactly the set the founding owner sees, resource by resource."
///
/// Asserted as equality of the two response bodies rather than as counts. A count would pass while
/// the two people were shown different jobs, which is the failure that matters.
#[sqlx::test(migrations = "./migrations")]
async fn co_owner_sees_exactly_what_the_founder_sees(pool: PgPool) {
    let (founder_router, founder_id) = found(&pool).await;
    let (job_id, worker_id) = seed_resources(&pool, founder_id).await;
    let credential = invite(&founder_router).await;

    let guest = router_for(&pool, "guest", "guest@example.com", "guest-token", false);
    let (status, claimed) = post(
        &guest,
        "/api/v1/project-invitations/claim",
        "guest-token",
        json!({ "invitation_credential": credential }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim should succeed");
    assert_eq!(claimed["project_id"], DEFAULT_PROJECT_ID.to_string());

    for path in [
        "/api/v1/operator/workers",
        "/api/v1/operator/jobs",
        "/api/v1/operator/project-members",
        "/api/v1/operator/project-invitations",
        &format!("/api/v1/operator/jobs/{job_id}/artifacts"),
    ] {
        let (founder_status, founder_body) = get(&founder_router, path, "founder-token").await;
        let (guest_status, guest_body) = get(&guest, path, "guest-token").await;
        assert_eq!(founder_status, StatusCode::OK, "founder {path}");
        assert_eq!(guest_status, StatusCode::OK, "guest {path}");
        assert_eq!(
            without_viewer_fields(founder_body),
            without_viewer_fields(guest_body),
            "co-owner parity at {path}"
        );
    }

    // Parity of access, not only of reading: the access Daniel asked for was "the same as mine",
    // and quarantining a worker is the sharpest edge of that.
    let (status, _) = post(
        &guest,
        &format!("/api/v1/operator/workers/{worker_id}/quarantine"),
        "guest-token",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a co-owner may quarantine a worker");
}

// ---------------------------------------------------------------------------------------------
// Non-member isolation
// ---------------------------------------------------------------------------------------------

/// "An identity in no project sees none of it, and receives the same answer for a resource that
/// exists in another project as for one that does not exist."
///
/// The second half is the one worth testing: a 403 on a real id and a 404 on an invented one would
/// tell a stranger which jobs exist.
#[sqlx::test(migrations = "./migrations")]
async fn a_non_member_cannot_tell_an_existing_resource_from_an_invented_one(pool: PgPool) {
    let (_founder_router, founder_id) = found(&pool).await;
    let (job_id, _worker_id) = seed_resources(&pool, founder_id).await;

    // An identity with no membership: the state a revoked member is left in.
    let stranger_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities \
         (id, provider, provider_subject, display_name, email, role) \
         VALUES ($1, 'identity-platform', 'stranger', 'Stranger', 'stranger@example.com', 'operator')",
    )
    .bind(stranger_id)
    .execute(&pool)
    .await
    .unwrap();

    let stranger = router_for(
        &pool,
        "stranger",
        "stranger@example.com",
        "stranger-token",
        false,
    );

    let (status, body) = get(&stranger, "/api/v1/operator/jobs", "stranger-token").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]), "a non-member sees no jobs");

    let (status, body) = get(&stranger, "/api/v1/operator/workers", "stranger-token").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]), "a non-member sees no workers");

    let real = get(
        &stranger,
        &format!("/api/v1/operator/jobs/{job_id}/artifacts"),
        "stranger-token",
    )
    .await;
    let invented = get(
        &stranger,
        &format!("/api/v1/operator/jobs/{}/artifacts", Uuid::new_v4()),
        "stranger-token",
    )
    .await;
    assert_eq!(
        real.0,
        StatusCode::NOT_FOUND,
        "another project's job is not found, not forbidden"
    );
    assert_eq!(
        real, invented,
        "an existing job and an invented one must be indistinguishable to a non-member"
    );
}

// ---------------------------------------------------------------------------------------------
// Claim races
// ---------------------------------------------------------------------------------------------

/// "Two simultaneous claims of one invitation produce one membership and one distinct conflict."
#[sqlx::test(migrations = "./migrations")]
async fn two_simultaneous_claims_produce_one_membership_and_one_conflict(pool: PgPool) {
    let (founder_router, _founder_id) = found(&pool).await;
    let credential = invite(&founder_router).await;

    let first = router_for(&pool, "first", "first@example.com", "first-token", false);
    let second = router_for(&pool, "second", "second@example.com", "second-token", false);
    let body = json!({ "invitation_credential": credential });

    let (left, right) = tokio::join!(
        post(
            &first,
            "/api/v1/project-invitations/claim",
            "first-token",
            body.clone()
        ),
        post(
            &second,
            "/api/v1/project-invitations/claim",
            "second-token",
            body
        ),
    );

    let mut statuses = [left.0, right.0];
    statuses.sort_by_key(axum::http::StatusCode::as_u16);
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "exactly one claim wins and the other is told the invitation is spent"
    );

    // The founder plus exactly one claimant.
    assert_eq!(active_members(&pool).await, 2, "one membership, not two");

    let consumed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project_invitations WHERE consumed_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(consumed, 1, "the invitation is consumed once");
}

/// "A claim racing a revocation never both succeeds and revokes."
///
/// Either outcome is acceptable -- the race has no correct winner -- so this asserts the pairing
/// rather than a particular result: a claim that succeeded must leave a membership, and a claim
/// that failed must leave none.
#[sqlx::test(migrations = "./migrations")]
async fn a_claim_racing_a_revocation_never_both_succeeds_and_revokes(pool: PgPool) {
    let (founder_router, _founder_id) = found(&pool).await;
    let (status, created) = post(
        &founder_router,
        "/api/v1/operator/project-invitations",
        "founder-token",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let credential = created["invitation_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    let invitation_id = created["invitation_id"].as_str().unwrap().to_owned();

    let guest = router_for(&pool, "guest", "guest@example.com", "guest-token", false);
    let revoke_path = format!("/api/v1/operator/project-invitations/{invitation_id}/revoke");
    let (claim, _revoke) = tokio::join!(
        post(
            &guest,
            "/api/v1/project-invitations/claim",
            "guest-token",
            json!({ "invitation_credential": credential })
        ),
        post(&founder_router, &revoke_path, "founder-token", json!({})),
    );

    let members = active_members(&pool).await;
    if claim.0 == StatusCode::OK {
        assert_eq!(members, 2, "a successful claim must leave its membership");
    } else {
        assert_eq!(
            members, 1,
            "a refused claim must leave the founder alone in the project"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Concurrent last-owner removal
// ---------------------------------------------------------------------------------------------

/// "Two owners removing each other at the same instant leave exactly one owner."
///
/// The rail this proves is the one that cannot be argued: each transaction counts the project's
/// owners while holding a lock, so the second to arrive sees the first's removal and refuses.
#[sqlx::test(migrations = "./migrations")]
async fn two_owners_removing_each_other_leave_exactly_one(pool: PgPool) {
    let (founder_router, founder_id) = found(&pool).await;
    let credential = invite(&founder_router).await;

    let guest = router_for(&pool, "guest", "guest@example.com", "guest-token", false);
    let (status, claimed) = post(
        &guest,
        "/api/v1/project-invitations/claim",
        "guest-token",
        json!({ "invitation_credential": credential }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let guest_id: Uuid = claimed["identity_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(active_members(&pool).await, 2);

    let remove_guest = format!("/api/v1/operator/project-members/{guest_id}/revoke");
    let remove_founder = format!("/api/v1/operator/project-members/{founder_id}/revoke");
    let (left, right) = tokio::join!(
        post(&founder_router, &remove_guest, "founder-token", json!({})),
        post(&guest, &remove_founder, "guest-token", json!({})),
    );

    assert_eq!(
        active_members(&pool).await,
        1,
        "a project must never be left with no owner"
    );
    // Exactly one removal may succeed. The refusal's status is deliberately not pinned: the loser
    // may be told it would leave the project ownerless (409), or -- if it was itself removed first
    // -- that it is no longer a member of anything (403). Both are correct refusals of the same
    // race, and which one occurs depends on who reached the lock first.
    let succeeded = [left.0, right.0]
        .iter()
        .filter(|status| **status == StatusCode::NO_CONTENT)
        .count();
    assert_eq!(
        succeeded, 1,
        "exactly one removal may succeed, got {:?} and {:?}",
        left.0, right.0
    );
    assert!(
        left.0.is_success() != right.0.is_success(),
        "the other removal must be refused, got {:?} and {:?}",
        left.0,
        right.0
    );
}

// ---------------------------------------------------------------------------------------------
// Attribution survives removal
// ---------------------------------------------------------------------------------------------

/// "After a member is revoked, the jobs and artefacts they created still name them."
///
/// This is why membership is revoked rather than deleted, and why `owner_identity_id` stayed on
/// every table instead of being replaced by `project_id`.
#[sqlx::test(migrations = "./migrations")]
async fn attribution_survives_removal(pool: PgPool) {
    let (founder_router, _founder_id) = found(&pool).await;
    let credential = invite(&founder_router).await;

    let guest = router_for(&pool, "guest", "guest@example.com", "guest-token", false);
    let (status, claimed) = post(
        &guest,
        "/api/v1/project-invitations/claim",
        "guest-token",
        json!({ "invitation_credential": credential }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let guest_id: Uuid = claimed["identity_id"].as_str().unwrap().parse().unwrap();

    // A job the guest owns, created while they were a member.
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, owner_identity_id, name, image_reference, timeout_seconds, project_id) \
         VALUES ($1, $2, 'Guest job', $3, 120, $4)",
    )
    .bind(job_id)
    .bind(guest_id)
    .bind(format!("example.test/work@sha256:{}", "b".repeat(64)))
    .bind(DEFAULT_PROJECT_ID)
    .execute(&pool)
    .await
    .unwrap();

    let (status, _) = post(
        &founder_router,
        &format!("/api/v1/operator/project-members/{guest_id}/revoke"),
        "founder-token",
        json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the founder may remove the guest"
    );

    let owner: Uuid = sqlx::query_scalar("SELECT owner_identity_id FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(owner, guest_id, "the job still names who created it");

    let still_named: i64 =
        sqlx::query_scalar("SELECT count(*) FROM human_identities WHERE id = $1")
            .bind(guest_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_named, 1, "the identity row outlives the membership");

    let claimed_event: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE actor_id = $1 AND action = 'project.invitation.claimed'",
    )
    .bind(guest_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(claimed_event, 1, "the audit trail still reads correctly");

    // And the access is genuinely gone.
    let (status, body) = get(&guest, "/api/v1/operator/jobs", "guest-token").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]), "a revoked member sees nothing");
}

// ---------------------------------------------------------------------------------------------
// Backfill
// ---------------------------------------------------------------------------------------------

/// "Every existing owned resource belongs to the default project after migration. A resource left
/// without a `project_id` is invisible to everyone."
///
/// This one cannot be tested on a migrated database, because by then the columns are already NOT
/// NULL and the rows already scoped. It has to build the schema as it stood the moment before,
/// put real rows in it, and then run the migration over them -- which is the only way to find a
/// table the backfill forgot.
#[sqlx::test(migrations = false)]
async fn the_backfill_claims_every_pre_existing_resource(pool: PgPool) {
    for migration in [
        include_str!("../../migrations/202609190001_worker_registry.sql"),
        include_str!("../../migrations/202609190002_worker_observation_time.sql"),
        include_str!("../../migrations/202609190003_human_roles.sql"),
        include_str!("../../migrations/202609190004_worker_registration_requests.sql"),
        include_str!("../../migrations/202609190005_job_queue.sql"),
        include_str!("../../migrations/202609200001_one_active_attempt_per_job.sql"),
        include_str!("../../migrations/202609200010_job_artifacts.sql"),
        include_str!("../../migrations/202609200020_job_lease_recovery.sql"),
        include_str!("../../migrations/202609200021_artifact_storage.sql"),
        include_str!("../../migrations/202609200022_artifact_upload_session_fingerprint.sql"),
        include_str!("../../migrations/202609200023_artifact_delivery_deadline.sql"),
        include_str!("../../migrations/202609200024_observation_streams.sql"),
        include_str!("../../migrations/202609200025_structured_job_results.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }

    let owner_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name, role) \
         VALUES ($1, 'identity-platform', 'founder', 'Founder', 'operator')",
    )
    .bind(owner_id)
    .execute(&pool)
    .await
    .unwrap();

    // One row in each table the migration adds a project to, so a forgotten backfill shows up as
    // a failed NOT NULL rather than as rows nobody can see.
    let worker_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workers \
         (id, owner_identity_id, agent_instance_id, display_name, protocol_version, status, capabilities) \
         VALUES ($1, $2, $3, 'Legacy worker', '1.1', 'idle', '{}'::jsonb)",
    )
    .bind(worker_id)
    .bind(owner_id)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, owner_identity_id, name, image_reference, timeout_seconds) \
         VALUES ($1, $2, 'Legacy job', $3, 120)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(format!("example.test/work@sha256:{}", "a".repeat(64)))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO compute_groups (id, owner_identity_id, name) VALUES ($1, $2, 'Legacy group')",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO worker_enrolments (id, owner_identity_id, token_verifier, expires_at) \
         VALUES ($1, $2, 'verifier', now() + interval '1 hour')",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO worker_registration_requests \
         (id, agent_instance_id, display_name, protocol_version, capabilities, public_key, \
          confirmation_code, expires_at) \
         VALUES ($1, $2, 'Legacy request', '1.0', '{}'::jsonb, $3, 'ABCD-2345', \
                 now() + interval '15 minutes')",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind([1_u8; 32].as_slice())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!(
        "../../migrations/202609200026_projects_and_invitations.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    // Every owned table, named explicitly. A loop over information_schema would pass by finding
    // nothing if the migration failed to add the column at all.
    for table in [
        "workers",
        "jobs",
        "compute_groups",
        "worker_enrolments",
        "worker_registration_requests",
    ] {
        let stray: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE project_id IS DISTINCT FROM $1"
        ))
        .bind(DEFAULT_PROJECT_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stray, 0, "{table} has rows outside the default project");

        let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 1, "{table} lost its row during migration");
    }

    // And the founding owner is a member of it, or the migrated data belongs to nobody.
    let members: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project_memberships \
         WHERE project_id = $1 AND identity_id = $2 AND revoked_at IS NULL",
    )
    .bind(DEFAULT_PROJECT_ID)
    .bind(owner_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        members, 1,
        "the founding owner must be in the default project"
    );
}

/// The pending-invitation list must never carry a credential, in any form.
///
/// A list endpoint is the natural place for one to leak back: the row holds a verifier, the
/// handler selects from that row, and a careless `SELECT *` would put the Argon2id hash on the
/// wire. This asserts against the whole serialised body rather than against named fields, so a
/// field added later is covered without anyone remembering to come back here.
#[sqlx::test(migrations = "./migrations")]
async fn the_invitation_list_never_carries_a_credential(pool: PgPool) {
    let (founder_router, _founder_id) = found(&pool).await;
    let credential = invite(&founder_router).await;

    let (status, body) = get(
        &founder_router,
        "/api/v1/operator/project-invitations",
        "founder-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.as_array().map(Vec::len),
        Some(1),
        "the pending invitation should be listed"
    );

    let serialised = serde_json::to_string(&body).unwrap();
    assert!(
        !serialised.contains(&credential),
        "the list must not return the credential"
    );
    assert!(
        !serialised.contains("kin_"),
        "the list must not contain anything credential-shaped"
    );
    assert!(
        !serialised.contains("$argon2"),
        "the list must not return the stored verifier"
    );

    // Revoking it takes it off the list, which is what makes the list actionable.
    let invitation_id = body[0]["invitation_id"].as_str().unwrap().to_owned();
    let (status, _) = post(
        &founder_router,
        &format!("/api/v1/operator/project-invitations/{invitation_id}/revoke"),
        "founder-token",
        json!({}),
    )
    .await;
    assert!(status.is_success(), "the founder may revoke an invitation");

    let (_, body) = get(
        &founder_router,
        "/api/v1/operator/project-invitations",
        "founder-token",
    )
    .await;
    assert_eq!(body, json!([]), "a revoked invitation is no longer pending");
}

/// The ceiling itself: the eleventh invitation is refused.
#[sqlx::test(migrations = "./migrations")]
async fn the_invitation_ceiling_refuses_the_next_one(pool: PgPool) {
    let (founder_router, _founder_id) = found(&pool).await;
    for _ in 0..MAX_PENDING_INVITATIONS {
        invite(&founder_router).await;
    }

    let (status, _) = post(
        &founder_router,
        "/api/v1/operator/project-invitations",
        "founder-token",
        json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the allowance is full and must say so"
    );

    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project_invitations \
         WHERE project_id = $1 AND consumed_at IS NULL AND revoked_at IS NULL \
           AND expires_at > now()",
    )
    .bind(DEFAULT_PROJECT_ID)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        pending, MAX_PENDING_INVITATIONS,
        "the ceiling must hold exactly, not approximately"
    );

    // Revoking one frees a slot, so the bound is a live allowance rather than a lifetime total.
    let (_, listed) = get(
        &founder_router,
        "/api/v1/operator/project-invitations",
        "founder-token",
    )
    .await;
    let victim = listed[0]["invitation_id"].as_str().unwrap().to_owned();
    let (status, _) = post(
        &founder_router,
        &format!("/api/v1/operator/project-invitations/{victim}/revoke"),
        "founder-token",
        json!({}),
    )
    .await;
    assert!(status.is_success());
    let (status, _) = post(
        &founder_router,
        "/api/v1/operator/project-invitations",
        "founder-token",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "a revoked slot is reusable");
}

/// The pending-invitation ceiling must be counted under a lock, not merely counted.
///
/// ADR-017 bounds the supply so a console session that has been taken over cannot mint an
/// unlimited number of ways in. A count taken outside a lock reads the same nine for every
/// concurrent request, each believes it is the tenth, and all of them commit.
///
/// The interleaving is forced rather than hoped for. Two requests fired with `tokio::join!` do
/// not overlap at all here -- a local `PostgreSQL` answers inside one poll, so the first
/// runs to completion before the second starts, and such a test passes with the lock deleted. So
/// a connection outside the pool holds the project row, the request blocks against it, and the
/// tenth invitation is written and committed underneath before the request is let through. What
/// distinguishes the two implementations is the final count: a request that counted before the
/// writer committed creates an eleventh.
#[sqlx::test(migrations = "./migrations")]
async fn the_invitation_ceiling_counts_under_the_lock(pool: PgPool) {
    let (founder_router, _founder_id) = found(&pool).await;
    for _ in 0..(MAX_PENDING_INVITATIONS - 1) {
        invite(&founder_router).await;
    }

    // The outside writer credits a different identity. Crediting the founder would make its
    // insert wait on the founder's `human_identities` row, which the blocked request already
    // holds `FOR UPDATE` from `authorize_operator` -- a cycle, and PostgreSQL kills one side as
    // a deadlock rather than letting the test observe anything.
    let colleague_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities          (id, provider, provider_subject, display_name, email, role)          VALUES ($1, 'identity-platform', 'colleague', 'Colleague', 'colleague@example.com', 'operator')",
    )
    .bind(colleague_id)
    .execute(&pool)
    .await
    .unwrap();

    let mut outside = PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    let mut blocker = outside.begin().await.unwrap();
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
        .bind(DEFAULT_PROJECT_ID)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let (attempt, ()) = tokio::join!(
        post(
            &founder_router,
            "/api/v1/operator/project-invitations",
            "founder-token",
            json!({})
        ),
        async {
            // Long enough for the request to have reached the lock and stopped there.
            tokio::time::sleep(Duration::from_millis(400)).await;
            sqlx::query(
                "INSERT INTO project_invitations \
                 (id, project_id, created_by_identity_id, token_verifier, expires_at) \
                 VALUES ($1, $2, $3, 'verifier', now() + interval '1 hour')",
            )
            .bind(Uuid::new_v4())
            .bind(DEFAULT_PROJECT_ID)
            .bind(colleague_id)
            .execute(&mut *blocker)
            .await
            .unwrap();
            blocker.commit().await.unwrap();
        },
    );

    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project_invitations \
         WHERE project_id = $1 AND consumed_at IS NULL AND revoked_at IS NULL \
           AND expires_at > now()",
    )
    .bind(DEFAULT_PROJECT_ID)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        pending, MAX_PENDING_INVITATIONS,
        "the ceiling must hold exactly; the request answered {:?}",
        attempt.0
    );
    assert_eq!(
        attempt.0,
        StatusCode::CONFLICT,
        "the request must see the invitation written while it waited"
    );
}

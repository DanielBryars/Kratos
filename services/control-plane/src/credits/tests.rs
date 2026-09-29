//! Credit ledger against a real `PostgreSQL` database: arithmetic at the edges of `i64`, request
//! canonicalisation, idempotent retries, reversal rules, authorisation and its rechecks under lock,
//! the append-only and reconciliation rules the migration enforces, and keyset pagination.
//!
//! The concurrency tests call `record_entry` below authentication. Authentication takes
//! `FOR UPDATE` on the caller's identity row, which an open ledger write holds a foreign-key share
//! lock on, so racing two HTTP requests would serialise on authentication and never reach the
//! ledger locks being tested.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::AUTHORIZATION},
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use tower::ServiceExt;
use uuid::Uuid;

use super::{
    CreateCreditEntryRequest, CreditEntryResponse, CreditsError, EntryRequest, create_entry,
    get_credits, parse_units, record_entry,
};
use crate::{
    AppState, ExternalLinksResponse,
    human_auth::{ClientAuthConfig, HumanAuth, HumanIdentity, IdentityVerifier, VerifyError},
    projects::DEFAULT_PROJECT_ID,
    registry::VerificationGate,
};

const CREDITS: &str = "/api/v1/operator/credits";
const ENTRIES: &str = "/api/v1/operator/credits/entries";
const I64_MAX: &str = "9223372036854775807";
const ACCOUNT_LOCK: &str = "%FROM project_credit_accounts WHERE project_id = $1 FOR UPDATE%";

/// Accepts `token-<subject>` for any subject. Nobody is the bootstrap operator, so a subject must
/// already exist to be authorised.
struct SubjectTokens;

#[async_trait]
impl IdentityVerifier for SubjectTokens {
    async fn verify(&self, id_token: &str) -> Result<HumanIdentity, VerifyError> {
        let Some(subject) = id_token.strip_prefix("token-") else {
            return Err(VerifyError::Rejected);
        };
        Ok(HumanIdentity {
            subject: subject.to_owned(),
            email: format!("{subject}@example.com"),
            display_name: subject.to_owned(),
        })
    }
}

/// The two credit routes on their own, so these tests do not depend on how `app` mounts them.
fn router(pool: &PgPool) -> Router {
    let auth = HumanAuth::new(
        Arc::new(SubjectTokens),
        "bootstrap-nobody@example.invalid",
        ClientAuthConfig {
            api_key: "test-api-key".to_owned(),
            auth_domain: "example.test".to_owned(),
            project_id: "test-project".to_owned(),
        },
    );
    Router::new()
        .route(CREDITS, get(get_credits))
        .route(ENTRIES, post(create_entry))
        .with_state(AppState {
            database: Some(pool.clone()),
            human_auth: Some(auth),
            artifact_storage: None,
            verification_gate: VerificationGate::default(),
            external_links: ExternalLinksResponse {
                grafana_url: None,
                mlflow_url: None,
            },
        })
}

async fn send(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn read(router: &Router, subject: &str, query: &str) -> (StatusCode, Value) {
    let request = Request::get(format!("{CREDITS}{query}"))
        .header(AUTHORIZATION, format!("Bearer token-{subject}"))
        .body(Body::empty())
        .unwrap();
    send(router, request).await
}

/// Post a raw body, sending one `Idempotency-Key` header per entry in `keys`.
async fn post_raw(
    router: &Router,
    subject: &str,
    keys: &[&str],
    body: &str,
) -> (StatusCode, Value) {
    let mut request = Request::post(ENTRIES)
        .header("content-type", "application/json")
        .header(AUTHORIZATION, format!("Bearer token-{subject}"));
    for key in keys {
        request = request.header("idempotency-key", *key);
    }
    send(router, request.body(Body::from(body.to_owned())).unwrap()).await
}

/// Post an entry under `key`.
async fn post_keyed(
    router: &Router,
    subject: &str,
    key: &str,
    body: &Value,
) -> (StatusCode, Value) {
    post_raw(router, subject, &[key], &body.to_string()).await
}

/// Post an entry under a key never used before.
async fn submit(router: &Router, subject: &str, body: &Value) -> (StatusCode, Value) {
    post_keyed(router, subject, &fresh_key(), body).await
}

fn fresh_key() -> String {
    Uuid::new_v4().to_string()
}

fn grant(amount: &str) -> Value {
    json!({"kind": "grant", "amount_units": amount, "reason": "Grant"})
}

fn adjustment(amount: &str) -> Value {
    json!({"kind": "adjustment", "amount_units": amount, "reason": "Adjustment"})
}

fn reversal(target: &Value) -> Value {
    json!({"kind": "reversal", "reason": "Reversal", "reverses_entry_id": target})
}

/// The write below authentication, from a JSON body exactly as the handler would parse it.
async fn record(
    pool: &PgPool,
    identity_id: Uuid,
    project_id: Uuid,
    body: Value,
) -> Result<(StatusCode, CreditEntryResponse), CreditsError> {
    record_keyed(pool, identity_id, project_id, Uuid::new_v4(), body).await
}

async fn record_keyed(
    pool: &PgPool,
    identity_id: Uuid,
    project_id: Uuid,
    key: Uuid,
    body: Value,
) -> Result<(StatusCode, CreditEntryResponse), CreditsError> {
    let request: CreateCreditEntryRequest = serde_json::from_value(body).unwrap();
    let request = EntryRequest::try_from(request).unwrap();
    record_entry(pool, identity_id, project_id, key, &request).await
}

/// Existing operator identity with an active membership of `project_id`.
async fn member(pool: &PgPool, subject: &str, project_id: Uuid) -> Uuid {
    let identity_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO human_identities (id, provider, provider_subject, display_name, role) \
         VALUES ($1, 'identity-platform', $2, $2, 'operator')",
    )
    .bind(identity_id)
    .bind(subject)
    .execute(pool)
    .await
    .unwrap();
    join(pool, identity_id, project_id).await;
    identity_id
}

async fn join(pool: &PgPool, identity_id: Uuid, project_id: Uuid) {
    sqlx::query("INSERT INTO project_memberships (project_id, identity_id) VALUES ($1, $2)")
        .bind(project_id)
        .bind(identity_id)
        .execute(pool)
        .await
        .unwrap();
}

/// A fixture only: nothing in the product grants this yet.
async fn make_manager(pool: &PgPool, project_id: Uuid, identity_id: Uuid) {
    sqlx::query("INSERT INTO project_credit_managers (project_id, identity_id) VALUES ($1, $2)")
        .bind(project_id)
        .bind(identity_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn manager(pool: &PgPool, subject: &str, project_id: Uuid) -> Uuid {
    let identity_id = member(pool, subject, project_id).await;
    make_manager(pool, project_id, identity_id).await;
    identity_id
}

async fn revoke_manager(pool: &PgPool, project_id: Uuid, identity_id: Uuid) {
    sqlx::query(
        "UPDATE project_credit_managers SET revoked_at = now() \
         WHERE project_id = $1 AND identity_id = $2",
    )
    .bind(project_id)
    .bind(identity_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn revoke_membership(pool: &PgPool, project_id: Uuid, identity_id: Uuid) {
    sqlx::query(
        "UPDATE project_memberships SET revoked_at = now(), revoked_by_identity_id = $2 \
         WHERE project_id = $1 AND identity_id = $2",
    )
    .bind(project_id)
    .bind(identity_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn second_project(pool: &PgPool) -> Uuid {
    let project_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects (id, name) VALUES ($1, 'Second project')")
        .bind(project_id)
        .execute(pool)
        .await
        .unwrap();
    project_id
}

async fn count(pool: &PgPool, query: &str) -> i64 {
    sqlx::query_scalar(query).fetch_one(pool).await.unwrap()
}

async fn accounts(pool: &PgPool) -> i64 {
    count(pool, "SELECT count(*) FROM project_credit_accounts").await
}

async fn entries(pool: &PgPool) -> i64 {
    count(pool, "SELECT count(*) FROM project_credit_entries").await
}

async fn audits(pool: &PgPool) -> i64 {
    count(
        pool,
        "SELECT count(*) FROM audit_events WHERE action = 'project.credit_entry.created'",
    )
    .await
}

/// Assert the cached balance and counter agree with the history, and return the balance.
async fn reconciled_balance(pool: &PgPool, project_id: Uuid) -> i64 {
    let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
        "SELECT a.balance_units, a.last_sequence, \
                COALESCE(sum(e.amount_units), 0)::bigint, count(e.id), \
                COALESCE(max(e.sequence), 0) \
         FROM project_credit_accounts a \
         LEFT JOIN project_credit_entries e ON e.project_id = a.project_id \
         WHERE a.project_id = $1 \
         GROUP BY a.project_id",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let (balance, last_sequence, total, rows, newest) = row;
    assert_eq!(balance, total, "balance is the sum of entries");
    assert_eq!(last_sequence, rows, "sequences have no gaps");
    assert_eq!(last_sequence, newest);
    balance
}

fn sequences(body: &Value) -> Vec<&str> {
    body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["sequence"].as_str().unwrap())
        .collect()
}

fn entry_id(entry: &Value) -> Uuid {
    Uuid::parse_str(entry["id"].as_str().unwrap()).unwrap()
}

/// Wait until `count` sessions are blocked on a lock while running a query matching `pattern`.
async fn wait_for_lock_waiters(pool: &PgPool, pattern: &str, count: i64) {
    for _ in 0..400 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE $1",
        )
        .bind(pattern)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("expected {count} sessions waiting on {pattern}");
}

/// Hold the account row the way a write does, with different query text so the holder is never
/// counted as a waiter.
async fn hold_account(pool: &PgPool, project_id: Uuid) -> Transaction<'static, Postgres> {
    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT project_id FROM project_credit_accounts WHERE project_id = $1 FOR NO KEY UPDATE",
    )
    .bind(project_id)
    .execute(&mut *holder)
    .await
    .unwrap();
    holder
}

#[test]
fn units_are_parsed_only_in_canonical_form() {
    assert_eq!(parse_units("0"), Some(0));
    assert_eq!(parse_units("7"), Some(7));
    assert_eq!(parse_units("-7"), Some(-7));
    assert_eq!(parse_units(I64_MAX), Some(i64::MAX));
    assert_eq!(parse_units("-9223372036854775808"), Some(i64::MIN));
    for text in [
        "",
        "-",
        "-0",
        "+1",
        "01",
        "-01",
        "00",
        "1.0",
        "1e3",
        " 1",
        "1 ",
        "0x10",
        "\u{0661}",
        "9223372036854775808",
        "-9223372036854775809",
    ] {
        assert_eq!(parse_units(text), None, "{text:?}");
    }
}

#[sqlx::test(migrations = "./migrations")]
#[allow(clippy::too_many_lines)]
async fn ledger_records_grants_adjustments_and_reversals(pool: PgPool) {
    let owner = manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let router = router(&pool);

    let (status, body) = read(&router, "owner", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "project_id": DEFAULT_PROJECT_ID,
            "balance_units": "0",
            "account_exists": false,
            "enforcement": "not_enforced",
            "can_manage": true,
            "entries": [],
            "next_before_sequence": null
        })
    );
    // Reading must not create an account.
    assert_eq!(accounts(&pool).await, 0);

    let initial = json!({"kind": "grant", "amount_units": "100", "reason": "  Initial grant\n"});
    let (status, granted) = submit(&router, "owner", &initial).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(granted["sequence"], "1");
    assert_eq!(granted["kind"], "grant");
    assert_eq!(granted["amount_units"], "100");
    assert_eq!(granted["balance_after_units"], "100");
    assert_eq!(granted["reason"], "Initial grant");
    assert_eq!(granted["actor_identity_id"], owner.to_string());
    assert_eq!(granted["reverses_entry_id"], Value::Null);

    let (status, adjusted) = submit(&router, "owner", &adjustment("-30")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(adjusted["sequence"], "2");
    assert_eq!(adjusted["kind"], "adjustment");
    assert_eq!(adjusted["amount_units"], "-30");
    assert_eq!(adjusted["balance_after_units"], "70");

    let (status, reversed) = submit(&router, "owner", &reversal(&adjusted["id"])).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(reversed["sequence"], "3");
    assert_eq!(reversed["kind"], "reversal");
    assert_eq!(reversed["amount_units"], "30");
    assert_eq!(reversed["balance_after_units"], "100");
    assert_eq!(reversed["reverses_entry_id"], adjusted["id"]);

    let (status, body) = read(&router, "owner", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["balance_units"], "100");
    assert_eq!(body["account_exists"], true);
    assert_eq!(body["enforcement"], "not_enforced");
    assert_eq!(sequences(&body), ["3", "2", "1"]);
    assert_eq!(body["entries"][0], reversed);
    assert_eq!(body["entries"][1], adjusted);
    assert_eq!(body["entries"][2], granted);
    assert_eq!(body["next_before_sequence"], Value::Null);

    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 100);
    assert_eq!(audits(&pool).await, 3);
    let (actor, detail): (Uuid, Value) = sqlx::query_as(
        "SELECT actor_id, detail FROM audit_events \
         WHERE action = 'project.credit_entry.created' AND target_id = $1",
    )
    .bind(entry_id(&reversed))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actor, owner);
    assert_eq!(detail["kind"], "reversal");
    assert_eq!(detail["amount_units"], "30");
    assert_eq!(detail["sequence"], "3");
    assert_eq!(detail["reverses_entry_id"], adjusted["id"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn amounts_stay_exact_to_the_limit_of_i64_and_never_overdraw(pool: PgPool) {
    manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let router = router(&pool);

    let (status, body) = submit(&router, "owner", &grant(I64_MAX)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["balance_after_units"], I64_MAX);
    let (_, account) = read(&router, "owner", "").await;
    assert!(account["balance_units"].is_string());
    assert_eq!(account["balance_units"], I64_MAX);

    for over in [grant("1"), adjustment("1")] {
        let (status, body) = submit(&router, "owner", &over).await;
        assert_eq!(status, StatusCode::CONFLICT, "{over}");
        assert_eq!(body["code"], "credit_balance_overflow", "{over}");
    }

    let (status, body) = submit(&router, "owner", &adjustment("-9223372036854775807")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["balance_after_units"], "0");

    for amount in ["-1", "-9223372036854775808"] {
        let (status, body) = submit(&router, "owner", &adjustment(amount)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{amount}");
        assert_eq!(body["code"], "insufficient_credit", "{amount}");
    }

    // Reversing a grant whose credit has since been taken away would overdraw too.
    let (_, granted) = submit(&router, "owner", &grant("10")).await;
    let (status, _) = submit(&router, "owner", &adjustment("-4")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = submit(&router, "owner", &reversal(&granted["id"])).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "insufficient_credit");

    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 6);
    assert_eq!(entries(&pool).await, 4);
    assert_eq!(audits(&pool).await, 4);
}

#[sqlx::test(migrations = "./migrations")]
#[allow(clippy::too_many_lines)]
async fn invalid_requests_are_rejected_without_creating_anything(pool: PgPool) {
    manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let router = router(&pool);
    let id = Uuid::new_v4();

    let bodies = [
        json!({"kind": "grant", "amount_units": "01", "reason": "r"}),
        json!({"kind": "grant", "amount_units": "+1", "reason": "r"}),
        json!({"kind": "grant", "amount_units": "-0", "reason": "r"}),
        json!({"kind": "grant", "amount_units": "0", "reason": "r"}),
        json!({"kind": "grant", "amount_units": "-5", "reason": "r"}),
        json!({"kind": "grant", "amount_units": "1.5", "reason": "r"}),
        json!({"kind": "grant", "amount_units": "", "reason": "r"}),
        json!({"kind": "grant", "amount_units": "9223372036854775808", "reason": "r"}),
        json!({"kind": "grant", "amount_units": 5, "reason": "r"}),
        json!({"kind": "grant", "reason": "r"}),
        json!({"kind": "grant", "amount_units": "5", "reason": "r", "reverses_entry_id": id}),
        json!({"kind": "adjustment", "amount_units": "0", "reason": "r"}),
        json!({"kind": "adjustment", "amount_units": "-9223372036854775809", "reason": "r"}),
        json!({"kind": "adjustment", "reason": "r"}),
        json!({"kind": "reversal", "reason": "r"}),
        json!({"kind": "reversal", "amount_units": "5", "reason": "r", "reverses_entry_id": id}),
        json!({"kind": "grant", "amount_units": "5", "reason": "   \t\n"}),
        json!({"kind": "grant", "amount_units": "5", "reason": "x".repeat(501)}),
        json!({"kind": "grant", "amount_units": "5", "reason": "invalid\0reason"}),
        json!({"kind": "grant", "amount_units": "5", "reason": null}),
        json!({"kind": "grant", "amount_units": "5"}),
        json!({"kind": "grant", "amount_units": "5", "reason": "r", "charge": true}),
        json!({"kind": "charge", "amount_units": "5", "reason": "r"}),
        json!({"amount_units": "5", "reason": "r"}),
        json!([]),
    ];
    for body in &bodies {
        let (status, reply) = submit(&router, "owner", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(reply["code"], "invalid_request", "{body}");
    }
    let (status, reply) = post_raw(&router, "owner", &[&fresh_key()], "not json").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply["code"], "invalid_request");

    let valid = grant("5").to_string();
    let key = fresh_key();
    let simple = Uuid::new_v4().simple().to_string();
    let braced = format!("{{{}}}", Uuid::new_v4());
    let malformed: [&[&str]; 5] = [
        &[],
        &["not-a-uuid"],
        &[simple.as_str()],
        &[braced.as_str()],
        &[key.as_str(), key.as_str()],
    ];
    for keys in malformed {
        let (status, reply) = post_raw(&router, "owner", keys, &valid).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{keys:?}");
        assert_eq!(reply["code"], "invalid_idempotency_key", "{keys:?}");
    }

    for query in [
        "?limit=0",
        "?limit=101",
        "?limit=01",
        "?limit=abc",
        "?limit=-1",
        "?before_sequence=0",
        "?before_sequence=01",
        "?before_sequence=-1",
        "?before_sequence=1.0",
        "?before_sequence=9223372036854775808",
        "?limit=2&limit=3",
        "?page=2",
    ] {
        let (status, reply) = read(&router, "owner", query).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}");
        assert_eq!(reply["code"], "invalid_request", "{query}");
    }

    assert_eq!(accounts(&pool).await, 0);
    assert_eq!(entries(&pool).await, 0);
    assert_eq!(audits(&pool).await, 0);

    // The limits themselves are accepted: 500 characters, counted as characters, after trimming.
    let reason = format!("  {}  ", "\u{e9}".repeat(500));
    let longest = json!({"kind": "grant", "amount_units": "5", "reason": reason});
    let (status, body) = submit(&router, "owner", &longest).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["reason"].as_str().unwrap().chars().count(), 500);
    let (status, _) = read(&router, "owner", "?limit=100").await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn requests_are_authenticated_before_they_are_examined(pool: PgPool) {
    let router = router(&pool);
    for authorization in [None, Some("Bearer not-a-token")] {
        // Invalid in every other respect too, so only authentication can answer.
        let mut listing = Request::get(format!("{CREDITS}?limit=0"));
        let mut writing = Request::post(ENTRIES)
            .header("content-type", "application/json")
            .header("idempotency-key", "not-a-uuid");
        if let Some(value) = authorization {
            listing = listing.header(AUTHORIZATION, value);
            writing = writing.header(AUTHORIZATION, value);
        }
        let (status, _) = send(&router, listing.body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
        let (status, _) = send(&router, writing.body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
    }

    // Unknown to Kratos.
    let (status, _) = read(&router, "stranger", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = submit(&router, "stranger", &grant("5")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(accounts(&pool).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
#[allow(clippy::too_many_lines)]
async fn only_active_managers_who_are_active_operator_members_may_write(pool: PgPool) {
    let router = router(&pool);

    // A member, but nothing made them a manager: nobody is one by default.
    member(&pool, "member", DEFAULT_PROJECT_ID).await;
    let (status, body) = read(&router, "member", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["can_manage"], false);
    let (status, body) = submit(&router, "member", &grant("5")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "credit_manager_required");

    // A manager whose manager row was revoked.
    let revoked = manager(&pool, "revoked", DEFAULT_PROJECT_ID).await;
    assert_eq!(read(&router, "revoked", "").await.1["can_manage"], true);
    revoke_manager(&pool, DEFAULT_PROJECT_ID, revoked).await;
    assert_eq!(read(&router, "revoked", "").await.1["can_manage"], false);
    let (status, body) = submit(&router, "revoked", &grant("5")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "credit_manager_required");

    // Managing another project, whose membership has since ended, confers nothing here.
    let other = second_project(&pool).await;
    let elsewhere = member(&pool, "elsewhere", DEFAULT_PROJECT_ID).await;
    join(&pool, elsewhere, other).await;
    make_manager(&pool, other, elsewhere).await;
    revoke_membership(&pool, other, elsewhere).await;
    let (status, body) = read(&router, "elsewhere", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["project_id"], DEFAULT_PROJECT_ID.to_string());
    assert_eq!(body["can_manage"], false);
    let (status, _) = submit(&router, "elsewhere", &grant("5")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A manager row outliving the membership it depends on.
    let former = manager(&pool, "former", DEFAULT_PROJECT_ID).await;
    revoke_membership(&pool, DEFAULT_PROJECT_ID, former).await;
    let (status, _) = read(&router, "former", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = submit(&router, "former", &grant("5")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A disabled identity.
    let disabled = manager(&pool, "disabled", DEFAULT_PROJECT_ID).await;
    sqlx::query("UPDATE human_identities SET disabled_at = now() WHERE id = $1")
        .bind(disabled)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = read(&router, "disabled", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = submit(&router, "disabled", &grant("5")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The write's own recheck refuses each of them even past authentication.
    for (identity, project_id) in [
        (revoked, DEFAULT_PROJECT_ID),
        (former, DEFAULT_PROJECT_ID),
        (disabled, DEFAULT_PROJECT_ID),
        (elsewhere, other),
    ] {
        let error = record(&pool, identity, project_id, grant("5"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some("credit_manager_required"));
    }

    assert_eq!(accounts(&pool).await, 0);
    assert_eq!(entries(&pool).await, 0);
    assert_eq!(audits(&pool).await, 0);

    // The same request from an active manager succeeds, so the refusals were about the caller.
    manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let (status, _) = submit(&router, "owner", &grant("5")).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrations = "./migrations")]
async fn ledgers_are_isolated_between_projects(pool: PgPool) {
    let other = second_project(&pool).await;
    manager(&pool, "alpha", DEFAULT_PROJECT_ID).await;
    let beta = manager(&pool, "beta", other).await;
    let router = router(&pool);
    let key = fresh_key();

    let (status, alpha_grant) = post_keyed(&router, "alpha", &key, &grant("50")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = read(&router, "beta", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["project_id"], other.to_string());
    assert_eq!(body["balance_units"], "0");
    assert_eq!(body["account_exists"], false);
    assert_eq!(body["entries"], json!([]));

    let (status, body) = submit(&router, "beta", &reversal(&alpha_grant["id"])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "credit_entry_not_found");
    // Nor may the refused write leave an account behind.
    assert_eq!(accounts(&pool).await, 1);

    // The same key, from another person in another project, is a different write.
    let (status, beta_grant) = post_keyed(&router, "beta", &key, &grant("50")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(beta_grant["id"], alpha_grant["id"]);
    assert_eq!(beta_grant["sequence"], "1");

    // Nor can a reversal be made to point across projects directly, even with an otherwise
    // consistent sequence and balance.
    let forged = forge_entry(
        &pool,
        other,
        beta,
        "reversal",
        -50,
        0,
        Some(entry_id(&alpha_grant)),
    )
    .await;
    assert!(forged.is_err());

    assert_eq!(read(&router, "alpha", "").await.1["balance_units"], "50");
    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 50);
    assert_eq!(reconciled_balance(&pool, other).await, 50);
}

#[sqlx::test(migrations = "./migrations")]
#[allow(clippy::too_many_lines)]
async fn a_retry_returns_the_original_entry_and_writes_nothing(pool: PgPool) {
    let owner = manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    manager(&pool, "co-owner", DEFAULT_PROJECT_ID).await;
    let router = router(&pool);
    let key = fresh_key();
    let top_up = json!({"kind": "grant", "amount_units": "100", "reason": "Top up"});

    let (status, first) = post_keyed(&router, "owner", &key, &top_up).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, replay) = post_keyed(&router, "owner", &key, &top_up).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, first);
    // Canonically identical: the reason is compared trimmed, fields in any order, and the key
    // as the UUID it spells.
    let reordered = json!({"reason": " Top up ", "amount_units": "100", "kind": "grant"});
    let (status, replay) = post_keyed(&router, "owner", &key.to_uppercase(), &reordered).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, first);

    for changed in [
        json!({"kind": "grant", "amount_units": "101", "reason": "Top up"}),
        json!({"kind": "grant", "amount_units": "100", "reason": "Top-up"}),
        json!({"kind": "adjustment", "amount_units": "100", "reason": "Top up"}),
        json!({"kind": "reversal", "reason": "Top up", "reverses_entry_id": first["id"]}),
    ] {
        let (status, body) = post_keyed(&router, "owner", &key, &changed).await;
        assert_eq!(status, StatusCode::CONFLICT, "{changed}");
        assert_eq!(body["code"], "idempotency_key_reused", "{changed}");
    }

    // Keys belong to their sender.
    let (status, theirs) = post_keyed(&router, "co-owner", &key, &top_up).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(theirs["id"], first["id"]);

    // A reversal replays too, rather than being refused as already reversed.
    let reversal_key = fresh_key();
    let undo = reversal(&first["id"]);
    let (status, reversed) = post_keyed(&router, "owner", &reversal_key, &undo).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, replay) = post_keyed(&router, "owner", &reversal_key, &undo).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, reversed);
    let elsewhere = reversal(&theirs["id"]);
    let (status, body) = post_keyed(&router, "owner", &reversal_key, &elsewhere).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "idempotency_key_reused");

    assert_eq!(entries(&pool).await, 3);
    assert_eq!(audits(&pool).await, 3);

    // A replay is authorised afresh: losing the right to write also loses the replay.
    revoke_manager(&pool, DEFAULT_PROJECT_ID, owner).await;
    let (status, body) = post_keyed(&router, "owner", &key, &top_up).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "credit_manager_required");
    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 100);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_refused_write_leaves_no_trace_and_frees_its_key(pool: PgPool) {
    manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let router = router(&pool);
    let key = fresh_key();

    // Refused after the account would have been created, on a project that has none yet.
    let (status, body) = post_keyed(&router, "owner", &key, &adjustment("-5")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "insufficient_credit");
    let missing = json!(Uuid::new_v4());
    let (status, _) = submit(&router, "owner", &reversal(&missing)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(accounts(&pool).await, 0);
    assert_eq!(audits(&pool).await, 0);

    // The key was never spent, and the refused writes consumed no sequence number.
    let (status, body) = post_keyed(&router, "owner", &key, &grant("5")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["sequence"], "1");

    let later = fresh_key();
    let (status, _) = post_keyed(&router, "owner", &later, &adjustment("-6")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body) = post_keyed(&router, "owner", &later, &adjustment("-5")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["sequence"], "2");
    assert_eq!(body["balance_after_units"], "0");
    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 0);
    assert_eq!(audits(&pool).await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_entry_is_reversed_at_most_once_and_reversals_are_final(pool: PgPool) {
    manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let router = router(&pool);

    let (_, granted) = submit(&router, "owner", &grant("40")).await;
    let (status, reversed) = submit(&router, "owner", &reversal(&granted["id"])).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(reversed["amount_units"], "-40");
    assert_eq!(reversed["balance_after_units"], "0");

    let (status, body) = submit(&router, "owner", &reversal(&granted["id"])).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "credit_entry_already_reversed");

    let (status, body) = submit(&router, "owner", &reversal(&reversed["id"])).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "credit_entry_not_reversible");

    assert_eq!(entries(&pool).await, 2);
    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 0);
}

/// Update the account and insert an entry directly, as a buggy writer might, in one transaction.
async fn forge_entry(
    pool: &PgPool,
    project_id: Uuid,
    actor: Uuid,
    kind: &str,
    amount: i64,
    balance_after: i64,
    reverses: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let sequence: i64 = sqlx::query_scalar(
        "UPDATE project_credit_accounts SET balance_units = $2, last_sequence = last_sequence + 1 \
         WHERE project_id = $1 RETURNING last_sequence",
    )
    .bind(project_id)
    .bind(balance_after)
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO project_credit_entries \
         (id, project_id, sequence, kind, amount_units, balance_after_units, reason, \
          actor_identity_id, reverses_entry_id, idempotency_key) \
         VALUES ($1, $2, $3, $4, $5, $6, 'Forged', $7, $8, $1)",
    )
    .bind(Uuid::new_v4())
    .bind(project_id)
    .bind(sequence)
    .bind(kind)
    .bind(amount)
    .bind(balance_after)
    .bind(actor)
    .bind(reverses)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

#[sqlx::test(migrations = "./migrations")]
#[allow(clippy::too_many_lines)]
async fn the_database_keeps_the_ledger_append_only_and_reconciled(pool: PgPool) {
    let owner = manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let (_, granted) = record(&pool, owner, DEFAULT_PROJECT_ID, grant("60"))
        .await
        .unwrap();

    for statement in [
        "UPDATE project_credit_entries SET reason = 'Edited'",
        "UPDATE project_credit_entries SET amount_units = 61, balance_after_units = 61",
        "DELETE FROM project_credit_entries",
        "TRUNCATE project_credit_entries",
        "DELETE FROM project_credit_accounts",
        "TRUNCATE project_credit_accounts CASCADE",
        "TRUNCATE projects CASCADE",
        "DELETE FROM projects",
        // Moving the balance without an entry: refused at once, or at commit once the counter
        // moves with it.
        "UPDATE project_credit_accounts SET balance_units = balance_units + 1",
        "UPDATE project_credit_accounts \
         SET balance_units = balance_units + 1, last_sequence = last_sequence + 1",
        "UPDATE project_credit_accounts SET last_sequence = last_sequence - 1",
    ] {
        let outcome = sqlx::query(statement).execute(&pool).await;
        assert!(outcome.is_err(), "{statement}");
    }

    // An account starts empty.
    let other = second_project(&pool).await;
    let open = "INSERT INTO project_credit_accounts (project_id, balance_units) VALUES ($1, $2)";
    let opened = sqlx::query(open)
        .bind(other)
        .bind(10_i64)
        .execute(&pool)
        .await;
    assert!(opened.is_err());
    sqlx::query(open)
        .bind(other)
        .bind(0_i64)
        .execute(&pool)
        .await
        .unwrap();

    // A consistent entry written directly is accepted, which shows the refusals below are about
    // their content rather than about writing directly.
    let project = DEFAULT_PROJECT_ID;
    forge_entry(&pool, project, owner, "grant", 20, 80, None)
        .await
        .unwrap();
    for (kind, amount, after, reverses) in [
        // A running balance that does not follow from the previous entry.
        ("adjustment", -10, 60, None),
        // Consistent with the balance, but not the exact negation of its target.
        ("reversal", -59, 21, Some(granted.id)),
        // A negative balance.
        ("adjustment", -90, -10, None),
        // A grant that removes credit.
        ("grant", -10, 70, None),
    ] {
        let forged = forge_entry(&pool, project, owner, kind, amount, after, reverses);
        assert!(forged.await.is_err(), "{kind} {amount}");
    }

    assert_eq!(entries(&pool).await, 2);
    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 80);

    // Revoking a manager is an ordinary update, and allowed.
    revoke_manager(&pool, DEFAULT_PROJECT_ID, owner).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn history_pages_newest_first_and_stay_coherent_under_writes(pool: PgPool) {
    let owner = manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let router = router(&pool);
    for amount in ["1", "2", "3", "4", "5"] {
        record(&pool, owner, DEFAULT_PROJECT_ID, grant(amount))
            .await
            .unwrap();
    }

    let (status, page) = read(&router, "owner", "?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sequences(&page), ["5", "4"]);
    assert_eq!(page["next_before_sequence"], "4");
    assert_eq!(page["balance_units"], "15");

    // A write between pages lands on the first page, not in the middle of the walk.
    record(&pool, owner, DEFAULT_PROJECT_ID, grant("6"))
        .await
        .unwrap();
    let (_, page) = read(&router, "owner", "?limit=2&before_sequence=4").await;
    assert_eq!(sequences(&page), ["3", "2"]);
    assert_eq!(page["next_before_sequence"], "2");
    let (_, page) = read(&router, "owner", "?before_sequence=2&limit=2").await;
    assert_eq!(sequences(&page), ["1"]);
    assert_eq!(page["next_before_sequence"], Value::Null);

    // A page that exactly fits is the last one; one short of it is not.
    let (_, page) = read(&router, "owner", "?limit=6").await;
    assert_eq!(sequences(&page), ["6", "5", "4", "3", "2", "1"]);
    assert_eq!(page["next_before_sequence"], Value::Null);
    let (_, page) = read(&router, "owner", "?limit=5").await;
    assert_eq!(page["next_before_sequence"], "2");
    let (_, page) = read(&router, "owner", "").await;
    assert_eq!(sequences(&page).len(), 6);
    assert_eq!(page["balance_units"], "21");
    let (_, page) = read(&router, "owner", "?before_sequence=9223372036854775807").await;
    assert_eq!(sequences(&page).len(), 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_debits_cannot_overdraw_together(pool: PgPool) {
    let owner = manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    record(&pool, owner, DEFAULT_PROJECT_ID, grant("100"))
        .await
        .unwrap();

    let holder = hold_account(&pool, DEFAULT_PROJECT_ID).await;
    let debits: Vec<_> = (0..2)
        .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                record(&pool, owner, DEFAULT_PROJECT_ID, adjustment("-60")).await
            })
        })
        .collect();
    // Both are past authorisation and queued on the account, having read no balance yet.
    wait_for_lock_waiters(&pool, ACCOUNT_LOCK, 2).await;
    holder.rollback().await.unwrap();

    let mut created = 0;
    let mut refused = 0;
    for debit in debits {
        match debit.await.unwrap() {
            Ok((status, entry)) => {
                assert_eq!(status, StatusCode::CREATED);
                assert_eq!(entry.balance_after_units, "40");
                created += 1;
            }
            Err(error) => {
                assert_eq!(error.code(), Some("insufficient_credit"));
                refused += 1;
            }
        }
    }
    assert_eq!((created, refused), (1, 1));
    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 40);
    assert_eq!(entries(&pool).await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_retries_write_one_entry_and_one_audit(pool: PgPool) {
    let owner = manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    record(&pool, owner, DEFAULT_PROJECT_ID, grant("1"))
        .await
        .unwrap();

    let key = Uuid::new_v4();
    let holder = hold_account(&pool, DEFAULT_PROJECT_ID).await;
    let retries: Vec<_> = (0..2)
        .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                record_keyed(&pool, owner, DEFAULT_PROJECT_ID, key, grant("5"))
                    .await
                    .unwrap()
            })
        })
        .collect();
    wait_for_lock_waiters(&pool, ACCOUNT_LOCK, 2).await;
    holder.rollback().await.unwrap();

    let mut results = Vec::new();
    for retry in retries {
        results.push(retry.await.unwrap());
    }
    results.sort_by_key(|(status, _)| status.as_u16());
    assert_eq!(results[0].0, StatusCode::OK);
    assert_eq!(results[1].0, StatusCode::CREATED);
    assert_eq!(results[0].1.id, results[1].1.id);
    assert_eq!(entries(&pool).await, 2);
    assert_eq!(audits(&pool).await, 2);
    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_write_waits_for_and_obeys_a_concurrent_manager_revocation(pool: PgPool) {
    let owner = manager(&pool, "owner", DEFAULT_PROJECT_ID).await;

    // First a revocation that rolls back, after which the write proceeds: that shows the write
    // really queues on the manager row, which the committed case relies on.
    for commit in [false, true] {
        let mut revocation = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE project_credit_managers SET revoked_at = now() \
             WHERE project_id = $1 AND identity_id = $2",
        )
        .bind(DEFAULT_PROJECT_ID)
        .bind(owner)
        .execute(&mut *revocation)
        .await
        .unwrap();
        let write = {
            let pool = pool.clone();
            tokio::spawn(async move { record(&pool, owner, DEFAULT_PROJECT_ID, grant("5")).await })
        };
        wait_for_lock_waiters(&pool, "%FROM project_credit_managers m%", 1).await;
        if commit {
            revocation.commit().await.unwrap();
            let error = write.await.unwrap().unwrap_err();
            assert_eq!(error.code(), Some("credit_manager_required"));
        } else {
            revocation.rollback().await.unwrap();
            assert_eq!(write.await.unwrap().unwrap().0, StatusCode::CREATED);
        }
    }
    assert_eq!(entries(&pool).await, 1);
    assert_eq!(reconciled_balance(&pool, DEFAULT_PROJECT_ID).await, 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_write_waits_for_and_obeys_a_concurrent_membership_removal(pool: PgPool) {
    let owner = manager(&pool, "owner", DEFAULT_PROJECT_ID).await;
    let co_owner = member(&pool, "co-owner", DEFAULT_PROJECT_ID).await;

    // Removed the way the membership handler removes someone: under the project row lock.
    let mut removal = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
        .bind(DEFAULT_PROJECT_ID)
        .execute(&mut *removal)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE project_memberships SET revoked_at = now(), revoked_by_identity_id = $2 \
         WHERE project_id = $3 AND identity_id = $1",
    )
    .bind(owner)
    .bind(co_owner)
    .bind(DEFAULT_PROJECT_ID)
    .execute(&mut *removal)
    .await
    .unwrap();
    let write = {
        let pool = pool.clone();
        tokio::spawn(async move { record(&pool, owner, DEFAULT_PROJECT_ID, grant("5")).await })
    };
    wait_for_lock_waiters(&pool, "%FROM projects WHERE id = $1 FOR KEY SHARE%", 1).await;
    removal.commit().await.unwrap();

    let error = write.await.unwrap().unwrap_err();
    assert_eq!(error.code(), Some("credit_manager_required"));
    assert_eq!(accounts(&pool).await, 0);
    assert_eq!(audits(&pool).await, 0);
}

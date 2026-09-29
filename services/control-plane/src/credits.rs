//! Per-project credit ledger: grants, adjustments and reversals, recorded but not enforced.
//!
//! Nothing here charges a job, reserves credit or refuses work. The ledger is the foundation that
//! enforcement will later read from, so what it must get right now is the record itself: every
//! entry is appended exactly once, a balance never goes below zero or beyond `i64`, and the cached
//! balance always equals the sum of the history. The migration makes the database check the same
//! rules, so a bug here fails a write instead of corrupting the ledger.
//!
//! Amounts cross the API as canonical decimal strings, never JSON numbers, so a browser cannot
//! round a large balance through a double.
//!
//! Writing needs an active row in `project_credit_managers` *and* an active operator membership
//! of the same project. Nothing grants the manager row yet: who should hold it is still an open
//! product decision, so every project starts with nobody able to write.
//!
//! Lock order for a write is identity, project, manager, account. Identity before project is the
//! order membership changes already take them in -- the caller's identity during authorisation,
//! then the project -- avoiding that lock inversion. Nothing here locks a job or a worker.

use axum::{
    Json,
    extract::{Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    AppState,
    operator::{OperatorError, authenticate_operator},
    registry::ErrorResponse,
};

#[cfg(test)]
mod lock_order_tests;
#[cfg(test)]
mod tests;

const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const DEFAULT_PAGE_SIZE: usize = 25;
const MAX_PAGE_SIZE: usize = 100;
const MAX_REASON_CHARS: usize = 500;

/// Whether `identity` may write `project`'s ledger right now: an active manager row, an active
/// membership of the same project, and an enabled operator identity. The read path's answer.
const ACTIVE_MANAGER: &str = "SELECT EXISTS ( \
     SELECT 1 FROM project_credit_managers m \
     JOIN project_memberships pm \
       ON pm.project_id = m.project_id AND pm.identity_id = m.identity_id \
     JOIN human_identities i ON i.id = m.identity_id \
     WHERE m.project_id = $1 AND m.identity_id = $2 AND m.revoked_at IS NULL \
       AND pm.revoked_at IS NULL AND i.role = 'operator' AND i.disabled_at IS NULL)";

/// The identity half of the write-time rule, holding the row until commit so it cannot be disabled
/// between this check and the entry. `FOR SHARE` because disabling is a non-key update, which
/// `FOR KEY SHARE` would let through.
const LOCK_ACTIVE_IDENTITY: &str = "SELECT id FROM human_identities \
     WHERE id = $1 AND role = 'operator' AND disabled_at IS NULL FOR SHARE";

/// The rest of the write-time rule, holding the manager row until commit so it cannot be revoked
/// between this check and the entry. Membership is held by the project lock taken before it, which
/// membership removal needs `FOR UPDATE`.
const LOCK_ACTIVE_MANAGER: &str = "SELECT m.identity_id FROM project_credit_managers m \
     JOIN project_memberships pm \
       ON pm.project_id = m.project_id AND pm.identity_id = m.identity_id \
     WHERE m.project_id = $1 AND m.identity_id = $2 AND m.revoked_at IS NULL \
       AND pm.revoked_at IS NULL \
     FOR SHARE OF m";

const ENTRY_COLUMNS: &str = "id, sequence, kind, amount_units, balance_after_units, reason, \
     actor_identity_id, reverses_entry_id, occurred_at";

#[derive(Debug)]
pub(crate) enum CreditsError {
    /// Authentication and project resolution, answered exactly as every operator route answers.
    Operator(OperatorError),
    Refused {
        status: StatusCode,
        code: &'static str,
        message: &'static str,
    },
}

impl From<OperatorError> for CreditsError {
    fn from(error: OperatorError) -> Self {
        Self::Operator(error)
    }
}

impl CreditsError {
    const fn refused(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self::Refused {
            status,
            code,
            message,
        }
    }

    const fn internal() -> Self {
        Self::Operator(OperatorError::internal())
    }

    const fn invalid_request() -> Self {
        Self::Operator(OperatorError::invalid_request())
    }

    const fn invalid_idempotency_key() -> Self {
        Self::refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_idempotency_key",
            "The Idempotency-Key header is required and must be a single hyphenated UUID.",
        )
    }

    const fn idempotency_key_reused() -> Self {
        Self::refused(
            StatusCode::CONFLICT,
            "idempotency_key_reused",
            "This Idempotency-Key was already used for a different credit entry.",
        )
    }

    const fn manager_required() -> Self {
        Self::refused(
            StatusCode::FORBIDDEN,
            "credit_manager_required",
            "Only a credit manager of this project may change its credit.",
        )
    }

    const fn entry_not_found() -> Self {
        Self::refused(
            StatusCode::NOT_FOUND,
            "credit_entry_not_found",
            "The credit entry to reverse was not found in this project.",
        )
    }

    const fn already_reversed() -> Self {
        Self::refused(
            StatusCode::CONFLICT,
            "credit_entry_already_reversed",
            "This credit entry has already been reversed.",
        )
    }

    const fn not_reversible() -> Self {
        Self::refused(
            StatusCode::CONFLICT,
            "credit_entry_not_reversible",
            "A reversal cannot itself be reversed.",
        )
    }

    const fn insufficient_credit() -> Self {
        Self::refused(
            StatusCode::CONFLICT,
            "insufficient_credit",
            "The entry would take the project's credit balance below zero.",
        )
    }

    const fn balance_overflow() -> Self {
        Self::refused(
            StatusCode::CONFLICT,
            "credit_balance_overflow",
            "The entry would take the project's credit balance beyond its maximum.",
        )
    }

    #[cfg(test)]
    fn code(&self) -> Option<&'static str> {
        match self {
            Self::Operator(_) => None,
            Self::Refused { code, .. } => Some(*code),
        }
    }
}

impl IntoResponse for CreditsError {
    fn into_response(self) -> Response {
        match self {
            Self::Operator(error) => error.into_response(),
            Self::Refused {
                status,
                code,
                message,
            } => (status, Json(ErrorResponse { code, message })).into_response(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CreditEntryKind {
    /// Adds credit. Always positive.
    Grant,
    /// A signed, non-zero correction.
    Adjustment,
    /// Exactly undoes one earlier grant or adjustment. Its amount is derived, never supplied.
    Reversal,
}

impl CreditEntryKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Grant => "grant",
            Self::Adjustment => "adjustment",
            Self::Reversal => "reversal",
        }
    }

    fn from_stored(kind: &str) -> Option<Self> {
        match kind {
            "grant" => Some(Self::Grant),
            "adjustment" => Some(Self::Adjustment),
            "reversal" => Some(Self::Reversal),
            _ => None,
        }
    }
}

/// Always `not_enforced` for now: the balance is recorded, and nothing is refused because of it.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CreditEnforcement {
    NotEnforced,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateCreditEntryRequest {
    kind: CreditEntryKind,
    /// Canonical decimal integer. Required for a grant (positive) and an adjustment (non-zero);
    /// must be absent for a reversal, whose amount is the exact negation of its target's.
    #[schema(value_type = Option<String>, example = "1000", pattern = "^-?(0|[1-9][0-9]*)$")]
    amount_units: Option<String>,
    /// 1 to 500 characters once surrounding whitespace is trimmed.
    reason: String,
    /// Required for a reversal and refused otherwise.
    reverses_entry_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct CreditEntryResponse {
    id: Uuid,
    /// Position in the project's ledger, from 1, as a decimal string.
    #[schema(value_type = String, example = "1")]
    sequence: String,
    kind: CreditEntryKind,
    #[schema(value_type = String, example = "1000")]
    amount_units: String,
    #[schema(value_type = String, example = "1000")]
    balance_after_units: String,
    reason: String,
    actor_identity_id: Uuid,
    reverses_entry_id: Option<Uuid>,
    occurred_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct CreditAccountResponse {
    project_id: Uuid,
    #[schema(value_type = String, example = "0")]
    balance_units: String,
    /// False until the first entry; the balance of a project without an account is zero.
    account_exists: bool,
    enforcement: CreditEnforcement,
    /// Whether the caller may add entries.
    can_manage: bool,
    /// Newest first.
    entries: Vec<CreditEntryResponse>,
    /// Pass as `before_sequence` for the next, older page; null on the last page.
    #[schema(value_type = Option<String>)]
    next_before_sequence: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreditsQuery {
    before_sequence: Option<String>,
    limit: Option<String>,
}

/// A validated write. The shape rules are encoded in the variants, so a reversal cannot carry an
/// amount and a grant cannot name a target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryRequest {
    Grant { amount: i64, reason: String },
    Adjustment { amount: i64, reason: String },
    Reversal { target: Uuid, reason: String },
}

impl EntryRequest {
    const fn kind(&self) -> CreditEntryKind {
        match self {
            Self::Grant { .. } => CreditEntryKind::Grant,
            Self::Adjustment { .. } => CreditEntryKind::Adjustment,
            Self::Reversal { .. } => CreditEntryKind::Reversal,
        }
    }

    fn reason(&self) -> &str {
        match self {
            Self::Grant { reason, .. }
            | Self::Adjustment { reason, .. }
            | Self::Reversal { reason, .. } => reason,
        }
    }

    const fn target(&self) -> Option<Uuid> {
        match self {
            Self::Reversal { target, .. } => Some(*target),
            Self::Grant { .. } | Self::Adjustment { .. } => None,
        }
    }

    /// Whether a stored entry is what this request, once canonicalised, would have written. A
    /// reversal's amount was derived rather than requested, so it is compared through its target.
    fn matches(&self, stored: &EntryRecord) -> bool {
        let same_amount = match self {
            Self::Grant { amount, .. } | Self::Adjustment { amount, .. } => {
                *amount == stored.amount_units
            }
            Self::Reversal { .. } => true,
        };
        stored.kind == self.kind().as_str()
            && stored.reason == self.reason()
            && stored.reverses_entry_id == self.target()
            && same_amount
    }
}

impl TryFrom<CreateCreditEntryRequest> for EntryRequest {
    type Error = CreditsError;

    fn try_from(request: CreateCreditEntryRequest) -> Result<Self, Self::Error> {
        let CreateCreditEntryRequest {
            kind,
            amount_units,
            reason,
            reverses_entry_id,
        } = request;
        let reason = reason.trim();
        if reason.is_empty() || reason.contains('\0') || reason.chars().count() > MAX_REASON_CHARS {
            return Err(CreditsError::invalid_request());
        }
        let reason = reason.to_owned();
        let amount = amount_units
            .as_deref()
            .map(|text| parse_units(text).ok_or_else(CreditsError::invalid_request))
            .transpose()?;
        match (kind, amount, reverses_entry_id) {
            (CreditEntryKind::Grant, Some(amount), None) if amount > 0 => {
                Ok(Self::Grant { amount, reason })
            }
            (CreditEntryKind::Adjustment, Some(amount), None) if amount != 0 => {
                Ok(Self::Adjustment { amount, reason })
            }
            (CreditEntryKind::Reversal, None, Some(target)) => {
                Ok(Self::Reversal { target, reason })
            }
            _ => Err(CreditsError::invalid_request()),
        }
    }
}

/// A canonical decimal integer within `i64`: optional leading minus, no plus sign, no leading
/// zeros, and no `-0`. Anything else has a second spelling, and two spellings of one amount would
/// make an identical retry look like a different request.
fn parse_units(text: &str) -> Option<i64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let canonical = !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && text != "-0";
    if canonical { text.parse().ok() } else { None }
}

/// A query parameter: a canonical positive decimal integer.
fn query_number(text: &str) -> Result<i64, CreditsError> {
    parse_units(text)
        .filter(|value| *value > 0)
        .ok_or_else(CreditsError::invalid_request)
}

/// The required `Idempotency-Key`. Only the hyphenated spelling, and exactly one header, so one
/// key cannot arrive in several forms or be chosen between.
fn idempotency_key(headers: &HeaderMap) -> Result<Uuid, CreditsError> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err(CreditsError::invalid_idempotency_key());
    };
    value
        .to_str()
        .ok()
        .filter(|text| text.len() == 36)
        .and_then(|text| Uuid::try_parse(text).ok())
        .ok_or_else(CreditsError::invalid_idempotency_key)
}

#[derive(Debug, sqlx::FromRow)]
struct EntryRecord {
    id: Uuid,
    sequence: i64,
    kind: String,
    amount_units: i64,
    balance_after_units: i64,
    reason: String,
    actor_identity_id: Uuid,
    reverses_entry_id: Option<Uuid>,
    occurred_at: DateTime<Utc>,
}

impl EntryRecord {
    fn into_response(self) -> Result<CreditEntryResponse, CreditsError> {
        Ok(CreditEntryResponse {
            id: self.id,
            sequence: self.sequence.to_string(),
            kind: CreditEntryKind::from_stored(&self.kind).ok_or_else(CreditsError::internal)?,
            amount_units: self.amount_units.to_string(),
            balance_after_units: self.balance_after_units.to_string(),
            reason: self.reason,
            actor_identity_id: self.actor_identity_id,
            reverses_entry_id: self.reverses_entry_id,
            occurred_at: self.occurred_at,
        })
    }
}

/// The caller's project credit balance and a page of its history, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/operator/credits",
    tag = "operator",
    security(("human_bearer" = [])),
    params(
        ("before_sequence" = Option<String>, Query, description = "Return entries older than this sequence, \
         a positive decimal string taken from `next_before_sequence`. Omit for the newest page."),
        ("limit" = Option<u32>, Query, minimum = 1, maximum = 100, description = "Page size, default 25.")
    ),
    responses(
        (status = 200, description = "Balance and history read from one consistent snapshot. Credit is recorded, not enforced.", body = CreditAccountResponse),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Identity is not an operator, or belongs to no project", body = ErrorResponse),
        (status = 409, description = "The caller belongs to several projects", body = ErrorResponse),
        (status = 422, description = "Malformed query", body = ErrorResponse),
    )
)]
pub(crate) async fn get_credits(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<CreditsQuery>, QueryRejection>,
) -> Result<Json<CreditAccountResponse>, CreditsError> {
    // Authorise before looking at the query, so a stranger learns nothing from its validation.
    let caller = authenticate_operator(&state, &headers).await?;
    let project_id = caller.sole_project()?;
    let Query(query) = query.map_err(|_| CreditsError::invalid_request())?;
    let before = query
        .before_sequence
        .as_deref()
        .map(query_number)
        .transpose()?;
    let page_size = match query.limit.as_deref() {
        None => DEFAULT_PAGE_SIZE,
        Some(text) => usize::try_from(query_number(text)?)
            .ok()
            .filter(|limit| *limit <= MAX_PAGE_SIZE)
            .ok_or_else(CreditsError::invalid_request)?,
    };
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    read_credits(database, caller.identity_id, project_id, before, page_size)
        .await
        .map(Json)
}

/// Read the latest account balance and requested history page from one repeatable-read
/// snapshot. Never creates an account.
async fn read_credits(
    database: &PgPool,
    identity_id: Uuid,
    project_id: Uuid,
    before: Option<i64>,
    page_size: usize,
) -> Result<CreditAccountResponse, CreditsError> {
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| CreditsError::internal())?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *transaction)
        .await
        .map_err(|_| CreditsError::internal())?;
    let can_manage: bool = sqlx::query_scalar(ACTIVE_MANAGER)
        .bind(project_id)
        .bind(identity_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| CreditsError::internal())?;
    let balance: Option<i64> = sqlx::query_scalar(
        "SELECT balance_units FROM project_credit_accounts WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| CreditsError::internal())?;
    // One more than the page, to learn whether an older page exists without counting.
    let fetch = i64::try_from(page_size + 1).map_err(|_| CreditsError::internal())?;
    let mut records = sqlx::query_as::<_, EntryRecord>(&format!(
        "SELECT {ENTRY_COLUMNS} FROM project_credit_entries \
         WHERE project_id = $1 AND ($2::bigint IS NULL OR sequence < $2) \
         ORDER BY sequence DESC LIMIT $3"
    ))
    .bind(project_id)
    .bind(before)
    .bind(fetch)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|_| CreditsError::internal())?;
    transaction
        .commit()
        .await
        .map_err(|_| CreditsError::internal())?;

    let next_before_sequence = if records.len() > page_size {
        records.truncate(page_size);
        records.last().map(|record| record.sequence.to_string())
    } else {
        None
    };
    let entries = records
        .into_iter()
        .map(EntryRecord::into_response)
        .collect::<Result<_, _>>()?;
    Ok(CreditAccountResponse {
        project_id,
        balance_units: balance.unwrap_or(0).to_string(),
        account_exists: balance.is_some(),
        enforcement: CreditEnforcement::NotEnforced,
        can_manage,
        entries,
        next_before_sequence,
    })
}

/// Append a grant, adjustment or reversal to the caller's project ledger.
///
/// A retry with the same `Idempotency-Key` and the same canonical request returns the original
/// entry (200) without writing again; the same key with a different request is refused (409).
/// A request that fails leaves nothing behind, so its key may be used again.
#[utoipa::path(
    post,
    path = "/api/v1/operator/credits/entries",
    tag = "operator",
    security(("human_bearer" = [])),
    params(
        ("Idempotency-Key" = Uuid, Header, description = "Required hyphenated UUID naming this write, \
         scoped to the caller and project. An identical retry returns the original entry.")
    ),
    request_body = CreateCreditEntryRequest,
    responses(
        (status = 200, description = "Identical replay; the entry the key first created", body = CreditEntryResponse),
        (status = 201, description = "Entry appended", body = CreditEntryResponse),
        (status = 401, description = "Identity token missing or invalid", body = ErrorResponse),
        (status = 403, description = "Not an active credit manager and operator member of the project", body = ErrorResponse),
        (status = 404, description = "Reversal target is not an entry of this project", body = ErrorResponse),
        (status = 409, description = "Key reused for a different request, several projects, balance below zero or beyond its maximum, or target already reversed or not reversible", body = ErrorResponse),
        (status = 422, description = "Invalid body or Idempotency-Key", body = ErrorResponse),
    )
)]
pub(crate) async fn create_entry(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateCreditEntryRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreditEntryResponse>), CreditsError> {
    // Authenticate before anything about the request is examined or answered.
    let caller = authenticate_operator(&state, &headers).await?;
    let project_id = caller.sole_project()?;
    let key = idempotency_key(&headers)?;
    let Json(request) = payload.map_err(|_| CreditsError::invalid_request())?;
    let request = EntryRequest::try_from(request)?;
    let database = state
        .database
        .as_ref()
        .ok_or_else(OperatorError::unavailable)?;
    record_entry(database, caller.identity_id, project_id, key, &request)
        .await
        .map(|(status, entry)| (status, Json(entry)))
}

/// The write itself, after authentication. Everything happens in one transaction, so a refusal at
/// any step -- including after the account row was created -- leaves no trace.
async fn record_entry(
    database: &PgPool,
    identity_id: Uuid,
    project_id: Uuid,
    key: Uuid,
    request: &EntryRequest,
) -> Result<(StatusCode, CreditEntryResponse), CreditsError> {
    let mut transaction = database
        .begin()
        .await
        .map_err(|_| CreditsError::internal())?;
    lock_manager(&mut transaction, identity_id, project_id).await?;
    let balance = lock_account(&mut transaction, project_id).await?;

    let previous = sqlx::query_as::<_, EntryRecord>(&format!(
        "SELECT {ENTRY_COLUMNS} FROM project_credit_entries \
         WHERE project_id = $1 AND actor_identity_id = $2 AND idempotency_key = $3"
    ))
    .bind(project_id)
    .bind(identity_id)
    .bind(key)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| CreditsError::internal())?;
    if let Some(previous) = previous {
        // Dropping the transaction rolls it back; nothing was written.
        return if request.matches(&previous) {
            Ok((StatusCode::OK, previous.into_response()?))
        } else {
            Err(CreditsError::idempotency_key_reused())
        };
    }

    let amount = match request {
        EntryRequest::Grant { amount, .. } | EntryRequest::Adjustment { amount, .. } => *amount,
        EntryRequest::Reversal { target, .. } => {
            reversal_amount(&mut transaction, project_id, *target).await?
        }
    };
    let balance_after = match balance.checked_add(amount) {
        None => return Err(CreditsError::balance_overflow()),
        Some(value) if value < 0 => return Err(CreditsError::insufficient_credit()),
        Some(value) => value,
    };

    let sequence: i64 = sqlx::query_scalar(
        "UPDATE project_credit_accounts \
         SET balance_units = $2, last_sequence = last_sequence + 1, updated_at = now() \
         WHERE project_id = $1 RETURNING last_sequence",
    )
    .bind(project_id)
    .bind(balance_after)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| CreditsError::internal())?;
    let record = sqlx::query_as::<_, EntryRecord>(&format!(
        "INSERT INTO project_credit_entries \
         (id, project_id, sequence, kind, amount_units, balance_after_units, reason, \
          actor_identity_id, reverses_entry_id, idempotency_key) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING {ENTRY_COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(project_id)
    .bind(sequence)
    .bind(request.kind().as_str())
    .bind(amount)
    .bind(balance_after)
    .bind(request.reason())
    .bind(identity_id)
    .bind(request.target())
    .bind(key)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|_| CreditsError::internal())?;
    audit_entry(&mut transaction, project_id, &record).await?;
    transaction
        .commit()
        .await
        .map_err(|_| CreditsError::internal())?;
    Ok((StatusCode::CREATED, record.into_response()?))
}

/// Recheck, under locks held to commit, that the caller may still write this project's ledger.
///
/// Authentication read all of this in its own, already committed transaction; a revocation may
/// have landed since. The identity comes first, as it does in every membership change, which holds
/// the acting identity from authorisation until it takes the project: the other way round, a
/// member removing someone while also writing credit could deadlock against themselves.
///
/// `FOR KEY SHARE` on the project conflicts with the `FOR UPDATE` membership removal takes, so the
/// membership read after it cannot change before commit. It is the weakest project lock that does,
/// and does not queue behind the scheduler's `FOR NO KEY UPDATE`.
async fn lock_manager(
    transaction: &mut Transaction<'_, Postgres>,
    identity_id: Uuid,
    project_id: Uuid,
) -> Result<(), CreditsError> {
    sqlx::query_scalar::<_, Uuid>(LOCK_ACTIVE_IDENTITY)
        .bind(identity_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|_| CreditsError::internal())?
        .ok_or_else(CreditsError::manager_required)?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM projects WHERE id = $1 FOR KEY SHARE")
        .bind(project_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|_| CreditsError::internal())?
        .ok_or_else(CreditsError::manager_required)?;
    sqlx::query_scalar::<_, Uuid>(LOCK_ACTIVE_MANAGER)
        .bind(project_id)
        .bind(identity_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|_| CreditsError::internal())?
        .ok_or_else(CreditsError::manager_required)?;
    Ok(())
}

/// The account's balance, creating the account on first use and locking it either way. Every write
/// to one project's ledger queues here, which is what makes the balance, idempotency and reversal
/// checks after it final.
async fn lock_account(
    transaction: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
) -> Result<i64, CreditsError> {
    sqlx::query(
        "INSERT INTO project_credit_accounts (project_id) VALUES ($1) \
         ON CONFLICT (project_id) DO NOTHING",
    )
    .bind(project_id)
    .execute(&mut **transaction)
    .await
    .map_err(|_| CreditsError::internal())?;
    sqlx::query_scalar(
        "SELECT balance_units FROM project_credit_accounts WHERE project_id = $1 FOR UPDATE",
    )
    .bind(project_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(|_| CreditsError::internal())
}

/// The amount that exactly undoes `target`, which must be an unreversed grant or adjustment of the
/// same project. Called with the account locked, so no other reversal of it can be in flight.
async fn reversal_amount(
    transaction: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    target: Uuid,
) -> Result<i64, CreditsError> {
    let (kind, amount, reversed) = sqlx::query_as::<_, (String, i64, bool)>(
        "SELECT e.kind, e.amount_units, EXISTS ( \
             SELECT 1 FROM project_credit_entries r WHERE r.reverses_entry_id = e.id) \
         FROM project_credit_entries e WHERE e.project_id = $1 AND e.id = $2",
    )
    .bind(project_id)
    .bind(target)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| CreditsError::internal())?
    .ok_or_else(CreditsError::entry_not_found)?;
    if kind == CreditEntryKind::Reversal.as_str() {
        return Err(CreditsError::not_reversible());
    }
    if reversed {
        return Err(CreditsError::already_reversed());
    }
    amount
        .checked_neg()
        .ok_or_else(CreditsError::balance_overflow)
}

async fn audit_entry(
    transaction: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    record: &EntryRecord,
) -> Result<(), CreditsError> {
    sqlx::query(
        "INSERT INTO audit_events \
         (id, actor_type, actor_id, action, target_type, target_id, outcome, detail) \
         VALUES ($1, 'human', $2, 'project.credit_entry.created', 'project_credit_entry', $3, \
         'succeeded', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(record.actor_identity_id)
    .bind(record.id)
    .bind(json!({
        "project_id": project_id,
        "sequence": record.sequence.to_string(),
        "kind": record.kind,
        "amount_units": record.amount_units.to_string(),
        "balance_after_units": record.balance_after_units.to_string(),
        "reverses_entry_id": record.reverses_entry_id,
    }))
    .execute(&mut **transaction)
    .await
    .map_err(|_| CreditsError::internal())?;
    Ok(())
}

export type CreditEntry = {
  id: string;
  sequence: string;
  kind: "grant" | "adjustment" | "reversal";
  amount_units: string;
  balance_after_units: string;
  reason: string;
  actor_identity_id: string;
  reverses_entry_id: string | null;
  occurred_at: string;
};

export type CreditAccount = {
  project_id: string;
  /** Every amount is pence of this currency. The console reads nothing else. */
  currency: "GBP";
  minor_unit_exponent: 2;
  balance_units: string;
  account_exists: boolean;
  enforcement: "not_enforced";
  can_manage: boolean;
  entries: CreditEntry[];
  next_before_sequence: string | null;
};

function integer(value: unknown): value is string {
  if (typeof value !== "string" || !/^(0|-?[1-9][0-9]*)$/.test(value)) return false;
  const parsed = BigInt(value);
  return parsed >= -9223372036854775808n && parsed <= 9223372036854775807n;
}

function nonnegative(value: unknown): value is string {
  return integer(value) && BigInt(value) >= 0n;
}

function positive(value: unknown): value is string {
  return integer(value) && BigInt(value) > 0n;
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** Never round ledger integers through JavaScript Number, including sequence cursors. */
export function parseCreditAccount(value: unknown): CreditAccount {
  if (!record(value) || typeof value.project_id !== "string"
    // Refuse rather than guess: a server that ever changed its unit would otherwise be shown at
    // the wrong scale, silently, by a factor of a hundred or more.
    || value.currency !== "GBP" || value.minor_unit_exponent !== 2
    || !nonnegative(value.balance_units) || typeof value.account_exists !== "boolean"
    || value.enforcement !== "not_enforced" || typeof value.can_manage !== "boolean"
    || !Array.isArray(value.entries)
    || !(value.next_before_sequence === null || positive(value.next_before_sequence))) {
    throw new Error("The credit ledger response could not be read.");
  }
  for (const entry of value.entries) {
    if (!record(entry) || typeof entry.id !== "string" || !positive(entry.sequence)
      || typeof entry.kind !== "string" || !["grant", "adjustment", "reversal"].includes(entry.kind)
      || !integer(entry.amount_units) || entry.amount_units === "0"
      || !nonnegative(entry.balance_after_units) || typeof entry.reason !== "string"
      || typeof entry.actor_identity_id !== "string"
      || !(entry.reverses_entry_id === null || typeof entry.reverses_entry_id === "string")
      || typeof entry.occurred_at !== "string" || Number.isNaN(Date.parse(entry.occurred_at))) {
      throw new Error("The credit ledger contains an unreadable entry.");
    }
  }
  return value as CreditAccount;
}

/**
 * Pence as pounds: "150000" is "£1,500.00". BigInt throughout, because a ledger integer can exceed
 * what a JavaScript number holds exactly, and a pound figure off by a penny is worse than none.
 */
export function formatCreditUnits(value: string, signed = false): string {
  if (!integer(value)) throw new Error("Invalid credit units.");
  const amount = BigInt(value);
  const magnitude = amount < 0n ? -amount : amount;
  const pounds = (magnitude / 100n).toLocaleString("en-GB");
  const pence = (magnitude % 100n).toString().padStart(2, "0");
  const sign = amount < 0n ? "-" : signed && amount > 0n ? "+" : "";
  return `${sign}£${pounds}.${pence}`;
}

export function creditHistoryUrl(before: string | null): string {
  if (before !== null && !positive(before)) throw new Error("Invalid credit history cursor.");
  return `/api/v1/operator/credits?limit=25${before === null ? "" : `&before_sequence=${before}`}`;
}

/** The largest balance the ledger can hold, in pence: `i64::MAX`. */
const MAX_PENCE = 9223372036854775807n;

/**
 * "12", "12.5" or "12.50" pounds as a canonical pence string ("1250"), or null if it is not a
 * positive amount of whole pence. Parsed as text, never as a number, so "0.1" is exactly 10 pence
 * and a large figure is not rounded on its way to the ledger.
 */
export function poundsToPence(text: string): string | null {
  const match = /^([0-9]+)(?:\.([0-9]{1,2}))?$/.exec(text.trim().replace(/,/g, ""));
  if (!match) return null;
  const pence = BigInt(match[1]) * 100n + BigInt((match[2] ?? "").padEnd(2, "0") || "0");
  if (pence <= 0n || pence > MAX_PENCE) return null;
  return pence.toString();
}

export type CreditGrantRequest = { kind: "grant"; amount_units: string; reason: string };

/** A grant of `pence`, or null when the reason is empty or longer than the ledger accepts. */
export function creditGrantRequest(pence: string, reason: string): CreditGrantRequest | null {
  const trimmed = reason.trim();
  if (!/^[1-9][0-9]*$/.test(pence) || trimmed.length === 0 || [...trimmed].length > 500) return null;
  return { kind: "grant", amount_units: pence, reason: trimmed };
}

export const CREDIT_ENTRIES_URL = "/api/v1/operator/credits/entries";

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

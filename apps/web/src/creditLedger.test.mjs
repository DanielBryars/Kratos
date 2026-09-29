import test from "node:test";
import assert from "node:assert/strict";
import { creditHistoryUrl, formatCreditUnits, parseCreditAccount } from "./creditLedger.ts";

const entry = {
  id: "entry", sequence: "9007199254740993", kind: "grant",
  amount_units: "9007199254740993", balance_after_units: "9007199254740993",
  reason: "Initial allocation", actor_identity_id: "actor", reverses_entry_id: null,
  occurred_at: "2026-09-29T12:00:00Z",
};
const account = {
  project_id: "project", balance_units: "9007199254740993", account_exists: true,
  enforcement: "not_enforced", can_manage: false, entries: [entry], next_before_sequence: "9007199254740993",
};

test("ledger units and history cursors preserve integers beyond JavaScript precision", () => {
  const parsed = parseCreditAccount(account);
  assert.equal(formatCreditUnits(parsed.balance_units), "9,007,199,254,740,993");
  assert.equal(formatCreditUnits("9223372036854775807", true), "+9,223,372,036,854,775,807");
  assert.equal(formatCreditUnits("-9223372036854775808", true), "-9,223,372,036,854,775,808");
  assert.equal(creditHistoryUrl(parsed.next_before_sequence), "/api/v1/operator/credits?limit=25&before_sequence=9007199254740993");
});

test("empty account has an explicit non-enforcing state", () => {
  const empty = { ...account, account_exists: false, balance_units: "0", entries: [], next_before_sequence: null };
  assert.equal(parseCreditAccount(empty).account_exists, false);
  assert.equal(creditHistoryUrl(null), "/api/v1/operator/credits?limit=25");
});

test("unreadable or lossy ledger amounts cannot be displayed as balances", () => {
  for (const balance of [9007199254740993, "01", "+1", "1.5", "-0", "-1", "9223372036854775808", null]) {
    assert.throws(() => parseCreditAccount({ ...account, balance_units: balance }));
  }
  assert.throws(() => parseCreditAccount({ ...account, enforcement: "enforced" }));
  assert.throws(() => parseCreditAccount({ ...account, entries: [{ ...entry, amount_units: 1 }] }));
  assert.throws(() => parseCreditAccount({ ...account, entries: [{ ...entry, occurred_at: "unknown" }] }));
});

test("history cursors cannot become zero, negative, rounded numbers or query injection", () => {
  for (const cursor of ["0", "-1", "01", "1&limit=1000", "9223372036854775808"]) {
    assert.throws(() => creditHistoryUrl(cursor));
    assert.throws(() => parseCreditAccount({ ...account, next_before_sequence: cursor }));
  }
});

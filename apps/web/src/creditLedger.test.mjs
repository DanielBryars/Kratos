import test from "node:test";
import assert from "node:assert/strict";
import { creditGrantRequest, creditHistoryUrl, formatCreditUnits, parseCreditAccount, poundsToPence } from "./creditLedger.ts";

const entry = {
  id: "entry", sequence: "9007199254740993", kind: "grant",
  amount_units: "9007199254740993", balance_after_units: "9007199254740993",
  reason: "Initial allocation", actor_identity_id: "actor", reverses_entry_id: null,
  occurred_at: "2026-09-29T12:00:00Z",
};
const account = {
  project_id: "project", currency: "GBP", minor_unit_exponent: 2, balance_units: "9007199254740993", account_exists: true,
  enforcement: "not_enforced", can_manage: false, entries: [entry], next_before_sequence: "9007199254740993",
};

test("ledger units and history cursors preserve integers beyond JavaScript precision", () => {
  const parsed = parseCreditAccount(account);
  // Units are pence: the last two digits are the pence, exactly, even beyond 2^53.
  assert.equal(formatCreditUnits(parsed.balance_units), "£90,071,992,547,409.93");
  assert.equal(formatCreditUnits("9223372036854775807", true), "+£92,233,720,368,547,758.07");
  assert.equal(formatCreditUnits("-9223372036854775808", true), "-£92,233,720,368,547,758.08");
  assert.equal(creditHistoryUrl(parsed.next_before_sequence), "/api/v1/operator/credits?limit=25&before_sequence=9007199254740993");
});

test("pence are shown as pounds and pence, never as a bare count", () => {
  assert.equal(formatCreditUnits("0"), "£0.00");
  assert.equal(formatCreditUnits("5"), "£0.05");
  assert.equal(formatCreditUnits("150000"), "£1,500.00");
  assert.equal(formatCreditUnits("-1"), "-£0.01");
  assert.equal(formatCreditUnits("1999", true), "+£19.99");
  assert.equal(formatCreditUnits("-1999", true), "-£19.99");
});

test("an account in any other currency or scale is refused rather than shown at the wrong scale", () => {
  for (const [currency, exponent] of [["USD", 2], ["GBP", 0], ["GBP", 3], [undefined, 2], ["GBP", undefined], ["GBP", "2"]]) {
    assert.throws(() => parseCreditAccount({ ...account, currency, minor_unit_exponent: exponent }));
  }
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

test("pounds typed in become exact pence, and nothing else is accepted", () => {
  const cases = { "12": "1200", "12.5": "1250", "12.50": "1250", "0.1": "10", "0.01": "1", " 1,500.00 ": "150000",
    "92233720368547758.07": "9223372036854775807" };
  for (const [typed, pence] of Object.entries(cases)) assert.equal(poundsToPence(typed), pence, typed);
  for (const bad of ["", "0", "0.00", "-5", "+5", "1.234", "1e3", ".5", "5.", "£5", "abc", "92233720368547758.08"]) {
    assert.equal(poundsToPence(bad), null, bad);
  }
});

test("a grant needs a reason the ledger will accept", () => {
  assert.deepEqual(creditGrantRequest("1250", "  Monthly top-up "), { kind: "grant", amount_units: "1250", reason: "Monthly top-up" });
  assert.equal(creditGrantRequest("1250", "   "), null);
  assert.equal(creditGrantRequest("1250", "x".repeat(501)), null);
  assert.notEqual(creditGrantRequest("1250", "x".repeat(500)), null);
  assert.equal(creditGrantRequest("0", "reason"), null);
  assert.equal(creditGrantRequest("12.50", "reason"), null);
});

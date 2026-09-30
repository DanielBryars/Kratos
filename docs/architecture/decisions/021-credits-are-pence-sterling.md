# ADR-021 — Credits are pounds sterling, recorded as whole pence

**Status:** Accepted
**Date:** 2026-09-30

## Context

The credit ledger (PR #118) stores amounts as `bigint` "units" and says nothing about what a unit
is. The API carries them as decimal-integer strings — to keep large values exact in a browser, not
to carry fractions — and the console showed them as a bare count ("1,500 units").

That was harmless while the ledger was empty. It stops being harmless at the first entry, for a
reason specific to this ledger: **history is append-only, enforced by triggers.** An entry written
while "unit" means one thing can never be rewritten to mean another; the only remedies afterwards
are compensating entries or silently reinterpreting the past. So the unit has to be fixed before
anything is granted, and it is being fixed while the tables are still empty.

Daniel's decision: credits are a currency, converted for display later if ever needed, and that
currency is **pounds sterling**. GPU-hours were considered and rejected because different GPUs cost
different amounts, so an hour is not a stable unit of value.

## Decision

**One unit is one penny (GBP, ISO 4217 minor-unit exponent 2).**

- **Pence, not pounds**, because the costs this will eventually record — GPU time — are fractions of
  a pound, and a ledger denominated in whole pounds has nowhere to put them.
- **Integers, not `numeric`**, because the ledger's arithmetic, `CHECK` constraints and idempotency
  comparisons are all exact integer operations, and money must stay exact.
- **Stated, not assumed.** The account response carries `"currency": "GBP"` and
  `"minor_unit_exponent": 2`, and the console refuses any account that declares anything else rather
  than displaying it at a guessed scale. The meaning is also recorded against the columns themselves
  by migration `202609300035_credit_units_are_pence.sql`, so a reader of the database sees it without
  reading application code.
- **Displayed as pounds and pence** using integer arithmetic only: `150000` is shown as `£1,500.00`.
  A ledger value can exceed what a JavaScript number holds exactly, so nothing is rounded through a
  float on the way to the screen.

Field names keep their `_units` suffix. Renaming them would change the API for no gain in safety,
since the response now states its currency and scale explicitly and the client checks both.

## Consequences

- **The unit is now permanent in practice.** Changing it later would mean every existing entry was
  written under a different meaning, which the append-only ledger cannot correct in place.
- **A second currency is not supported and would be a real change**, not a setting: the ledger has
  no currency column, because one fixed currency is stated rather than stored. That is deliberate —
  multi-currency accounts raise conversion, rounding and rate-timing questions nobody has asked.
- **Converting for display is a presentation concern.** Showing pounds as dollars later would need
  a rate and a timestamp for it; the ledger itself stays in pence sterling.

## What this does not decide

- **Where credit comes from.** The denomination is now real money; the source is not. A grant still
  has no payment behind it, which is why [ADR-020](020-credit-manager-authority.md) treats the
  authority to grant as the authority to create money.
- **Prices.** No rate per GPU, per hour or per job is set here, and none should be invented.
- **Enforcement.** The ledger still refuses nothing.

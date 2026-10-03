# ADR-020 — Who may move credit, and how they come to hold that authority

**Status:** Deferred — superseded for now by an interim rule (see below)
**Date:** 2026-09-29

## Interim decision (2026-09-30)

Daniel chose the simpler rule for now: **any active operator member of a project may add credit to
it.** No manager role, no bootstrap, no delegation. The write path checks an unrevoked membership and
an enabled operator identity, rechecked under locks at commit.

The cost is the one this ADR describes, accepted knowingly: inviting somebody to a project lets them
grant it credit, and credit is pence sterling ([ADR-021](021-credits-are-pence-sterling.md)). It is
tolerable today because there is one operator and nothing enforces a balance. It stops being
tolerable at the first of: a second person invited to a project that holds real credit, or any
enforcement reading the balance. Either is the trigger to implement the design below, whose
`project_credit_managers` table is kept, empty, for that purpose.

Decision 4 — revoking a membership revokes credit authority — holds trivially under the interim
rule, since authority *is* membership.

## Context

The credit ledger shipped in PR #118 and enforces nothing: no job is charged, reserved against or
refused because of it. Its write path is already strict. `credits.rs` requires, in one transaction
and holding row locks until commit, an unrevoked row in `project_credit_managers`, an unrevoked
`project_memberships` row for the same project, and an enabled identity whose role is `operator`.
History is append-only, guarded by triggers rather than by the handler that writes it, because the
application role holds `UPDATE` and `DELETE` and a grant cannot express "never".

**And `project_credit_managers` is empty, with nothing able to fill it.** There is no bootstrap, no
delegation, and no endpoint. That was deliberate — the migration says so: *"nothing grants this,
and there is no bootstrap, because who should hold it is a product decision still to be made."*
This ADR is that decision. It is the gate named in the handover: *project membership currently
implies broad owner access; it must not silently become authority to mint credits.*

### Why this authority is different in kind

[ADR-017](017-invitations-and-shared-ownership.md) decided that *"every member is an owner"* and
deferred any role below owner rather than inventing one nothing distinguished. Membership therefore
carries broad access: see and submit jobs, see workers, cancel work. That was the right call for
those powers, because they are all powers over the project's own work.

Credit is not like that, for a reason worth stating plainly: **a grant has no source.** There is no
payment, no provider invoice, and no external account it is drawn from. Codex's reviewed next-step
note is explicit that no money rates, initial grants or provider payouts are to be invented. So
`kind = 'grant'` creates spending power from nothing, and manager authority is, in effect, the
authority to create money inside this system — and since
[ADR-021](021-credits-are-pence-sterling.md), that is literal: a unit is one penny sterling.

That is why it must not fall out of membership. Inviting somebody to look at a training run is a
normal, low-stakes act — Daniel has done it — and it must not also hand them the ability to conjure
budget.

### What the schema already gives us, and one thing it cannot

The composite foreign key on `project_credit_managers` points at
`project_memberships (project_id, identity_id)`, so a manager row can only ever name a real
membership of the same project. The migration is candid that this is partial: the constraint
*"holds for revoked memberships too"*. A revoked member's manager row survives, and only the
handler's `pm.revoked_at IS NULL` predicate stops them writing. That gap is the origin of decision 4
below.

Two other facts constrain the answer:

- **`projects` records no founder or owner.** It is `id`, `name`, `created_at`. So the first manager
  cannot be derived from whoever created the project, because nothing records who that was.
- **There is no platform-level role.** `human_identities.role` is `CHECK (role IN ('member',
  'operator'))`. There is no principal above a project to appoint into one.

## Decision

### 1. Membership never implies manager, and no automatic path creates one

No sign-in, invitation claim, project creation or membership grant creates a
`project_credit_managers` row, now or later. Every such row is the result of a deliberate,
attributed act whose only purpose is to confer this authority.

### 2. The first manager of a project is appointed out of band

By a migration, or by an operator with database access, recorded in `audit_events` naming the
mechanism used. Not by an API.

This is the uncomfortable half of the decision, so the reasoning matters. Any in-product bootstrap
is a path from *membership* to *money creation*, and with no founder recorded and no platform role
to hang it on, every in-product option reduces to "some member becomes a manager because of a
property members have". A first-claim endpoint is a race any invited member could win. Deriving from
a founder would require adding founder data, and membership-shaped data becoming authority is the
exact failure this ADR exists to prevent.

The honest answer for a system with one operator and no payments is that the first manager is
installed by hand and written down. If a platform-level role is ever introduced, this decision
should be revisited — but introducing one is a larger decision than this ADR should make on the way
past.

### 3. An active manager may appoint another manager of the same project

Delegation uses the same predicate as the write path, checked in one transaction with the rows
locked until commit:

- the appointer holds an unrevoked manager row for the project, an unrevoked membership of it, and
  an enabled `operator` identity;
- the target holds an unrevoked membership of **the same project** and an enabled `operator`
  identity;
- the appointment records the appointer's identity and a reason, and is written to `audit_events`
  in the same transaction as the manager row.

A manager may revoke another manager, and may revoke themselves. Revocation sets `revoked_at`; rows
are never deleted, so *who could move credit, and until when* stays answerable.

### 4. Revoking a membership revokes that identity's manager row, in the same transaction

This is the trap the schema comment points at. Without it, removing somebody from a project leaves
their manager row intact but inert, and **re-adding them later silently restores their authority to
create credit** — an invisible privilege grant produced by an act that looks routine.

Restoring the authority must be a fresh appointment under decision 3, attributed and audited like
any other. Membership revocation and manager revocation commit together or not at all.

### 5. A project may reach zero managers, and its ledger then freezes

Rather than forbidding removal of the last manager, which creates a principal nobody can remove, or
promoting somebody automatically, which recreates the escalation this ADR forbids.

A project with no active manager cannot be credited or adjusted. Its balance and history remain
readable. Recovery is decision 2 — an out-of-band appointment — which is exactly why zero is
tolerable here: there is a documented way back. Note the contrast with membership, where ADR-017's
acceptance requires that two owners removing each other leave one, because a project with no members
would be orphaned with no equivalent recovery.

### 6. Nothing here is enabled by this ADR

No endpoint, no migration, no seeded manager, no role or balance grant. The ledger stays
non-enforcing and unwritable until decisions 2 to 5 are implemented and reviewed, and enforcement is
a separate decision again.

## Consequences

**The ledger remains inert after this ADR, deliberately.** This decides *who may write and how they
come to be allowed to*, which is the gate; it does not open it. An implementation is a bounded
follow-up: a migration adding the appointment audit actions, a delegation endpoint, and the
membership-revocation change in decision 4.

**Decision 4 is a change to an existing path**, not new surface. Whatever revokes a membership today
gains a second statement in its transaction, and it needs a test that a removed-and-reinstated
member does not regain manager authority — which is the kind of thing that passes review by
inspection and fails in practice, so it should be proved by inverting it.

**Out-of-band bootstrap is an operational burden and should be recorded as one.** It belongs in the
takeover checklist rather than in somebody's memory, because a project whose managers have all been
revoked cannot be recovered from inside the product.

## What this does not decide

- **Separation of duties.** Whether one person may both grant credit and spend it is not addressed.
  With a single operator it is moot; it will not stay moot, and pretending this ADR settled it would
  be worse than naming it.
- **Where credit comes from.** [ADR-021](021-credits-are-pence-sterling.md) settles the
  denomination — pence sterling — but not the source: a grant still has no payment behind it.
  Rates, payments, quotes and provider payouts are explicitly out of scope, per the handover.
- **Enforcement.** Reservation, settlement, the worker shutdown reserve and replay-safe accounting
  are the reviewed next-step work, and must be proved end to end before anything is enforced.
- **Whether a platform-level role should exist.** Decision 2 would be revisited if one did.
- **Anything about existing data.** No manager is appointed and no balance changed by this document.

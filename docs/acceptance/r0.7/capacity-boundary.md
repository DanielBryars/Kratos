# R0.7 capacity boundary acceptance

**Status:** Implemented and proved against the fake provider. **Nothing here can start or spend
anything**: there is no cloud dependency, no Terraform, and the feature is off unless
`KRATOS_CAPACITY_ENABLED=true`.

## What this is

Kratos stays the authoritative queue. A capacity provider supplies bounded machines and decides
nothing about fairness, budgets, job state or result identity. This is the boundary that keeps
that true, plus a fake provider so the behaviour could be settled before anything costs money.

The boundary is deliberately attempt-derived for this first slice: a request exists for an attempt
that already exists. True pre-attempt provisioning — capacity as the thing that *permits* an
attempt — is a later design change and explicitly outside this lane.

## The two ideas the schema is shaped around

**A request is replayable, not repeatable.** Its idempotency key is `kratos-attempt-<attempt_id>`,
derived rather than random, so a controller that has lost its memory computes the same key and is
answered with the same machine. A provider must treat one key as one machine, forever.

**Deciding and telling are separate.** `capacity_requests.status` is what Kratos believes;
`capacity_dispatches` is the durable intention to make the provider agree. Collapsing them would
make a crash between "we decided" and "we told them" indistinguishable from never having decided.

## The four properties, and how each is proved

Each test was also run with its protection removed, because a test that passes either way proves
nothing. The observed failures are recorded here so the next person does not have to repeat that.

### 1. Two controllers cannot double-dispatch

Authority for a request is a `PostgreSQL` advisory lock held for the dispatch. The control plane
runs as more than one instance, and two of them provisioning one request would buy two machines
and strand one, since only one `external_id` can be stored.

The test holds the same lock from another connection — **forcing** the overlap rather than hoping
two futures interleave, which they do not against a local database.

*With the lock made inert:* `a held request must be skipped, not duplicated — left: 1, right: 0`.

### 2. Queued cancellation creates no request

A job cancelled while `queued` never had an attempt, and the foreign key means there is nothing to
derive a request from. No controller logic decides this, so no later change of mind in the
scheduler can get it wrong.

### 3. Provisioning cancellation converges to released

Two cases, and the second is the one that costs money:

- Cancelled before the provider answered: nothing was made, so the request converges straight to
  `released` and the pending dispatch is cancelled. A late dispatch finds the request already
  terminal and never asks for a machine.
- **Cancelled while the provider was answering:** the machine exists by the time we hear back. It
  must be handed back rather than forgotten. The test forces exactly this race — the fake holds
  its provision open until the test has released the request.

*With the post-provision reconciliation disabled:* the released request came back as
`status: "ready", external_id: Some("fake-…"), released_at: None` — a live machine with nothing
pointing at it, which is precisely the stranded capacity this boundary exists to prevent.

### 4. Restart replay reuses the same external id

The crash this design is shaped around: the provider made a machine and the controller died
before recording it. On restart the dispatch is still pending and is sent again, and because the
key is attempt-derived the provider answers with the machine it already has.

The test undoes only what the controller recorded, leaving the provider's world untouched — which
is exactly what that crash leaves behind. A companion test proves `external_id` is never
overwritten, since it is the only handle by which a machine can be released.

### 5. A stale give-up cannot overwrite another controller's success

The overlap that hides a live machine: controller A reads a due dispatch, is slow talking to the
provider and fails; meanwhile B provisions successfully. Failure bookkeeping is therefore done
while the advisory lock is still held, and is a compare-and-set against the `completed_at` and
`attempts` the controller actually read. The re-check under the lock covers `next_attempt_at` and
`attempts` too, not just completion, so a dispatch another controller has already failed and
backed off is no longer treated as due.

*With the compare-and-set removed:* `nor report a failure against a request that succeeded — left:
Some("provider unreachable"), right: None`.

A second, independent guard: **a request holding an `external_id` is never marked `failed`**,
whatever the attempt count says. `failed` is excluded from the open-request index, so filing a
request that holds a machine under it would make the machine both unreleased and invisible.

*With the external-id guard removed:* `capacity that exists must stay visible, not be filed as
failed — status: "failed", external_id: Some("fake-held")`.

### 6. A dispatch only goes to the provider that owns it

`capacity_requests.provider` is the authority, not whichever provider object a caller constructed.
The due query filters on it and the controller re-checks it under the lock. An `external_id` means
nothing to a provider that did not issue it, and asking the wrong one to release it either errors
or — worse — succeeds against something else.

*With the filter and the check removed:* the test's deliberately panicking provider was reached —
`a request recorded against another provider must never reach this one`.

### 7. The schema constrains attempt, job and project together

Three independent foreign keys are each satisfied by an attempt belonging to one job and a job
belonging to another project: capacity billed to a project that never asked for it. Composite keys
against `job_attempts(id, job_id)` and `jobs(id, project_id)` make the database prove the pairs,
the same trick `job_artifacts` already uses.

*With the independent keys restored:* `an attempt belonging to another job must be refused`.

### 8. A parent delete cannot take the release handle with it

Cascading a job or attempt away would delete `external_id` — the only handle the machine can ever
be released by — and the pending release dispatch with it, leaving a paid machine running with
nothing pointing at it. A `BEFORE DELETE` trigger refuses while the request still holds capacity.
A trigger rather than `ON DELETE RESTRICT` because the rule is conditional on row state: a settled
request holds nothing and should not obstruct retention tidying a finished job away, which the
test also checks.

*With the trigger removed:* `deleting a job that still holds capacity must be refused`.

## Failure handling

A refused dispatch records its reason and is retried with exponential backoff, stored as a
timestamp rather than a sleep so it survives a restart. Eight attempts is the stated limit and is
now also the enforced one: the due query will not select a dispatch that has reached it, so the
provider stops being called rather than being asked forever.

What is left behind depends on what was being attempted.

A **provision** that never obtained a handle made nothing, so the request is marked `failed` and
left for a person. A **release** is different: the obligation outlives its retries and the machine
is presumed alive, so the request stays `releasing` — inside the open-request index — and is
surfaced by `outstanding_releases`, the operator view for the one failure this boundary cannot
resolve by itself. Marking it `failed` would drop it out of the index and hide exactly the machine
someone needs to go and deal with.

*With the attempts cutoff removed:* `the provider must be asked exactly as many times as the limit
says, then left alone — left: 12, right: 8`.

### One inversion that did not fire, and what it means

Removing the release-specific branch in `record_failure` did **not** fail its test. That branch is
not what keeps an exhausted release out of `failed`: the `external_id IS NULL` condition on the
terminal update already does, because a releasing request always holds a handle. The branch is
kept because it stops an exhausted release being logged as "given up" — which would tell an
operator the opposite of the truth — but it is not load-bearing for the state invariant, and is
recorded here rather than presented as a protection that has been proved.

## What is not proved here

No real provider exists, so none of this says anything about a cloud API's actual idempotency
semantics — which is the single most important thing a real implementation must get right, and
the first thing to test against a live provider behind a spend limit.

The feature has never been switched on, and switching it on with only the fake registered does
nothing.

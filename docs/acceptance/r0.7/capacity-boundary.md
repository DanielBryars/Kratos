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

### 3. Cancellation converges, but only one case converges immediately

Three cases, distinguished by what is actually known rather than by whether a handle has been
recorded. Getting this distinction wrong is the subject of sections 10 and 12, and it took three
review rounds to remove from the code, so it is worth stating precisely here.

- **Cancelled before the provider was called.** Not "before it answered" — before it was *called*,
  which is a state the code can check: the request is still `requested`, because the claim in
  `provision` moves it out of that state before any provider call. Nothing was made, so the
  request converges straight to `released` and the pending dispatch is cancelled. A late dispatch
  finds the request terminal and never asks for a machine.
- **Cancelled while the provider was answering.** The machine exists by the time we hear back, so
  it must be handed back rather than forgotten. The test forces the race — the fake holds its
  provision open until the test has released the request.
- **Cancelled while the outcome is unknown.** The provider was called and did not answer usably,
  so nobody knows whether a machine exists. This does *not* converge: it cannot, without asserting
  the one thing nobody knows. See section 12.

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

### 9. Capacity in flight cannot be deleted

The delete guard's test is `released_at IS NULL`, not "holds a handle". A request that is merely
`requested` or `provisioning` has no handle *yet* and may have a provider call in flight this
instant: cascade it away and the provider's answer arrives with nowhere to record the machine it
describes. Being certain nothing is running is exactly what `released` means, and it is the only
state that means it.

The test forces the window — the fake holds its provision open, the deletion is attempted while
the handle is still null, and the dispatch is then allowed to finish and record its machine.

*With the guard back at "holds a handle":* the delete succeeded and the test failed at the
assertion that it must not.

A narrower window sits just before that one: the request is read, and is gone by the time the
dispatch acts on it. The `requested -> provisioning` update is therefore the *permission* to call
the provider, and its `rows_affected` is checked rather than assumed.

*With the claim's result assumed:* the provider was asked to build a machine for a request that no
longer existed.

### 10. An unknown outcome is not recorded as a failure

`CapacityError::Unavailable` promises nothing about side effects. A provider can build the machine
and lose the reply, and after enough attempts the old code marked the request `failed` — a claim
that nothing exists, made about the one case where nobody knows.

The distinction is now part of the provider contract rather than an inference: `Refused` is a
promise that nothing was created, and an implementation must use `Unavailable` for anything it
cannot promise that about. An exhausted provision that ended in a refusal becomes `failed`; one
that ended in anything else becomes `unreconciled`, stays in the open-request index, and appears
in `ambiguous_provisions`.

Such a request has two resolutions, and both are tested. `reconcile_provision` asks the provider
the same question again — the entire reason the idempotency key is derived rather than random — and
the machine's lost name comes back, leaving ordinary `ready` capacity that can be released
properly. `resolve_unreconciled` is a person asserting there is no machine, which is never reached
by any automatic path and whose reason is kept, because it is a judgement rather than an
observation.

Cancelling the job does *not* settle it: converging an unreconciled request to `released` would
record the one thing nobody knows.

*With the outcome inferred to be harmless:* `an unknown outcome must not be recorded as a failure —
status: "failed", external_id: None`, while the fake provider held a machine built under that
request's key.

*With cancellation allowed to settle it:* `cancelling must not assert that an unknown machine does
not exist — left: "released", right: "unreconciled"`.

The opposite error is tested too: a provision refused eight times is still recorded as `failed`,
so "we cannot be sure" does not swallow the cases where we can be.

### 11. The last attempt commits as one transaction

Advancing the dispatch, recording the request's error and moving the request to its terminal state
are one decision. Applied separately, a crash after the dispatch reaches its final attempt leaves
it permanently past the due query's cutoff while the request still looks live and appears on no
operator surface — stuck in a way nothing would ever notice.

The test makes the terminal statement, and only the terminal statement, fail, then checks the
dispatch is still retryable rather than spent.

*With the bookkeeping split across separate commits:* `a give-up that could not be completed must
leave the dispatch retryable, not spent — left: 8, right: 7`. Eight is the cutoff: that dispatch
would never have been selected again.

### 12. Cancellation outlives an unknown outcome

The same mistake as section 10, in the other place it lived. Having stopped `record_failure`
inferring "no handle recorded" means "no machine", the inference was still sitting in
`release_capacity`: a cancellation with a null handle deleted the provision dispatch and declared
the request released. After a single create-then-lost-response that threw away the only route back
to a running machine's name.

Cancellation is now a durable fact — `release_requested_at` — recorded before anything else and
independently of whether a handle is known. Two states still converge immediately, and only two,
because they are the only ones where a null handle really does mean no machine: `requested`, which
the provisioning claim moves a request out of *before* the provider is called and which is read
under a row lock, and `failed`, which is only ever reached from a positive refusal. Everything else
keeps its provision dispatch, because that dispatch is the only path back to the handle.

When the handle finally arrives, what it means is decided by `release_requested_at` rather than by
the status — the status could not have been moved to `releasing` at cancellation time, there being
nothing to release — so a recovered machine for a cancelled job converges to a release and never to
ready capacity.

*With the null-handle branch restored:* `a machine of unknown existence must not be declared
released — status: "released", external_id: None`, with the fake holding a machine built under that
request's key.

*With release intent read from the status:* `a cancelled job must not regain ready capacity — left:
"ready", right: "releasing"`.

### 13. The two paths take their locks in one order

`release_capacity` locks the request with `FOR UPDATE` and then touches the outbox. `write_failure`
did the reverse. A job cancelled at the moment a controller is recording a provider error is an
overlap to expect rather than to hope against, and taken in opposite orders the two deadlock —
PostgreSQL aborts one, losing either the failure bookkeeping or the cancellation itself.

Every transaction in this module now takes the request row first and its dispatches second. The
test forces the overlap: a transaction holds the request row exactly as a cancellation does, and
the failure bookkeeping is started underneath it.

*With the dispatch locked first:* the cancellation's outbox write aborted rather than completing.

### 14. A cancelled request the provider refused outright settles itself

The last corner of the same question. A refusal is *knowledge* — it establishes that no machine
exists. Combined with a durable cancellation, both halves are answered: there is nothing to release
and nobody wants it. So the request settles as `released` in the same transaction that spends the
dispatch's final attempt, rather than landing in `failed`.

Left as `failed` it sat in neither operator view, with `released_at` null and the delete guard
blocking cleanup, converging only if some external caller happened to ask for release a second
time — a quiet dead end rather than a visible one.

*With the refusal path ignoring the cancellation:* `a cancelled request the provider has refused
outright must settle itself — left: "failed", right: "released"`, with `release_requested_at` set
and `released_at` null.

Note the asymmetry with section 10, which is the point: an *ambiguous* exhaustion after
cancellation must **not** settle, because nothing established that the machine is absent. Only a
refusal licenses converging.

## Failure handling

A refused dispatch records its reason and is retried with exponential backoff, stored as a
timestamp rather than a sleep so it survives a restart. Eight attempts is the stated limit and is
now also the enforced one: the due query will not select a dispatch that has reached it, so the
provider stops being called rather than being asked forever.

What is left behind depends on what was being attempted.

A **provision** that was positively refused made nothing, so the request is marked `failed`. A
provision that ended any other way is `unreconciled`, because the machine may exist. A **release**
is different again: the obligation outlives its retries and the machine is presumed alive, so the
request stays `releasing` and is surfaced by `outstanding_releases`.

All three stay inside the open-request index, whose predicate is now `released_at IS NULL` and
nothing else. Excluding terminal-*looking* statuses was how a machine could stop being anybody's
problem: `released` is the only state that means nothing is held.

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
the first thing to test against a live provider behind a spend limit. The contract this boundary
leans on hardest is the honesty of `Refused` versus `Unavailable`: a real provider that reports a
timeout as a refusal would defeat section 10 entirely.

**The operator surfaces can be read, but nothing tells anyone to read them.** `ambiguous_provisions`
and `outstanding_releases` are the two states the boundary cannot resolve by itself. Both are now
served, read-only, at `GET /api/v1/operator/capacity/attention`, scoped to the caller's projects
like every other operator list and read from one snapshot. The endpoint and the functions share one
query each, so what counts as needing a person cannot drift between them. Tests prove a member sees
their own project's rows and not another project's, and that a release with attempts left is not
listed. *With the project filter removed:* the scoping test failed — see PR for the recorded run.

That is visibility on request, not the whole requirement. There is still no console panel and no
alert, so nothing routes these states to a person who is not already looking. A first real provider
must not be switched on before that exists, because both states mean "a machine may be running and
Kratos has stopped trying" — which is precisely the situation that costs money quietly.
`resolve_unreconciled` also still has no caller: settling an open question is, by design, a
deliberate human act that needs its own authority decision, and there is currently no way for a
human to perform it.

The feature has never been switched on, and switching it on with only the fake registered does
nothing.

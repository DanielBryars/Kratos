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

## Failure handling

A refused dispatch records its reason and is retried with exponential backoff, stored as a
timestamp rather than a sleep so it survives a restart. After eight attempts the request is marked
`failed` and left for a person: failing quietly forever is how a stuck request becomes invisible.

## What is not proved here

No real provider exists, so none of this says anything about a cloud API's actual idempotency
semantics — which is the single most important thing a real implementation must get right, and
the first thing to test against a live provider behind a spend limit.

The feature has never been switched on, and switching it on with only the fake registered does
nothing.

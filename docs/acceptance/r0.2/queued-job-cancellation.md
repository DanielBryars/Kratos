# R0.2 queued-job cancellation evidence

**Observed:** 2026-09-20 08:32–08:40 Europe/London  
**Status:** Passed in production

## Requirements

- TRN-007 — a queued job can be cancelled
- ACC-005 — a cancelled queued job never begins execution
- ACC-018 — evidence identifies software, hardware, inputs and observed outcomes

## Exercise

The `THESHED2` agent was stopped before submission so that a heartbeat could not race the operator's
cancellation request. The authenticated production interface queued `Cancellation acceptance` as run
`710a927b-9f60-4276-bb38-5275ce87a2f1` with:

- workload image
  `ghcr.io/danielbryars/kratos-gpu-health-check@sha256:3ee068a54416c67c32b5d6369e9120fd4ee9b62ffd7865dcde7a688f482168a9`;
- one GPU; and
- a 120-second execution limit.

The interface displayed `QUEUED` and an unavailable completion estimate. The operator cancelled the
run four seconds after submission. The durable run state changed to `CANCELLED` with no assigned
worker.

Docker Desktop was restarted after concurrent local container maintenance, and the existing
`kratos-agent-state` volume and pinned agent image remained present. The same agent container then
started with worker identity `f6681ff0-c6f8-4e1f-b61c-55ec6779900f`. After its next signed heartbeat,
the production interface showed `THESHED2` as `ONLINE IDLE`; the cancelled run remained unassigned and
no `kratos-job-*` container was created.

## Automated evidence

The control-plane integration test creates and cancels a queued job twice, verifies that the original
completion timestamp is stable, sends an eligible healthy-worker heartbeat, and asserts that:

- the repeated cancellation is idempotent;
- the heartbeat returns no assignment;
- the job has no assigned worker; and
- the database contains zero attempts for the cancelled job.

The test runs against PostgreSQL with the production migrations and passed in CI before deployment.

# Durable training output exercise

**Status:** Control-plane prerequisites are deployed; execution is blocked until the protocol 1.1
worker upload loop is merged and deployed.

This exercise proves that a real CUDA training job can stage a model checkpoint on a worker, upload
it directly to private Cloud Storage, and expose independently verified storage evidence without
giving the workload network access or a cloud credential.

## Preconditions

- The control plane SHALL include the GCS artefact endpoints from ADR-014.
- The selected worker SHALL advertise protocol 1.1 and run the reviewed upload implementation.
- The training image SHALL be referenced by its published `sha256` digest.
- The worker SHALL be online, approved, idle and report a healthy GPU.
- The operator SHALL request mandatory output `model.pt`, role `model`, media type
  `application/x-pytorch`, with a 1 MiB limit.

The exercise MUST NOT start while the worker still advertises protocol 1.0. The scheduler is
expected to leave an output job queued in that state.

## Worker upgrade checkpoint

The existing THESHED2 enrolment SHALL be upgraded in place. The replacement SHALL remove only the
`kratos-agent` container and SHALL preserve the `kratos-agent-state` Docker volume. Before the
replacement, the operator SHALL verify that the volume exists and retain the current immutable
agent digest as the rollback target. The replacement SHALL reuse the same display name, control
plane, state volume and immutable GPU health-check image. If the new container does not reconnect
under the existing worker identity, the operator SHALL restore the rollback digest with the same
arguments; the state volume SHALL NOT be deleted or recreated.

| Upgrade input | Recorded value |
|---|---|
| Container | `kratos-agent` |
| Worker | `THESHED2` |
| State volume | `kratos-agent-state` |
| Rollback agent digest | `sha256:6b47ca98310f9278dc4df7f6295298ade41c66db6aab119fb765a4b5a68d1312` |
| Health-check digest | `sha256:3ee068a54416c67c32b5d6369e9120fd4ee9b62ffd7865dcde7a688f482168a9` |

## Procedure

1. Record the control-plane revision, worker image digest and training image digest below.
2. Submit **Kratos Shapes durable model** through the operator UI with a 300-second runtime limit
   and the mandatory output contract above.
3. Observe that the assigned workload has no network and writes `/kratos/outputs/model.pt`.
4. Observe the artefact move through `declared`, `uploading`, `verifying` and `verified`. A fast
   transfer MAY make an intermediate state too brief to capture; server audit records remain the
   authoritative transition evidence.
5. Confirm the job reaches `succeeded` only after `model.pt` is verified.
6. Confirm the UI shows the declared and verified SHA-256 and CRC32C values, exact byte length,
   immutable Cloud Storage generation and verification time.
7. Confirm no bucket name, object key, resumable session URI or read credential appears in the
   operator response, UI, worker logs or captured evidence.
8. Reload the page and restart the agent. Confirm the same verified evidence remains visible and
   the worker retains its identity.

## Acceptance criteria

- The workload exits successfully and reports `checkpoint_staged: true`.
- Exactly one current-attempt `model.pt` artefact is visible.
- The declared and verified lengths, SHA-256 values and CRC32C values match.
- The artefact has a decimal Cloud Storage generation and status `verified`.
- The job has status `succeeded`; it SHALL NOT succeed before verification.
- The worker returns to `online` and `idle` with no duplicate execution attempt.
- Refresh and agent restart preserve the verified evidence.

## Evidence

| Item | Value |
|---|---|
| Control-plane revision | `1ca045014b4b0d4da38dd45012ba81c1432cd39a` |
| Worker image digest | Pending protocol 1.1 release |
| Training image digest | `sha256:c0f8df79289f200706c5a2b19bb45f45c0e1258c9b774eba5f11c8c51fe2cc31` |
| Job ID | Pending live run |
| Attempt ID | Pending live run |
| Artefact ID | Pending live run |
| Storage generation | Pending live run |
| Declared SHA-256 / CRC32C | Pending live run |
| Verified SHA-256 / CRC32C | Pending live run |
| Result | Pending live run |

The deployment workflow for control-plane revision `1ca0450` completed successfully. Live
`/healthz` and `/readyz` checks returned `ok`, the database reported `ready`, and the published
OpenAPI document included the authenticated protocol 1.1 `abandon-upload` operation with its two
required request fields. This proves the server recovery prerequisite is deployed; it is not a
substitute for the end-to-end workload evidence above.

Local preflight on an NVIDIA GeForce RTX 5090 completed successfully before publication. The
container ran with no network and a read-only root filesystem, wrote a 3,081-byte `model.pt` through
the isolated output mount, and reported SHA-256
`cee85ca2610837fa04a45edaf69cb03173e409e8b01ff38733e406a676932f2b`. This proves the workload
contract only; it is not evidence of upload or cloud durability.

## Failure handling

If upload or verification fails, record the job, attempt and artefact identifiers and leave the
result as failed or incomplete. Do not edit database state or manufacture verification evidence.
The agent SHALL retain its local transfer state for retry, and the control plane SHALL continue to
hide unverified storage evidence.

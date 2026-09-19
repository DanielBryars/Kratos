# R0.2 first scheduled GPU job evidence

**Observed:** 2026-09-19 22:44 Europe/London  
**Status:** First cloud-scheduled GPU container completed successfully on the home worker

## Requirements

- ACC-003 — Linux container performs actual GPU computation without CPU fallback
- ACC-018 — acceptance evidence identifies software, hardware, runtime, input and outcomes
- ACC-022 — a compatible, authorised worker receives the job
- R0.2 scheduling slice — immutable image submission, queueing, assignment lease, bounded execution,
  terminal result and runner cleanup

This record demonstrates the first scheduling slice. It does not accept the complete R0.2 release;
dataset provenance, durable artefacts, training telemetry, cancellation and network-loss exercises
remain outstanding.

## Environment and identity

| Item | Observed value |
|---|---|
| Control-plane release | Git commit `0c80fa9` (`feat: schedule first GPU jobs (#22)`) |
| Worker | `THESHED2` |
| Worker identity | `f6681ff0-c6f8-4e1f-b61c-55ec6779900f` |
| Worker state before submission | `ONLINE IDLE`, group `Home` |
| GPU | NVIDIA GeForce RTX 5090, 31.8 GB displayed capacity |
| Agent image | `ghcr.io/danielbryars/kratos-agent@sha256:d7d03326f402b2342d538a76f38718ccefe23341d96ceab588d4182f40aa5b42` |
| Workload image | `ghcr.io/danielbryars/kratos-gpu-health-check@sha256:3ee068a54416c67c32b5d6369e9120fd4ee9b62ffd7865dcde7a688f482168a9` |
| Requested resources | One GPU; 120-second maximum runtime |

## Scheduled execution

An authenticated operator submitted `RTX 5090 matrix check` through the production web interface at
22:44:08. The control plane displayed the job as `QUEUED`. On the next signed heartbeat, the
PostgreSQL scheduler atomically assigned the oldest compatible job to the approved worker. The
agent ran the immutable image as a constrained sibling container with no network, a read-only root
filesystem, dropped capabilities, `no-new-privileges`, bounded CPU, memory, process and runtime
limits, and GPU device zero only.

Expected: the job transitions from queued to a terminal success state, reports an actual CUDA
operation with zero error, remains within its runtime bound and returns the worker to idle.

Observed: the production interface displayed `SUCCEEDED`, worker `f6681ff0`, and this result:

```json
{
  "schema_version": "1.0",
  "status": "healthy",
  "checked_at": "2026-09-19T21:44:30.736188+00:00",
  "device_index": 0,
  "device_name": "NVIDIA GeForce RTX 5090",
  "operation": "float32 matrix multiplication by identity",
  "matrix_size": 512,
  "max_absolute_error": 0.0,
  "duration_ms": 308.492,
  "cuda_driver_api_version": "13.3",
  "cuda_runtime_version": "12.9"
}
```

The agent remained running with zero restarts. After the result acknowledgement, no
`kratos-job-*` container remained and the fleet interface again displayed the worker as
`ONLINE IDLE`. This verifies cleanup after successful result delivery.

## Remaining R0.2 acceptance work

- Run an approved training workload with exact code, dataset and output artefact provenance.
- Stream training progress, logs and OpenTelemetry signals and associate the run with MLflow.
- Cancel a queued job and verify that it never starts.
- Exercise connectivity loss across assignment and result delivery and verify that execution is
  not duplicated.
- Persist and retrieve durable output artefacts.

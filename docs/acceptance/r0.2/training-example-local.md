# R0.2 CUDA training evidence

**Observed:** 2026-09-20 08:41 Europe/London  
**Status:** Published image passed local checks and a production-scheduled GPU run

## Requirements

- DAT-002 — stable dataset version and verifiable content identity
- DAT-004 — workload validates its dataset before training
- DAT-007 — run output identifies the exact dataset version
- REP-001 — run output records configuration, seed and hardware/runtime details
- REP-003 — the environment is built as an immutable container image
- REP-007 — repeatability settings do not claim universal numerical determinism
- ACC-003 — actual GPU computation succeeds and loss of GPU access fails without CPU fallback
- ACC-018 — evidence identifies software, hardware, inputs and outcomes

## Environment and inputs

| Item | Observed value |
|---|---|
| Local image identity | `sha256:02339ca67c49ac2f2da5904c72e1efdb35db22fc28439ea3fb8cd3d2575ab49d` |
| PyTorch | `2.8.0+cu128` |
| CUDA runtime | `12.8` |
| GPU | NVIDIA GeForce RTX 5090, compute capability 12.0 |
| Workload | `kratos-training-example-v1` |
| Dataset | `kratos-shapes-v1`, 384 rows |
| Dataset SHA-256 | `c338e2ffabc1a0470ad2d4c0b9efa3ab53a82135aaca54845b3a3ebafd746451` |
| Configuration | 180 epochs, Adam, learning rate 0.025, seed 20260920, deterministic algorithms enabled |

The initial PyTorch 2.7 build could not execute kernels for the RTX 5090 `sm_120` target. The
workload was corrected to PyTorch 2.8 with CUDA 12.8 before this evidence was collected.

## Positive case

The image ran with GPU device zero, no network, a read-only root filesystem and a bounded temporary
filesystem. It exited zero after an actual CUDA training loop.

| Metric | Observed value |
|---|---:|
| Initial training loss | 1.207397 |
| Final training loss | 0.000382 |
| Training accuracy | 1.0 |
| Validation accuracy | 1.0 |
| Training duration | 555.746 ms |
| Model-state SHA-256 | `fde871c1341bfba0cff973aad4f7850418cccf38f536722930f26917b20e7c7c` |
| Checkpoint SHA-256 | `0bc20fb6b5e89eb91ad855338a11e4a11b1c8baa725ec6c2f20425c4e0175ca9` |

The checkpoint was intentionally ephemeral and the result reported `checkpoint_durable=false`.
Durable upload remains a separate R0.2 acceptance gate.

## Negative case

The same image ran without a GPU device request. It exited one and returned a structured failure with
`cpu_fallback=false`, `error_type=RuntimeError` and detail `CUDA GPU is required; CPU fallback is
disabled`. No CPU training was attempted or reported as successful.

## Published production run

The reviewed image passed the fixable HIGH/CRITICAL vulnerability gate and was published with SBOM
and provenance before Kratos scheduled its immutable digest through the production control plane.

| Item | Observed value |
|---|---|
| Completed | 2026-09-20 09:55 Europe/London |
| Published image | `ghcr.io/danielbryars/kratos-training-example@sha256:63df9dbfbfc38ad8045e23720890f17b258946959246501431aa743b77dcde4d` |
| Job ID | `2da0c0f7-5807-4a7b-bcf0-8ccebca8f1dc` |
| Attempt ID | `d6a7047f-bf63-4230-ad53-2d4762f7bad0` |
| Worker | `f6681ff0-c6f8-4e1f-b61c-55ec6779900f` (`THESHED2`) |
| Result | Succeeded on NVIDIA GeForce RTX 5090 |
| Training duration | 605.784 ms |
| Training and validation accuracy | 1.0 / 1.0 |
| Dataset SHA-256 | `c338e2ffabc1a0470ad2d4c0b9efa3ab53a82135aaca54845b3a3ebafd746451` |
| Model-state SHA-256 | `fde871c1341bfba0cff973aad4f7850418cccf38f536722930f26917b20e7c7c` |
| Checkpoint SHA-256 | `0bc20fb6b5e89eb91ad855338a11e4a11b1c8baa725ec6c2f20425c4e0175ca9` |

The result carried matching `kratos.job.id` and `kratos.attempt.id` resource attributes. The agent
removed the attempt container after acknowledgement and returned to `ONLINE IDLE` without losing its
registered identity.

The first registry pull dominated the wall-clock time. The console displayed “Waited 0s · ran 5m
34s”, while the structured workload measured 605.784 ms of training. The control plane currently
uses assignment/submission timestamps as execution timestamps and the agent does not heartbeat while
pulling or supervising an image, so the worker temporarily appeared stale. These observations are
limitations to fix, not training-performance evidence.

## Remaining acceptance

Durable checkpoint storage, authoritative GCS verification, telemetry export, MLflow association and
the witnessed network-loss exercise remain outstanding.

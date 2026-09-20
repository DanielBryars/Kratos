# R0.2 local CUDA training evidence

**Observed:** 2026-09-20 08:41 Europe/London  
**Status:** Corrected PyTorch 2.8/CUDA 12.8 image passed positive and negative GPU checks

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

## Remaining acceptance

CI SHALL publish the reviewed image and record its registry digest. Kratos SHALL then schedule that
immutable digest through the production worker path and retain its structured result. Durable
checkpoint storage, telemetry and MLflow association remain outstanding.

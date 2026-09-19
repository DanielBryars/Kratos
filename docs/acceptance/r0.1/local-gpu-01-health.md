# R0.1 GPU health evidence — local-gpu-01

**Observed:** 2026-09-19  
**Status:** Partial R0.1 evidence; agent-to-control-plane reporting remains outstanding

## Requirements

- ENV-001 — Linux training container on a Windows host
- ENV-002 — GPU exposed to the Linux container
- ENV-004 — actual GPU computation in the intended container environment
- ENV-005 — no silent CPU fallback
- ACC-003 — successful computation and explicit loss-of-GPU failure
- ACC-018 — recorded version, hardware, input, expected and observed result

## Environment

| Item | Observed value |
|---|---|
| Worker alias | `local-gpu-01` |
| Host OS report | Windows product `Windows 10 Pro`, version `2009`, build `26200` |
| Linux execution environment | WSL2 kernel `6.6.87.2-microsoft-standard-WSL2`, `x86_64` |
| Docker client/server | `29.5.3` / `29.5.3` |
| GPU | NVIDIA GeForce RTX 5090 |
| GPU memory reported by `nvidia-smi` | 34,190,917,632 bytes |
| NVIDIA driver | `610.88` |
| Health image local identity | `sha256:bac3c52d80e3a7d4ab12b13d74c747349f1a5cc49567887aa2f38bb810bab1a8` |
| Python | `3.12.3` |
| CuPy | `13.6.0` |

The worker alias intentionally excludes the physical hostname.

## Positive case

The health image was started with GPU device zero assigned. It created two 512×512 `float32`
matrices, multiplied the input by an identity matrix through CUDA, synchronised the device, copied
the result back and compared it with the input.

Expected: exit zero, `status=healthy` and zero maximum absolute error.

Observed:

```json
{
  "schema_version": "1.0",
  "status": "healthy",
  "device_index": 0,
  "device_name": "NVIDIA GeForce RTX 5090",
  "operation": "float32 matrix multiplication by identity",
  "matrix_size": 512,
  "max_absolute_error": 0.0,
  "duration_ms": 251.198,
  "cuda_driver_api_version": "13.3",
  "cuda_runtime_version": "12.9"
}
```

The duration includes first-use CUDA library initialisation and is evidence of completion, not a
performance benchmark.

## Negative case

The same image was started without a GPU device request.

Expected: non-zero exit and a structured unhealthy result; no CPU fallback.

Observed: exit one with `status=unhealthy`, `error_type=CUDARuntimeError` and a CUDA driver/runtime
availability error. No result was reported as healthy.

## Remaining work

- Publish the health image by immutable registry digest.
- Make the agent invoke that digest through its executor interface.
- Attach the structured result and image digest to the capability heartbeat.
- Repeat the same evidence on `local-gpu-02`.

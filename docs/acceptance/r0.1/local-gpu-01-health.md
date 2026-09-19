# R0.1 GPU health evidence — local-gpu-01

**Observed:** 2026-09-19  
**Status:** GPU health passed on the first home worker; second-host evidence remains outstanding

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
| Published health image | `ghcr.io/danielbryars/kratos-gpu-health-check@sha256:3ee068a54416c67c32b5d6369e9120fd4ee9b62ffd7865dcde7a688f482168a9` |
| Deployed agent image | `ghcr.io/danielbryars/kratos-agent@sha256:33ac7674395978a0c85638e8373ad6ac21b5d705a2211fd6a82f10d582b54c68` |
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

## Agent executor case

The non-root agent container was given the Docker socket and its actual socket group as a
supplementary group. It launched the immutable local image identity through the Docker API with:

- networking disabled;
- a read-only root filesystem;
- all Linux capabilities dropped;
- `no-new-privileges`;
- one CPU, 1 GiB memory and 128-process limits;
- bounded temporary filesystems; and
- only GPU device zero assigned.

The agent received and validated healthy structured evidence for the RTX 5090, then removed the
health-check container. No managed health-check container remained after collection.

The published images passed their CI vulnerability gates and were anonymously pullable from GHCR.
After the compatible control plane was deployed, the installed agent was upgraded in place while
retaining worker identity `f6681ff0-c6f8-4e1f-b61c-55ec6779900f` and agent instance identity
`e10cb854-7aaf-4dd4-8fff-2dabbefd6b45`. Its next signed capability heartbeat reported the cached
startup check as healthy. On 2026-09-19, the live fleet UI displayed `GPU computation passed on
NVIDIA GeForce RTX 5090 in 276.578 ms` while the worker was online, idle and assigned to `Home`.

## Remaining work

- Repeat the same evidence on `local-gpu-02`.

Agent-to-control-plane registration and live capability reporting are now complete for this worker.
See [the liveness evidence](local-gpu-01-liveness.md) for its registered identity, Home-group membership,
stale/offline transitions and credential-preserving reconnect.

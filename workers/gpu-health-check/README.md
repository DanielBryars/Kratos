# GPU health check

This image executes a CUDA-backed `float32` matrix multiplication, synchronises the device and
verifies the result. It exits unsuccessfully when no GPU is available or when computation fails; it
does not fall back to CPU execution.

From the repository root:

```shell
docker build --file workers/gpu-health-check/Dockerfile --tag kratos-gpu-health-check .
docker run --rm --gpus device=0 kratos-gpu-health-check
```

GitHub Actions publishes `edge`, `sha-<commit>` and `health-v*` tags with provenance and an SBOM. The
worker agent accepts only an immutable digest, invokes the image as a constrained sibling container
and attaches its structured JSON result to every subsequent capability heartbeat.

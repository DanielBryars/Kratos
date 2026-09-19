# GPU health check

This image executes a CUDA-backed `float32` matrix multiplication, synchronises the device and
verifies the result. It exits unsuccessfully when no GPU is available or when computation fails; it
does not fall back to CPU execution.

From the repository root:

```shell
docker build --file workers/gpu-health-check/Dockerfile --tag kratos-gpu-health-check .
docker run --rm --gpus device=0 kratos-gpu-health-check
```

The worker agent will eventually invoke this image by immutable digest and attach its structured JSON
result to the worker capability report.


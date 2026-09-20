# Kratos workload template

Copy this directory when starting a Python GPU workload. It is deliberately framework-neutral;
add PyTorch, JAX, TensorFlow or another pinned dependency to `pyproject.toml`, then run `uv lock`.
The complete PyTorch example in `workers/training-example` shows a real training loop and model
checkpoint.

The starter already handles the parts every Kratos workload needs:

- validates the job and attempt identifiers injected by the worker;
- fails if the assigned NVIDIA GPU is unavailable;
- runs as a non-root user in the offline, read-only Kratos sandbox;
- writes `result.json` atomically beneath `/kratos/outputs`;
- emits one bounded JSON result line to standard output.

After copying the directory, update its source paths in the two `Dockerfile` `COPY` instructions.
Replace `WORKLOAD_VERSION` and the body of `run_user_workload`. Add code, fixed configuration and
any data needed at runtime to the image in the `Dockerfile`. Never put credentials in the image.
The scheduled job must request `result.json` with role `result`, media type `application/json`, and
a maximum size large enough for your result.

See [the workload authoring guide](../../docs/guides/workload-authoring.md) for build, local test,
publish and scheduling steps.

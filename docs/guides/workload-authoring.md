# Authoring a Kratos workload

This is the shortest path from Python code to a GPU job on Kratos. Start with
`workers/workload-template` for a new framework-neutral workload, or copy
`workers/training-example` when a working PyTorch training job is the closer fit.

## The container contract

A workload is an immutable Linux `amd64` container image. Kratos gives the container one NVIDIA
GPU and starts its entry point with:

- no network;
- a read-only root filesystem;
- a writable temporary filesystem at `/tmp`;
- a writable, per-attempt durable-output directory at `/kratos/outputs`;
- `KRATOS_JOB_ID` and `KRATOS_ATTEMPT_ID` environment variables;
- `OTEL_RESOURCE_ATTRIBUTES` containing the same two identifiers.

The process SHALL exit with code zero only after its required outputs are complete. It SHOULD emit
one compact JSON line on standard output. Kratos retains at most 64 KiB from each output stream, so
large logs, models and reports belong under `/kratos/outputs`.

The current release does not mount datasets, pass arbitrary job parameters or inject secrets. Code,
fixed configuration and data needed during execution therefore SHALL be copied into the image at
build time. Images may be public and workloads run without cloud credentials, so the image SHALL
contain no secrets. Dataset references, parameterised jobs and scoped secret delivery need explicit
control-plane contracts before they can be used safely.

## Adapt the starter

Copy `workers/workload-template` to a new directory. Change the project name and
`WORKLOAD_VERSION`, update both `COPY workers/workload-template/...` source paths in the
`Dockerfile`, then replace `run_user_workload` in `workload.py`. Add pinned framework dependencies
to `pyproject.toml` and regenerate `uv.lock`:

```shell
cd workers/my-workload
uv lock
uv run ruff format .
uv run ruff check .
uv run mypy workload.py tests
uv run pytest
```

Write every durable file beneath `/kratos/outputs`. Use a relative logical path such as
`checkpoints/model.pt`, and write to a temporary file in the same directory before atomically
renaming it to the declared path. Do not create or depend on files elsewhere: the root filesystem
is read-only and the container is deleted after collection.

CUDA sees the assigned device as device zero inside the container. A workload SHALL fail clearly if
its framework cannot use that device; silently falling back to CPU would consume a GPU worker while
producing misleading timing and cost data.

## Test the real sandbox locally

From the repository root, build and run the unmodified starter:

```shell
mkdir -p .tmp/workload-outputs
docker build --file workers/workload-template/Dockerfile --tag my-kratos-workload .
docker run --rm --gpus device=0 --network none --read-only \
  --cap-drop ALL --security-opt no-new-privileges \
  --memory 8g --cpus 4 --pids-limit 512 \
  --env KRATOS_JOB_ID=22222222-2222-4222-8222-222222222222 \
  --env KRATOS_ATTEMPT_ID=11111111-1111-4111-8111-111111111111 \
  --mount type=bind,source="$PWD/.tmp/workload-outputs",target=/kratos/outputs \
  --tmpfs /tmp:rw,noexec,nosuid,size=1g my-kratos-workload
```

The command SHALL exit zero, print one JSON result, and create
`.tmp/workload-outputs/result.json`. Run this exact sandbox after adding your code; a normal Docker
run can hide writes to the root filesystem or accidental network access that Kratos will reject.

## Publish an immutable image

Create a package for the workload in a container registry, build for `linux/amd64`, and publish it.
Kratos SHALL be given the resulting digest, never a mutable tag:

```shell
docker buildx build --platform linux/amd64 \
  --file workers/my-workload/Dockerfile \
  --tag ghcr.io/<owner>/my-workload:dev --push .
docker buildx imagetools inspect ghcr.io/<owner>/my-workload:dev
```

Copy the `sha256:...` digest from the inspection result and form the image reference
`ghcr.io/<owner>/my-workload@sha256:<digest>`. The registry package must be public until Kratos has
an explicit private-registry credential flow. The existing training-example publishing workflow
shows the required vulnerability scan, provenance and SBOM steps for a permanent workload.

## Queue it in Kratos

Open **Schedule GPU work** in the web interface and enter:

1. a name that identifies the experiment;
2. the complete digest-pinned image reference;
3. a maximum runtime between 30 and 3,600 seconds;
4. **Require a durable output** when the workload writes an output;
5. the exact relative output path, role, media type and maximum size.

For the unchanged starter, declare `result.json`, role `result`, media type `application/json`, and
1 MiB. A declared output is mandatory: a missing file, a file over its limit or an upload that
cannot be verified makes the job fail. The current web form supports one declared output; the API
contract supports more when a workload needs them.

The worker pulls the immutable image, assigns one healthy GPU, runs the sandbox and reports the
actual container start and finish times. After the process stops, it hashes each declared file and
uploads it without giving the workload any storage credential. Kratos marks the output verified
only after cloud storage confirms its length and checksums.

The job result view is the first place to diagnose a failure. A non-zero exit means the workload
rejected its environment or its own code failed. A missing-output failure means the process exited
without completing the declared path. Storage and delivery failures occur after your process exits
and are reported separately from its standard output.

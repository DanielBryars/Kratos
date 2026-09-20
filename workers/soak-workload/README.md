# CUDA soak workload

This image is Kratos' fixed-duration GPU workload. It exists for witnessed exercises that need a job
to still be running while something is done to its worker, such as the R0.2 network-loss exercise in
which a worker's network is unplugged and reconnected mid-job to prove the job is not executed
twice. The fast smoke workload in `workers/training-example` finishes in about half a second and is
unchanged; this workload SHALL NOT be used as a substitute for it.

The workload trains a small neural-network classifier on a stream of synthetic batches that are
generated on the GPU from a recorded seed and labelled by a fixed, noisy teacher, so the loss stays
meaningful for the whole run. There is no dataset to download and no network access. It requires
CUDA and fails rather than falling back to CPU execution. GPU memory use stays well under 2 GB, and
the measured peak is reported in the result.

## Fixed duration

Job submission accepts only an immutable image digest and a timeout, so the duration is baked into
the image: the training loop runs for **600 seconds** of wall-clock time. The clock starts once CUDA
is initialised, so the container lives a few seconds longer than that. Schedule the image with a
runtime limit of at least 900 seconds.

`KRATOS_SOAK_SECONDS` MAY override the duration for local testing only. It SHALL be a whole number
between 10 and 3300; any other value is a structured failure, never a silent default. The Kratos
worker injects only `KRATOS_JOB_ID`, `KRATOS_ATTEMPT_ID` and `OTEL_RESOURCE_ATTRIBUTES`, so a
scheduled job always runs for 600 seconds. The result records the duration and where it came from
(`image-default` or `KRATOS_SOAK_SECONDS`).

## Output

Every line on standard output is one compact JSON object with a `record` discriminator:

- `progress` records are spread evenly over the run, at most 19 of them: one every 30 seconds for
  the default duration. Each carries the elapsed seconds, steps completed, training loss and the
  mean throughput since the start **with its unit** (`steps/s`).
- The last line is always a single `result` record. Its `status` is `succeeded`, `interrupted` or
  `failed`. It records the start and finish times, configured and actual duration, steps completed,
  throughput with its unit, seed, PyTorch, CUDA, driver and GPU identities, Python and platform, and
  `cpu_fallback: false`. The driver version is `null` when `nvidia-smi` cannot be queried; nothing
  is guessed.

The worker keeps only the first 64 KiB of standard output. The workload therefore enforces a total
budget of 16 KiB across all records, of which 4 KiB is reserved for the final result. A progress
record that would not fit is suppressed and counted in the result rather than allowed to truncate
it. A full run writes roughly 8 KiB.

The worker supplies `KRATOS_JOB_ID` and `KRATOS_ATTEMPT_ID` to every job container. This workload
requires valid UUID values and emits them under `run` in every record, so each line can be
correlated with its attempt. The result repeats them as the future OpenTelemetry resource attributes
`kratos.job.id` and `kratos.attempt.id`. Failed runs preserve the identifiers when they were valid.

Because the loop is bounded by wall-clock time, the number of steps, and therefore the final loss
and model state, vary with hardware and load. The seed and deterministic settings are recorded, but
the result states plainly that these values are not claimed to be reproducible.

## Termination

On `SIGTERM` the workload stops within a few training steps, prints a `result` record with status
`interrupted` describing what had been completed, and exits with code 143. It exits 0 only when the
full duration has elapsed, and 1 on any failure. The handler is installed once Python and PyTorch
have loaded; a signal in the first few seconds of start-up is left to Docker's stop timeout.

Nothing is written to the filesystem, so the workload runs under the worker sandbox: no network, a
read-only root filesystem, all capabilities dropped and a non-root user.

## Running locally

From the repository root, a 30 second run under the worker's sandbox flags:

```shell
docker build --file workers/soak-workload/Dockerfile --tag kratos-soak-workload .
docker run --rm --gpus device=0 --network none --read-only --cap-drop ALL \
  --security-opt no-new-privileges --memory 8g --cpus 4 --pids-limit 512 \
  --tmpfs /tmp:rw,noexec,nosuid,size=1g \
  --env KRATOS_SOAK_SECONDS=30 \
  --env KRATOS_JOB_ID=22222222-2222-4222-8222-222222222222 \
  --env KRATOS_ATTEMPT_ID=11111111-1111-4111-8111-111111111111 \
  kratos-soak-workload
```

`just soak` does the same. Omit `KRATOS_SOAK_SECONDS` to run the full 600 seconds.

Schedule the immutable `ghcr.io/danielbryars/kratos-soak-workload@sha256:...` reference. GitHub
Actions publishes `edge`, `sha-<commit>` and `soak-v*` tags with provenance and an SBOM.

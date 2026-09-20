# CUDA training example

This image is Kratos' first complete training workload. It trains a small neural-network classifier
on an exact, bundled version of the synthetic **Kratos Shapes** dataset. The workload requires CUDA
and fails rather than falling back to CPU execution.

The committed CSV is dataset version `kratos-shapes-v1`, containing 384 labelled, four-feature
samples. Its required content digest is
`sha256:c338e2ffabc1a0470ad2d4c0b9efa3ab53a82135aaca54845b3a3ebafd746451`.
The data was generated once with Python's seeded Mersenne Twister from three separated Gaussian
clusters and then committed; training reads the CSV rather than regenerating it.

The single JSON line written to standard output records:

- workload, dataset and resolved training configuration;
- seed and deterministic CUDA settings;
- PyTorch, CUDA and GPU identities;
- loss, accuracy and training duration;
- a canonical model-state hash and serialized checkpoint hash.

The worker supplies `KRATOS_JOB_ID` and `KRATOS_ATTEMPT_ID` to every job container. This workload
requires valid UUID values, emits them under `run`, and repeats them as the future OpenTelemetry
resource attributes `kratos.job.id` and `kratos.attempt.id`. Failed runs preserve the identifiers
when they were valid, allowing operator output to be correlated before collectors or MLflow are
deployed.

The checkpoint is written to `/kratos/outputs/model.pt`, the isolated output directory mounted by
the worker for this attempt. Schedule the job with a mandatory `model.pt` output requirement, role
`model`, media type `application/x-pytorch`, and a limit of at least 1 MiB. The workload fails if the
worker has not supplied that mount, so a successful result cannot claim a durable checkpoint that
was written only to ephemeral container storage. The worker hashes and uploads the stopped
container's file before Kratos marks the job and output as verified. Deterministic settings improve
repeatability but cannot promise bit-identical results when GPU, driver, CUDA or PyTorch versions
differ.

The workload result says `checkpoint_staged: true` after the local file is complete. It does not
claim cloud durability. Only the control plane's verified artefact evidence establishes that claim.

From the repository root:

```shell
mkdir -p .tmp/outputs
docker build --file workers/training-example/Dockerfile --tag kratos-training-example .
docker run --rm --gpus device=0 --network none --read-only \
  --env KRATOS_JOB_ID=22222222-2222-4222-8222-222222222222 \
  --env KRATOS_ATTEMPT_ID=11111111-1111-4111-8111-111111111111 \
  --mount type=bind,source="$PWD/.tmp/outputs",target=/kratos/outputs \
  --tmpfs /tmp:rw,noexec,nosuid,size=1g kratos-training-example
```

The image is designed for the existing Kratos job sandbox. Schedule its immutable
`ghcr.io/danielbryars/kratos-training-example@sha256:...` reference with a runtime limit of at least
120 seconds. GitHub Actions publishes `edge`, `sha-<commit>` and `training-v*` tags with provenance
and an SBOM.

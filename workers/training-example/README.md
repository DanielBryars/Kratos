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

The checkpoint is written to `/tmp/kratos-training-example-v1.pt`. Under the R0.2 worker sandbox it
is intentionally ephemeral, so the result says `checkpoint_durable: false`. Durable artifact upload
is a later platform slice. The hashes still prove the identity of the model produced during this
run. Deterministic settings improve repeatability but cannot promise bit-identical results when GPU,
driver, CUDA or PyTorch versions differ.

From the repository root:

```shell
docker build --file workers/training-example/Dockerfile --tag kratos-training-example .
docker run --rm --gpus device=0 --network none --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=1g kratos-training-example
```

The image is designed for the existing Kratos job sandbox. Schedule its immutable
`ghcr.io/danielbryars/kratos-training-example@sha256:...` reference with a runtime limit of at least
120 seconds. GitHub Actions publishes `edge`, `sha-<commit>` and `training-v*` tags with provenance
and an SBOM.

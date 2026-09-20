# Local observability stack

The [ADR-009](../docs/architecture/decisions/009-observability-and-mlflow.md) services — Grafana,
Prometheus, Loki, Tempo, an OpenTelemetry Collector gateway and MLflow — as a Compose bundle that
runs on one machine. It exists so the configuration, retention and cardinality limits can be proven
before anything is deployed to GCP. It costs nothing to run and creates no cloud resource.

This is **not** the deployment. The cloud stack is a separate, cost-gated Terraform root; these
files are its rehearsal and the source of its configuration.

## Run it

```shell
cd observability
cp .env.example .env          # .env is ignored by Git
# set KRATOS_GRAFANA_ADMIN_PASSWORD in .env
docker compose up -d
docker compose run --rm smoke
```

The smoke service sends one metric, one log record and one span through the gateway and logs one
MLflow run, all carrying the same worker, job and attempt identifiers, then reads each one back
from the store that should hold it, checks all three Grafana datasources report healthy, and checks
that Grafana refuses an unauthenticated request. It prints `"status": "passed"` and the evidence.

- Grafana: <http://127.0.0.1:13000> (sign in with the values from `.env`)
- MLflow: <http://127.0.0.1:15000>
- OTLP ingestion: `127.0.0.1:14317` (gRPC) and `127.0.0.1:14318` (HTTP)

`docker compose down` stops the stack; add `-v` to discard its data.

## What the configuration commits to

**Exposure.** Prometheus, Loki and Tempo have no published port and sit on an `internal` Docker
network with no route off the host. Only Grafana, MLflow and the gateway are published, and only on
`127.0.0.1`. Grafana has anonymous access and sign-up disabled and its datasources provisioned
read-only. MLflow's host-header middleware is left enabled and given an explicit allow-list.

**Pinning.** Every image is pinned by digest; the tag beside it is documentation. Grafana's plugin
preinstall is disabled, because it otherwise downloads unpinned plugins at startup.

**Identity (MON-018).** `service.name` and the Kratos worker, job and attempt identifiers travel as
OTLP resource attributes and are promoted to Prometheus labels. In Loki only `service.name` and the
worker identifier become index labels; job and attempt stay in structured metadata, which is
searchable without creating a stream per run.

**Retention and cardinality (MON-019).** Prometheus keeps 15 days or 4 GB, whichever comes first,
and accepts samples up to 30 minutes late so a worker that was offline can replay. Loki keeps 7
days, with ingestion-rate, stream-count, label-count and line-length limits and truncation of long
lines. Tempo keeps 72 hours with per-tenant ingestion and trace-size limits. The collector has a
memory limiter, so exhaustion is a visible refusal rather than a kill.

**Containers.** Every service runs read-only, drops all capabilities, sets `no-new-privileges`, has
a memory limit and uses bounded local logging.

## Measured on this machine

A first `docker compose up -d` on a warm image cache reaches ready in about 40 seconds. Idle
memory, with the smoke test passing:

| Service | Idle memory | Limit |
|---|---:|---:|
| MLflow | ~550 MiB | 1 GiB |
| Grafana | ~230 MiB | 512 MiB |
| Loki | ~60 MiB | 1 GiB |
| Tempo | ~50 MiB | 1 GiB |
| Prometheus | ~40 MiB | 1 GiB |
| OTel gateway | ~37 MiB | 512 MiB |

About 1 GiB in total at idle, which is the floor for sizing the eventual VM. Storage is not
included; the retention limits above bound it.

## Notes for the cloud deployment

- **MLflow's job runner must stay off.** With `MLFLOW_SERVER_ENABLE_JOB_EXECUTION` left at its
  default, an idle MLflow 3.16 server holds about 2 GiB across several hundred processes and is
  killed repeatedly under a 1 GiB limit. It runs the generative-AI scoring features, which Kratos
  does not use. The server also runs with two workers rather than four.
- **Tempo 3 moved block retention** out of the configuration file into
  `-backend-scheduler.provider.work.compaction.block-retention`, set in `compose.yaml`.
- **The collector renamed its exporters**: `otlp_http` and `otlp_grpc`; the old `otlphttp` and
  `otlp` aliases warn.
- SQLite and local artefact and block storage are local conveniences. ADR-009 puts MLflow metadata
  in PostgreSQL, and MLflow, Loki and Tempo object data in Cloud Storage.
- Nothing here authenticates a worker sending OTLP. ADR-009 requires a scoped, revocable
  credential; that decision is still open and is noted in
  [ADR-015](../docs/architecture/decisions/015-job-telemetry-without-job-network.md).

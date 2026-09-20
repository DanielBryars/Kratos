# Local observability stack

The [ADR-009](../docs/architecture/decisions/009-observability-and-mlflow.md) services — Grafana,
Prometheus, Loki, Tempo, an OpenTelemetry Collector gateway and MLflow — as a Compose bundle that
runs on one machine. It exists so the configuration, retention and cardinality limits can be proven
before anything is deployed to GCP. It costs nothing to run and creates no cloud resource.

> **Local rehearsal only.** This bundle is not a deployable configuration and is not the source of
> a cloud configuration as it stands. Its OTLP and MLflow endpoints are plaintext and
> unauthenticated, which is safe only because every published port binds to `127.0.0.1` on a single
> trusted machine. A cloud deployment SHALL put an authenticated edge in front of these services,
> and nothing here resolves the scoped, revocable worker telemetry credential ADR-009 requires;
> that decision is still open and blocks
> [ADR-015](../docs/architecture/decisions/015-job-telemetry-without-job-network.md). Treat the
> service configuration as a starting point to be re-reviewed against an authenticated edge, not as
> settled. The cloud overlay `compose.cloud.yaml`, applied by
> [`infrastructure/observability`](../infrastructure/README.md#observability-cost-gate), is what
> adds the authenticated edge, durable storage and secret delivery.

## Validate it without running it

```shell
sh observability/validate.sh
```

This checks the Compose file and every service configuration with that component's own validator,
at the pinned digests, and fails if any image is not pinned.

CI runs that, and then **starts the stack and runs the smoke service**, because static validation
cannot show that a signal arrives, that no per-run identifier reaches a metric series, or that an
exemplar links a point to its trace.

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
OTLP resource attributes. The **gateway deletes the job, attempt and project identifiers from
metrics** before they can reach Prometheus. Declining to promote them is not enough on its own: an
unpromoted resource attribute still lands on `target_info`, which is one series per attempt and
exactly the growth this prevents. Job and attempt identity stays in Loki structured metadata and on
traces and in MLflow, and a metric point reaches its trace through an **exemplar**. The smoke test
fails if any of those identifiers appears on a Prometheus series, `target_info` included, and
asserts the exemplar resolves to the span that produced the point.

**Retention and cardinality (MON-019).** Prometheus keeps 15 days or 4 GB, whichever comes first,
and accepts samples up to 30 minutes late so a worker that was offline can replay. Loki keeps 7
days, with ingestion-rate, stream-count, per-stream-rate, label-count and line-length limits and
truncation of long lines. Tempo keeps 72 hours with per-tenant ingestion and trace-size limits. The
collector has a memory limiter, so exhaustion is a visible refusal rather than a kill.

Only Prometheus has an enforced **byte** ceiling. Loki, Tempo, MLflow and Grafana are bounded by
age and by ingestion rate, which bounds how fast they can grow but not the absolute bytes on disk,
and MLflow has no automatic cleanup at all: a run and its artefacts stay until deleted. Compose
cannot impose a quota on a local volume, so the policy is alert-and-act rather than a hard ceiling:

```shell
sh observability/budget.sh        # or: sh observability/budget.sh 512
```

It reports what each volume holds and exits non-zero when one is over budget, printing the ordered
response — shorten retention first, then delete unreferenced MLflow runs and run `mlflow gc`, and
only then raise the budget and record why. Run it from a timer on any host that keeps this stack
for longer than a demonstration. Deriving real byte ceilings belongs to the cloud deployment.

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
included, and as noted above only Prometheus has an enforced byte ceiling.

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
- The cloud overlay republishes each service on the port its load-balancer backend expects,
  moves Prometheus, Loki, Tempo and Grafana state onto the persistent disk, sends Loki chunks,
  Tempo blocks and MLflow artefacts to Cloud Storage, and points MLflow at Cloud SQL through
  the Auth Proxy. `validate.sh` checks it alongside the local bundle.
- Nothing here authenticates a worker sending OTLP. ADR-009 requires a scoped, revocable
  credential; that decision is still open and is noted in
  [ADR-015](../docs/architecture/decisions/015-job-telemetry-without-job-network.md).

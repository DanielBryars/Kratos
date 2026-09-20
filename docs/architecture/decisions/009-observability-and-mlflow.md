# ADR-009 — Self-hosted Grafana stack, OpenTelemetry and MLflow

**Status:** Accepted  
**Date:** 2026-09-19

## Context

Kratos needs correlated control-plane, worker and workload telemetry, plus experiment tracking for
training parameters, scalar metrics, artefacts and models. Workers can lose internet connectivity,
and telemetry delivery must not become an unbounded dependency for job execution.

Grafana is a visualisation service rather than a telemetry store. MLflow is an experiment tracking
service rather than a general OpenTelemetry metrics backend.

## Decision

Kratos SHALL use OpenTelemetry, abbreviated OTel, for platform metrics, logs and traces. Each worker
SHALL run a local OpenTelemetry Collector. Applications and training containers SHALL export to the
local collector rather than directly to cloud backends. The collector SHALL add trusted worker, job,
attempt and project attributes, batch requests, retry transient failures and maintain a bounded
persistent queue.

The initial cloud observability deployment SHALL run on a Terraform-managed Compute Engine VM using
versioned Linux containers:

- Grafana OSS for dashboards, exploration and alerting;
- Prometheus for operational metrics;
- Loki for logs;
- Tempo for distributed traces;
- an OpenTelemetry Collector gateway; and
- MLflow for experiment tracking, trace exploration and model artefacts.

Only authenticated HTTPS entry points SHALL be externally reachable. Prometheus, Loki, Tempo,
databases and administrative ports SHALL remain private. Human access SHALL use the selected managed
identity provider. Worker telemetry SHALL use a scoped, revocable credential and encrypted
transport.

Prometheus SHALL receive control-plane, agent, host, container and GPU operational metrics. Loki
SHALL retain structured logs. Tempo SHALL retain platform and workload traces. MLflow SHALL receive
training parameters, scalar training metrics and artefacts through its tracking API. Supported
workload traces MAY also be exported to MLflow's OTLP/HTTP trace endpoint.

The platform SHALL NOT describe MLflow as an OTLP metrics backend. When a training observation such
as loss or throughput is useful in both systems, the workload integration SHALL record it through
MLflow Tracking and emit a corresponding OTel metric. As defined by
[ADR-015](015-job-telemetry-without-job-network.md), `observation_stream_id` SHALL be allocated with
the attempt and SHALL be the stable cross-system correlation key from the first observation. Both
records SHALL carry the same Kratos project, job, attempt, worker and observation-stream identifiers
where applicable. The asynchronously created MLflow run identifier SHALL be recorded against the
observation stream; telemetry SHALL NOT depend on that later identifier being available.

Large MLflow artefacts and supported Loki and Tempo object data SHALL use Cloud Storage. Durable
metadata SHALL use the selected PostgreSQL service. Prometheus data SHALL use a persistent disk with
documented retention and backup limits.

## Initial endpoints

- `grafana.kratos.bryars.com` — human dashboards and alerting;
- `mlflow.kratos.bryars.com` — authorised experiment tracking and model views; and
- `otel.kratos.bryars.com` — authenticated OTLP ingestion.

## Failure behaviour

Loss of the cloud telemetry endpoint SHALL NOT immediately stop an otherwise authorised job. Local
queues SHALL be bounded. On exhaustion, the worker SHALL preserve execution-control and accounting
events ahead of diagnostic telemetry, report the loss and follow the active lease and budget policy.

## Alternatives

| Option | Assessment |
|---|---|
| Grafana Cloud | Reduces operations, but does not meet the selected self-hosted cloud-instance goal. |
| GKE observability stack | Supports later horizontal scale, but adds cluster cost and operations before current volume requires them. |
| Cloud Run for every backend | Fits stateless services; continuous stateful telemetry ingestion and local storage are a poor initial fit. |
| Mimir instead of Prometheus | Valuable for horizontally scaled multi-tenant metrics; unnecessary for the initial workload. |
| Direct exporters in every service | Couples applications to storage backends and duplicates buffering, authentication and routing logic. |

## Consequences

- One small VM provides a comprehensible initial operational stack but is a documented single point
  of failure until a later availability target justifies replication.
- Terraform, CI/CD, persistent-disk snapshots, object retention and restoration evidence must cover
  the observability services.
- Telemetry volume and cardinality require explicit limits, especially for per-batch training data.
- The observation-stream identifier allows Grafana and MLflow views to be correlated from the first
  observation without pretending they store the same data model or making MLflow a scheduling
  dependency.

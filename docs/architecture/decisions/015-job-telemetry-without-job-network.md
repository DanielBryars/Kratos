# ADR-015 — Job telemetry and MLflow records without job networking

**Status:** Proposed
**Date:** 2026-09-20

## Context

[ADR-009](009-observability-and-mlflow.md) requires supported workloads to emit OpenTelemetry
signals through a worker-local collector and to record training parameters, scalar metrics and
artefacts in MLflow with Kratos job and attempt identifiers. MON-001 and MON-003 require progress,
metrics and logs to be visible while a run is executing, and MON-018 requires stable project, job,
attempt, worker and MLflow run identifiers on telemetry.

The executor built under [ADR-008](008-worker-job-execution.md) starts every job container with
networking disabled, a read-only root filesystem, no container-management socket and no credential.
[ADR-014](014-gcs-job-artefact-transfer.md) depends on that boundary. A job container therefore
cannot reach a collector or an MLflow server, and ADR-009 does not say how its data leaves.

Three further facts constrain the answer:

- The agent reports only the first 64 KiB of stdout, after the container exits. Nothing is visible
  during a run, and a long run can push its final result beyond that bound.
- The approved workloads already print single-line JSON to stdout. The GPU health check and the
  training example print a final result, and the fixed-duration workload now being added also
  prints periodic progress records.
- Identity asserted by a workload is not trustworthy evidence. ADR-009 requires the *collector* to
  add trusted worker, job, attempt and project attributes.

## Decision

### The job container keeps its sandbox

A job container SHALL continue to run without networking, without a socket to a collector or the
container runtime, and without any telemetry, MLflow or cloud credential. A supported workload
SHALL NOT need an OpenTelemetry or MLflow client library.

### Standard output is the workload's only telemetry channel

A supported workload SHALL write **Kratos records** to stdout: one JSON object per line, UTF-8, at
most 8 KiB, carrying `schema_version` and a `record` discriminator. Version 1 defines:

| `record` | Meaning | Required fields |
|---|---|---|
| `param` | A configuration value, written once | `name`, `value` |
| `metric` | A scalar observation | `name`, `value`, `step`; optional `unit` |
| `progress` | Position in the run | `step`; optional `total_steps`, `unit` |
| `result` | The final structured result | The workload's existing result object |

Any other stdout line, and every stderr line, SHALL be treated as an opaque log line. An image that
knows nothing about Kratos records therefore still has its logs collected.

A record SHALL NOT carry labels or dimensions in version 1. Metric and parameter names SHALL match
`^[a-z][a-z0-9_.]{0,63}$`. A workload MAY repeat the job and attempt identifiers it was given, as
the training example does, but the agent SHALL ignore them as identity.

### The trusted agent reads, bounds, stamps and forwards

While it supervises a container, the agent SHALL follow the container's stdout and stderr through
the Docker Engine API it already holds. It SHALL start each job container with bounded local log
retention so a workload cannot fill the host disk through its log driver.

The agent SHALL apply documented limits before forwarding: the line length above, a maximum record
and log-line rate, a maximum number of distinct metric names per attempt and a total byte budget.
On exhaustion it SHALL drop diagnostic data, count what it dropped by reason, report those counts,
and continue to supervise the job. Malformed or over-long records SHALL be counted and forwarded
as log lines at most, never parsed further. These limits satisfy MON-019 for the job boundary.

The agent SHALL attach the worker, job, attempt, project and MLflow run identifiers from its
**assignment**, never from the stream. It SHALL preserve each line's original Docker timestamp.
After a restart it SHALL resume from the last forwarded timestamp, and downstream writes SHALL be
idempotent on attempt, record name, step and timestamp so a replayed line is harmless.

The agent SHALL report the **last** `result` record as the job's structured result, within the
existing 64 KiB bound, instead of the first 64 KiB of stdout.

### Two sinks, both outside the job

Operational signals: the agent SHALL convert log lines to OpenTelemetry logs, and `metric` and
`progress` records to OpenTelemetry metrics, and export them by OTLP to the worker-local collector
required by ADR-009. Until that collector is delivered, the agent MAY export directly to the
gateway; the rest of this decision is unchanged when the collector arrives.

Experiment records: the **control plane** SHALL own MLflow runs. When it creates an attempt it
SHALL create the MLflow run, tag it with the Kratos job, attempt, image and dataset identities,
record the run identifier on the attempt and return it in the assignment. The agent SHALL send
`param`, `metric` and `progress` records in bounded batches to an authenticated attempt endpoint,
and the control plane SHALL write them to MLflow. A worker SHALL NOT hold an MLflow credential,
and MLflow SHALL NOT be reachable with a worker credential.

As ADR-009 requires, an observation useful in both systems is written to both, carrying the same
identifiers, by the agent's fan-out rather than by the workload.

### Failure behaviour

Telemetry SHALL NOT start, stop, fail or extend a job. Loss of the collector, gateway, control plane
or MLflow SHALL NOT interrupt supervision or lease enforcement. The agent SHALL keep undelivered
experiment records in a bounded spool in its protected state directory and replay them when the
link returns; on exhaustion it SHALL drop the oldest diagnostic records first, never the job
result, and SHALL make the loss visible in the run view. Durable replay across long outages remains
R0.4 scope.

## Alternatives

| Option | Assessment |
|---|---|
| OTLP over a Unix socket mounted into the container | Keeps networking off and uses standard SDKs, and is the natural route if workloads must emit spans. But one shared collector cannot tell which job is writing, so trusted identity needs a receiver per attempt or a proxy; MLflow's client cannot use it; and every workload gains SDK dependencies. |
| An internal container network that reaches only the collector | Breaks the no-network boundary ADR-014 relies on, lets a workload probe the collector and other jobs, and still needs a second path for MLflow. |
| A record file on a writable mount, tailed by the agent | Equivalent trust model, but needs another mount, does nothing for unmodified images, and duplicates what stdout already provides. It remains suitable for high-volume data later. |
| Let the workload call MLflow or the gateway directly | Puts a credential and a network route inside the job. Rejected. |
| Give each worker an MLflow credential | MLflow's access control is coarse; one worker could write to any project's runs. The control plane already authorises attempts. |
| A bridge on the observability host that turns OpenTelemetry metrics into MLflow writes | Removes MLflow traffic from the control plane and inherits the collector's queue, but adds a custom service and creates runs lazily, so the run identifier is unknown to the agent and the control plane. Worth revisiting if relay volume becomes material. |
| Keep reporting only the final output | Cannot satisfy MON-001 or MON-003 for a running job. |

## Consequences

- Workloads stay simple and offline: printing a JSON line is the whole integration, and the
  sandbox, credential boundary and ADR-014 are unchanged.
- The agent gains a parser for untrusted input. It must be bounded, must never raise into
  supervision, and needs the same adversarial testing as the output manifest builder.
- Stdout becomes a contract. Human-readable output belongs on stderr, and the result-extraction
  change must ship with the first record-aware agent.
- Spans from inside a workload are not supported. Traces cover the control plane and agent only.
- The control plane gains an MLflow client, a run identifier on the attempt, an observation
  endpoint and a route to MLflow. Scalar metrics are small, unlike the artefact bytes that ADR-014
  deliberately keeps away from Cloud Run.
- Metric cardinality is bounded by construction, because the workload supplies names but no labels.

## Deliberately deferred

How the gateway authenticates a worker, which ADR-009 requires to be scoped and revocable, needs its
own decision; a short-lived token issued by the control plane and verified by the collector is the
expected direction. This ADR does not define MLflow artefact or model registration, which SHALL
reference verified ADR-014 artefacts; labelled metrics; workload spans; or the worker-local
collector's configuration.

## Conditions for reconsideration

Reconsider if supported workloads need spans or labelled metrics, if observation rates make the
control-plane relay or the Docker log stream a bottleneck, if third-party images must report
metrics without adopting Kratos records, or if hostile workloads come into scope.

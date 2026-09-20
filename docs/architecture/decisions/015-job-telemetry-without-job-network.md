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
most 8 KiB including its newline, carrying `schema_version` and a `record` discriminator. Version 1
defines:

| `record` | Meaning | Required fields |
|---|---|---|
| `param` | A configuration value, written once | `name`, `value` |
| `metric` | A scalar observation | `name`, `value`, `step`; optional `unit` |
| `progress` | Position in the run | `step`; optional `total_steps`, `unit` |
| `result` | The final structured result | The workload's existing result object |

Value types are exact. A `metric` `value` SHALL be a finite JSON number: `NaN` and both infinities
are rejected, not coerced. A `step` SHALL be an integer in `[0, 2^53)` and SHALL NOT decrease within
a name. A `param` `value` SHALL be a string, finite number or boolean, at most 512 bytes once
encoded; a structured parameter SHALL be flattened by the workload. A `unit` SHALL be at most 32
bytes. Names SHALL match `^[a-z][a-z0-9_.]{0,63}$`. A record that breaks any of these is not a
record.

Any other stdout line, and every stderr line, SHALL be treated as an opaque log line. An image that
knows nothing about Kratos records therefore still has its logs collected.

A record SHALL NOT carry labels or dimensions in version 1. A workload MAY repeat the job and
attempt identifiers it was given, as the training example does, but the agent SHALL ignore them as
identity.

### The trusted agent reads, bounds, stamps and forwards

While it supervises a container, the agent SHALL follow the container's stdout and stderr through
the Docker Engine API it already holds. Reading, parsing and export SHALL run off the supervision
path: the agent SHALL start each job container with a bounded, non-blocking local log driver so a
workload cannot stall on a full pipe or fill the host disk, and SHALL hand lines to a separate
bounded reader and export queue. Deadline enforcement, the lease and the result report SHALL NOT
wait on any telemetry work, and a telemetry failure SHALL NOT raise into supervision.

The agent SHALL apply documented limits before forwarding: the line length above, a maximum record
and log-line rate, a maximum number of distinct metric and parameter names per attempt, and a total
byte budget per attempt. On exhaustion it SHALL drop diagnostic data rather than block. It SHALL
count what it dropped by reason — rate, budget, name limit, malformed, oversize — and SHALL report
those counters with the attempt so a gap is visible as a number rather than as silence. Those
counters are execution evidence: they SHALL survive queue exhaustion and accompany the job result
even when every observation was dropped. Malformed or over-long records SHALL be counted and
forwarded as log lines at most, never parsed further.

The agent SHALL attach the worker, job, attempt and project identifiers, and the observation-stream
identifier below, from its **assignment**, never from the stream. It SHALL preserve each line's
original Docker timestamp as an attribute.

Delivery is ordered by an agent-assigned sequence, not by timestamp, because container timestamps
can repeat and are not a cursor. The agent SHALL number every forwarded record within an attempt
with a monotonic sequence starting at one, SHALL group records into batches with a durable batch
identifier, and SHALL persist the batch and its sequence range in its protected state before
sending. A sink SHALL acknowledge a batch identifier, and the agent SHALL advance a durable
acknowledged high-water mark only on acknowledgement, updating that cursor atomically with the
spool. After a restart it SHALL resume from the high-water mark. Replaying a batch SHALL be
harmless: a sink SHALL be idempotent on attempt and sequence. Where retention or a bounded spool
has discarded records below the mark, the agent SHALL report the missing sequence range explicitly
as a gap rather than leave the absence to be inferred.

The agent SHALL report the **last** `result` record as the job's structured result, within the
existing 64 KiB bound, instead of the first 64 KiB of stdout.

### Two sinks, both outside the job

Operational signals: the agent SHALL convert log lines to OpenTelemetry logs, and `metric` and
`progress` records to OpenTelemetry metrics, and export them by OTLP to the worker-local collector
required by ADR-009. That collector, with the scoped revocable credential and bounded persistent
queue ADR-009 requires, is a prerequisite: this decision SHALL NOT be implemented through a direct
agent-to-gateway path, because that path has neither equivalent authentication nor equivalent
durable queueing.

Metric identity is deliberately narrow. Only `service.name` and the worker identifier SHALL become
metric attributes. The project, job, attempt and MLflow run identifiers SHALL NOT be attached to
metric series, because one series per attempt is unbounded growth across a fleet however few labels
the workload supplies. They remain on logs, on traces and in MLflow, and metrics SHALL carry
exemplars referencing the attempt so a chart can still reach the run behind a point. A deployment
SHALL additionally enforce an active-series ceiling and a metric-name allowlist at the collector,
so a new workload cannot expand cardinality without a configuration change.

Experiment records: the **control plane** SHALL own MLflow runs, and MLflow SHALL NOT be a
scheduling dependency. Creating an attempt SHALL NOT call MLflow. The control plane SHALL instead
allocate a Kratos-owned `observation_stream_id` with the attempt, record it on the attempt, return
it in the assignment, and durably enqueue an outbox entry describing the run to be created. A
separate worker SHALL create and tag the MLflow run from that outbox and record the resulting
MLflow run identifier against the stream. The agent SHALL send `param`, `metric` and `progress`
records in bounded batches to an authenticated attempt endpoint, addressed by the stream identifier;
the control plane SHALL durably accept and acknowledge them whether or not the MLflow run exists
yet, and SHALL apply them in sequence order once it does. MLflow being unavailable SHALL therefore
delay visibility, never job admission, execution or completion. A worker SHALL NOT hold an MLflow
credential, and MLflow SHALL NOT be reachable with a worker credential.

As ADR-009 requires, an observation useful in both systems is written to both, carrying the same
identifiers, by the agent's fan-out rather than by the workload.

### Failure behaviour

Telemetry SHALL NOT start, stop, fail or extend a job. Loss of the collector, gateway, control plane
or MLflow SHALL NOT interrupt supervision or lease enforcement. The agent SHALL keep undelivered
records in a bounded spool in its protected state directory and replay them from the acknowledged
high-water mark when the link returns. On exhaustion it SHALL drop the oldest diagnostic records
first, never the job result and never the drop counters or the resulting gap report, and the loss
SHALL be visible in the run view. Durable replay across long outages remains R0.4 scope.

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
| Create the MLflow run synchronously when the attempt is created | Simplest mapping, but makes MLflow availability a scheduling dependency, which contradicts the failure guarantee. Rejected in favour of a Kratos-owned stream identifier and an outbox. |
| Identify metric series by job and attempt | Gives a chart the run for free, but creates one series per attempt across the fleet, which no workload-label rule bounds. Exemplars carry the association instead. |
| Order delivery by container timestamp | Needs no extra state, but timestamps repeat, can go backwards and cannot express a gap. An agent-assigned sequence with acknowledged batches can. |

## Consequences

- Workloads stay simple and offline: printing a JSON line is the whole integration, and the
  sandbox, credential boundary and ADR-014 are unchanged.
- The agent gains a parser for untrusted input. It must be bounded, must never raise into
  supervision, and needs the same adversarial testing as the output manifest builder.
- The agent gains durable per-attempt telemetry state: a spool, batch identifiers and an
  acknowledged high-water mark, all of which must survive restart alongside the execution
  authority it already persists.
- The worker-local collector becomes a prerequisite for this decision rather than a later
  refinement, which brings ADR-009's scoped worker telemetry credential onto the critical path.
- Stdout becomes a contract. Human-readable output belongs on stderr, and the result-extraction
  change must ship with the first record-aware agent.
- Spans from inside a workload are not supported. Traces cover the control plane and agent only.
- The control plane gains an MLflow client, an observation-stream identifier and outbox, an
  observation endpoint and a route to MLflow. Scalar metrics are small, unlike the artefact bytes
  that ADR-014 deliberately keeps away from Cloud Run.
- Metric cardinality is bounded by the narrow attribute set and the collector's ceilings, not by
  the absence of workload labels alone.

## Deliberately deferred

How the gateway authenticates a worker, which ADR-009 requires to be scoped and revocable, needs its
own decision; a short-lived token issued by the control plane and verified by the collector is the
expected direction. Because the worker-local collector is now a prerequisite, that decision blocks
implementation of this one. This ADR does not define MLflow artefact or model registration, which SHALL
reference verified ADR-014 artefacts; labelled metrics; workload spans; or the worker-local
collector's configuration.

## Conditions for reconsideration

Reconsider if supported workloads need spans or labelled metrics, if observation rates make the
control-plane relay or the Docker log stream a bottleneck, if third-party images must report
metrics without adopting Kratos records, or if hostile workloads come into scope.

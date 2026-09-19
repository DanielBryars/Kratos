# 06 — Monitoring and Recovery

[Overview](README.md) · Version 0.5

## Monitoring and diagnostics

| ID | Requirement |
|---|---|
| MON-001 | Users SHALL be able to view run progress, elapsed time, training metrics and allocated resources. |
| MON-002 | Throughput SHALL include its unit of measurement. |
| MON-003 | Authorised users SHALL be able to inspect logs during and after execution. |
| MON-004 | Failed runs SHALL identify the failed stage and available diagnostic information. |
| MON-005 | The platform SHALL distinguish workload, worker connectivity and platform service failures where detectable. |
| MON-006 | Run history and recorded metrics SHALL remain available after platform restarts. |
| MON-007 | The interface SHALL distinguish historical, live and stale data. |
| MON-008 | The platform SHOULD support filtering by status, owner, project, dataset, configuration and date. |
| MON-009 | Run views SHALL include predicted completion, accrued cost, authorised budget and any scheduling blockers. |
| MON-010 | Simulated measurements SHALL be labelled and SHALL NOT be presented as actual training evidence. |

## Checkpointing and recovery

| ID | Requirement |
|---|---|
| REC-001 | Supported workloads SHALL save checkpoints at configurable intervals. |
| REC-002 | Each checkpoint SHALL identify its originating run and progress position. |
| REC-003 | Checkpoints SHALL include model, optimiser, scheduler, progress, random-number-generator and data traversal state where needed to provide the documented recovery behaviour. |
| REC-004 | Users SHALL be able to resume interrupted training from compatible checkpoints. |
| REC-005 | The platform SHALL validate checkpoint integrity and known compatibility constraints before resuming. |
| REC-006 | Recovery attempts SHALL preserve their relationship to the original run. |
| REC-007 | The interface SHALL show restored progress and known progress loss. |
| REC-008 | Incomplete checkpoint writes SHALL NOT replace the latest valid checkpoint. |
| REC-009 | The platform SHOULD support configurable automatic recovery for eligible failures in a subsequent release. |
| REC-010 | Recovery SHALL revalidate access, resource availability, secrets and remaining credit before execution. |
| REC-011 | Platform restarts SHALL reconcile active workers and SHALL NOT duplicate a workload whose original execution is still active. |
| REC-012 | The platform SHALL document whether a workload resumes at an epoch boundary, batch boundary or another progress boundary. |
| REC-013 | Historical charges SHALL remain intact when a job resumes; new usage SHALL be attributed to the new attempt. |

## Internet disconnection and reconciliation

| ID | Requirement |
|---|---|
| MON-011 | Workers SHALL buffer logs, progress events and usage records durably within configured storage limits during temporary cloud disconnection. |
| MON-012 | Reconnection SHALL replay buffered events with stable identities and timestamps, without duplicating metrics or charges. |
| MON-013 | Buffer exhaustion SHALL follow a documented policy that preserves accounting and execution-control records and explicitly reports any telemetry loss. |
| REC-014 | Recovery on a different worker SHALL use a durably uploaded compatible checkpoint and revalidate project trust and data access. |
| REC-015 | The UI SHALL distinguish unreachable workers from confirmed stopped jobs and SHALL NOT claim immediate remote cancellation without evidence. |

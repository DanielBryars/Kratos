# 04 — Jobs, Scheduling and Estimates

[Overview](README.md) · Version 0.5

## Submission and control

| ID | Requirement |
|---|---|
| TRN-001 | Users SHALL be able to select a dataset version, supported training configuration and compute requirements through the web interface. |
| TRN-002 | Users SHALL be able to review the resolved configuration, expected timing and credit quote before submission. |
| TRN-003 | The platform SHALL validate submissions and explain blocking errors before execution. |
| TRN-004 | Every accepted run SHALL receive a unique identifier. |
| TRN-005 | The platform SHALL display current run state and record state transitions. |
| TRN-006 | The platform SHALL distinguish queued, starting, training, evaluating, completed, failed and cancelled states. |
| TRN-007 | Users SHALL be able to cancel their queued and active jobs; authorised operators SHALL be able to cancel jobs within their operational scope. |
| TRN-008 | Cancellation SHALL report separately when execution has stopped and when resources have been released. |
| TRN-009 | The submitted workload configuration SHALL be immutable; changes SHALL require a new run. |
| TRN-010 | The platform SHOULD support submission through an API or command-line interface. |
| TRN-011 | Submission retries SHALL NOT create duplicate jobs when the same submission identity is reused. |

## Scheduling

| ID | Requirement |
|---|---|
| SCH-001 | The platform SHALL queue jobs and assign compatible resources automatically. |
| SCH-002 | Users SHALL be able to submit a job for immediate eligibility or a future earliest-start time. |
| SCH-003 | Users SHALL be able to specify a maximum credit spend and maximum acceptable rate before execution. |
| SCH-004 | Scheduling SHALL consider verified resource compatibility, worker trust and sharing policy, availability, project quotas, authorised priority, job age and credit eligibility. |
| SCH-005 | The scheduling policy SHALL be documented and visible to users, including reasons a job cannot start. |
| SCH-006 | Scheduling SHALL include an ageing or equivalent fairness mechanism to prevent indefinite starvation of eligible jobs. |
| SCH-007 | Project owners or administrators SHALL be able to configure per-user or per-project concurrency and resource quotas within their authority. |
| SCH-008 | The platform SHALL show estimated queue position or an explanation when resource constraints make a single queue position misleading. |
| SCH-009 | The platform SHALL respect worker maintenance and availability windows. |
| SCH-010 | Releases R0.2–R0.6 SHALL NOT automatically preempt running jobs to admit higher-priority jobs. |
| SCH-011 | Recovery after a scheduler restart SHALL reconcile worker state before issuing new allocations. |
| SCH-012 | Future earliest-start times SHALL NOT be represented as guaranteed reservations. |
| SCH-013 | The platform SHOULD suggest lower-cost eligible execution windows and their predicted completion times. |
| SCH-014 | Recurring schedules and guaranteed capacity reservations MAY be added in a subsequent release. |
| SCH-015 | Scheduling timestamps SHALL be stored with an unambiguous time basis and displayed with the user's timezone. |

## Predicted completion

| ID | Requirement |
|---|---|
| EST-001 | Before submission, the platform SHALL present estimated queue wait, execution duration and completion time, or explicitly report that an estimate is unavailable. |
| EST-002 | Completion estimates SHALL include configured image and dataset downloads, preparation, training, evaluation, export and durable artefact uploads rather than training time alone. |
| EST-003 | Predictions SHALL use relevant historical runs, workload size, training configuration and target hardware where data is available. |
| EST-004 | Predictions SHALL display an uncertainty range or confidence category, the last update time and significant assumptions. |
| EST-005 | Unknown workloads SHALL be labelled as low-confidence estimates or unavailable; the platform SHALL NOT invent precise predictions without supporting data. |
| EST-006 | Active-job predictions SHALL update from observed progress and throughput at a documented interval. |
| EST-007 | Queued-job predictions SHALL be recalculated when relevant resource availability, queue state or job duration estimates change. |
| EST-008 | Users SHALL be able to distinguish predicted start, predicted finish and actual timestamps. |
| EST-009 | The platform SHALL retain prediction-versus-actual measurements to assess estimate quality. |
| EST-010 | Predictions SHALL NOT be presented as guaranteed deadlines. |
| EST-011 | The platform SHOULD explain material estimate changes, including worker loss, slower progress and queue changes. |

## Remote assignment and estimates

Users may select a compute group or allow the scheduler to select an eligible group. Group selection does not reserve every member or require a single-worker job to consume the entire group.

| ID | Requirement |
|---|---|
| SCH-016 | Each execution attempt SHALL receive a unique assignment and a bounded lease defining its permitted resources, time and budget. |
| SCH-017 | Workers SHALL checkpoint and stop within their authorised lease and budget when renewal is unavailable. |
| SCH-018 | The scheduler SHALL NOT reassign an execution until the prior worker has acknowledged termination or its execution authority has expired; stale attempts SHALL be rejected when publishing authoritative results. |
| SCH-019 | Cancellation of a disconnected job SHALL remain visibly pending until termination is acknowledged or the execution lease expires. |
| SCH-020 | Users SHALL be able to constrain placement to an authorised compute group or request automatic placement among eligible groups. |
| SCH-021 | Group-aware scheduling SHALL consider each member's capacity and workload compatibility rather than treating all group GPUs as interchangeable. |
| SCH-022 | A worker's membership in overlapping groups or pools SHALL NOT permit duplicate resource allocation. |
| EST-012 | Estimates SHALL account for worker-specific observed throughput, cache state and input/output transfer time where measurements are available. |
| EST-013 | Uncertain internet transfer times and disconnected workers SHALL widen the displayed uncertainty or make affected estimates unavailable. |

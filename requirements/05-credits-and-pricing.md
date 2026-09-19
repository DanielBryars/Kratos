# 05 — Credits and Pricing

[Overview](README.md) · Version 0.5

## Accounting model

Credits are internal units used to allocate compute fairly and expose the cost of resource choices. The initial release does not exchange credits for money. Currency-based cost estimates, if added, are informational and separate from the credit ledger.

| ID | Requirement |
|---|---|
| CST-001 | The platform SHALL maintain project credit accounts and attribute each job's usage to its submitting user. |
| CST-002 | Authorised account managers SHALL be able to allocate credits and configure user or project spending limits. |
| CST-003 | Users SHALL be able to view their authorised balances, reservations, usage and transaction history. |
| CST-004 | Credit grants, reservations, charges, releases and adjustments SHALL be recorded in an append-only auditable ledger. |
| CST-005 | Ledger corrections SHALL use compensating transactions rather than overwriting prior transactions. |
| CST-006 | Available balance SHALL exclude credit already reserved by other jobs. |
| CST-007 | Balance checks and reservations SHALL be atomic across concurrent submissions. |
| CST-008 | The platform SHALL define billable resource units and a versioned base-rate table. |

## Historical contention pricing

The initial pricing model uses published time windows and resource classes. Rates rise for windows with persistently high historical contention. Contention represents competition for compatible capacity, not GPU utilisation alone.

| ID | Requirement |
|---|---|
| PRC-001 | Resource rates SHALL include a multiplier derived from historical contention for the relevant resource class and time window. |
| PRC-002 | The contention measure SHALL include eligible queued demand relative to available compatible capacity. |
| PRC-003 | The pricing policy SHALL document its history window, time buckets, timezone, minimum sample requirements, update cadence, multiplier bounds and smoothing rules. |
| PRC-004 | Under the same policy and data sufficiency, higher measured contention SHALL NOT produce a lower multiplier. |
| PRC-005 | The policy SHALL assign a multiplier above the baseline when its documented high-contention threshold is met. |
| PRC-006 | Sparse-history windows SHALL use a published fallback rate and SHALL be identified as such. |
| PRC-007 | Users SHALL be able to inspect current and upcoming published rates and the reason for peak pricing. |
| PRC-008 | Pricing updates SHALL be versioned and published before taking effect with a documented notice period. |
| PRC-009 | A quote SHALL record resource quantities, base rates, multipliers, estimated billable duration, uncertainty, policy version and expiry. |
| PRC-010 | The platform SHALL revalidate an expired or changed quote before starting a queued job. |
| PRC-011 | A queued job SHALL remain blocked if the current quote exceeds the user's authorised maximum rate or spend. |
| PRC-012 | The accepted rate for each allocated resource SHALL be locked at execution start for that execution attempt. Later pricing updates SHALL NOT change those rates. |
| PRC-013 | A resumed execution attempt SHALL receive a new quote and remain within the remaining authorised job budget. |
| PRC-014 | Pricing SHALL use aggregated demand data without exposing other users' private workloads. |
| PRC-015 | The contention calculation SHALL exclude unauthorised, duplicate and resource-incompatible requests and SHALL apply documented limits against demand inflation by a single user or project. |

## Metering and budget enforcement

For a resource billed by allocated time, the charge is the sum of its allocated quantity multiplied by its measured billable duration and locked rate. The implementation defines explicit units and rounding rules for each resource class.

| ID | Requirement |
|---|---|
| BIL-001 | The platform SHALL show estimated credits before submission and actual accrued credits while a job is active. |
| BIL-002 | Queue waiting SHALL NOT consume execution credits. |
| BIL-003 | Charging SHALL begin only after the assigned runtime passes readiness checks and the workload begins execution. |
| BIL-004 | The pricing policy SHALL identify billable preparation, training, evaluation, export and checkpoint time, including when a resource remains allocated but idle. |
| BIL-005 | The platform SHALL meter actual allocated resources and billable duration using durable, deduplicated usage records. |
| BIL-006 | The platform SHALL reserve the authorised maximum spend before execution and release unused credit after settlement. |
| BIL-007 | The platform SHALL stop or checkpoint a job before its authorised spend is exhausted, allowing a documented shutdown reserve. |
| BIL-008 | Charges SHALL NOT exceed the authorised maximum spend; any unavoidable enforcement overrun SHALL be absorbed by the platform account and recorded. |
| BIL-009 | Cancellation SHALL stop charging once billable resources are released, with the cancellation-to-release interval visible to the user. |
| BIL-010 | Failed and cancelled jobs SHALL be charged for valid usage according to a published failure and refund policy. |
| BIL-011 | Uncertain usage caused by worker disconnection SHALL be marked provisional until reconciled and SHALL NOT be silently treated as confirmed usage. |
| BIL-012 | Usage replay, worker retries and service restarts SHALL NOT produce duplicate charges. |
| BIL-013 | Final settlement SHALL show quantities, durations, rates, policy version, adjustments and total credits. |
| BIL-014 | Users SHOULD receive warnings as their job approaches configurable budget thresholds. |
| BIL-015 | The platform MAY display estimated energy or currency costs separately, with assumptions and conversion rates stated explicitly. |

## Remote worker accounting

| ID | Requirement |
|---|---|
| BIL-016 | Offline execution SHALL be limited to credit reserved before disconnection; worker reconnect SHALL reconcile usage against the same assignment without duplicate charges. |
| BIL-017 | Usage reports SHALL be authenticated, sequenced and checked against assignment resource limits and elapsed-time bounds before settlement. |
| BIL-018 | The accounting policy SHALL document the trust placed in worker-reported usage and how anomalous or disputed reports are reviewed. |
| BIL-019 | Cloud storage, network transfer and control-plane costs SHALL be identified separately from worker execution charges; any credits charged for these costs SHALL be quoted and budgeted explicitly. |
| PRC-016 | Contention pricing SHALL use the compatible worker pool and resource class relevant to the job, rather than treating all registered GPUs as interchangeable capacity. |

Worker participation does not imply a marketplace or payment entitlement. Provider payouts remain outside the initial release.

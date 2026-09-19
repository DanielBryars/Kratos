# 09 — Releases and Acceptance

[Overview](README.md) · Version 0.5

## Scope and interpretation

This chapter is the authoritative release allocation. Chapters 01–08 describe the cumulative target platform, not a requirement to implement the whole platform in R0.1. Earlier references to an initial release mean the release that first introduces the relevant capability according to this chapter.

Each release SHALL retain the behaviour accepted in earlier releases. A mandatory requirement becomes an acceptance gate when its capability is introduced. Security, authorisation, provenance of deployed software and operational requirements apply from the first release of the component they protect.

R0.5 SHALL complete the mandatory single-worker platform requirements in Chapters 01–08 except explicitly optional, future and distributed capabilities. R0.6 SHALL add all DST requirements. Recommendations retain their SHOULD status.

## Release roadmap

| Release | Outcome | Main scope | Dependencies |
|---|---|---|---|
| R0.1 — Foundation and worker registration | Deploy the cloud application and register the two home machines. | Build scaffolding, Terraform, CI/CD, login, worker identities, capability discovery, LAN group and fleet UI. | Accepted R0.1 technology choices, GCP bootstrap and actual host validation. |
| R0.2 — First reproducible training | Launch and observe one real GPU workload end to end. | Approved workload, dataset versions, simple queue, assignment leases, logs, metrics, artefacts and provenance. | R0.1 |
| R0.3 — Shared compute and fixed-rate credits | Multiple users share resources with explicit spending limits. | Project roles, quotas, fairness, future starts, ledger, reservations, fixed rates, metering and initial ETA. | R0.2 |
| R0.4 — Recovery and demand-based pricing | Recover interrupted work and make time/cost trade-offs visible. | Checkpoint resume, offline reconciliation, historical contention rates, rate locking and improved estimates. | R0.3 |
| R0.5 — Evaluation and model delivery | Produce comparable, validated model artefacts. | Evaluation gates, comparison, registry, ONNX checks, latency benchmarks and remaining single-worker operational acceptance. | R0.4 |
| R0.6 — Distributed LAN training | Run one training job across the two-machine compute group. | DDP, verified peer networking, atomic multi-worker allocation, failure recovery and performance comparison. | R0.5 |
| R0.7 — Bounded cloud GPU expansion | Add provisioned cloud capacity with interruption recovery. | Cloud GPU registration, SkyPilot provider evaluation, provisioning ownership, Spot fault tolerance, spend limits and teardown. | R0.4 minimum; follows R0.6 in the planned sequence. |

## R0.1 — Foundation and worker registration

### R0.1a — Build scaffolding

- Establish source layout, dependency pinning, container builds, local setup and a documented development workflow.
- Create API health/version endpoints and a minimal authenticated web shell.
- Define a versioned worker enrolment, heartbeat and capability-report contract.
- Add relevant unit and integration checks, linting, secret detection and image/dependency scanning.
- Record the selected R0.1 technologies and rationale before implementing dependent components.
- Implement the control-plane scaffold in Rust/Axum according to [ADR-004](../docs/architecture/decisions/004-rust-control-plane.md), with a pinned toolchain and locked dependencies.

### R0.1b — Cloud infrastructure and CI/CD

- Provision GCP resources with Terraform, including environment separation, remote state and bootstrap instructions.
- Build immutable images and promote verified image identities through test and production.
- Configure scoped deployment identities, runtime secrets, health checks, rollback and deployment history.
- Deploy the authenticated Grafana, Prometheus, Loki, Tempo, OpenTelemetry gateway and MLflow service foundations in GCP.
- Provide durable registry storage, basic backup/restore evidence, cloud budget alerts and operational logs.
- Document ownership of image updates, migrations and Terraform-managed settings.
- Keep privileged deployment credentials unavailable to untrusted builds and GPU test jobs.

### R0.1c — Register the home machines

- Install an agent on each of the two Windows machines using a validated Linux execution environment.
- Enrol each worker using a single-use bootstrap credential and approved, revocable identity.
- Permit an agent to request registration without a copied secret; require operator code comparison
  and cryptographic proof of device-key possession before issuing its worker credential.
- Publish the secret-free Linux AMD64 agent image from CI with immutable identity, provenance and an
  SBOM so manually rented machines can join through the same approval flow.
- Advertise CPU, RAM, GPU model/count/memory, runtime versions and available capacity.
- Perform a real container GPU health check; display unsupported or unhealthy capability accurately.
- Retain the GPU computation result, image identity, host/runtime versions and observed device as acceptance evidence.
- Create one approved home compute group and list its two individual members.
- Show online, offline, unapproved and unhealthy states with last contact and stale-data indicators.
- Support heartbeat reconnect, duplicate-enrolment handling and worker revocation.
- Keep group peer-network validation visibly unverified until actually tested; full distributed validation belongs to R0.6.

### Explicit boundary

R0.1 SHALL NOT expose general training launch or imply that registration proves distributed-training readiness. It SHALL include basic authenticated administrator access and server-side restrictions, but full project collaboration arrives in R0.3. Controlled GPU health checks are permitted; they are not a general job scheduler.

A basic versioned installation/update and recovery procedure is required. Automatic staged fleet updates are completed by R0.5. Native Linux worker support is accepted by R0.2; R0.1 acceptance targets the two actual Windows hosts.

### Exit criteria

| ID | Requirement |
|---|---|
| REL-001 | A clean checkout SHALL build and verify the web, API and agent artefacts using documented commands and CI. |
| REL-002 | Terraform SHALL reproduce the selected GCP deployment from a documented bootstrap, with separate test and production state and deployment authority. |
| REL-003 | CI/CD SHALL deploy a verified release and demonstrate rollback without exposing persistent cloud credentials to builds. |
| REL-004 | An authorised user SHALL see both home machines registered in one compute group with actual capabilities and GPU health-check results. |
| REL-005 | Disconnecting either agent SHALL make its status stale or offline within the documented timeout; reconnection SHALL preserve its identity without a duplicate worker record. |
| REL-006 | Unapproved or revoked agents SHALL be denied protected operations, and user/worker credentials SHALL remain absent from logs and build artefacts. |
| REL-007 | The cloud interface and durable worker registry SHALL remain available when both home machines are off; a backup/restore exercise SHALL preserve registry identity and membership. |
| REL-008 | R0.1 acceptance SHALL record actual host/runtime versions, service response and heartbeat targets, observations and known limitations. |

## Later release boundaries and exit evidence

### R0.2 — First reproducible training

Introduce DAT/REP requirements for one dataset and workload; TRN submission/cancellation; basic SCH allocation and leases; MON logs/metrics; XFR input/output transfer; native Linux support. Security extends to job-scoped data and secrets as soon as execution exists. A small operator-only job quota and duration limit constrain operation before credits are introduced.

Exit: submit one real training job, record code/image/data identity, observe progress, obtain durable outputs and cancel another job safely. Network loss SHALL NOT permit duplicate execution. Training does not wait for the credit system to be built, and the UI SHALL NOT display fabricated cost accounting.

### R0.3 — Shared compute and fixed-rate credits

Complete multi-user project workflows, concurrency quotas, queue fairness and future earliest-start constraints. Introduce CST/BIL credit accounts, fixed-rate quotes, reserved budgets and settlement. Introduce EST estimates with explicit cold-start uncertainty. Fixed-rate pricing is an intermediate milestone; PRC contention requirements become mandatory in R0.4.

Exit: two users contend for compatible resources; project boundaries hold; budget reservations cannot overspend; repeated usage events do not duplicate charges; a budget-limited job stops within its authority. A basic ETA uses measured throughput or reports insufficient evidence.

### R0.4 — Recovery and demand-based pricing

Complete user-initiated checkpoint recovery, offline buffered reconciliation, price history, contention multipliers, locked attempt rates and updated ETAs. Existing lease enforcement is extended with offline reserved-credit enforcement.

Exit: interrupted training resumes without duplicate charges; a high-contention window produces a higher published rate; expired quotes are revalidated; the user sees comparable timing and cost for alternative eligible windows. Automatic retry remains optional.

### R0.5 — Evaluation and model delivery

Complete EVA/MOD requirements and the remaining mandatory single-worker security, delivery, observability and operating requirements. Integrate experiment tracking, registry views, comparison and validated model export. Complete staged worker updates and full backup/restore acceptance across metadata, accounting and artefacts.

Exit: compare two runs, inspect evaluation thresholds, reject an invalid candidate and download an ONNX artefact whose outputs meet recorded tolerance. All mandatory non-distributed requirements SHALL have acceptance evidence or a formally revised scope; silent deferral is not permitted.

### R0.6 — Distributed LAN training

Implement all DST requirements and workload-specific group network validation.

Exit: execute training across the two home machines using LAN collectives, recover from a lost worker and compare measured performance against single-worker execution. Group membership alone SHALL NOT bypass compatibility checks.

### R0.7 — Bounded cloud GPU expansion

This release expands the previous exclusion of automatic cloud GPU provisioning. SkyPilot SHALL be evaluated behind the provider boundary defined by ADR-010 before implementation; adoption remains subject to provisioning, recovery, security and cost evidence.

- Provision and enrol cloud GPU workers with explicit quotas and cloud spend limits.
- Assign one owner to each provisioned resource to prevent Terraform and an orchestrator fighting over its lifecycle.
- Demonstrate a bounded Spot training run and checkpoint recovery after interruption.
- Remove temporary capacity after completion or failure and detect leaked resources.
- Keep internal credit estimates distinct from provider charges.

Exit: provision, train, interrupt, recover and tear down with a verified cost record and no orphaned billable resources.

## Requirement and acceptance allocation

| Capability | First delivery | Completion and principal evidence |
|---|---|---|
| GOV, CI/CD, cloud infrastructure, deployed-component security | R0.1 | Relevant controls each release; all mandatory single-worker controls by R0.5. ACC-002/012–016/025/026 are applied to delivered components. |
| CMP/ENV/WRK/GRP registration and inventory | R0.1 | Windows registration first; native Linux by R0.2; staged updates by R0.5; distributed network checks R0.6. REL-004–008, ACC-003/021/027. |
| USR collaboration and project policies | Basic protected admin/worker access R0.1 | Full multi-user project scope R0.3. ACC-001/005. |
| DAT/REP/TRN/XFR and basic MON | R0.2 | Advanced transfer recovery by R0.4. ACC-004/006/022/024. |
| SCH/EST | Basic queue R0.2; fairness and ETA R0.3 | Full mandatory timing and placement behaviour by R0.4. ACC-005/006/022/023. |
| CST/BIL/PRC | Fixed credits R0.3 | Contention pricing and complete offline settlement R0.4. ACC-007–009/023. |
| REC and offline reconciliation | Safe leases R0.2 | Manual resume and durable event replay R0.4. ACC-010/012/023/024. |
| EVA/MOD | R0.5 | ACC-011 and cumulative operational acceptance. |
| DST | R0.6 | ACC-017/028. |
| Provisioned cloud GPUs | R0.7 | New provisioning-specific acceptance plus existing worker, recovery and accounting checks. |

## Exclusions

Monetary credit purchases, provider payouts, hostile multi-tenant execution, unrestricted uploaded code, production robot deployment and complete feature parity with external experiment trackers remain outside this roadmap. Robotics episode datasets and richer workflow orchestration require a later scoped release.


## Acceptance scenarios

| ID | Scenario and required evidence |
|---|---|
| ACC-001 | Two users operate concurrently in separate projects. Authorised sharing succeeds; unauthorised reads, submissions, live subscriptions and artefact downloads are denied. |
| ACC-002 | A workload retrieves only its granted secret. Rotation and revocation follow the documented policy. Source, images, logs and artefacts contain no test secret value. Audit entries identify access without revealing the value. |
| ACC-003 | A Linux container executes an actual GPU computation on a supported Windows host. Loss of GPU access causes an explicit failure rather than CPU fallback. |
| ACC-004 | A valid dataset is published and used by exact version. An invalid dataset is rejected with sample-level diagnostic information. |
| ACC-005 | Jobs from multiple users are queued, scheduled within quotas and given understandable blockers. An earliest-start constraint is respected; a cancelled queued job never begins execution. |
| ACC-006 | Job submission shows provenance, timing estimates and a credit quote. Live progress revises completion estimates. Cold-start estimates are clearly labelled. Predicted and actual times are retained. |
| ACC-007 | Controlled historical demand inputs produce a higher rate for a high-contention window than its baseline. The rate policy, notice period and sparse-history fallback are observable. |
| ACC-008 | Concurrent jobs cannot reserve the same available credit. Expired quotes are revalidated, rate limits are respected and a running attempt retains its locked rate after pricing changes. |
| ACC-009 | A job reaches its budget stop threshold, checkpoints or stops according to policy, and does not charge beyond its authorised spend. Cancellation and failure charges match published rules. |
| ACC-010 | A worker interruption is detected. Training resumes from a valid checkpoint with provenance and accounting linked to the new attempt. No duplicate execution or charge occurs. |
| ACC-011 | Successful training produces evaluation results and an ONNX export whose outputs satisfy recorded tolerances. Failed checks block approval. Artefacts and reports are downloadable by authorised users. |
| ACC-012 | Platform and host restarts preserve run history and ledger state. Incomplete checkpoints are not selected as valid recovery points. |
| ACC-013 | A source change passes CI, deploys to test and is promoted to the cloud environment using the verified image. A deliberately failed deployment exercises the documented recovery path. |
| ACC-014 | A worker-network outage during an agent update leaves the worker's existing release intact; cloud deployment proceeds independently. Untrusted build execution cannot obtain production deployment credentials. |
| ACC-015 | A backup is restored into an isolated environment and checked for metadata, ledger and artefact consistency. |
| ACC-016 | Recorded load and recovery exercises demonstrate the agreed service targets. |
| ACC-017 | For the distributed release, two Windows-hosted Linux GPU workers complete a training run, recover from worker loss and record topology and measured throughput. |
| ACC-021 | Windows and native Linux GPU servers on separate networks enrol using outbound connections, advertise capabilities and pass health checks. Unapproved or revoked workers receive no new assignments or data grants. |
| ACC-022 | A job requiring a particular capability is assigned only to a compatible, authorised worker. Changed or expired capability reports prevent inappropriate placement. |
| ACC-023 | A worker loses internet connectivity during training. Its lease and budget bound continued execution, cancellation remains pending as appropriate, and reconnection produces neither duplicate execution nor duplicate charges. |
| ACC-024 | An interrupted data transfer resumes and verifies integrity. A locally saved checkpoint is distinguished from an uploaded checkpoint, and recovery on another eligible worker uses the uploaded copy. |
| ACC-025 | With every worker offline, the cloud interface remains usable for history, administration and queued submissions. No unavailable capacity is reported as ready. |
| ACC-026 | Agent updates are staged and integrity checked. An incompatible agent cannot accept jobs, and an offline worker reconnects under the documented protocol compatibility policy. |
| ACC-027 | Two home workers appear in one compute group with individual GPU capabilities. Group-constrained single-worker placement respects access and capacity limits. An unrelated worker cannot join merely by advertising the group identifier. |
| ACC-028 | For the distributed release, container-to-container connectivity is verified across the home LAN and a two-worker job uses that LAN for training traffic. Loss of peer connectivity blocks new distributed placement even if both workers can still reach the cloud. |

| ID | Requirement |
|---|---|
| ACC-018 | Acceptance records SHALL identify software version, hardware, runtime, inputs, expected outcomes, observed outcomes and linked requirement identifiers. |
| ACC-019 | Outstanding mandatory requirements SHALL prevent acceptance of the affected release. |
| ACC-020 | SHOULD deviations SHALL include their rationale and resulting operational limitations. |

## Decisions to resolve during design

These are design inputs, not permission gates for preparing the implementation.

| Decision | Required outcome |
|---|---|
| Hardware inventory | Windows and Linux versions, GPU models, memory, available machines, owners and network links. |
| Execution stack | Tested Windows-to-Linux and native Linux GPU runtimes and compatibility matrix. |
| Identity and secrets | Identity provider, secret store, bootstrap mechanism and key custody. |
| Storage | Dataset, checkpoint, registry, metadata and backup placement and capacity. |
| Scheduling | Priority policy, ageing rules, maintenance windows and quota defaults. |
| Pricing | Credit allocations, resource units, base rates, contention windows, threshold, bounds, update cadence and notice period. |
| Budget enforcement | Metering interval, shutdown reserve, overrun handling and failure/refund rules. |
| Estimation | Initial estimator, cold-start fallback, uncertainty method and accuracy reporting. |
| Service targets | Concurrent users and jobs, response-time targets, telemetry freshness, recovery objectives and retention. |
| Delivery | Cloud provider and region, source and image registry, runner isolation, test environment, promotion policy and worker update path. |
| Initial workload | Supported model, dataset, evaluation thresholds and ONNX validation tolerances. |
| Worker federation | Enrolment approval, ownership, trust classes, sharing policy and supported agent versions. |
| Disconnection | Lease duration, renewal interval, offline budget, shutdown allowance and event buffering limits. |
| Remote storage | Cloud storage region, resumable transfer mechanism, worker caching policy and transfer cost treatment. |
| Distributed network | Eligible worker groups and secure data-plane connectivity, bandwidth and latency requirements. |

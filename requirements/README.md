# Training Platform — Requirements Overview

Version 0.5 · Draft

## Purpose

The platform SHALL enable multiple users to prepare versioned datasets, schedule reproducible training workloads on registered GPU workers, monitor experiments, recover interrupted training, evaluate results and export validated models through a web interface.

The platform SHALL host its web interface and control services in the cloud. Registered GPU workers SHALL connect over the internet and execute training in Linux containers on supported Windows or Linux hosts. It SHALL provide explicit secret management, resource scheduling, completion estimates and an auditable credit system with higher prices during periods of historically high contention.

## Requirement terminology

**SHALL** and **SHALL NOT** denote mandatory requirements and prohibitions. **SHOULD** and **SHOULD NOT** denote recommendations from which deviations require a documented rationale. **MAY** denotes an optional capability. These terms apply only within the release scope defined below; future capabilities do not become initial-release obligations by being described here.

Requirement identifiers are stable references for implementation and acceptance evidence. Each normative statement is independently identifiable. Examples and explanatory prose do not introduce additional requirements.

## Chapters

| Chapter | Contents |
|---|---|
| [01 — Users, access and secrets](01-users-access-and-secrets.md) | Multiple users, roles, isolation, audit and the secret lifecycle. |
| [02 — Compute and execution](02-compute-and-execution.md) | Worker enrolment, capability discovery, Windows and Linux hosts, GPU access and distributed training. |
| [03 — Datasets and provenance](03-datasets-and-provenance.md) | Validation, immutable versions and reproducibility. |
| [04 — Jobs, scheduling and estimates](04-jobs-scheduling-and-estimates.md) | Submission, queues, reservations, fairness and predicted completion. |
| [05 — Credits and pricing](05-credits-and-pricing.md) | Budgets, metering, historical contention pricing and settlement. |
| [06 — Monitoring and recovery](06-monitoring-and-recovery.md) | Metrics, logs, checkpoints and recovery. |
| [07 — Evaluation and models](07-evaluation-and-models.md) | Evaluation, comparison, model approval and export. |
| [08 — Delivery and operations](08-delivery-and-operations.md) | CI/CD, cloud and worker deployment, security, durability and service targets. |
| [09 — Releases and acceptance](09-releases-and-acceptance.md) | Release boundaries, verification scenarios and unresolved decisions. |

## Users

| Role | Purpose |
|---|---|
| Researcher | Prepare datasets, submit jobs and inspect results within authorised projects. |
| Project owner | Manage project membership, shared resources and credit allocations. |
| Platform operator | Approve workers, maintain services and investigate operational failures. |
| Worker owner | Enrol GPU servers, advertise capacity and manage their availability and sharing policy. |
| Administrator | Manage identities, access policy, pricing and platform-wide credit allocations. |

One person may hold several roles. Operator privileges do not inherently grant access to all private project data.

## System boundaries

The cloud control plane hosts the web interface, API, scheduler, worker registry, experiment metadata and credit ledger. Durable dataset and artefact storage SHALL be reachable by authorised workers independently of any one home machine. Worker agents initiate authenticated outbound connections to receive assignments and send status, logs and usage. GPU workloads run on registered home machines or other approved servers.

Worker membership does not itself grant access to projects or jobs. Scheduling considers advertised and verified capabilities, owner policy and project trust requirements. Independent jobs may run on geographically separate workers; distributed training requires a separately validated network topology.

A **compute group** identifies workers that can communicate directly over a shared network. The initial deployment has one home compute group containing the owner's two LAN-connected machines. Both workers connect independently to the cloud control plane; intra-group training traffic stays on the group's configured network. A **worker pool** is a logical access or scheduling collection and does not imply network reachability.

The web interface presents authoritative state from execution, tracking, storage and accounting services. The platform manages approved training workloads; it does not initially provide unrestricted user-supplied code execution. Credits represent internal resource accounting, not monetary payments or a financial balance.

## Release summary

R0.1 SHALL provide build scaffolding, Terraform-managed GCP deployment, CI/CD and registration of the two home GPU machines in one compute group. It SHALL provide protected access, worker identities, capability discovery and a fleet status interface.

R0.2 adds reproducible single-worker training; R0.3 adds shared scheduling and fixed-rate credits; R0.4 adds recovery and contention pricing; R0.5 completes evaluation and model export; R0.6 adds distributed LAN training; R0.7 adds bounded cloud GPU provisioning. [Chapter 09](09-releases-and-acceptance.md) is authoritative for release allocation and acceptance gates. Chapters 01–08 define the cumulative target, not the scope of R0.1 alone.

## Technology decisions

GCP, Terraform, Rust/Axum for the control plane, and the VS Code Linux dev-container workflow are accepted project choices. Remaining options, including Cloud Run, PostgreSQL, Secret Manager, GitHub Actions and MLflow, are documented as proposals in the [technology decision register](../docs/architecture/README.md). Proposed choices SHALL remain distinguishable from accepted decisions.

## Document governance

| ID | Requirement |
|---|---|
| GOV-001 | Changes to requirements SHALL be version controlled and reviewed with their effect on scope and acceptance criteria. |
| GOV-002 | Implementation and verification records SHALL reference the requirement identifiers they satisfy. |
| GOV-003 | Unresolved policy values SHALL be recorded explicitly and resolved before the affected feature is accepted. |

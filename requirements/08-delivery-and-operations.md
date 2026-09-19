# 08 — Delivery and Operations

[Overview](README.md) · Version 0.5

## CI/CD

Release allocation is defined in [Chapter 09](09-releases-and-acceptance.md). GCP and Terraform are accepted decisions; supporting service choices remain in the [technology decision register](../docs/architecture/README.md).

| ID | Requirement |
|---|---|
| CIC-001 | Application code, infrastructure configuration, container definitions, deployment manifests and database migrations SHALL be version controlled. |
| CIC-002 | Proposed changes SHALL execute automated formatting or lint checks, relevant tests, secret detection and dependency or image vulnerability checks. |
| CIC-003 | Failed mandatory checks SHALL prevent release promotion. |
| CIC-004 | Release builds SHALL produce immutable Linux container images linked to their source revision and build record. |
| CIC-005 | The same verified image identities SHALL be promoted between test and production without rebuilding them. |
| CIC-006 | The pipeline SHALL deploy to a test environment and execute service integration checks before production promotion. |
| CIC-007 | GPU-affecting releases SHALL pass a GPU smoke test on a representative Windows-hosted Linux worker before promotion. |
| CIC-008 | Control-plane deployment SHALL target the selected cloud environment through authenticated CI/CD identities; worker updates SHALL use authenticated outbound retrieval without requiring public host administration ports. |
| CIC-009 | Untrusted contributions SHALL NOT execute on privileged deployment agents or receive production credentials. |
| CIC-010 | Build execution SHALL be isolated from production deployment authority. |
| CIC-011 | Deployment credentials SHALL be scoped and managed according to Chapter 01. |
| CIC-012 | The pipeline SHALL perform post-deployment health checks and record the deployed image identities and outcome. |
| CIC-013 | Failed deployments SHALL support restoration of the previous compatible application release. |
| CIC-014 | Database migrations SHALL have documented compatibility, backup and recovery procedures; image rollback alone SHALL NOT be treated as database recovery. |
| CIC-015 | Worker updates SHALL drain affected workers or explicitly defer deployment while jobs remain active. |
| CIC-016 | Production promotion SHALL be an explicit, audited release action, whether initiated by an authorised person or an approved automated policy. |
| CIC-017 | Cloud deployment, worker-network or registry outages SHALL leave the running release intact and report a pending or failed deployment without reporting false success. |
| CIC-018 | Deployment workflows SHALL be repeatable without manual edits to running containers. |
| CIC-019 | Application and container updates SHALL be distinct from host operating system, GPU driver and virtualisation updates; host updates SHALL follow a documented maintenance procedure. |

## Service quality and distributed operation

| ID | Requirement |
|---|---|
| QLT-001 | Routine dataset, scheduling, monitoring, recovery, accounting and model workflows SHALL be available through the web interface. |
| QLT-002 | Errors SHALL explain the failure and an actionable next step where known. |
| QLT-003 | Metadata, ledger entries, completed artefacts and history SHALL survive application and host restarts. |
| QLT-004 | The UI SHALL reflect authoritative execution, tracking, storage and accounting state. |
| QLT-005 | Private data and execution actions SHALL require authorisation. |
| QLT-006 | User-facing configurations, logs and artefacts SHALL exclude secrets. |
| QLT-007 | Host addresses, storage paths, quotas and service endpoints SHALL be configurable. |
| QLT-008 | Backup and restoration procedures SHALL cover metadata, ledger state, artefacts and secret-store recovery material. |
| QLT-009 | Metric refresh intervals and worker-disconnection timeouts SHALL be configurable and documented. |
| QLT-010 | Capacity, UI response time, telemetry freshness, recovery time and acceptable data-loss targets SHALL be assigned measurable values before release acceptance. |
| QLT-011 | The platform SHALL be tested against those targets with a recorded workload and user count. |
| QLT-012 | Existing worker jobs SHOULD continue during temporary loss of cloud connectivity only within an unexpired execution lease and reserved budget, with required inputs available locally. |
| QLT-013 | Host boot and worker-agent recovery procedures SHALL reconnect to cloud services and reconcile interrupted jobs without duplicate execution or charging. |
| QLT-014 | Cloud user and worker endpoints SHALL use documented authenticated and encrypted access paths. |
| QLT-015 | Cross-host service traffic and remote user access SHALL be encrypted. |
| QLT-016 | Storage exhaustion SHALL produce actionable alerts and SHALL prevent unsafe new allocations. |
| QLT-017 | Backup restoration SHALL be exercised before initial acceptance and thereafter at a documented interval. |
| QLT-018 | The platform SHALL publish separate availability expectations for cloud services and worker pools, including worker dependence on local power and networking. |
| QLT-019 | Retention policies SHALL be configurable for logs, checkpoints, datasets and artefacts, with accounting and audit retention managed separately. |

## Cloud control plane and worker delivery

| ID | Requirement |
|---|---|
| CLD-001 | The web interface, API, scheduler, worker registry, authoritative run metadata and credit ledger SHALL be deployed in the cloud. |
| CLD-002 | The control plane SHALL remain usable for history, submission and administration while all GPU workers are offline, showing affected jobs as blocked or queued. |
| CLD-003 | Cloud infrastructure, persistent storage and network policy SHALL be reproducible from version-controlled deployment definitions. |
| CLD-004 | User, worker and deployment identities SHALL have distinct scopes and least-privilege permissions. |
| CLD-005 | Worker agents SHALL obtain authenticated, integrity-verified versioned releases through an outbound update mechanism. |
| CLD-006 | Agent rollout SHALL support staged deployment, compatibility checks and return to a previous compatible version. |
| CLD-007 | Control-plane releases SHALL support the documented worker-agent compatibility window so temporarily offline workers can reconnect safely. |
| CLD-008 | Control-plane availability, worker availability and internet transfer reliability SHALL have separate monitoring and service targets. |
| CLD-009 | Cloud budgets and alerts SHALL cover hosting, storage and network charges independently of internal user credits. |
| CLD-010 | GPU health checks SHALL cover each supported host execution stack affected by a release. |
| CLD-011 | GCP SHALL host the cloud control plane. |
| CLD-012 | GCP infrastructure SHALL be defined and maintained using Terraform, with environment-specific state, restricted access and documented bootstrap and recovery procedures. |
| CLD-013 | Implementation SHALL reference accepted architecture decisions for technology choices and SHALL NOT silently treat proposed choices as approved. |

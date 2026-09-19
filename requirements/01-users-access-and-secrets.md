# 01 — Users, Access and Secrets

[Overview](README.md) · Version 0.5

## Multiple users and authorisation

| ID | Requirement |
|---|---|
| USR-001 | The platform SHALL support multiple individually authenticated users operating concurrently. |
| USR-002 | Each dataset, job, model, artefact and credit account SHALL have an identifiable owner and project scope. |
| USR-003 | The platform SHALL enforce project membership and role-based permissions on server-side operations, including APIs, live updates and artefact downloads. |
| USR-004 | Users SHALL be able to inspect their own jobs and resources shared with their projects. |
| USR-005 | Project owners SHALL be able to grant and revoke project membership within their authority. |
| USR-006 | Administrative, operational and researcher permissions SHALL be separable. |
| USR-007 | Revocation SHALL prevent new authorised operations and invalidate affected sessions or credentials within a documented maximum interval. |
| USR-008 | The platform SHALL apply a documented policy to active jobs when their owner or project access is revoked. |
| USR-009 | Service identities SHALL be distinct from human identities and restricted to their required operations. |
| USR-010 | Changes to membership, roles, secret grants, pricing, credit allocations and model approval SHALL generate audit events recording actor, target, time and outcome. |
| USR-011 | The platform SHOULD integrate with an existing identity provider rather than implement password management itself. |
| USR-012 | Projects SHALL be private by default; cross-project sharing SHALL require an explicit grant. |

## Secret-management design

Secrets include registry credentials, storage credentials, service tokens, deployment credentials and private keys. Secret references and non-sensitive metadata may appear in configuration; secret values may not.

| ID | Requirement |
|---|---|
| SEC-001 | The platform SHALL use a dedicated secret store as the authoritative source for persistent runtime secrets. |
| SEC-002 | Persistent secret values SHALL be encrypted at rest and protected in transit. |
| SEC-003 | Secret-store decryption keys and bootstrap credentials SHALL be held outside the encrypted data they protect. |
| SEC-004 | Jobs and service configurations SHALL reference secrets by identifier and SHALL NOT contain literal secret values. |
| SEC-005 | Workloads SHALL obtain only explicitly granted secrets through authenticated workload identities. |
| SEC-006 | Secrets SHALL be supplied at runtime through a documented injection mechanism; secrets SHALL NOT be embedded in container images, source control or build artefacts. |
| SEC-007 | The injection mechanism SHOULD use short-lived credentials or restricted, ephemeral file mounts where supported. |
| SEC-008 | Secret values SHALL NOT be returned by ordinary read APIs, displayed in the UI, included in logs or included in training artefacts. |
| SEC-009 | The platform SHALL apply redaction to platform-managed logs and error reporting as defence in depth. |
| SEC-010 | Authorised administrators SHALL be able to create, rotate and revoke secrets without rebuilding application images. |
| SEC-011 | Rotation and revocation SHALL have documented effects on active workloads, credential leases and subsequent access. |
| SEC-012 | Secret retrieval, modification and denied access SHALL be audited without recording secret values. |
| SEC-013 | A workload whose required secrets cannot be obtained SHALL fail closed with a diagnostic identifying the missing reference without revealing its value. |
| SEC-014 | Development, test and production environments SHALL use separate credentials and secret scopes. |
| SEC-015 | Secret-store backup, restoration and emergency access procedures SHALL be documented and tested. |
| SEC-016 | Provenance SHALL record non-sensitive secret reference and version metadata where required to explain a run, but SHALL NOT preserve revoked credentials for replay. |
| SEC-017 | The deployment design SHALL document the selected secret store, bootstrap trust, key custody, workload authentication, injection, rotation and recovery procedures before production deployment. |
| SEC-018 | Ephemeral secret material SHALL be removed when a workload ends, including after failures, subject to a documented cleanup interval. |

## Trust boundary

The initial platform runs approved workloads for authorised users. Linux containers on a shared host do not constitute a requirement for hostile multi-tenant execution.

| ID | Requirement |
|---|---|
| SEC-019 | The platform SHALL document which administrators can access host memory, storage and runtime credentials. |
| SEC-020 | Jobs SHALL NOT receive privileged container access, host management sockets or unrelated host directories unless an explicitly reviewed workload policy requires them. |

## Worker identity and trust

| ID | Requirement |
|---|---|
| SEC-021 | Worker credentials SHALL authorise only that worker's assigned jobs and necessary status, transfer and metering operations. |
| SEC-022 | Workers SHALL NOT receive control-plane database credentials, platform master secrets or access to other workers' assignments. |
| SEC-023 | Dataset and artefact access grants SHALL be scoped to the assigned job, permitted operations and a bounded validity period. |
| SEC-024 | Each project SHALL specify eligible worker pools or trust classes; new worker enrolment SHALL NOT implicitly grant access to private workloads. |
| SEC-025 | The platform SHALL document that an administrator of an execution host may access workload data and injected credentials on that host; container isolation SHALL NOT be presented as protection from that administrator. |
| SEC-026 | Worker revocation SHALL prevent new assignments and credential renewal; existing disconnected execution SHALL be bounded by its previously issued lease. |
| SEC-027 | Workers SHALL remove job-scoped data and credentials according to project cache and retention policy after execution. |

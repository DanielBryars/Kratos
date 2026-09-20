# ADR-011 — Cloud SQL metadata and managed human identity

**Status:** Accepted  
**Date:** 2026-09-19

## Context

Kratos needs durable relational state for users, projects, workers, compute groups, jobs, leases,
audit events and the later credit ledger. It also needs concurrent human login without implementing
password storage. The Rust control plane runs on Cloud Run and must reconnect safely as instances
start and stop.

Human sessions, cloud service identity and worker identity have different trust and revocation
requirements. Reusing one credential type for all three would grant unnecessary authority and make
incident response harder.

## Decision

Kratos SHALL use Cloud SQL for PostgreSQL as its durable relational metadata store. Database schema
changes SHALL use versioned migrations that run separately from ordinary application startup. The
credit ledger SHALL use transactional PostgreSQL records and idempotency keys when it is introduced.

The Cloud Run revision SHALL run the Rust ingress container with a Cloud SQL Auth Proxy v2 sidecar.
The proxy SHALL use automatic IAM database authentication and listen only within the Cloud Run
instance. The Rust service SHALL use a bounded connection pool against that local endpoint. The
runtime service account SHALL receive `roles/cloudsql.client` and
`roles/cloudsql.instanceUser`; it SHALL be added as an IAM database user and granted only the SQL
permissions required by the application role.

The proxy obtains and refreshes short-lived access tokens for the Cloud Run service account. Kratos
SHALL NOT store a PostgreSQL password for the control plane. The instance SHALL require IAM database
authentication, SHALL expose no authorised-network password path, and SHALL use encrypted connector
traffic. The initial public IP is a connector transport path rather than an unauthenticated database
endpoint; a private IP SHALL be reconsidered when a VPC is required for other services.

Kratos SHALL use Google Cloud Identity Platform for human authentication. The browser SHALL complete
the provider login and send the resulting Identity Platform ID token to the Rust API.

The API SHALL establish the identity behind that token on **every** authenticated request. For R0.2
it does so by calling Identity Platform's `accounts:lookup` rather than verifying the token
locally, and the trade is deliberate in both directions. It fails **closed**: Identity Platform
being unavailable makes Kratos unavailable to humans, because a request whose identity cannot be
established is refused rather than assumed. In exchange, an account disabled at the provider is
observed on the **next** request rather than at the end of a cache lifetime, and a token that has
been revoked stops working immediately.

Local verification of the signature, issuer, audience, expiry and required claims against cached
signing keys is a later optimisation. It SHALL be adopted only if it preserves every one of those
checks and the deliberate acceptance of a provider-disablement window is recorded, because that
window is the whole of what is being bought. Kratos SHALL store application roles, project membership and worker ownership in PostgreSQL;
identity-provider login alone SHALL NOT grant project or administrative authority.

Human authentication SHALL initially use Google sign-in. Additional Identity Platform providers MAY
be added without changing Kratos authorisation identifiers. Kratos SHALL key a human identity by the
stable provider subject and SHALL NOT use a mutable email address as its primary identity.

Worker authentication SHALL remain separate from human login. An authorised operator SHALL create a
short-lived, single-use enrolment secret. Kratos SHALL store only a slow password hash of that secret.
Successful enrolment SHALL issue a unique, scoped and revocable worker credential; the database SHALL
store only its hash. Worker credentials SHALL authorise only that worker's protocol operations and
SHALL NOT be accepted as human sessions or Google Cloud credentials.

Secret Manager SHALL remain the authoritative store for persistent runtime secrets that cannot use
workload identity. Terraform SHALL manage secret containers and access policy, but SHALL NOT place
secret values in configuration or Terraform state. OAuth provider secrets, telemetry ingestion
secrets and future external service credentials SHALL be injected into only the services that need
them. Browser configuration values such as an Identity Platform API key SHALL be treated as public
identifiers and protected by API restrictions; they SHALL NOT be described as proof of user identity.

## Initial development profile and cost gate

The Terraform database module SHALL default to disabled. Enabling it is a deliberate deployment
decision because Cloud SQL accrues cost continuously. The initial development profile is one zonal
`db-f1-micro` PostgreSQL 16 instance with 10 GiB of SSD storage, automated backups, point-in-time
recovery and deletion protection.

At the published price of USD 0.0105 per hour, 730 hours of shared-core compute is approximately
USD 7.67 per month. Storage, retained backups, logs and network traffic are additional. Shared-core
instances have no availability SLA and may produce higher IAM-login latency under CPU pressure. This
profile is suitable for development evidence, not a production availability commitment. The actual
estimate SHALL be reviewed in the Google Cloud pricing calculator before enabling the resource.

Identity Platform Tier 1 providers, including Google social sign-in, currently include 50,000 monthly
active users at no charge. The project SHALL still configure budget alerts because free allowances
and prices can change.

## Alternatives

| Option | Assessment |
|---|---|
| Firestore | Good serverless operation, but the worker/job relationships, leases, audit queries and transactional ledger fit PostgreSQL more directly. |
| PostgreSQL on the observability VM | Lower apparent instance count, but couples authoritative control state to the disposable single-node telemetry stack and adds database patching and recovery work. |
| Password-authenticated Cloud SQL | Broadly supported, but creates a long-lived secret and rotation path that workload identity avoids. |
| Auth0 or another hosted OIDC provider | Viable, but adds another vendor and billing surface when Identity Platform integrates with the selected cloud. |
| Self-hosted identity | Provides control at the cost of password, patching, availability and account-recovery responsibilities that are outside the initial project. |
| Identity-Aware Proxy only | Useful for operator-only access, but does not by itself provide Kratos project membership, worker ownership or API authorisation semantics. |

## Consequences

- Cloud SQL becomes the authoritative store for application state; MLflow and telemetry stores retain
  their specialist data rather than becoming control-plane databases.
- Database availability and connection limits now affect control-plane operations, so health checks,
  pool limits, backups and restoration evidence are required.
- Identity Platform authenticates people while Kratos remains responsible for authorisation and
  audit decisions.
- Operators can revoke Cloud Run database access through IAM without locating or rotating a stored
  database password.
- A database migration identity needs elevated schema privileges separate from the runtime identity.
- The bootstrap enrolment secret is visible once to its creator; losing it requires creating a new
  enrolment rather than retrieving the old value.

## Conditions for reconsideration

Reconsider the database tier or topology when measured load exceeds shared-core limits, an accepted
availability target requires regional high availability, or private service networking is otherwise
introduced. Reconsider Identity Platform if required organisations, providers, account lifecycle or
pricing cannot satisfy the user requirements.

## References

- [Cloud SQL IAM authentication](https://docs.cloud.google.com/sql/docs/postgres/iam-authentication)
- [Automatic IAM database login](https://docs.cloud.google.com/sql/docs/postgres/iam-logins)
- [Cloud SQL Auth Proxy](https://docs.cloud.google.com/sql/docs/postgres/sql-proxy)
- [Cloud Run sidecar containers](https://docs.cloud.google.com/run/docs/deploying)
- [Identity Platform pricing](https://cloud.google.com/identity-platform/pricing)
- [Cloud SQL pricing](https://cloud.google.com/sql/pricing)

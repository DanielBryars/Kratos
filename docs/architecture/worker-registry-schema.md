# Worker registry persistence

The first PostgreSQL migration implements the R0.1 identity and worker-registry boundary described
by [ADR-011](decisions/011-metadata-and-human-identity.md) and the
[worker protocol](../protocol/worker-v1.md).

## Records

| Record | Purpose |
|---|---|
| `human_identities` | Stable Identity Platform provider subjects; email is metadata rather than identity. |
| `worker_enrolments` | Short-lived, single-use bootstrap grants owned by a human identity. |
| `workers` | Individual agent identity, approval state, latest capabilities and heartbeat sequence. |
| `worker_credentials` | Scoped credentials belonging to exactly one worker. |
| `compute_groups` | Owner-controlled network topology groups. |
| `compute_group_members` | Explicitly approved group membership; an agent cannot insert itself. |
| `audit_events` | Append-oriented security and administration outcomes without secret values. |

Enrolment and worker credentials contain a typed prefix, a random lookup identifier and 256 bits of
random secret material. The prefix and identifier are not authenticators. PostgreSQL stores only an
Argon2id verifier of the entire credential. Plaintext exists only in the creation response and the
worker's restricted credential file; Rust debug formatting always emits `[REDACTED]`.

The schema constrains worker states, protocol versions, heartbeat ordering, active credentials and
group membership. Capability reports remain JSON because they are versioned protocol documents,
while identity, ownership and lifecycle state remain relational.

The second migration adds `workers.last_observed_at`. `last_seen_at` records when the control plane
accepted a heartbeat, while `last_observed_at` records when the worker says it collected the report.
Keeping both prevents clock skew on a home machine from changing the server-side liveness decision.

The R0.2 artefact migration adds immutable per-job output requirements, one manifest per attempt and
its declared files. Composite foreign keys keep every manifest, attempt and requirement within one
job. A `verified` file requires complete upload evidence plus matching generation, length and CRC32C
observed by the server-side Cloud Storage verifier; PostgreSQL rejects a bare status transition.

## Runtime operations

The enrolment exchange SHALL lock the bootstrap row, create the worker and scoped credential, mark
the bootstrap credential as consumed, and write the audit event in one transaction. The heartbeat
update SHALL change capabilities only when its sequence is newer. Repeating the accepted sequence
and identical protocol/capability observation is idempotent. Reusing that sequence with different
observation data or sending an older sequence returns a conflict. Job assignment revalidates the
accepted sequence and observation under the same worker-row lock used to change scheduling state.

The service SHALL look up a credential by its non-secret random identifier before running Argon2id.
Known identifiers are rate limited and share a bounded verification pool. Missing identifiers are
rejected without running the expensive password hash. Neither logs nor error responses include the
credential supplied by the caller.

## Migration operation

`kratos-migrate` is a separate binary in the control-plane image. It reads the same non-secret
database connection settings as the service and applies embedded, checksummed SQLx migrations.
Application startup SHALL NOT alter the schema. The deployment pipeline runs this binary as a
single-task Cloud Run job under a dedicated migration identity before applying a database-enabled
application revision. A failed migration stops the deployment before the service is changed.

The migration identity receives the PostgreSQL `cloudsqlsuperuser` role; the runtime identity does
not. After every migration, the job transactionally grants the runtime identity only database
connect, public-schema usage, table `SELECT`/`INSERT`/`UPDATE`/`DELETE`, and sequence usage. Default
privileges apply the same data access to objects created by later migrations. PostgreSQL performs
identifier quoting for the IAM database usernames before any grant statement is constructed.

CI applies every migration to a disposable PostgreSQL 16 service. That test service explicitly uses
PostgreSQL `trust` host authentication, meaning any process able to reach its runner-local port can
log in without a password. It contains no persistent data, exists only for one isolated CI job and
is destroyed with the runner. This is a test convenience and is not the production authentication
design. Cloud SQL uses automatic IAM database authentication through its local proxy sidecar.

## Readiness

`/healthz` reports that the process can serve HTTP. `/readyz` verifies `SELECT 1` when persistence is
configured and returns `503` if PostgreSQL is unavailable. With the cost gate closed and database
configuration wholly absent, `/readyz` returns `200` with `database: disabled`. Partial database
configuration is a startup error rather than a silent fallback.

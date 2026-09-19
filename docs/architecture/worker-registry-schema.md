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

## Migration operation

`kratos-migrate` is a separate binary in the control-plane image. It reads the same non-secret
database connection settings as the service and applies embedded, checksummed SQLx migrations.
Application startup SHALL NOT alter the schema. A later deployment increment will run this binary
under a dedicated migration identity before routing a database-enabled revision.

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

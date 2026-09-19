# Worker protocol v1

This document defines the R0.1 enrolment, capability and heartbeat contract. The protocol uses
outbound HTTPS requests from an agent running inside the worker's Linux execution environment. It
does not require an inbound port on the worker network.

## Compatibility

Every message SHALL carry a `protocol_version` in `major.minor` form. The initial version is `1.0`.
The control plane SHALL reject an unsupported major version with an actionable response. It MAY
accept an older minor version when all required fields retain their meaning.

Unknown fields are rejected in v1. A capability report therefore fails visibly when the control
plane and agent disagree about its shape.

## Capability report

The Python type `kratos_agent.models.WorkerCapabilities` is the executable schema for the initial
report. It includes:

- collection time, hostname, operating system and architecture;
- logical CPU count, total memory and available storage;
- Python runtime version;
- each detected NVIDIA GPU's index, model, memory and driver version; and
- a separate GPU computation-health state and explanation.

Hardware detection SHALL NOT be treated as a successful computation check. Detection through
`nvidia-smi` produces `unverified`; only the controlled GPU health-check container can produce
`healthy`. Detection failure produces `unavailable` and SHALL NOT silently fall back to CPU.

## Enrolment exchange

### Interactive radio-in

On first start without a bootstrap credential, the agent SHALL generate a random instance UUID and
an Ed25519 key pair, persist them before making a network request, and call
`POST /api/v1/worker-registration-requests`. The request SHALL contain the public key, display name
and current capability report. Retrying with the same instance and key SHALL return the existing
open request rather than create another.

The response SHALL contain an opaque request identifier, a fifteen-minute expiry, a polling
interval and a short comparison code derived from the public key. The agent SHALL print the code.
The authenticated operator console SHALL show the same code and advertised capabilities. An
operator SHALL approve or reject the request. Approval SHALL create a random claim challenge and an
audit event; it SHALL NOT return a worker credential to the browser.

The agent SHALL poll `GET /api/v1/worker-registration-requests/{registration_id}`. Once approved, it
SHALL sign this exact UTF-8 message, where the identifiers use their canonical response encoding:

```text
kratos-worker-claim-v1
{registration_id}
{base64url-without-padding challenge}
```

The agent SHALL send the base64url-without-padding signature to
`POST /api/v1/worker-registration-requests/{registration_id}/claim`. The control plane SHALL verify
the signature with the pending request's public key, atomically create an approved worker, and
return its scoped worker credential. The credential SHALL be reproducibly derived from the signed
claim so retrying an identical claim after a lost response returns the same identity and credential
without creating another worker. A request that is expired, rejected or not approved SHALL NOT be
claimable. The private key SHALL never leave the agent state volume.

Pending registration volume SHALL be bounded. Edge and application rate limits SHOULD restrict
request creation, status polling and failed claims. The identifier and comparison code are not
credentials; possession of the private key is the authentication proof.

### Automated bootstrap

An operator SHALL obtain a short-lived Identity Platform ID token through the web login and call
`POST /api/v1/operator/worker-enrolments`. The control plane SHALL ask Identity Platform to validate
the token and SHALL separately require the stored Kratos identity to have the `operator` role. The
first operator MAY be established only when its verified provider email matches the deployment's
bootstrap email; that match SHALL bind the role to the stable provider subject.

The operator endpoint SHALL create a credential lasting between five and sixty minutes, with a
fifteen-minute default. It SHALL return the plaintext only once, store only its Argon2id verifier,
and write an audit event without secret material.

`POST /api/v1/worker-enrolments` SHALL accept a single-use bootstrap credential in the
`Authorization` header and a request containing:

- `protocol_version`;
- a stable, randomly generated `agent_instance_id` used to make retries idempotent;
- an operator-provided display name; and
- the current capability report.

On the first valid exchange, the control plane SHALL atomically consume the bootstrap credential,
create one worker identity in the unapproved state and return its worker identifier and scoped
worker credential. A repeated request with the same bootstrap credential and instance identifier
MAY return the same result during a short retry window; it SHALL NOT create a second worker.
A consumed credential used with a different instance identifier SHALL be rejected.

Bootstrap and worker credentials SHALL be redacted from logs and error bodies. The bootstrap value
SHALL NOT be accepted as a command-line argument. The worker credential SHALL be stored only in a
file readable by the agent service identity and SHALL authorise operations for that worker alone.
Server-side storage SHALL retain a verifier rather than the credential value. Revocation SHALL make
subsequent authenticated requests fail.

The endpoint SHALL return `503 persistence_unavailable` when the worker registry is not configured.
Authentication failures SHALL use one generic `401 unauthorized` response and SHALL NOT reveal
whether a credential identifier exists, has expired or has been revoked. A correctly authenticated
credential which has already been consumed SHALL return `409 enrolment_consumed`.

Credential strings are opaque to agents. The current envelope carries a type prefix and random
lookup identifier followed by 256 bits of random secret material. The prefix and identifier are not
proof of authority. The control plane SHALL authenticate the complete value against its Argon2id
verifier and SHALL perform rate limiting before expensive verification work.

## Heartbeat

`PUT /api/v1/workers/{worker_id}/heartbeat` SHALL authenticate the worker and accept:

- `protocol_version`;
- a monotonically increasing sequence number;
- the observation time; and
- the current capability report.

The response will include the accepted worker state and the next heartbeat interval. Duplicate
sequence numbers SHALL be idempotent. Older sequence numbers SHALL NOT replace newer observations.
An unapproved or quarantined worker may report status but receives no work. A revoked worker is
denied.

The initial heartbeat interval is 30 seconds. A worker becomes `stale` after 90 seconds without an
accepted heartbeat and `offline` after five minutes. The UI SHALL show the last accepted contact and
the age of displayed capabilities.

## Operator fleet controls

Authenticated operators SHALL retrieve the worker inventory from `GET /api/v1/operator/workers`.
The response SHALL include the durable worker state, derived connectivity, last accepted heartbeat,
latest capabilities and compute-group memberships. Connectivity is a presentation of heartbeat age;
it SHALL NOT overwrite the durable worker state.

`POST /api/v1/operator/workers/{worker_id}/approve` SHALL change an eligible worker to `idle` and MAY
add it to a named compute group in the same database transaction. The control plane, rather than the
agent, SHALL own that membership decision. The quarantine and revoke endpoints SHALL record audited
state changes. Revocation SHALL also revoke every active credential for the worker atomically.

## Compute groups

An agent MAY report network observations, but it cannot grant itself membership of a compute group.
Group membership is a separate administrator-approved control-plane record. R0.1 will show the home
group's peer connectivity as unverified until the container-to-container test is delivered in R0.6.

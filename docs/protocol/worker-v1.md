# Worker protocol v1

This document defines the R0.1 enrolment, capability and heartbeat contract. The protocol uses
outbound HTTPS requests from an agent running inside the worker's Linux execution environment. It
does not require an inbound port on the worker network.

## Compatibility

Every message SHALL carry a `protocol_version` in `major.minor` form. The initial version is `1.0`.
The control plane SHALL reject an unsupported major version with an actionable response. It MAY
accept an older minor version when all required fields retain their meaning.
An accepted newer heartbeat sequence SHALL atomically replace the worker's stored protocol version
and capability report. Scheduling SHALL use that stored current version rather than the version
seen during enrolment. A duplicate sequence SHALL be accepted only when its protocol version and
capability report exactly match the stored observation. Scheduling SHALL recheck that same
observation while holding the worker row lock; if a newer heartbeat wins first, the older request
SHALL conflict without claiming work.

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
`healthy` or `unhealthy`. Detection failure produces `unavailable` and SHALL NOT silently fall back
to CPU.
A verified result SHALL include the complete structured evidence and immutable health-check image
digest. The reported status SHALL match the nested evidence status. The control plane SHALL reject
mutable image references, incomplete healthy evidence and mismatched status values.

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

When configured with an immutable health-check image, the trusted agent SHALL run the check once at
startup through its constrained Docker executor. Subsequent heartbeats SHALL carry that result and
its original check time; the agent SHALL NOT rerun a CUDA workload at every heartbeat.

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

## R0.2 job assignment extension

An eligible heartbeat response MAY include one `assignment`. The assignment SHALL identify the job
and attempt, an immutable image reference, one GPU index, a bounded runtime and an absolute lease
deadline. Only an approved `idle` worker whose current report contains a verified healthy GPU SHALL
receive new work.

The agent SHALL persist the accepted heartbeat sequence before starting the assignment. It SHALL
execute at most one active assignment and SHALL reject an assignment whose lease has already expired.
If a heartbeat or result acknowledgement is lost, the control plane SHALL return the same attempt;
the agent SHALL inspect its stable attempt-named container and SHALL NOT knowingly start a duplicate.

### Execution authority and network loss

The agent SHALL execute an attempt at most once. It SHALL fetch the image, durably record the
whole assignment, including its runtime bound and lease deadline, in its protected state and only
then create the container. For a recorded
attempt it SHALL resume the existing container and SHALL NOT create another; if that container is
missing it SHALL report a failure, because it cannot prove that the workload did not run. A
container that was created but never started SHALL also be reported as a failure.

A container's authority ends at the earlier of its runtime bound, measured from the container's
actual start, and the lease deadline. The agent SHALL kill the container at that time. It SHALL
NOT first ask it to stop, because a grace period would let a workload that ignores the request run
beyond its authority. Enforcement SHALL NOT depend on reaching the control plane: whenever the
agent holds a recorded assignment, including immediately after a restart, it SHALL supervise that
container to the end of its authority before any operation that needs the network. A container
found to have finished more than thirty seconds after that time was unsupervised and SHALL NOT be
reported as successful. A container that
exited within its authority SHALL be reported with its actual exit status even when the result can
only be delivered after the lease deadline; the control plane SHALL reject it as a late result once
the lease has expired.

The control plane SHALL durably close an expired attempt before retrying its job. It SHALL create no
more than two attempts for one job: the initial attempt and one automatic retry from scratch. A
worker SHALL treat each attempt identifier as independent execution authority and SHALL NOT infer
checkpoint continuity between them. The database SHALL permit no more than one assigned or running
attempt for a job, and a late result from a closed attempt SHALL return that attempt's terminal state
without changing the current job or replacement attempt.

An operator cancellation of queued work is immediately terminal. Cancellation of assigned or
running work remains pending until the worker next reports a result or the lease expires. Because
protocol 1.x has no cancellation command, a repeated heartbeat MAY continue returning that attempt;
the agent's existing runtime and lease bounds still apply. The control plane SHALL record the job
and attempt as `cancelled`, and release the worker, when either completion signal arrives.

A transport failure, `429` or `5xx` response, or local container-runtime error SHALL NOT end the
agent. It SHALL retain the container and its recorded attempt, continue heartbeats at the normal
interval and replay the result when the control plane next returns the same attempt. Any other
rejection, including `401`, remains fatal. When a heartbeat response no longer carries a recorded
attempt, the control plane has closed it: the agent SHALL remove that container, running or not,
and clear its record.

The agent SHALL continue heartbeats at the normal interval while it supervises a container, so a
busy worker's displayed connectivity reflects its link rather than its workload. A heartbeat that
fails for a temporary reason SHALL NOT interrupt the workload. A valid heartbeat response that no
longer carries the attempt, or an explicit `4xx` rejection other than `429`, withdraws the
attempt's authority: the agent SHALL kill the container and report the failure. A malformed
response SHALL NOT be treated as a withdrawal. No failure inside a heartbeat, including a failure
to observe the host's capabilities, SHALL end supervision of a running container.

Known limitations of this slice: the lease deadline is compared with the worker's clock, so worker
clock error shifts the local bound; a heartbeat in progress can delay enforcement of a bound by up
to about twenty-five seconds, being a ten-second capability collection followed by a fifteen-second
request timeout, plus the polling interval; and no bound is enforced while the agent process itself
is not running, which is why an overrun found afterwards is reported as a failure. A container that
cannot be killed is reported as no result at all: the attempt stays recorded and enforcement is
retried, because a result would tell the control plane the attempt had ended while it had not.

The agent SHALL send the bounded exit status, timeout flag, stdout, stderr and failure summary to
`PUT /api/v1/workers/{worker_id}/job-attempts/{attempt_id}/result`. Result submission SHALL be
idempotent. The control plane SHALL release the worker only after it has durably recorded a terminal
job and attempt state. Output fields SHALL be limited to 64 KiB each and SHALL NOT contain granted
secrets because this initial slice grants none.

### Training-run correlation environment

Before starting a job container, the agent SHALL set `KRATOS_JOB_ID` to the assignment's stable job
UUID and `KRATOS_ATTEMPT_ID` to the assignment's stable attempt UUID. The agent SHALL also set
`OTEL_RESOURCE_ATTRIBUTES` to include `kratos.job.id` and `kratos.attempt.id` with the same values.
These identifiers are correlation metadata, not credentials, and SHALL NOT be regenerated by the
worker or workload.

Supported training workloads SHALL validate both UUIDs and emit them in structured success and
failure results whenever valid identifiers were supplied. This establishes the correlation contract
for later OpenTelemetry and MLflow integrations. It does not enable telemetry export, provision a
collector or create an MLflow run. A later integration SHALL add the authoritative worker, project
and MLflow run attributes described by ADR-009.

## R0.2 durable output extension (protocol 1.1)

A job MAY contain an immutable `output_requirements` array. Each requirement defines an exact
relative path below `/kratos/outputs`, a role, media type, mandatory flag and maximum byte length.
The control plane SHALL NOT assign a job with output requirements to a worker advertising protocol
1.0. Assignments without output requirements remain compatible with protocol 1.0.

After execution, a protocol 1.1 agent SHALL calculate the byte length, lowercase SHA-256 digest and
canonical base64 CRC32C of each produced regular file. It SHALL submit one immutable manifest with a
client-generated UUID to
`PUT /api/v1/workers/{worker_id}/job-attempts/{attempt_id}/artifact-manifest`. The manifest SHALL
contain every mandatory exact path, MAY contain declared optional paths, and SHALL NOT contain an
undeclared path. Replaying the same UUID and content returns the same artefact identifiers and object
keys. A changed replay or second manifest conflicts.

The control plane exposes three replay-safe transfer calls:

- `PUT .../artifacts/{artifact_id}/upload` moves a declared artefact to `uploading`. The control
  plane signs and performs the XML resumable-initiation `POST`, including generation-match zero and
  the exact declared upload length, then returns only the resulting session URI. Concurrent and
  replayed calls SHALL NOT initiate in parallel; a call during the short durable `initiating` window
  is retryable, and calls after activation return the same session.
  The worker SHALL keep the session URI private and upload only the declared bytes through it.
- `PUT .../artifacts/{artifact_id}/abandon-upload` consumes a resumable session that GCS has rejected
  with 400, 404 or 410. The request SHALL contain protocol version `1.1` and
  `session_uri_sha256`, calculated over the exact UTF-8 session URI bytes and encoded as 64
  lowercase hexadecimal digits. The URI itself SHALL NOT be copied into the request body. A 204
  response means the matching cancellation was consumed and the worker SHOULD call the upload
  endpoint for a replacement. A 503 means durable cancellation is pending and the worker SHOULD
  retry abandon. A 409 `upload_session_changed` means the fingerprint is stale; the worker SHALL
  discard that local URI and reload current manifest/session state. Matching abandon replays SHALL
  return 204, including after the secret URI has been removed. A late request SHALL NOT cancel a
  newer session. The upload endpoint applies the same cancellation-before-replacement rule to an
  expired session and returns 503 until cancellation is consumed.
- `PUT .../artifacts/{artifact_id}/complete-upload` records the immutable Cloud Storage generation,
  returned byte length and CRC32C, then independently reads that exact object generation from GCS.
  A matching retry returns the stored verified response; different evidence conflicts. An
  authenticated matching retry remains valid after the attempt becomes terminal, but a terminal
  attempt cannot start or alter an upload.

The worker's completion report alone SHALL NOT mark an artefact `verified`. The control plane SHALL
match the configured bucket, opaque object key, immutable generation, byte length, CRC32C and the
`kratos-sha256` custom metadata against the declaration and completion report. A missing object or
temporary GCS failure leaves verification retryable. A metadata mismatch marks the artefact
`rejected`; it SHALL NOT be published as valid output. A successful job result SHALL be rejected
while any mandatory output lacks verified evidence. Failed job results do not require mandatory
outputs, so a workload failure cannot leave the worker permanently occupied.

The control plane SHALL durably record lifecycle protection as pending after metadata verification,
then protect the exact generation and publish it as verified. API retries and a background reconciler
SHALL finish pending protection after interruption. It SHOULD delete a rejected object immediately at
its exact generation; lifecycle cleanup is the fallback for abandoned uploads and unavailable
immediate cleanup.

Upload initiation responses SHALL use `Cache-Control: no-store`. Resumable session URIs are
credentials: agents and the control plane SHALL redact them from logs. The control plane MAY retain
the one URI per artefact in its encrypted database solely for authenticated idempotent replay.
Revocation, cancellation, lease abandonment and rejection SHALL consume reachable URIs through a
retryable GCS cancellation reconciler. A session orphaned between GCS creation and database commit is
never worker-reachable and expires at GCS; the initiating record rate-limits replacement creation.

The initial limits are 100 files, 5 GiB per file, 10 GiB across the manifest and 240 UTF-8 bytes per
logical path. Absolute paths, empty segments, `.` and `..` segments, backslashes and control
characters are rejected. Logical paths remain metadata; object keys use opaque server-generated
artefact identifiers below the owning identity, job and attempt scopes.

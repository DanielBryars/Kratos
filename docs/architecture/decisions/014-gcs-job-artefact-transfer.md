# ADR-014 — Job artefacts in Cloud Storage

**Status:** Accepted for R0.2
**Date:** 2026-09-20

Implementation note: the R0.2 control plane uses keyless GCS V4 signing through IAM Credentials
`signBlob`, performs each resumable-initiation request itself, and returns one replay-safe session per
artefact. It finalizes worker-reported uploads from authoritative object metadata through a durable
protection-pending reconciler. Automated cleanup removes unverified objects after seven days;
downloads and project retention remain deferred.

## Context

R0.2 must preserve the outputs of a real training job after its worker is unavailable. Home and
rented workers have unreliable internet links, while the training container deliberately has no
network access. A successful job cannot depend on stdout or a worker-local directory: mandatory
outputs must be integrity checked, durably stored and associated with their exact job and attempt.

The upload design must not place a service-account key or general Cloud Storage credential on a
worker. It must also distinguish a completed local file, an incomplete transfer and an artefact that
the control plane has verified as durable.

## Decision

### Storage and authority

Kratos SHALL store R0.2 job artefacts in a private, environment-specific Google Cloud Storage bucket
in the same region as the control plane. Terraform SHALL enable uniform bucket-level access and
public-access prevention. Objects SHALL use Google-managed encryption initially; customer-managed
keys MAY be introduced when a project policy requires them.

A dedicated artefact-upload signer service account SHALL have object-create authority on the bucket.
The control-plane runtime SHALL be allowed to request signatures from that identity through the IAM
Credentials API, but SHALL NOT hold or export its private key. Neither the worker nor the training
container SHALL receive a Google service-account key, OAuth refresh token or bucket-wide access
token.

After accepting an output manifest, the control plane SHALL create one resumable session per approved
object. It SHALL construct an HTTPS V4 signed initiation request which:

- initiate a resumable upload for one deterministic object name;
- expire after ten minutes;
- include the required resumable-upload and metadata headers in its signature;
- sign `x-goog-if-generation-match: 0`, the XML API precondition that prevents replacement; and
- sign `x-upload-content-length` with the exact declared byte length.

The control plane SHALL send that signed request itself so a worker cannot replay it to create parallel
sessions. Before contacting GCS it SHALL commit an `initiating` record with a unique initiation ID.
While that record is fresh, concurrent or replayed calls SHALL NOT contact GCS. After GCS responds,
the control plane SHALL persist and return only the resulting Cloud Storage resumable-session URI;
later replays SHALL return the same URI. That URI is a bearer credential scoped to
the one object. The agent SHALL store it only in its protected state directory, redact it from logs
and status messages, and remove it after acknowledgement or permanent failure.
It SHALL cancel the session when abandoning a transfer while it can still reach Cloud Storage. A
session can remain usable for up to one week if a disconnected worker cannot cancel it; it cannot
read, list, delete or write a different object, and an incomplete session does not publish an object.
If the process stops after GCS creates a session but before PostgreSQL stores its URI, that session is
unreachable by any worker. The durable `initiating` record suppresses retry fan-out for fifteen
minutes; a later retry may create one replacement, while the unreachable GCS session expires without
publishing an object. This cross-system orphan window cannot be made atomic and is an explicit bounded
cleanup risk.

The control plane SHALL create or return upload authority only for the authenticated worker that owns
the attempt, while the attempt is in an authorised output-transfer state. Revoking the worker or
cancelling the job SHALL prevent new sessions. Previously returned session URIs remain usable until
expiry or cancellation, so the deterministic object scope, generation precondition, declared length
and server-side finalisation check are part of the security boundary.
Worker or credential revocation, job cancellation, lease abandonment and artefact rejection SHALL
atomically mark every reachable session `cancel_pending`. A reconciler SHALL cancel each URI at GCS,
then erase it and record `cancelled`; transient cancellation failure SHALL remain retryable.

### Container-to-cloud flow

1. Before execution, the agent SHALL create a fresh attempt directory on the Linux filesystem and
   mount only its `outputs` subdirectory writable at `/kratos/outputs`. The training container SHALL
   retain no network access and SHALL receive no upload credential or container-management socket.
2. The supported workload contract SHALL declare mandatory outputs and allowed optional output
   patterns. After the container stops, the agent SHALL make the output tree read-only for the
   transfer phase and SHALL NOT follow symbolic links.
3. The agent SHALL enumerate regular files, reject unsafe paths and unsupported file types, enforce
   the output limits, and calculate each file's byte length, SHA-256 digest and CRC32C checksum. It
   SHALL submit this manifest to the authenticated attempt endpoint before any upload begins.
4. The control plane SHALL validate the manifest against the immutable job output contract and
   persist an artefact record before creating one resumable session. The control plane SHALL perform
   the signed GCS initiation request itself; replayable signed initiation authority SHALL never be
   exposed to a worker. Object names SHALL use opaque
   artefact identifiers under
   `v1/projects/{project_id}/jobs/{job_id}/attempts/{attempt_id}/artefacts/{artefact_id}`. A
   container-supplied filename SHALL remain metadata and SHALL NOT become a storage key.
5. The agent SHALL upload in fixed-size chunks, persist the acknowledged offset with the protected
   session URI, and query the session before resuming after a network or process failure. It SHALL
   send the expected CRC32C for Cloud Storage to verify and SHALL NOT change a file after hashing.
6. After Cloud Storage reports completion, the agent SHALL send the artefact identifier, object
   generation and returned checksums to the control plane. Finalisation SHALL be idempotent.
7. The control plane SHALL read authoritative object metadata and require the expected bucket, key,
   generation, length, CRC32C and recorded SHA-256 metadata before marking the artefact `verified`.
   A mismatch SHALL reject the artefact and SHALL NOT publish it as a valid output.
8. The attempt MAY report that execution has finished while outputs are uploading. The job SHALL NOT
   enter `succeeded` until every mandatory artefact is verified and the successful execution result
   is durably recorded. The result acknowledgement SHALL identify the resulting artefacts. Failure
   and cancellation MAY retain approved diagnostic artefacts, but missing training outputs SHALL not
   turn those outcomes into success.

SHA-256 is the Kratos content identity used in manifests and later download verification. CRC32C is
the transport-integrity check enforced by Cloud Storage. Because the execution host is trusted in
R0.2, the platform accepts the agent's declaration that the SHA-256 was calculated from the workload
output; an authorised host administrator can inspect or alter workload data and credentials.

### Metadata

PostgreSQL SHALL remain the authoritative catalogue. An artefact record SHALL include:

- artefact, project, job and attempt identifiers;
- logical relative path, role, media type and whether the output is mandatory;
- byte length, SHA-256 and CRC32C;
- bucket, object key and immutable Cloud Storage generation;
- `declared`, `uploading`, `verified`, `rejected` or `deleted` state;
- declaration, upload, verification, retention and deletion timestamps; and
- rejection or deletion reason where applicable.

The database SHALL enforce uniqueness for an attempt's logical path and artefact identifier. Upload
grants SHALL record their artefact, recipient worker, issue time, expiry and the one resumable session
URI needed for authenticated idempotent replay. The URI SHALL be treated as a bearer secret, returned
only to that worker, redacted from logs and deleted with the artefact record. Signed initiation URLs
SHALL NOT be persisted or returned. Object metadata SHALL contain only non-secret Kratos
identifiers and checksums. User-facing downloads SHALL require project authorisation before the
control plane creates a separate short-lived, read-only URL.

MLflow MAY reference a verified Kratos artefact URI after R0.2 integration is defined. MLflow SHALL
NOT be the authoritative store or acknowledgement path for this first artefact flow.

### Initial limits and cleanup

The first R0.2 profile SHALL allow at most 100 files, 5 GiB per file, 10 GiB in total and a 256 KiB
manifest per attempt. A logical path SHALL be valid UTF-8, relative, no longer than 240 bytes, contain
no empty, `.` or `..` segment, and identify a regular file. Symbolic links, hard-link aliases, devices,
sockets and named pipes SHALL be rejected. Limits SHALL be configurable downwards by deployment or
project policy, and the agent SHALL reserve sufficient local space before starting work.

The agent SHALL retain local outputs and transfer state until the control plane acknowledges every
mandatory artefact or a configured failure-retention interval expires. Local storage pressure SHALL
stop new assignments before deleting unacknowledged mandatory outputs. Cleanup SHALL remove upload
credentials first and SHALL emit an auditable reason when output data must be discarded.

Objects SHALL use an environment-specific lifecycle policy that removes unverified objects after
seven days. Verified-object retention SHALL follow project policy and SHALL not be shortened while a
retained run or model references the artefact. Deletion SHALL update catalogue state and use the
recorded generation precondition. A reconciliation process SHALL inspect stale `uploading` records,
verify a completed deterministic object where possible, or mark the transfer failed without
presenting partial data as valid.

After metadata passes verification, the control plane SHALL durably record the exact generation and a
`protection_pending` action before placing its temporary hold. The API retry path and a background
reconciler SHALL repeat that idempotent action until the hold is confirmed, and only then publish the
artefact as `verified`. The lifecycle rule therefore removes abandoned uploads while leaving verified
objects intact without leaking an untracked hold after a crash. Retention deletion SHALL clear that
hold as part of its later audited flow. An
object rejected for a size, checksum or metadata mismatch SHOULD also be deleted immediately at its
exact generation; lifecycle cleanup is the fallback when immediate cleanup is unavailable.

### Operator visibility

An authenticated operator MAY list the declared artefacts for a job they own through
`GET /api/v1/operator/jobs/{job_id}/artifacts`. The response SHALL distinguish declaration, upload,
verification and rejection state and SHALL expose both the worker-declared size/checksums and the
authoritative storage evidence only after verification succeeds. A protection-pending artefact SHALL
be presented as `verifying`, never as verified.

The operator response SHALL NOT include a bucket, object key, resumable session URI or read
credential. An absent declaration, a failed status request and an artefact without verified evidence
SHALL remain visibly distinct in the web interface. Direct download authority remains deferred.

## Alternatives

| Option | Assessment |
|---|---|
| Proxy all bytes through the Rust control plane | Centralises authorisation, but adds Cloud Run bandwidth, timeout, memory and scaling pressure to large home-network transfers. |
| Give each worker a service-account key | Simple client support, but creates a persistent, exportable cloud credential with a broad rotation and incident-response burden. |
| Downscoped OAuth access tokens | Credential Access Boundaries can restrict a short-lived token to a Cloud Storage prefix and remain a valid later option. Control-plane-created per-object sessions grant less authority for the small R0.2 manifest and avoid sending a general API token. |
| Direct upload from the training container | Breaks the no-network workload boundary and exposes upload authority to user code. |
| Store outputs only through MLflow | Couples job completion to the experiment-tracking service and obscures Kratos project authorisation and transfer state. |
| Worker-local outputs only | Cannot meet durable completion, cross-worker recovery or cloud history requirements. |

## Consequences

- Training code writes a normal directory and does not need a cloud SDK.
- The trusted agent gains responsibility for path validation, hashing, resumable transfer and local
  retention, while the control plane remains authoritative for permission and completion.
- Direct worker-to-GCS transfer avoids routing large artefacts through Cloud Run and supports
  interrupted residential links.
- Session URIs are secrets even though they are narrow. Redaction, protected local
  state and expiry tests are release requirements.
- A successful container exit and a successful job become distinct events; the UI must show the
  output-transfer phase and its progress.
- Storage, abandoned uploads, egress and retention create real GCP cost outside the later Kratos
  credit system and require budgets and alerts.

## Deliberately deferred

R0.2 SHALL NOT add cross-job content deduplication, worker output caches, peer-to-peer transfer,
multipart composition, customer-managed encryption keys, automatic model promotion, artefact
previews, legal holds or hostile-tenant guarantees. Dataset download grants, checkpoint publication
and MLflow registration SHALL reuse the same principles but require separate contracts. Advanced
offline reconciliation and recovery on another worker remain R0.4 scope.

## Conditions for reconsideration

Reconsider the transfer mechanism if per-job file counts make signed initiation material, if a
supported backend is not Cloud Storage, if resumable-session expiry cannot satisfy a project policy,
or if measured Cloud Storage/API cost and throughput favour a downscoped-token or gateway design.
Reconsider limits and retention from measured model sizes, residential upload rates and accepted
recovery objectives.

## References

- [Create downscoped short-lived credentials](https://docs.cloud.google.com/iam/docs/create-downscoped-short-lived-credentials)
- [Cloud Storage signed URLs](https://docs.cloud.google.com/storage/docs/access-control/signed-urls)
- [Cloud Storage resumable uploads](https://docs.cloud.google.com/storage/docs/resumable-uploads)

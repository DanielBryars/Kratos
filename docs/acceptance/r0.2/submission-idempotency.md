# R0.2 job submission idempotency

**Status:** Implemented in the control plane; automated PostgreSQL tests only. No live acceptance
has been run, and the console's retry behaviour is still pending.

## Requirement

- TRN-011 — a submission retried after an uncertain outcome does not queue a second job

## Contract

`POST /api/v1/operator/jobs` accepts an optional `Idempotency-Key` request header.

| Request | Response |
| --- | --- |
| No header | `201`, a new job on every request, exactly as before this change |
| Header, key not yet used by this caller in this project | `201`, the job is queued and the key is bound to it |
| Same key, same request | `200`, the job the key created, in its current state; nothing is queued |
| Same key, different request | `409` with code `idempotency_key_reused`; nothing is queued or revealed |
| Header that is not exactly one hyphenated UUID | `422` with code `invalid_idempotency_key` |

- **Key format.** A single hyphenated UUID, case-insensitive. The simple, braced and URN spellings,
  an empty value and a repeated header are all refused rather than interpreted, because silently
  ignoring a key the client believes it sent would turn a safe retry into a duplicate job.
- **Scope.** A key belongs to the pair (project, submitting identity). Another member of the same
  project can use the same key and gets a separate job; the key neither finds nor reveals anyone
  else's submission. The same person using a key again after moving to another project starts a
  new key space there.
- **Authorisation runs on every replay.** A replay is a normal authenticated request, and the key
  is looked up only after authorisation, within the caller's current project. An invalid token
  gets `401`. A caller left with no project gets `403`. A caller who has moved to another project
  has the key looked up there, so they can never reach the job in the project they left.
  The header is only parsed after authorisation, so its shape tells an unauthenticated caller
  nothing.
- **What "the same request" means.** The server compares a SHA-256 fingerprint of a canonical form
  of the typed request, not the raw bytes. The canonical form:
  - trims the job name, as it is trimmed before storage;
  - treats an omitted `output_requirements` or `dataset_inputs` as an empty list, and an omitted
    `earliest_start_at` as null;
  - compares `earliest_start_at` as a UTC instant at full input precision, so
    `10:30:00+01:00` and `09:30:00Z` are the same request;
  - sorts output requirements by logical path and dataset inputs by alias, since neither list's
    order means anything once stored;
  - compares every other field exactly, including image reference, timeout, each output's role,
    media type, `mandatory` and `max_bytes`, and each input's dataset version and view.
  The fingerprint is versioned. Changing the canonical form in future would make replays that
  straddle the deployment answer `409` rather than silently match.
- **Replay response.** A replay returns the same body shape as `GET /api/v1/operator/jobs` for that
  job: the original outputs and frozen dataset lineage, and the *current* status, worker, attempt,
  observation stream and artifacts. Expired leases are reconciled first, as the list does. Output
  requirements come back ordered by logical path; the original `201` echoed them in request order.
- **No revalidation of a created job.** Request shape validation runs on every request, including
  replays. Dataset readiness and size limits are checked only when the job is created. A replay of a
  job whose dataset version has since changed state still returns `200`.

## Atomicity

The guarantee is the database's. Migration `202609290033_job_submission_idempotency` adds nullable
`jobs.submission_idempotency_key` (uuid) and `jobs.submission_fingerprint` (64 lowercase hex). A
check requires both to be null or both present, and a partial unique index covers
`(project_id, owner_identity_id, submission_idempotency_key) WHERE submission_idempotency_key IS NOT NULL`.

Creation is `INSERT .. ON CONFLICT (..) WHERE .. DO NOTHING RETURNING`. When two submissions with
one key arrive together, PostgreSQL makes the second wait on the first's uncommitted row. If the
first commits, the second inserts nothing and then reads the committed job in a separate query,
comparing fingerprints. If the first rolls back, the second's insert goes through. Only the
submission that inserted the job row writes its output requirements, dataset inputs and the single
`job.queued` audit event, all in the same transaction. Any failure in that transaction, such as an
unready dataset or a storage error, rolls back the job row with it, so the key is not consumed and
the same request can be retried. Replays add no job, output/input rows or submission audit event. Normal expired-lease reconciliation may still update existing work when building the current job response.

The audit event records `idempotency_key_supplied` as a boolean. The key itself is not logged or
audited. It is a client retry handle, not a credential, but nothing needs to read it back.

Keys do not expire. A key stays bound to its job for as long as the job row exists.

## Persistence boundary in the console

The server only deduplicates retries that present the same key. A key has to outlive whatever
failure caused the retry. The console generates a key per
submission attempt and holds it **in memory**. It therefore covers retries within one page
lifetime, such as a timed-out request or a network error followed by resubmission. It does not
cover a page reload or a closed tab between the lost response and the retry, because the key is
gone and the retry is a new submission. After a reload, the job list is the way to tell whether the
job was queued. Other API clients get the same guarantee only if they persist their key across
their own retries.

## Automated evidence

PostgreSQL tests in `services/control-plane/src/operator/submission/tests.rs`, using the
production migrations:

- identical replay returns the original job (`200`, same id and submission time, lineage and
  outputs), including an equivalent spelling (padded name, offset timestamp, reordered outputs,
  upper-case key), with exactly one job, one audit event, one set of outputs and one dataset input;
- a used key rejects each changed request with `409 idempotency_key_reused`: name, image, timeout,
  `earliest_start_at` changed or removed, outputs removed or with changed limits or `mandatory`,
  dataset inputs removed or re-aliased. Each changed request is valid, and one is shown to be
  queued under a fresh key;
- simultaneous submissions are forced to overlap: an uncommitted job holds the key while both
  submissions are observed blocked inside `INSERT INTO jobs`, then it rolls back, and exactly one
  `201` and one `200` result, with the same job, one audit event and one set of outputs. This test
  calls the post-authorisation core directly, because through HTTP the second request would wait on
  the first's identity row lock during authorisation instead of on the key;
- keys are separate per submitter in one project, and per project for one submitter;
- malformed, non-hyphenated, empty and repeated headers get `422` and write nothing, and an
  unauthenticated request with a malformed key gets `401`;
- a submission that fails after inserting its job row leaves no job. It fails because its dataset
  version is not yet in the catalogue. Once that version exists, the identical request with the
  same key succeeds with `201`;
- a replay after the job has been assigned and started reports `running`, the worker and the
  current attempt, and still succeeds after its dataset version is no longer ready. The same body
  under a new key is refused, which shows the replay skipped that check rather than passed it;
- a replay with an invalid token gets `401`, and after the caller's membership is revoked it gets
  `403`, without revealing the job id;
- requests without the header are never deduplicated.

Unit tests cover header parsing and the fingerprint's equivalences and differences. The OpenAPI
test checks that the header is documented as optional and that `200`, `201`, `409` and `422` are
documented.

## Not yet evidenced

- The required CI job runs the PostgreSQL tests, strict Rust checks and migration application. These tests were written without live workloads
  and their revision-specific results are recorded in the pull request checks.
- No live or production exercise. No deployed control plane has been exercised with retries.
- The console retains an exact payload and key in memory for an uncertain request. An initial definite client refusal unlocks editing; any earlier uncertainty preserves the original retry even after a later refusal. Successful confirmation clears the key, and a deliberately new submission gets a new key. Reload/sign-out loses the retry, with this limitation stated in the UI. Stale queue refreshes cannot hide a newly confirmed submission. All 60 web tests and the production build pass locally.

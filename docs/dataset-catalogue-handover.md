# Dataset catalogue handover

This is the working checklist for the dataset catalogue, upload, review, and training-input path.
The design contract is [ADR-018](architecture/decisions/018-dataset-catalogue-and-curation.md).

## Ready on `feature/dataset-catalogue`

- [x] Embed Leroboscope as `apps/leroboscope` and preserve its source provenance.
- [x] Build Leroboscope with the control-plane image and serve it at `/leroboscope/`.
- [x] Support exact Hugging Face revisions in the viewer URL.
- [x] Add the Kratos review bridge for episode decisions and uploaded-file sources.
- [x] Define project-scoped datasets, immutable versions, declared files, upload sessions,
  episode curation, and immutable curated views in a forward-only migration.
- [x] Add authenticated APIs to list datasets, import a Hugging Face revision, declare an upload,
  upload and verify files, curate episodes, and publish a view.
- [x] Resolve a Hugging Face symbolic revision to a 40-character commit SHA before registration.
- [x] Verify every uploaded object against its declared byte length and SHA-256 before publishing.
- [x] Delete resumable-session credentials after successful verification.

## Finish before merging the feature

- [x] Add PostgreSQL integration tests proving project isolation, immutable version numbering,
  duplicate-name handling, upload integrity rejection, and curated-view snapshots.
  All five hold. Version numbering needed a new endpoint to be true: see below.
- [x] Add the console dataset screen: catalogue, Hugging Face import, folder upload, upload progress,
  version status, viewer launch, curation summary and immutable curated-view publication.
  Path handling, manifest validation and byte-weighted progress live in `datasetUpload.ts` with
  18 tests, mirroring `validate_file_declarations` so an invalid folder is named per file before
  anything is hashed rather than as one 422 afterwards.
- [x] Add the publish-view action to the console. It publishes the server-side snapshot of included
  episodes and reports the immutable manifest identity and episode count.
- [x] Declare the private dataset bucket CORS policy for direct browser resumable uploads and
  signed preview reads from the exact Kratos console origin. Live apply remains deliberately
  pending until the code review completes.
- [ ] Prove a real multi-file LeRobot upload, preview and curation flow end to end after apply.
- [x] Add the signed-read capability the preview session needs. `ArtifactStorage::signed_read_url`
  mints a V4-signed GET for one object with a caller-chosen lifetime, reusing the IAM signBlob
  path the resumable upload already used. A read signs over the host header alone: every extra
  signed header is a way for the request to fail after the URL has been handed out.
- [x] Add the short-lived, version-scoped preview session. Migration 028 adds
  `dataset_preview_sessions`; `POST /operator/dataset-versions/{id}/preview` opens one over a
  **ready** version only; `GET /dataset-previews/{id}/files/{*path}` validates the hashed token
  and answers 307 to a signed read with `Cache-Control: no-store`. The token travels in the
  `Authorization` header rather than a query string, so it never reaches an access log, and it is
  stored only as an Argon2id verifier. Only verified files resolve, only within the session's own
  version, and an expired, revoked, invented or mistyped session all answer 404 alike. The file
  endpoint can redirect a normal client or return the same short-lived signed read as uncacheable
  JSON for a browser media element. It never exposes bucket-wide credentials or durable object
  locations.
- [x] Connect the console to Leroboscope with `postMessage`. The mounted viewer announces readiness,
  accepts one validated review-context shape, resolves each private object through the preview
  capability and assigns only short-lived signed URLs to fetches and media elements. Live proof
  remains part of the end-to-end acceptance item above.
- [x] Add `POST /datasets/{dataset_id}/versions`, which adds an immutable version to an existing
  dataset. Separate from the upload endpoint on purpose: that one must keep refusing a
  duplicate name, so it cannot also read a repeated name as a request for the next version.
  The next number is read behind a lock on the dataset row, proved by a test that fails
  without it.
- [x] Add audit events for dataset registration, upload completion or rejection, curation updates,
  and view publication. Written inside the transaction that makes each change; curation
  gained a transaction so its decision and its record commit together. Tests assert the
  detail carries no object key or session URI.
- [x] Run `cargo fmt`, Clippy with warnings denied, the complete Rust tests with PostgreSQL, web tests,
  Leroboscope tests/type-check/build, Terraform validation, migration application, and the
  control-plane container build. All passed locally on 2026-09-27; GitHub CI remains the merge gate.

## Marked for revisit

- [ ] **The preview file endpoint requires no identity token.** Accepted by Daniel on 2026-09-27 as
  good enough for now, on the basis that the session is version-scoped, expires in minutes and is
  stored only as a verifier. The cost is that a leaked token is usable by whoever holds it until it
  expires, and the audit trail records which session read a file rather than which person.
  Requiring the identity token as well would fix both, at the cost of the viewer frame needing one.
  Revisit before any dataset contains material that is not ours to lose.

## Training integration after the catalogue merges

- [ ] Extend a job specification with exact `dataset_version_id` and optional `dataset_view_id`.
- [ ] Reject scheduling until every selected version is ready and the worker-input protocol is
  available.
- [ ] Add agent-side, digest-keyed caching and stage inputs read-only under
  `/kratos/inputs/<alias>` without mutating the canonical dataset.
- [ ] Record dataset, version, source revision or manifest hash, and curated view in MLflow.
- [ ] Add garbage collection only after reference tracking, active-job leases, and retention policy
  are implemented.

## Safe restart procedure

1. Work from `feature/dataset-catalogue` in `F:\git\Kratos-datasets`.
2. Rebase on `origin/main`; PR #87 is already merged and is an ancestor of this branch.
3. Start with the tests above before adding more endpoints. The migration and API module are the
   authoritative current implementation; ADR-018 is the contract.
4. Do not deploy the migration until the console and preview-session path pass end-to-end tests.
5. Preserve the existing THESHED2 containers, agent state, completed training artifacts, and
   observation evidence while this feature is under development.

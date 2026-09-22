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
- [ ] Add the console dataset screen: catalogue, Hugging Face import, folder upload, upload progress,
  version status, viewer launch, curation summary, and publish-view action.
- [ ] Configure the private dataset bucket CORS policy for direct browser resumable uploads from
  the Kratos console origin, then prove a real multi-file LeRobot upload end to end.
- [ ] Add a short-lived, version-scoped preview session. **Needs a new storage capability first:**
  `ArtifactStorage` can initiate uploads and read metadata but cannot mint a signed GET URL,
  so V4 signing has to be added to the trait and its Google implementation before the
  endpoint can redirect to one. Its file endpoint should validate a hashed
  token and redirect each requested logical path to a short-lived signed GET URL. Never expose
  bucket-wide credentials or durable object locations to the browser.
- [ ] Connect the console to Leroboscope with `postMessage`: pass only the in-memory identity token,
  version metadata, current decisions, and the short-lived preview base URL.
- [x] Add `POST /datasets/{dataset_id}/versions`, which adds an immutable version to an existing
  dataset. Separate from the upload endpoint on purpose: that one must keep refusing a
  duplicate name, so it cannot also read a repeated name as a request for the next version.
  The next number is read behind a lock on the dataset row, proved by a test that fails
  without it.
- [x] Add audit events for dataset registration, upload completion or rejection, curation updates,
  and view publication. Written inside the transaction that makes each change; curation
  gained a transaction so its decision and its record commit together. Tests assert the
  detail carries no object key or session URI.
- [ ] Run `cargo fmt`, Clippy with warnings denied, the complete Rust tests with PostgreSQL, web tests,
  Leroboscope type-check/build, Terraform validation, and the container build.

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

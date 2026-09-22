# ADR-018 — Project datasets, curation and training inputs

**Status:** Accepted for the first dataset slice
**Date:** 2026-09-21

## Context

Kratos can run an immutable workload image and preserve its outputs, but today a workload must
package its training data inside that image. That prevents users from uploading or importing a
dataset once, reviewing individual episodes and selecting an exact dataset version when scheduling
several runs. It also makes the dataset used by a run difficult to identify independently of the
container image.

Leroboscope already provides the useful review experience for LeRobot datasets: it understands the
v2 and v3 layouts, reads Parquet, synchronises video, plots joints and replays supported robots in
MuJoCo. Kratos should reuse that viewer rather than create a second, less capable previewer. The
catalogue, authorisation and immutable training identity remain Kratos responsibilities.

## Decision

### Catalogue and immutable versions

Every dataset belongs to one project. A dataset is a named catalogue entry; its versions contain the
content identity used by review and training. Project membership authorises every catalogue,
preview, curation and download operation. An unknown dataset and one outside the caller's projects
return the same response.

A version begins in `draft`. It becomes `ready` only after Kratos has an immutable source identity
and a valid LeRobot metadata summary. A `ready` version is immutable. Replacing any file, changing a
Hugging Face revision, or changing generated metadata creates another version.

The first slice supports two sources:

1. **Hugging Face import.** The user supplies a `namespace/name` dataset repository and an optional
   revision. Kratos resolves a symbolic revision such as `main` to a forty-character commit SHA
   before publishing the version. The symbolic name remains display metadata and never identifies a
   training input.
2. **Direct upload.** The user creates a draft version and declares each file's safe relative path,
   byte length, SHA-256 and media type. Kratos creates one resumable, generation-protected upload
   session per file using the same keyless Cloud Storage signing boundary as job artefacts. The
   browser receives only a one-object session URI. Kratos publishes the version after authoritative
   object metadata matches every declaration and `meta/info.json` has been validated.

Uploaded objects use opaque identifiers under
`v1/projects/{project_id}/datasets/{dataset_id}/versions/{version_id}/files/{file_id}`. A supplied
filename remains metadata and cannot choose a storage key. Dataset uploads reuse the configured
private Kratos object store initially; a separately managed dataset bucket can be introduced when
retention or measured transfer volume requires it.

Each version records a canonical manifest with the source kind, exact source revision where
applicable, ordered files, sizes and SHA-256 digests. The SHA-256 of the canonical manifest is the
version's content identity. Source metadata and validation output are retained separately so their
presentation can evolve without changing the content identity.

### Review and curation

Leroboscope is the LeRobot preview client. Kratos exposes a project-authorised review description
for a version. A public Hugging Face import may use exact-revision source URLs. Uploaded and private
data use short-lived, file-scoped read URLs or a same-origin range proxy. The viewer never receives a
bucket credential or authority outside the selected version.

The first slice records one decision per episode: `included`, `excluded` or `needs_review`, plus an
optional note and the identity and time of the last change. These decisions do not modify the source
Parquet files.

A curator publishes the current decisions as an immutable **dataset view**. The view records the
base version, an ordered inclusion set, explicit exclusions, its creator and a canonical manifest
hash. Subsequent curation creates a new view. A job may select either the complete ready version or a
published view.

Large datasets must not require the browser to download all Parquet content before showing the
catalogue. Kratos stores summary metadata such as episode count, task labels, frame count and known
camera keys. The preview path should use HTTP range reads or precomputed per-episode summaries and
thumbnails. Whole-file browser reads remain acceptable only for explicitly bounded small files.

### Scheduling and worker delivery

A job references an exact `dataset_version_id` and optional `dataset_view_id`. Job creation rejects a
non-ready version, a view from another version, or any input outside the caller's projects. These
references are copied into each attempt and do not follow later catalogue state.

The scheduler assigns the job only to a worker in the same project, as required by ADR-017. Before
starting the workload, the agent stages every selected file into a content-addressed cache, verifies
its digest, and mounts the selected input read-only at `/kratos/inputs/training`. The workload
container receives no object-store credential and retains no network access. A cache entry is usable
only after complete digest verification; partial transfers are never mounted.

The first catalogue and curation change may ship before agent staging. Until staging is released,
the console SHALL identify dataset-backed scheduling as unavailable and the job API SHALL reject a
dataset reference rather than silently run without it.

### Provenance

The job and MLflow run record:

- dataset and version identifiers;
- source kind and exact source revision;
- canonical manifest SHA-256;
- optional dataset-view identifier and manifest SHA-256; and
- the validation report revision used at submission.

The catalogue is authoritative. MLflow presents the lineage but does not grant access or define
dataset retention.

## Initial limits

- A dataset has at most 10,000 files in the first upload profile.
- A relative path is valid UTF-8, at most 512 bytes, and contains no empty, `.` or `..` segment.
- Upload declarations include a positive byte length and a lowercase 64-character SHA-256.
- `meta/info.json` is mandatory before a LeRobot version can become ready.
- Curator notes are at most 2,000 Unicode scalar values.
- Episode indexes are non-negative and lower than the validated episode count.

Deployment policy may lower byte and file-count limits. Large transfers use resumable sessions and
must verify authoritative object metadata before publication.

## Consequences

- A dataset can be uploaded or imported once and reused by exact identity across runs.
- Leroboscope supplies a capable review interface while Kratos remains the trust boundary.
- Curation is reproducible because it creates immutable views rather than rewriting Parquet.
- Private preview and worker staging require short-lived read authority and range-capable access.
- Dataset retention and transfer create storage and egress cost, so the initial deployment uses
  explicit limits and existing budget controls.
- Worker staging is a separate release boundary; catalogue availability cannot imply execution
  support before the agent verifies and mounts inputs.

## Deliberately deferred

Cross-dataset deduplication, arbitrary transforms, automatic quality scoring, collaborative comments,
merging datasets, writable training inputs, peer-to-peer caches, shared SkyPilot pools and public
dataset publishing are deferred. The first viewer remains focused on LeRobot; other formats may add
preview adapters behind the same catalogue contract.

## References

- [Dataset and provenance requirements](../../../requirements/03-datasets-and-provenance.md)
- [ADR-014: Job artefacts in Cloud Storage](014-gcs-job-artefact-transfer.md)
- [ADR-017: Invitations and shared ownership](017-invitations-and-shared-ownership.md)
- [Leroboscope](https://github.com/DanielBryars/leroboscope)

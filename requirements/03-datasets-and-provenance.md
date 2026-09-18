# 03 — Datasets and Provenance

[Overview](README.md) · Version 0.5

## Dataset management

| ID | Requirement |
|---|---|
| DAT-001 | Users SHALL be able to browse authorised dataset versions and inspect metadata and validation results. |
| DAT-002 | Each dataset version SHALL have a stable identifier and verifiable content manifest. |
| DAT-003 | Published versions SHALL be immutable; content changes SHALL create a new version. |
| DAT-004 | The platform SHALL validate datasets against the selected workload's requirements. |
| DAT-005 | Validation reports SHALL identify failed checks and affected samples. |
| DAT-006 | The platform SHALL prevent training on datasets that fail mandatory validation checks. |
| DAT-007 | Each run SHALL reference the exact dataset version used. |
| DAT-008 | The platform SHOULD record dataset lineage, source versions and transformations. |
| DAT-009 | The platform SHOULD report duplicates, missing values and distribution statistics where applicable. |
| DAT-010 | Dataset access controls SHALL apply to previews, validation reports and underlying data downloads. |
| DAT-011 | Retention and deletion policies SHALL protect dataset versions referenced by retained runs, or explicitly record that the run can no longer be reproduced. |

## Provenance and repeatability

| ID | Requirement |
|---|---|
| REP-001 | Each run SHALL record its code identity, environment identity, dataset version, resolved configuration, seeds and hardware details. |
| REP-002 | Code identity SHALL identify the executed source, including modifications outside the referenced revision. |
| REP-003 | Environment identity SHALL reference an immutable container image and its versioned build specification. |
| REP-004 | Checkpoints, evaluation reports and exported models SHALL reference their originating run. |
| REP-005 | Users SHALL be able to create a new run from a previous run's recorded specification. |
| REP-006 | Missing inputs, revoked access and known compatibility issues SHALL be reported before repeated execution. |
| REP-007 | The platform SHALL NOT imply numerical determinism solely because recorded inputs have been reproduced. |
| REP-008 | The platform SHOULD record determinism settings and known sources of nondeterminism. |
| REP-009 | Repeating a run SHALL create a new execution and accounting record under current access and pricing rules. |

## Remote data and artefact transfer

| ID | Requirement |
|---|---|
| XFR-001 | Authorised workers SHALL obtain versioned inputs and upload outputs through authenticated encrypted transfer endpoints. |
| XFR-002 | Transfers SHALL verify content integrity before inputs are used or outputs are published as complete. |
| XFR-003 | Large transfers SHALL support retry or resume without publishing partial artefacts as valid. |
| XFR-004 | The UI SHALL distinguish a checkpoint saved locally from a checkpoint durably uploaded and available for recovery on another worker. |
| XFR-005 | A job SHALL NOT be marked successfully complete until mandatory outputs and metadata have been durably acknowledged by platform storage. |
| XFR-006 | Worker caching SHOULD reuse verified content by version or hash while preserving project access and retention controls. |
| XFR-007 | Scheduling SHALL account for input accessibility and sufficient local staging capacity. |
| XFR-008 | Transfer progress and failures SHALL be visible separately from training progress. |

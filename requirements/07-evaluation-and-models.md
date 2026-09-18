# 07 — Evaluation and Models

[Overview](README.md) · Version 0.5

## Evaluation and comparison

| ID | Requirement |
|---|---|
| EVA-001 | Successful training SHALL trigger its configured evaluation workflow. |
| EVA-002 | Evaluations SHALL record model or checkpoint identity, dataset version, configuration and resulting metrics. |
| EVA-003 | Reports SHALL display applicable thresholds and pass or fail outcomes. |
| EVA-004 | Evaluation execution failures SHALL be distinguishable from failed acceptance checks. |
| EVA-005 | Users SHALL be able to compare at least two authorised runs by configuration, dataset, metrics and duration. |
| EVA-006 | Comparisons SHALL identify differing evaluation conditions that affect interpretation. |
| EVA-007 | Models SHALL NOT be marked approved when mandatory checks have failed or remain incomplete. |
| EVA-008 | Run comparison SHALL include actual credit cost, allocated compute and observed throughput. |

## Model artefacts

| ID | Requirement |
|---|---|
| MOD-001 | Model entries SHALL link to training provenance and evaluation results. |
| MOD-002 | Users SHALL be able to export supported models to ONNX. |
| MOD-003 | Export validation SHALL compare outputs with the source model using recorded inputs and tolerances. |
| MOD-004 | Latency reports SHALL record hardware, runtime, input shape, batch size and benchmark configuration. |
| MOD-005 | Export completion and validation outcomes SHALL be displayed separately. |
| MOD-006 | Authorised users SHALL be able to download model artefacts and associated reports. |
| MOD-007 | Artefacts SHALL have integrity checksums. |
| MOD-008 | Additional export formats and inference runtimes MAY be supported in future releases. |
| MOD-009 | Model approval SHALL record the actor or automated policy, timestamp and supporting evaluation. |
| MOD-010 | Approval status SHALL NOT imply that a model has been deployed. |

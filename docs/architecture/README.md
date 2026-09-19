# Kratos — Technology Decisions

Status distinguishes a selected direction from a recommendation still under discussion. An accepted decision does not mean it has been implemented or deployed.

| Decision | Status | Record |
|---|---|---|
| GCP control plane; remote GPU workers | Accepted | [ADR-001](decisions/001-gcp-control-plane.md) |
| Terraform for GCP infrastructure | Accepted | [ADR-002](decisions/002-terraform.md) |
| Infrastructure in the Kratos repository | Accepted for the weekend MVP | [ADR-003](decisions/003-repository-boundaries.md) |
| Rust control plane with Axum | Accepted | [ADR-004](decisions/004-rust-control-plane.md) |
| VS Code and Linux dev container | Accepted | [ADR-005](decisions/005-developer-workflow.md) |
| Public HTTPS edge and custom domain | Accepted | [ADR-006](decisions/006-public-edge-and-domain.md) |
| Python worker agent and outbound HTTPS | Accepted | [ADR-007](decisions/007-python-worker-agent.md) |
| Worker-controlled sibling job containers | Accepted | [ADR-008](decisions/008-worker-job-execution.md) |
| Self-hosted Grafana stack, OpenTelemetry and MLflow | Accepted | [ADR-009](decisions/009-observability-and-mlflow.md) |
| SkyPilot as a later cloud-capacity provider | Accepted | [ADR-010](decisions/010-skypilot-boundary.md) |
| Frontend details, database and identity | Open | [Technology options](technology-options.md) |

Each decision record SHALL state its status, context, alternatives, rationale, consequences and conditions for reconsideration. Proposed decisions SHALL NOT be treated as approved implementation constraints.

See the [release plan](../../requirements/09-releases-and-acceptance.md) for sequencing. Requirements describe required behaviour; decision records describe how and why a technology is selected.

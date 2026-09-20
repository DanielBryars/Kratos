# Technology options for discussion

GCP, Terraform, Rust/Axum for the control plane, and the VS Code dev-container workflow are accepted choices. Other recommendations below form a proposed baseline to discuss; naming a candidate does not approve it or imply deployment.

## Decide before implementing R0.1

| Area | Options | Proposed starting point and rationale | Main trade-off |
|---|---|---|---|
| Repository — accepted | One Kratos repository; separate Kratos.Infrastructure | One repository with isolated infrastructure workflows; [ADR-003](decisions/003-repository-boundaries.md). | Revisit when ownership or approval boundaries change. |
| Control plane — accepted | Rust/Axum; Kotlin/Ktor; C#/ASP.NET Core; TypeScript; Python | Rust/Axum selected; [ADR-004](decisions/004-rust-control-plane.md). | Agent and training languages remain separate choices. |
| Web interface | React/TypeScript SPA; server-rendered UI | React/TypeScript for the interactive fleet and experiment UI. | Separate frontend tooling versus a simpler server-rendered first release. |
| Developer workflow — accepted | VS Code dev container; RustRover; native tools | VS Code with a Linux dev container; [ADR-005](decisions/005-developer-workflow.md). | Real GPU tests still require the host execution environment. |
| Cloud runtime | Cloud Run service; Compute Engine VM; GKE | Cloud Run for the HTTP control plane. R0.1 does not need a continuously running scheduler. | VM offers process control with patching duties; Kubernetes adds cluster operations before a concrete need. |
| Metadata — accepted | PostgreSQL; Firestore | Cloud SQL for PostgreSQL with automatic IAM database authentication; [ADR-011](decisions/011-metadata-and-human-identity.md). | Starts as a deliberately enabled, zonal development instance; scale and availability follow evidence. |
| CI/CD | GitHub Actions; Cloud Build | GitHub Actions with GCP Workload Identity Federation for review-to-deployment traceability. | GCP-native build execution is an alternative; do not add two overlapping pipelines without a reason. |
| Secrets | Secret Manager; self-managed Vault | Secret Manager for the control plane; scoped agent credentials rather than direct broad vault access. | Dynamic credential needs may later justify another component. |
| Human identity — accepted | Managed OIDC provider; self-hosted identity service | Identity Platform with Google sign-in and Kratos-owned roles; [ADR-011](decisions/011-metadata-and-human-identity.md). | Provider login authenticates a person; API authorisation remains in Kratos. |
| Agent protocol — accepted | Outbound HTTPS polling; WebSocket; gRPC streaming | HTTPS enrolment and periodic heartbeat for R0.1; [ADR-007](decisions/007-python-worker-agent.md). | Polling has request overhead; persistent connections add lifecycle and reconnect complexity. |
| Agent implementation — accepted | Python; Go; .NET | Python for GPU/runtime inspection and ML tooling proximity; [ADR-007](decisions/007-python-worker-agent.md). | The wire protocol remains language-neutral. |
| Windows execution | WSL2 Linux runtime; another supported Linux container stack | Validate WSL2-based GPU execution on the actual two hosts before choosing packaging. | Startup, device access, container-to-container LAN reachability and updates must be tested; no compatibility assumed. |
| Agent packaging | Linux service inside the execution environment; container with constrained runtime access | Select after a host smoke test and privilege review. | Container packaging does not make access to the container-management socket low privilege. |
| Worker execution — accepted | Docker Engine API; host supervisor; Kubernetes | Agent-controlled sibling containers for the initial trusted hosts; [ADR-008](decisions/008-worker-job-execution.md). | Docker socket access makes the agent a host-administrative component. |
| Observability — accepted | Self-hosted Grafana stack; Grafana Cloud; provider-native tools | Grafana, Prometheus, Loki, Tempo and an OTel gateway on a GCE VM; [ADR-009](decisions/009-observability-and-mlflow.md). | A single VM is operationally simple but initially a single point of failure. |
| Experiment tracking — accepted | MLflow; custom tracking; provider service | Self-hosted MLflow correlated with OTel; [ADR-009](decisions/009-observability-and-mlflow.md). | MLflow tracking metrics and general operational telemetry use different ingestion paths. |

## Decide when the dependent release approaches

| Area | Options and proposed direction | Needed by |
|---|---|---|
| Scheduler runtime | Separate persistent process, initially on Cloud Run worker pools or a small VM; choose based on actual lease and coordination requirements. | R0.2 |
| Work queue | PostgreSQL-backed queue first versus a separate broker; decide from concurrency and delivery requirements. | R0.2 |
| Experiment tracking | MLflow is selected by ADR-009; define workload integration, retention and model promotion policy. | R0.2 |
| Artefacts — accepted | Cloud Storage with per-object signed resumable uploads and worker-local staging; [ADR-014](decisions/014-gcs-job-artefact-transfer.md). | R0.2 |
| Pricing/ETA | Transparent rule-based pricing and historical throughput estimates first; keep assumptions visible. | R0.3–R0.4 |
| Distributed training | PyTorch DDP first; introduce FSDP when workload memory requirements justify it. | R0.6 |
| Cloud GPU provisioning | Evaluate SkyPilot behind the provider boundary in ADR-010; avoid dual ownership of provisioned workers. | R0.7 |

## Discussion order

1. Frontend details and agent language; repository, Rust control plane and developer workflow are accepted.
2. R0.1 frontend component and data-fetching choices.
3. Windows runtime validation, agent packaging and enrolment protocol.
4. CI/CD promotion and Terraform state/bootstrap design.

For each selected option, add an accepted ADR stating the alternatives, reason, costs, security implications and conditions for reconsideration.

## Technical references

- [Cloud Run services and background execution models](https://docs.cloud.google.com/run/docs/overview/what-is-cloud-run)
- [ASP.NET Core fundamentals](https://learn.microsoft.com/en-us/aspnet/core/fundamentals/)
- [FastAPI documentation](https://fastapi.tiangolo.com/)
- [GCP repository boundary guidance](https://docs.cloud.google.com/docs/terraform/best-practices/version-control)
- [GitHub Actions federation with GCP](https://docs.cloud.google.com/iam/docs/workload-identity-federation-with-deployment-pipelines)

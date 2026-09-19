# ADR-002 — Terraform for GCP infrastructure

Status: Accepted

## Decision and rationale

GCP infrastructure SHALL be defined in Terraform. Versioned plans provide reviewable changes, repeatable environment setup and a documented recovery path. Console-only configuration would obscure these properties. Other infrastructure-as-code tools are viable, but adding another tool has no identified benefit for this project.

## Required operating model

- Reusable modules and environment-specific root configurations SHALL be separated.
- Test and production SHALL have separate state and scoped deployment identities; production state SHALL NOT be writable by the test deployment identity.
- Remote state SHALL use a restricted GCS backend with locking and object versioning. State and plan files SHALL be treated as sensitive and SHALL NOT be committed to source control.
- The bootstrap procedure SHALL document how initial state storage and federated CI identity are established before normal automation is available.
- CI SHALL validate Terraform and produce a reviewable plan. Production apply SHALL use the approved revision and plan, with concurrency control and an auditable promotion action.
- Dependency lock files SHALL be committed and Terraform/provider versions constrained.
- Normal deployment SHALL use short-lived federated credentials; long-lived service-account keys SHALL NOT be the default.
- Terraform SHALL manage secret resources and access policy without placing workload secret values in committed configuration. Any design that writes sensitive values to state SHALL explicitly document its protection and retention.
- Every deployed field SHALL have one owner. The service image update path SHALL be defined so application deployment and Terraform do not repeatedly undo one another's changes.
- Terraform SHALL provision infrastructure; database migrations and home-worker host setup SHALL have separate documented procedures.

## Consequences

Bootstrap remains a small, explicit prerequisite. State access requires stronger protection than ordinary code access. Infrastructure and application deployment need separate permissions even if their files share a repository.

## Reconsider when

Provider support or organisational constraints prevent safe operation. Repository layout can change without changing this decision.

## References

- [Root-module structure](https://docs.cloud.google.com/docs/terraform/best-practices/root-modules)
- [Remote state in Cloud Storage](https://docs.cloud.google.com/docs/terraform/resource-management/store-state)
- [Federated pipeline authentication](https://docs.cloud.google.com/iam/docs/workload-identity-federation-with-deployment-pipelines)

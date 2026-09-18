# ADR-001 — GCP control plane with remote GPU workers

Status: Accepted

## Context

Kratos needs a cloud-hosted interface and coordination services that remain available independently of its GPU workers. Workers include Windows machines running Linux containers and native Linux servers. Directly connected machines form compute groups.

## Decision

GCP SHALL host the control plane. Training SHALL initially run on registered external GPU workers, including the two-machine home LAN group. Cloud provider selection SHALL NOT determine the worker operating system or require GPU training to run in the cloud.

## Alternatives and rationale

| Option | Assessment |
|---|---|
| GCP | Selected as a coherent platform for container hosting, storage, secrets and federated deployment identity, with a later path to cloud GPU capacity. |
| AWS | Viable alternative. Switching offers no demonstrated requirement advantage at this stage and would change the deployment and identity implementation. |
| Azure | Viable alternative. Windows workers do not require an Azure control plane; worker portability remains an application responsibility. |

This is a project design choice, not a claim that GCP is universally cheaper or technically superior.

## Proposed service mapping

These are recommendations pending the technology discussion, not separately accepted decisions.

| Capability | Candidate |
|---|---|
| Web interface and HTTP API | Cloud Run service |
| Persistent scheduling | Separate background service; choose its runtime before R0.2 |
| Metadata | PostgreSQL; evaluate Cloud SQL against operational cost |
| Datasets and checkpoints | Cloud Storage with scoped transfers and worker caching |
| Secrets | Secret Manager |
| Container images | Artifact Registry |
| CI/CD | GitHub Actions using Workload Identity Federation |
| Experiments | MLflow backed by PostgreSQL and object storage |

## Consequences

- Control-plane availability is separate from worker availability.
- Kratos owns enrolment, scheduling, leases and accounting for external workers.
- Dataset downloads and checkpoint uploads introduce transfer latency and potential network charges.
- Distributed collectives use the group's validated network rather than the cloud API.
- GCP-specific identity and infrastructure definitions are accepted dependencies; training containers and worker protocols remain portable.
- A later release can provision bounded Spot GPU capacity and verify interruption recovery without changing the initial execution model.

## Reconsider when

A concrete hosting, compliance, cost or capability constraint cannot be met economically on GCP.

## References

- [Cloud Run execution models](https://docs.cloud.google.com/run/docs/overview/what-is-cloud-run)
- [Workload Identity Federation for deployment pipelines](https://docs.cloud.google.com/iam/docs/workload-identity-federation-with-deployment-pipelines)
- [Cloud Storage signed URLs](https://docs.cloud.google.com/storage/docs/access-control/signed-urls)
- [Compute Engine Spot VMs](https://docs.cloud.google.com/compute/docs/instances/spot)

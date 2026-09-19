# ADR-003 — Repository boundaries

Status: Accepted for the weekend MVP

## Recommendation

Keep application, worker and infrastructure source in the existing Kratos repository initially. Place Terraform under `infrastructure/`, with separate workflows, state and deployment identities.

```text
Kratos/
  apps/web/
  services/api/
  services/scheduler/
  workers/agent/
  infrastructure/
    bootstrap/
    modules/
    environments/test/
    environments/production/
  .github/workflows/
  requirements/
  docs/architecture/
```

## Options

| Option | Benefits | Costs |
|---|---|---|
| One repository with infrastructure directory | One change can update API, agent contract and cloud resources together; one release history; straightforward local navigation. | Workflow permissions and path ownership must be deliberate; repository access is shared. |
| Kratos plus Kratos.Infrastructure | Independent repository permissions, infrastructure lifecycle and approval policy. | Related changes need coordinated pull requests, cross-repository version contracts and drift management. |

## Why the first option is proposed

The same project currently owns application and infrastructure. The early releases will change both frequently. Separate workflows and identities give useful operational separation without introducing cross-repository coordination yet. Directory ownership is a review mechanism, not a substitute for repository-level confidentiality or deployment IAM.

## Boundary rules whichever option is selected

- Application CI SHALL NOT inherit infrastructure administrator permissions.
- Terraform apply SHALL be serialised per state and separately authorised from ordinary builds.
- Changes affecting shared contracts SHALL run cross-component checks even when path filters limit routine builds.
- Production image promotion and Terraform configuration SHALL have an explicit ownership contract.
- State files and secret values SHALL NOT be stored in either source repository.

## When a separate repository becomes preferable

Use `Kratos.Infrastructure` when infrastructure has different owners, repository access restrictions, approval rules or a lifecycle shared by multiple applications. Preserve module and output contracts so extraction remains practical.

## Reference

[Google Cloud repository guidance](https://docs.cloud.google.com/docs/terraform/best-practices/version-control) recommends basing boundaries on ownership and approval requirements. The recommendation here applies that principle to the current project structure.

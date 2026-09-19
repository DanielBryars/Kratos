# Weekend MVP plan

The weekend objective is R0.1: a deployed cloud control plane showing two real home GPU workers in one compute group. The cut line is strict: training, scheduling, credits and distributed execution do not enter the weekend release.

## Friday night — foundation

- Lock architecture decisions and repository structure.
- Create the Rust/Axum API, React/TypeScript UI and OpenAPI endpoint.
- Add the VS Code dev container, one-action debugging and local PostgreSQL.
- Add build, lint, test and container verification in CI.
- Create the GCP billing account and projects when the account owner is available.

Exit: a clean checkout builds locally, the UI reaches the API, and the production API container passes a smoke test.

## Saturday morning — cloud deployment

- Bootstrap Terraform state and GitHub-to-GCP Workload Identity Federation.
- Provision the minimum GCP services for R0.1.
- Build and publish immutable container images.
- Deploy the control plane and web interface with health checks.
- Add a low cloud budget and visible deployment version.

Exit: a merge can deploy a verified revision to the development environment without a stored service-account key.

## Sunday morning — real workers

- Implement single-use enrolment and revocable worker identity.
- Implement heartbeat and capability reports.
- Install the agent on both home machines.
- Detect actual GPU and runtime capabilities.
- Create the home compute group and show both members in the fleet UI.
- Verify offline, stale, reconnect and revoked states.

Exit: both machines appear from the cloud with actual capabilities and respond correctly to disconnect and reconnect.

## Scope cut order

If time is lost, cut in this order:

1. Visual polish beyond a clear fleet page.
2. Automatic agent updates; retain a documented manual update.
3. Production environment; deploy development cleanly and keep production Terraform ready but unapplied.
4. PostgreSQL persistence only if an explicitly labelled temporary store can survive the demonstration; durable identity remains the preferred requirement.

Do not cut worker authentication, actual GPU detection, immutable build identity, secret hygiene or honest offline/stale status. A simulated worker may aid development but does not satisfy acceptance.

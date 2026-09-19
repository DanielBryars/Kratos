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

## Saturday 14:41 checkpoint

The Friday foundation and Saturday deployment exits are complete. The development service is live at
`https://kratos.bryars.com`, merges deploy through Workload Identity Federation, and all build,
analysis, migration and container checks run in CI.

The server half of Sunday worker registration is also complete: the durable registry schema,
single-use enrolment exchange, scoped worker credentials, capability reports and replay-safe
heartbeats are implemented. The Python agent can now persist its identity securely, enrol from a
mounted one-time credential file and send periodic heartbeats.

The remaining critical path is:

1. Enable the development database cost gate; the implemented deployment job will run migrations
   before updating the service.
2. Configure Identity Platform and deploy the implemented operator path that creates one-time
   enrolment credentials.
3. Publish and install the agent on both GPU machines.
4. Use the fleet console to approve each home worker and add it to the Home compute group.
5. Register the second home machine with the public agent image.
6. Exercise disconnect, reconnect and revocation acceptance checks against both machines.

Visual polish and automatic agent updates are outside the cut line. Production remains unapplied.

## Control-plane checkpoint

The development Cloud SQL instance is enabled and healthy. The deployment pipeline now applies the
schema through the dedicated migration job before releasing the application, and the live readiness
probe reports `database: ready`.

Identity Platform is initialised for the development project, the deployed domain is authorised,
and the browser configuration plus bootstrap operator email are supplied through environment-scoped
GitHub Actions variables. The operator console obtains a Google ID token, while the Rust API
independently validates the token and the Kratos operator role before issuing a short-lived,
single-use worker enrolment credential.

The remaining critical path is:

1. Create the Google OAuth web client and enable the Google Identity Platform provider.
2. Deploy the operator console and exercise one real operator sign-in.
3. Publish and install the agent on both GPU machines.
4. Use the fleet console to approve each home worker and add it to the Home compute group.
5. Register the second home machine with the public agent image.
6. Exercise disconnect, reconnect and revocation acceptance checks against both machines.

## Fleet-console checkpoint

The authenticated fleet API and console now show every registered machine, its current capabilities,
last heartbeat, derived `online`, `stale`, `offline` or `never_seen` connectivity, durable worker state
and compute-group memberships. An operator can approve a worker and place it into a named group in one
audited transaction, quarantine it, or revoke it and its active credential. THESHED2 can therefore be
placed into the Home group from the deployed console; the remaining R0.1 hardware task is registering
the second home machine and recording the two-host acceptance evidence.

# ADR-012 — Worker federation and registration

## Status

Accepted.

## Context

Kratos workers can be long-lived home machines, manually rented GPU hosts, or temporary capacity
created through a provider. Every worker connects to the public control plane over outbound HTTPS.
An operator needs a convenient way to recognise and approve a machine without treating a hostname
or a user-entered identifier as proof of identity.

## Decision

An interactively installed agent SHALL generate an Ed25519 key pair and a random instance UUID on
first start. The private key SHALL remain in the agent state volume with the worker credential. The
agent SHALL submit its public key, advertised capabilities and display name as a pending registration
request. The server and agent SHALL independently derive the same short comparison code from the
public key.

The operator console SHALL show pending machines and their comparison codes. An operator SHALL
compare the console code with the code printed by the agent before approval. The comparison code is
not an authentication secret. After approval, the server SHALL issue a random challenge. The agent
SHALL sign the versioned claim message and the server SHALL verify possession of the registered
private key before returning a scoped worker credential.

Pending requests SHALL expire, SHALL be deduplicated by agent instance and key, and SHALL be bounded
to protect persistent storage. Approval and rejection SHALL be audited. Registration SHALL NOT open
an inbound port on the worker or grant compute-group membership.

The existing short-lived, single-use enrolment credential SHALL remain available for automation.
Capacity created by Kratos through a cloud provider MAY use that flow because the provisioner can
deliver the bootstrap credential directly. Independently started machines SHALL require operator
approval.

The public agent image SHALL contain no deployment credential. GitHub Actions SHALL publish the
Linux AMD64 image to `ghcr.io/danielbryars/kratos-agent` with immutable commit and release tags,
provenance and an SBOM. Package visibility SHALL be public.

## Execution environments

Kratos SHALL distinguish worker registration from workload execution:

| Environment | Planned execution adapter |
|---|---|
| Trusted home machine or Linux VM | Agent controls constrained sibling containers through the Docker Engine API. |
| Container-only rented GPU such as a Runpod Pod | One assignment runs within the provisioned worker container. |
| Provider-created VM | The normal sibling-container adapter is installed with the agent. |

The registration protocol can support all three environments. General workload execution begins in
R0.2. Provider provisioning, price ingestion and automatic teardown remain R0.7 scope. SkyPilot may
implement the provisioning boundary but SHALL NOT become authoritative for Kratos identity,
scheduling, budgets or job state.

## Consequences

- Starting a home or manually rented worker requires no copied bootstrap secret.
- A copied UUID, hostname or confirmation code cannot claim an approved worker identity.
- Loss of the state volume loses the private key and worker credential; the replacement instance
  must be registered again and the old identity revoked.
- Public registration endpoints require rate limiting and monitoring at the application and edge.
- A Runpod Pod can register now, but it cannot execute general training until the container-native
  execution adapter is delivered.


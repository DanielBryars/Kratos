# ADR-019 — How a credential reaches a workload on compute we do not control

**Status:** Proposed
**Date:** 2026-09-29

## Context

Every credential Kratos has placed on a machine so far has been on a machine we or the user
control: THESHED2 in a shed, or Cloud Run. R0.7 changes that. Rented GPU capacity means a workload
running on hardware operated by somebody else, and the questions that were previously about
*process* isolation become questions about *host* trust.

[PR #113](../../runpod-skypilot-feasibility.md) established the specific thing that prompted this.
SkyPilot's RunPod adapter declares `get_credential_file_mounts` returning
`{'~/.runpod/config.toml': '~/.runpod/config.toml'}` — it copies the RunPod API key onto the Pod.
That key can create and destroy Pods across the whole account. On RunPod's Community Cloud the Pod
runs on "peer-to-peer providers", which is to say somebody's own machine; Secure Cloud is
"T3/T4 data centers", a different proposition but still not ours.

So the credential that *buys capacity* would sit on the capacity, readable by the workload and, on
shared hardware, potentially by whoever runs the host. That is the pattern this ADR exists to
forbid before anyone builds on it.

Kratos already has the shape of the answer. [ADR-016](016-worker-telemetry-credential.md) decided
that *"a worker never holds a telemetry credential of its own"*, and instead receives a short-lived
scoped token minted by the control plane. That reasoning generalises, and this ADR generalises it.

### What the delivery mechanisms actually cost

Recording this because the choice is usually made by habit rather than on the merits.

**Baked into an image.** Layers are immutable and widely distributed: deleting the file in a later
layer leaves it readable in the earlier one, and `--build-arg` values persist in image history. A
credential in an image is a credential given to everyone who can pull it, forever.

**Environment variables.** The default nearly everywhere, and weak. Readable through
`/proc/<pid>/environ`, **inherited by every child process** whether or not it needs them, and prone
to surfacing in crash dumps, error reporters, orchestrator APIs and any log line that dumps the
environment. Set once at start, so no rotation.

**Files on tmpfs.** Better: not inherited, permissionable to one uid, rewritable for rotation if the
reader re-reads, and never written to disk. This is why a mounted secret beats an environment
variable even when both come from the same source.

**Fetched at runtime against a workload identity.** Better again, because no durable secret sits in
the deployment description. The bootstrap problem moves to the platform proving who the workload is.

**No reusable secret at all.** Best: a short-lived, audience-scoped token that a stolen copy cannot
replay elsewhere or later. This is what Kratos already does for IAP and for worker telemetry.

### Two distinctions that decide most cases

**Bearer versus bound.** A bearer credential authorises whoever holds it; possession *is*
authorisation. A bound credential requires proving possession of a key, so a copy is inert. This is
not abstract for us: the dataset preview capability is a bearer token, which is exactly why *bind
preview reads to caller identity* is an open item in
[manual-takeover.md](../../manual-takeover.md) — a leaked token works for the holder until it
expires, and the audit trail names the session rather than the person.

**Ambient authority.** Instance metadata services hand the host's identity to any process that can
make an HTTP request, which is why a server-side request forgery bug so often becomes cloud
credentials. Anything that grants authority by *location* rather than by presenting a credential
deserves suspicion.

## Decision

### 1. The credential that provisions capacity never reaches the capacity

The control plane holds the provider credential. A provisioned machine is never given a credential
that can provision, terminate, or enumerate other machines.

This is the rule the RunPod finding violates, and it is the one that matters most: a workload that
can mint capacity can mint capacity for its own purposes, destroy another tenant's, or pivot to
whatever else that key reaches. Since SkyPilot's adapter performs the mount itself, using SkyPilot's
own launch path to run a Kratos workload is **not** compatible with this rule as it stands.

### 2. A workload receives only short-lived, audience-scoped, attempt-scoped credentials

Following ADR-016, anything a workload needs to talk to is reached with a token that:

- expires in minutes, not for the life of the machine;
- names a single audience, so it is useless against any other endpoint;
- is scoped to one attempt, so its blast radius is one job;
- and is minted by the control plane, not stored on the machine.

### 3. We assume the host can read everything on the machine

Not that the operator is hostile — that the property we need must not depend on their goodwill.
Anything placed on rented capacity is treated as disclosed to the host as well as to the workload.
It follows that no credential with authority beyond a single attempt may be placed there, and no
user data may be placed there that we would not accept disclosing, until a host-trust decision says
otherwise.

### 4. Secure Cloud is selected explicitly, never by default

Where a provider distinguishes vetted hardware from peer-supplied hardware, the vetted tier is named
explicitly in configuration. On RunPod that selector is the third field of an instance-type string
(`1x_RTX5090_SECURE`), which is precisely the kind of thing a copied example gets wrong silently.

### 5. Experiment credentials are created for the experiment and revoked after it

A provider key used for a learning experiment is created for that experiment, held only by the
operator running it, and revoked when it finishes. It is not the account's main key and it does not
outlive the run.

### 6. Prefer public images for anything on rented capacity

Pulling a private image requires giving the provider a registry credential — on RunPod,
`create_container_registry_auth` stores a username and password in the RunPod account. A public
base image removes that exchange entirely. Where a private image is genuinely required, the registry
credential must be a deploy token scoped to read one repository, never a personal or org-wide token.

## Consequences

**The first RunPod experiment stays deliberately credential-free.** A public base image, synthetic
data generated on the Pod, a throwaway provider key held only by the operator, and no Kratos
credential of any kind on the machine. That experiment therefore proves provisioning and teardown
and proves nothing about integration — which is the honest scope for it anyway.

**Real integration needs the attempt-scoped token path before it needs anything else.** A workload
on rented capacity must be able to report results, observations and artefacts without holding
anything durable. ADR-016's mechanism is the model; whether it is reused or extended is an
implementation question this ADR does not settle.

**SkyPilot's launch path cannot be used as-is for Kratos workloads.** Rule 1 rules it out while the
adapter mounts the provider key into the Pod. Provisioning through a path we control — or upstream
change — is required first.

**This is a constraint on the capacity boundary, not a change to it.** The merged boundary
(PR #105) already keeps provisioning in the control plane, which is the right side of rule 1. What
this ADR adds is that the boundary must never be satisfied by handing a provider credential to a
worker for convenience.

## What this does not decide

- **Whether we use RunPod at all**, or with what provider abstraction. That is R0.7 design and a
  spend decision.
- **How an attempt-scoped credential reaches a Pod that has no Kratos agent on it.** #113 records
  that our current agent cannot run there as-is; until the execution model is settled, the delivery
  mechanism cannot be.
- **The host-trust question for user data.** Rule 3 says what to assume; it does not decide what we
  would then be willing to run on rented capacity, which needs a decision about whose data it is.
- **Anything about existing credentials.** No rotation, revocation or change to THESHED2, IAP,
  MLflow or worker credentials is proposed here.

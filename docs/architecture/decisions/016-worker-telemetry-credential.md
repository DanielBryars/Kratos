# ADR-016 — How a worker authenticates to the telemetry gateway

**Status:** Proposed
**Date:** 2026-09-20

## Context

[ADR-009](009-observability-and-mlflow.md) requires that "worker telemetry SHALL use a scoped,
revocable credential and encrypted transport", and names `otel.kratos.bryars.com` as
*authenticated* OTLP ingestion. It does not say how.

[ADR-015](015-job-telemetry-without-job-network.md) made a worker-local collector with a durable
queue a prerequisite rather than a refinement, and deferred this decision explicitly: "Because the
worker-local collector is now a prerequisite, that decision blocks implementation of this one."
This ADR is that decision.

Nothing authenticates today, and the infrastructure says so rather than pretending otherwise.
`enable_otlp_ingress` defaults to false, with the reason recorded in
`infrastructure/observability/variables.tf`: enabling it "would expose an unauthenticated
ingestion endpoint to the internet". The OTLP backend is deliberately **not** behind Identity-Aware
Proxy, because IAP authenticates humans and this is a machine-to-machine path.

Four facts about the system as it stands constrain the answer.

**A worker's existing credential is long-lived and expensive to verify.** It is a `kwc_` random
secret stored as an Argon2id verifier; `worker_credentials.expires_at` is nullable and production
code never sets it. Verification costs an Argon2 hash (19 MiB, t=2) behind a per-credential rate
limit, and revocation is immediate because it is a database read on every request.

**The control plane already mints short-lived scoped grants.** ADR-014's upload path is the
precedent: the control plane authenticates the worker, then hands back a credential scoped to
exactly one object with an explicit expiry, and the worker never holds the underlying authority.

**Telemetry must not make the control plane a dependency of itself.** ADR-015 requires that a
telemetry failure never interrupt a run. A gateway that called the control plane to check every
push would put the control plane on the path of every observation and make its outage a telemetry
outage, at a request rate far above anything else the worker does.

**The gateway is the OpenTelemetry Collector contrib distribution**
(`otel/opentelemetry-collector-contrib`, pinned by digest in `observability/compose.yaml`), so its
authentication extensions are available without changing what is deployed.

## Decision

### A worker never holds a telemetry credential of its own

The gateway SHALL NOT accept the worker credential, and a worker SHALL NOT be issued any
long-lived secret that authorises telemetry ingestion. A credential that lives on a worker
indefinitely is one that leaves with the machine, and these machines are in people's homes.

### The control plane issues a short-lived telemetry token

The control plane SHALL mint a **telemetry token**: a JWT signed with a Kratos-owned key, carrying

| Claim | Value |
|---|---|
| `iss` | the control plane's public origin |
| `aud` | the telemetry gateway's origin |
| `sub` | the worker identifier |
| `exp` | at most **15 minutes** after issue |
| `kratos.project` | the owning identity |
| `kratos.streams` | the observation streams this token may write to |

It SHALL be returned on the heartbeat the worker already sends, not from a new endpoint. The
heartbeat is authenticated, rate-limited and happens every thirty seconds, so a token issued on it
is refreshed roughly thirty times within its own lifetime and costs no additional authentication.
A worker that cannot heartbeat stops receiving tokens, which is the same condition under which it
stops receiving work.

`kratos.streams` is what makes the token *scoped* rather than merely *identified*. A token carrying
only worker and project identity says who is speaking, not what they may speak about, and would
authorise writing to any stream including another attempt's.

**It names a set, not one stream, and that is a correction rather than a convenience.** The first
version of this decision scoped a token to the single attempt a worker currently held. That is
unimplementable against ADR-015's durable collector queue, and the failure is worse than the gap it
was closing. The queue outlives the attempt that filled it: telemetry for attempt A can still be
waiting when attempt B begins, at which point the only token available names B, so A's records are
either misattributed to B or refused. Once A has ended, no token naming A is ever issued again, so
that queue can never drain. It would have worked on an idle worker and failed on a busy one.

The control plane SHALL therefore include every stream the worker currently holds **and** the
most recently ended ones, bounded by **both** a count and an age:

| Bound | Value | Why this one binds |
|---|---|---|
| Streams named in a token | at most **16** | Binds when attempts are short and numerous |
| Age of an ended attempt | at most **24 hours** | Binds when attempts are long and few |

Whichever is smaller applies. A worker with no current attempt and nothing inside those bounds
SHALL receive no telemetry token at all.

**The count exists because the collector's queue is capacity-bounded, not age-bounded.** A
persistent queue holds a number of batches; it has no notion of how old they are, so "as long as
the queue might hold data" is not a duration anyone can compute. A worker running many short
attempts can therefore have data from far more streams in its queue than a worker running one long
one, and an age bound alone would have silently stopped covering it. The count is what makes the
rule hold at both ends.

**The queue SHALL be sized to fit the bound, not the other way round.** ADR-015 caps a worker at
64 MiB of forwarded bytes per attempt, so a queue capacity above sixteen attempts' worth can hold
records this token can never authorise. The deployment SHALL configure the collector's queue
capacity below that, and SHALL treat the two numbers as one decision: raising the queue without
raising the count reintroduces exactly the undrainable queue this section exists to prevent.

This has a property worth stating plainly: **no old token ever needs to be retained.** Whatever
token is current authorises the whole of what the queue may still hold, so a retry after a restart
works with the token the agent has now rather than one it had to keep. That is what lets the
loopback proxy hold no state.

Records still queued after their stream leaves the window are refused and lost. That is bounded,
visible in the collector's own telemetry, and better than the alternatives: a window without a
limit, or credentials retained for work that finished.

The token SHALL authorise **ingestion only**. There is no telemetry read path for a worker, and the
gateway SHALL NOT accept a telemetry token on any route but OTLP ingestion.

### The gateway verifies it offline, and identity comes from the claims

The gateway SHALL verify the signature, `iss`, `aud` and `exp` with the Collector's `oidc`
authenticator extension. Verification SHALL NOT call the control plane per request: that is what
keeps the control plane off the telemetry path, so the gateway can validate every push while the
control plane is unavailable and an observation is never lost because it was busy.

**Publishing the keys is a contract, not a URL.** The `oidc` extension performs OpenID Connect
discovery against `issuer_url` by default; a JWKS address alone does not satisfy it. The control
plane SHALL therefore serve a discovery document at `{iss}/.well-known/openid-configuration`
carrying at least `issuer`, `jwks_uri`, `id_token_signing_alg_values_supported` and
`response_types_supported`, with `issuer` exactly equal to the token's `iss`, and SHALL serve the
JWK Set at `jwks_uri`. Both SHALL be publicly readable and cacheable; neither carries a secret.

Discovery happens when the extension starts. A gateway that cannot reach the discovery document at
startup therefore fails to start rather than accepting unverified data, which is the behaviour to
want. The deployment SHALL NOT place the discovery document behind IAP, because the gateway is not
a human.

**Where a public discovery document is unacceptable**, the extension's `public_keys_file` mode
SHALL be used instead: the JWK Set is distributed to the instance as a file and discovery is
disabled. That trades an endpoint for a distribution and rotation procedure, and this decision
does not choose it by default because a rotation that has to reach a file on a VM is a rotation
that will one day not reach it.

**A batch SHALL carry records for exactly one stream.** One authorisation decision covers one
request, so a request mixing streams could be neither accepted nor refused as a whole. The
collector's batching SHALL be keyed so that this holds.

**The check itself needs a component that does not exist yet, and this decision names it rather
than implying one.** The `oidc` authenticator validates a token and `attributes/from_context`
copies claims onto data; neither compares a claim against the stream inside an OTLP request, and
assuming they did was an error in an earlier draft of this decision.

A **telemetry admission service** SHALL sit in front of the gateway's OTLP receiver and perform,
per request: verification of the token against the published keys; extraction of the stream the
request's records carry; refusal unless all records agree on one stream; and refusal unless that
stream is in `kratos.streams`. Only then does it forward to the collector, whose receiver SHALL be
bound so that nothing can reach it except through this service.

It SHALL refuse with an HTTP status the sender's exporter treats as retryable or permanent as
appropriate, and SHALL NOT acknowledge a request it refused. That is the property the whole
arrangement rests on: an unacknowledged push stays in the worker's queue, so a refusal caused by a
stream that has aged out of the token is visible as a retry that keeps failing rather than as data
that quietly vanished.

It is a small service and it holds no state. It is, however, ours to write and operate, and that
cost belongs in this decision rather than in the surprise of discovering the Collector will not do
it.

**Identity SHALL be derived from the validated claims, never from what the worker sent.** The
gateway SHALL overwrite the worker and project resource attributes on every accepted request
from `auth.claims.*`, using the Collector's `from_context` attribute source, and SHALL accept the
request's stream only if it is one of `kratos.streams`. A stream cannot simply be assigned from the
claims now that they name a set, so it is checked against them instead. A resource
attribute a worker supplies is an assertion by the sender and SHALL NOT establish authorisation or
attribution; only a claim the gateway verified may do that.

### Revocation is by expiry, and that is a deliberate limit

Revoking a worker SHALL stop token issuance immediately, because issuance happens on an
authenticated heartbeat that already re-reads revocation, expiry and status from the database.

A token already issued SHALL remain valid until it expires. **A revoked worker can therefore
continue to send telemetry for up to fifteen minutes.** That is accepted, and it is accepted for a
specific reason rather than for convenience: the token authorises appending observations to that
worker's own streams and nothing else. It grants no read access, no job assignment, no artefact
authority and no ability to affect execution. The worst a revoked worker can do with it is write
misleading telemetry about itself, bounded by the collector's own ingestion limits, for one token
lifetime.

Immediate revocation would require either a per-request check against the control plane, which this
decision rejects above, or a revocation list the gateway polls, which is a cache with the same
staleness problem and more moving parts. Where immediate revocation genuinely matters — job
assignment, artefact upload, database access — the control plane already provides it.

### The token reaches the gateway through a loopback proxy

A short-lived token has to be replaced without restarting the collector, because a restart discards
the durable queue ADR-015 depends on. The Collector's `bearertokenauth` extension cannot do this:
its `filename` option parses a token from a file, and the extension is documented as *static* token
authentication. Nothing in it promises to notice the file changing. An earlier draft of this
decision assumed it did, which would have produced a stack that worked for fifteen minutes and then
stopped, in a way no test of the happy path would catch.

The agent SHALL therefore run a **token-injecting proxy** on loopback. The collector exports OTLP to
the proxy with no credential of its own; the proxy reads the current token **per outbound request**
from the file the agent replaces atomically, sets the `Authorization` header, and forwards to the
gateway over HTTPS.

The proxy SHALL return the gateway's status to the collector rather than absorbing it. That is the
property that matters: the collector's durable queue stays authoritative, so a rejected or failed
push is retried by the queue that ADR-015 made the acknowledgement boundary, and the proxy holds no
state of its own that could disagree with it.

The token SHALL be written with `0600` permissions to the agent's private state directory and
replaced atomically. The agent SHALL NOT log it, and it SHALL NOT be written into the collector's
configuration, an image, or Terraform state.

### Signing, keys and rotation

Tokens SHALL be signed with **RS256**, and the header SHALL carry a `kid` matching a key in the
published JWK Set. RS256 is chosen because it is the algorithm every JWT verifier supports,
including the one behind the `oidc` extension; a faster curve is not worth a compatibility question
on a path whose whole purpose is to be verified by someone else's code. The discovery document's
`id_token_signing_alg_values_supported` SHALL list exactly the algorithms in use, and the gateway
SHALL reject any token whose `alg` is not among them. `alg: none` SHALL be rejected unconditionally.

The private key SHALL live in Secret Manager and be read by the control plane at startup. It SHALL
NOT appear in an image, in Terraform state, in logs, or in any response. The public JWK Set carries
no secret and is served publicly.

**Rotation SHALL publish before it signs.** A new key is added to the JWK Set and allowed to
propagate for at least the JWKS cache lifetime before any token is signed with it; otherwise a
gateway holding a cached set rejects every token minted in the gap. A retired key SHALL remain in
the set for at least the maximum token lifetime plus that same cache lifetime, so tokens already
issued under it continue to verify until they expire. Both keys are valid during the overlap, which
is the point of it.

### When the control plane is unavailable

Past fifteen minutes with no reachable control plane, a worker holds no valid telemetry token and
the gateway SHALL reject its pushes. Nothing about the job changes: ADR-015 forbids telemetry from
interrupting execution, the agent keeps supervising, the lease is enforced locally, and the
control-plane observation path is a different sink with its own durable spool.

Rejected pushes remain the collector's responsibility, held in its persistent queue and retried
under its own limits until they succeed or that queue's bounds discard them. A worker that has been
unable to reach the control plane for fifteen minutes has usually also stopped receiving work, so
in practice the telemetry it cannot send is telemetry about a job that is finishing or already
finished.

### Transport

The collector SHALL reach the gateway over HTTPS through the existing load balancer. The gateway
SHALL NOT accept unencrypted ingestion from outside the instance's own network.

The agent-to-local-collector hop carries the same token but does not leave the worker; it is a
loopback or private-bridge connection on a machine the agent already trusts enough to run the
workload.

### The gate stays shut until this exists

`enable_otlp_ingress` SHALL remain false, and the Terraform root SHALL refuse an enabled plan
unless the gateway is configured with an authenticator. An unauthenticated ingestion endpoint on
the public internet is worse than no telemetry, because it accepts anyone's data and bills for
storing it.

## Alternatives

| Option | Assessment |
|---|---|
| A single shared bearer token for all workers | One secret on every machine, revocable only by rotating every worker at once, and it identifies nobody. The gateway could not attribute or limit by tenant. |
| The worker's existing `kwc_` credential | Puts a long-lived credential into a collector's configuration, and makes the gateway either verify Argon2 per push or call the control plane per push. The first is expensive by design; the second is what this decision rejects. |
| Mutual TLS with a per-worker client certificate | Genuinely strong and needs no token plumbing, but it adds a certificate authority, issuance and a renewal path to a system whose whole worker-enrolment story is already built around bearer credentials. Worth revisiting if workers ever need to authenticate to something other than Kratos. |
| Proxy OTLP through the control plane | Reuses the existing authentication exactly, but puts every observation through Cloud Run, which is the argument ADR-014 already made against routing artefact bytes that way. It also makes a control-plane outage a telemetry outage. |
| An opaque token the gateway introspects | Allows immediate revocation, at the cost of a control-plane call per push — the dependency this decision exists to avoid. |
| `bearertokenauth` reading the token file directly | What an earlier draft assumed. The extension is documented as static token authentication and does not promise to re-read the file, so the stack would work until the first token expired. Rejected on the documentation rather than on taste. |
| Restarting the collector on each rotation | Removes the proxy, but discards the durable queue every fifteen minutes, which is the one thing ADR-015 made the acknowledgement boundary. |
| A token scoped only to the worker | Simpler to mint, but it authorises writing to any stream, including another attempt's. Identity is not scope. |
| A token scoped to one stream | What the first version decided. Unimplementable against a durable queue that outlives its attempt: telemetry for a finished attempt is either misattributed to the current one or refused, and once that attempt ends no token naming it is issued again, so the queue can never drain. Found in review. |
| Retaining each stream's token until its queue drains | Removes the set, but the agent cannot know when the collector's queue has drained — that is the collector's business by ADR-015 — so it would hold credentials indefinitely for work that finished. |
| Stock `oidc` plus `attributes/from_context` for the stream check | What an earlier draft assumed. Those authenticate a token and copy claims onto data; neither compares a claim against the stream inside a request, so the scope would have been documented and unenforced. |
| An age bound alone on the stream set | Simpler, and wrong for a worker running many short attempts: a persistent queue is bounded by capacity rather than age, so the number of streams it can hold is not a function of time. |
| A revocation list the gateway polls | Gains faster revocation than expiry alone, but it is still a cache with a staleness window, and it adds an endpoint, a poller and a failure mode for a token that grants only ingestion. |

## Consequences

- The control plane gains a signing key, a JWKS endpoint and a key-rotation path. The key is
  Kratos-owned rather than a cloud service-account key, so rotation does not touch IAM.
- A key rotation must publish the new key in the JWKS **before** signing with it, or tokens minted
  during the overlap will fail verification at a gateway holding a cached JWKS.
- The gateway's clock matters. `exp` is checked against it, so an instance with a badly wrong clock
  rejects every token or accepts expired ones; the observability instance already runs NTP through
  Container-Optimized OS, and this makes that a dependency rather than a detail.
- The agent gains one more file in its state directory, one more thing to keep out of logs, and a
  loopback proxy process to run and supervise. That proxy is new code on the worker, and it is the
  real cost of this decision: it must be small, must add no state, and must pass the gateway's
  answer back unchanged.
- The gateway gains a **telemetry admission service** in front of its OTLP receiver. This is
  bespoke code we write and operate, not Collector configuration, because no stock component
  compares a claim against the stream inside a request. It is the single largest cost of this
  decision and the one most likely to be underestimated.
- The collector's queue capacity and the stream count stop being independent knobs. Raising one
  without the other reintroduces an undrainable queue.
- ADR-015's OTLP sink becomes implementable, and its second cursor stops being theoretical.
- A revoked worker retains ingestion for up to fifteen minutes, as set out above.
- A worker may write to a recently finished attempt's stream as well as its current one. That is
  the cost of a queue that outlives the work, and it is bounded by the window rather than open.
- The worker-local collector becomes a component the agent must configure and supervise, which is
  not yet built and is the next thing ADR-015's OTLP half needs.

## Deliberately deferred

This decision does not define the worker-local collector's own configuration, the loopback proxy's
or the admission service's implementation, or how either is supervised. It fixes the queue capacity
*relative to* the stream count without choosing the absolute numbers, which belong with the
collector's configuration; nor per-tenant ingestion quotas at the gateway, which the
`kratos.project` claim makes possible but which need their own limits; nor how the gateway's
authenticator is configured in Terraform, which follows once the shape here is accepted.

## Conditions for reconsideration

Reconsider if telemetry ever needs to carry authority beyond appending a worker's own observations,
if workers must authenticate to a service outside Kratos, if a fifteen-minute revocation window
becomes unacceptable for a reason that actually applies to ingestion, or if the control plane's
heartbeat stops being the natural place to hand a worker something short-lived.

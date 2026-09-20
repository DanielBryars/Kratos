# ADR-017 — Inviting a second person, and what ownership means once there is one

**Status:** Proposed
**Date:** 2026-09-20

## Context

The user wants to show Kratos to a curated set of people and give them the same access they have.

The feature sounds like an invitation. Most of it is not. Reading the system as it stands, the
invitation is the small part and the honest difficulty is somewhere else.

**Authorisation identity and ownership are currently the same thing.** `authorize_operator`
resolves the caller to a `human_identities.id`, and that same UUID is then the ownership filter on
everything: `operator.rs:1293` lists jobs `WHERE owner_identity_id = $1`, and `:1346`, `:1399`,
`:853`, `artifacts.rs:283` and `:1819` do the same for their resources. A second person given
`role = 'operator'` would authenticate perfectly and then see **an empty console**, because every
query filters to rows owned by their own new identity. "Same access" is therefore not a permission
to grant; it is an indirection that does not exist yet.

**There is no way to create a second human at all.** The only production insert into
`human_identities` is gated on a normalised comparison against a single configured bootstrap email
(`operator.rs:1455-1462`, `human_auth.rs:76-78`) and hard-codes `role = 'operator'`. The `'member'`
value in the role check is unreachable: nothing creates one, and `:1478` would reject it.

**Identity is keyed by the provider subject, not the email.** ADR-011 is explicit: "Kratos SHALL
key a human identity by the stable provider subject and SHALL NOT use a mutable email address as
its primary identity." An invitation therefore **cannot pre-create an account**, because the
subject is not known until that person signs in. Whatever is issued has to be claimed.

**There is no email capability anywhere in the system** — no library, no provider, no queue, no
Pub/Sub, no Cloud Tasks, no secret, no egress path. The only email Kratos causes today is GCP's own
budget alerting, which is not ours.

**There is no way to remove a human.** `human_identities.disabled_at` is read on every request and
**never written**; there is no revoke endpoint. That is tolerable while exactly one person exists.
It stops being tolerable the moment a second one does.

Finally, one precedent is worth copying rather than reinventing: the `ken_` worker enrolment
credential. It is a single-use, expiring, revocable secret, stored only as an Argon2id verifier,
carrying a non-secret UUID inside the token so lookup is an indexed read rather than a scan,
shown once to its creator, consumed under `SELECT ... FOR UPDATE` with every condition re-checked
inside the transaction, and audited on both creation and consumption. An invitation is the same
shape.

## Decision

### An invitation is a claimable credential, not an account

Kratos SHALL issue an **invitation credential** in the established shape: prefix `kin_`, 32 random
bytes, the wire form `kin_{uuid}_{secret}`, stored only as an Argon2id verifier, single-use,
expiring, and revocable. The row SHALL record who issued it, the scope it grants, its expiry, and
— once used — when and by which identity.

It SHALL be returned in plaintext exactly once, to its creator, and never retrievable again. Losing
it means issuing another, which is already how `ken_` behaves and already how the console treats it.

### Kratos does not send the email

The console SHALL present the invitation as a link for the owner to send themselves.

This is the deliberate scope cut. Sending mail would add an external provider, a secret, a sending
domain with SPF and DKIM, bounce and complaint handling, and an outbound path that is an abuse
surface — for a feature whose purpose is to hand a link to people the owner already knows. The
owner's own mail client does this better, today, with no new dependency. Email delivery MAY be
added later without changing anything decided here, because the credential does not care how it
travelled.

### Claiming binds the invitation to whoever signs in

The invitee opens the link, signs in with Google, and the browser presents **both** the Identity
Platform ID token and the invitation credential to a claim endpoint.

The control plane SHALL, in one transaction: verify the ID token as it does for any request; parse
the invitation's UUID before touching the database; re-read the row `FOR UPDATE` and re-check that
it is neither expired, revoked, nor consumed; create the `human_identities` row for that provider
subject; record the membership below; mark the invitation consumed by that identity; and write an
audit event. A repeated claim SHALL fail with a distinct conflict rather than a generic denial, as
`enrolment_consumed` already does.

The subject that claims an invitation is **whoever signs in**, not whoever the sender had in mind.
The link is the credential. This is the same property `ken_` already has, and it is why expiry is
short and revocation exists.

### Ownership becomes a scope, and this is the substantive change

A new table SHALL map identities to the ownership scope they may act within, and every ownership
predicate SHALL be rewritten from `owner_identity_id = $caller` to membership of the scope that
owns the row.

Attribution SHALL NOT change. Rows continue to record the individual `owner_identity_id` that
created them, audit events continue to name the individual, and no two people share an identity
row. Only *visibility and authority* widen to the scope. That distinction is the whole point: the
alternative — aliasing a second provider subject onto one existing identity — would be a smaller
change that makes every audit event a lie about who did it, on a system whose audit table exists
precisely to answer that.

This rewrite is the risk in this decision, not the credential. A predicate missed in one direction
hides a person's own data; missed in the other, it shows them someone else's. It SHALL therefore
ship with a test that asserts a co-owner sees exactly the set the founding owner sees, resource by
resource, and a test that a non-member sees none of it.

### An invited co-owner has the owner's authority, with one rail

The user has asked for co-owners with the same access, explicitly including revoking workers,
deleting artefacts and issuing further invitations. That is what this grants.

One rail SHALL apply: **the last remaining owner of a scope cannot be removed**, and an owner
cannot remove themselves while they are the last. This prevents a scope becoming permanently
unadministrable, and it removes no authority anyone would want.

Whether a co-owner may remove the founding owner is left to the reviewer. Recommended: yes,
because "the same as me" was the requirement and a special case here creates two classes of owner
that nothing else in the system models.

### Removing a person has to exist before a second one does

Kratos SHALL gain the ability to revoke a membership and to set `human_identities.disabled_at`,
which is read on every request today and written by nothing. Without it, the first invitation is
irreversible, and an irreversible grant of full authority is not a demo feature.

Revocation takes effect on the next authenticated request, because `authorize_operator` reads
`disabled_at` and the membership per request. It does **not** invalidate the Identity Platform
session: that person's browser continues to mint valid ID tokens, and Kratos refuses them. This is
the same trade already made for worker credentials, and it SHALL be stated in the console rather
than left to be discovered.

A pending invitation SHALL also be revocable. The `ken_` precedent has `revoked_at` on the table
and no endpoint that sets it; this decision does not repeat that omission.

### Limits

An invitation SHALL expire within a bounded window, defaulting to the same order as an enrolment
credential rather than days. The number of unconsumed invitations for a scope SHALL be bounded, so
a compromised console session cannot mint an unbounded supply of ways in.

## Alternatives

| Option | Assessment |
|---|---|
| Alias a second provider subject onto the existing identity row | The smallest possible change: no scope, no predicate rewrite. Rejected because every audit event, every `owner_identity_id` and every "who did this" answer would name one person for the actions of several, on a system that keeps an audit table specifically to answer that. |
| Per-resource sharing, as USR-012 eventually wants | The right long-term model, and far more than this needs. An owner showing colleagues their system wants one boundary, not an access-control matrix. |
| Give the invitee the bootstrap operator email | Works today with no code at all, and is the reason this decision is needed: it means sharing a Google account, so there is no second identity, no attribution and no revocation. |
| Kratos sends the invitation email | Deferred, not rejected. It adds a provider, a secret, a sending domain and an abuse surface to a feature that works without any of them. The credential is indifferent to how it travels. |
| Invite by email address, binding the invitation to it | Appealing, and it would stop a forwarded link being claimed by the wrong person. But ADR-011 keys identity by subject precisely because email is mutable, and Identity Platform's verified email is the only thing that could be checked. Worth reconsidering if invitations ever leave a trusted circle. |

## Consequences

- A second person can exist, which nothing in the system currently allows.
- Six or so ownership predicates change meaning. Until they all do, the feature is half-built in a
  way that is invisible: the invitee signs in successfully and sees nothing.
- The console gains an invitation view, a copy-once link, a list of pending invitations and current
  members, and a revoke control for each.
- The web application gains its first URL handling. It has no router today, does not read
  `location` at all, and signs in through a popup; an invitation landing page is new work, and a
  redirect-based sign-in may suit it better than a popup.
- Anyone holding an unexpired invitation link can become a co-owner. That is what an invitation is,
  and it is why they expire, are single-use, and can be revoked before they are used.
- Every co-owner can revoke workers and delete artefacts. That is the access that was asked for; it
  is recorded here so that it is a decision rather than a surprise.
- The audit table becomes genuinely useful for the first time, because there is more than one
  person in it.

## Deliberately deferred

Email delivery. Projects and per-project membership as USR-002 and USR-003 describe them; this adds
one scope, not a project system. Roles below owner — the `'member'` value stays unreachable rather
than being given a meaning it does not yet need. Session invalidation on revocation, which needs
Identity Platform token revocation rather than a Kratos change.

## Conditions for reconsideration

Reconsider if invitations ever go to people outside a circle the owner already trusts, if more than
one boundary is needed and a scope stops being a fair model of a project, if attribution needs to
survive a person being removed, or if anyone asks for an access level between viewer and owner —
at which point the unreachable `'member'` role is where that belongs.

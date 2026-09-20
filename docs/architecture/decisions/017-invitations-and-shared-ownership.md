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

**The secret SHALL travel in the URL fragment, never in the path or the query string.** A fragment
is not sent to the server, so it cannot reach load-balancer logs, application access logs, a
`Referer` header or analytics. A path or query secret reaches all four, and the one in an access
log outlives the invitation by whatever the retention period is.

On landing, the browser SHALL read the fragment into memory and immediately clear it from the
address bar and history with `history.replaceState`, SHALL NOT write it to local or session
storage, and SHALL send it only in the body of the HTTPS claim request after sign-in.

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

### Ownership becomes a project, and this is the substantive change

This SHALL be the first real slice of projects rather than a bespoke scope. USR-002 already
requires every resource to carry an owner **and a project scope**; USR-003 and USR-005 already
require project membership and its revocation. A generic ownership-scope abstraction would be
renamed or wrapped the moment those are built, so it is not introduced.

Kratos SHALL add `projects` and `project_memberships`, backfill every existing owned resource into
one default project, and add `project_id` to owned resources. Every ownership predicate SHALL be
rewritten from `owner_identity_id = $caller` to membership of the project that owns the row.

`owner_identity_id` SHALL be retained on those rows as **individual attribution**. It stops being
the authorisation predicate and becomes the record of who did it. Audit events continue to name the
individual, and no two people share an identity row. Only visibility and authority widen. The
alternative — aliasing a second provider subject onto one existing identity — needs no rewrite at
all and was rejected because it would make every audit event name one person for the actions of
several, on a system whose audit table exists to answer exactly that.

This rewrite is the risk in this decision, not the credential. A predicate missed in one direction
hides a person's own data; missed in the other, it shows them someone else's.

### An invited co-owner has the owner's authority, with one rail

The user has asked for co-owners with the same access, explicitly including revoking workers,
deleting artefacts and issuing further invitations. That is what this grants.

A co-owner MAY remove the founding owner. "The same as me" was the requirement, and a special case
would create two classes of owner that nothing else in the system models.

One rail SHALL apply: **a project cannot be left with no owner.** The count and the revocation
SHALL happen in one transaction, locking the project's membership rows, so that two concurrent
removals cannot each observe a second owner and both proceed. A check outside the transaction would
pass every test written against it and fail exactly once, in production, leaving a project nobody
can administer.

### Removing a person has to exist before a second one does

Without removal the first invitation is irreversible, and an irreversible grant of full authority
is not a demo feature.

**Three separate actions, and conflating them would be a mistake.**

*Revoking a membership* removes one person from one project. It SHALL set `revoked_at` on the
membership row, and every project-authorised request SHALL check it. An identity may later belong
to several projects, so being removed from one says nothing about the others.

*Disabling an identity* is a platform-wide security action on `human_identities.disabled_at`,
which is read on every request today and written by nothing. It remains separate, and this decision
does not make project removal reach for it.

*Revoking a pending invitation* stops a credential being claimed at all. The `ken_` precedent has
`revoked_at` on the table and no endpoint that sets it; this decision does not repeat that
omission.

Revocation of any kind takes effect on the **next authenticated request**, because authorisation
reads the membership and the identity per request. It does **not** invalidate the Identity Platform
session: that person's browser continues to mint valid ID tokens, and Kratos refuses them. This is
the same trade already made for worker credentials, and the console SHALL say so rather than leave
it to be discovered.

### Limits

An invitation SHALL expire within a bounded window, defaulting to the same order as an enrolment
credential rather than days. The number of unconsumed invitations for a scope SHALL be bounded, so
a compromised console session cannot mint an unbounded supply of ways in.

## Acceptance conditions

These are the conditions, not a suggestion of tests. The credential is easy to get right and the
ownership rewrite is not, so most of them are about the rewrite.

- **Backfill.** Every existing owned resource belongs to the default project after migration, and
  the founding owner is its owner. A resource left without a `project_id` is invisible to everyone.
- **Every predicate.** Each ownership predicate is rewritten, enumerated in the pull request, and
  each one is exercised. The list begins at `operator.rs:1293`, `:1346`, `:1399`, `:853`,
  `artifacts.rs:283` and `:1819`, and the review is expected to find more rather than to trust it.
- **Co-owner parity.** A co-owner sees exactly the set the founding owner sees, resource by
  resource, not merely "some jobs".
- **Non-member isolation.** An identity in no project sees none of it, and receives the same answer
  for a resource that exists in another project as for one that does not exist.
- **Claim races.** Two simultaneous claims of one invitation produce one membership and one
  distinct conflict, and a claim racing a revocation never both succeeds and revokes.
- **Concurrent last-owner removal.** Two owners removing each other at the same instant leave
  exactly one owner, proven against the transaction rather than argued.
- **Attribution survives removal.** After a member is revoked, the jobs and artefacts they created
  still name them, and the audit trail still reads correctly.

## Alternatives

| Option | Assessment |
|---|---|
| Alias a second provider subject onto the existing identity row | The smallest possible change: no scope, no predicate rewrite. Rejected because every audit event, every `owner_identity_id` and every "who did this" answer would name one person for the actions of several, on a system that keeps an audit table specifically to answer that. |
| A generic ownership scope rather than projects | Fewer concepts today, but USR-002, USR-003 and USR-005 already require projects by name, so it would be renamed or wrapped as soon as they are built. Rejected in review. |
| The secret in the link's path or query string | What the first draft implied by saying only "a link". Rejected: both reach load-balancer logs, access logs and `Referer` headers, where the secret outlives the invitation by the log retention period. |
| Per-resource sharing, as USR-012 eventually wants | The right long-term model, and far more than this needs. An owner showing colleagues their system wants one boundary, not an access-control matrix. |
| Give the invitee the bootstrap operator email | Works today with no code at all, and is the reason this decision is needed: it means sharing a Google account, so there is no second identity, no attribution and no revocation. |
| Kratos sends the invitation email | Deferred, not rejected. It adds a provider, a secret, a sending domain and an abuse surface to a feature that works without any of them. The credential is indifferent to how it travels. |
| Invite by email address, binding the invitation to it | Appealing, and it would stop a forwarded link being claimed by the wrong person. But ADR-011 keys identity by subject precisely because email is mutable, and Identity Platform's verified email is the only thing that could be checked. Worth reconsidering if invitations ever leave a trusted circle. |

## Consequences

- A second person can exist, which nothing in the system currently allows.
- Six or so ownership predicates change meaning, and a `project_id` appears on every owned table.
  Until they all change, the feature is half-built in a way that is invisible: the invitee signs in
  successfully and sees nothing.
- Existing data is migrated into a default project. A backfill that misses a table makes those rows
  invisible to everyone, including the founding owner.
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

Email delivery. Per-project *roles* — this adds projects and membership, and every member is an
owner; USR-003's finer permissions come later. Roles below owner — the `'member'` value stays unreachable rather
than being given a meaning it does not yet need. Session invalidation on revocation, which needs
Identity Platform token revocation rather than a Kratos change.

## Conditions for reconsideration

Reconsider if invitations ever go to people outside a circle the owner already trusts, if more than
one boundary is needed and a scope stops being a fair model of a project, if attribution needs to
survive a person being removed, or if anyone asks for an access level between viewer and owner —
at which point the unreachable `'member'` role is where that belongs.

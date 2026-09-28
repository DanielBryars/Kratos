# R0.2 invitation claim exercise

**Status:** Ready to execute; needs two real Google accounts and about ten minutes
**Witness:** the user, who holds both accounts, and who is the only person who can perform this

This is the acceptance the automated suite cannot stand in for. Every invitation test in the
control plane uses a fake Identity Platform verifier: it proves the claim transaction, the
last-owner rail and the project predicates, and it proves nothing about whether a second **real**
Google identity can sign in and be admitted. The one time this project trusted a verifier stand-in
over the live path, the live path failed for a reason no test could have shown — Daniel's Google
token carries no hosted-domain claim, so IAP rejected it as external (PRs #82, #83).

## Requirements

- ADR-017 — an invitation admits a second identity as a full co-owner of one project
- ADR-011 — identity is keyed by provider subject, never by email address
- ACC-018 — the record identifies software, inputs, expected and observed outcomes

## Why a second account cannot simply sign in

`is_bootstrap_operator` compares the caller's email against the single configured bootstrap
address. Any other identity that has never been invited is refused, and no identity row is created
for it. So an invitation is the *only* way a second person enters, which is what makes this
exercise meaningful rather than ceremonial.

## Prerequisites

1. Two Google accounts, in two browsers or one browser and one private window. They must be
   genuinely different accounts, not the same account twice — the second is referred to below as
   the **guest**.
2. The console at `https://kratos.bryars.com`, signed in as the founding operator.
3. Record the deployed control-plane revision before starting, so the evidence names what ran:

   ```shell
   gcloud run services describe kratos \
     --project=kratos-dev-509011 --region=europe-west2 \
     --format="value(status.latestReadyRevisionName)"
   ```

## Exercise

### 1. Issue the invitation

As the founder, open the console, find **People → Share this Kratos**, and press
**Create invitation link**. Record:

- that the wording above the button states the consequence before you commit to it — a co-owner
  can revoke workers and delete artefacts;
- that the link is shown **once**, and that the panel says so;
- the expiry the panel reports.

Copy the link. Do not paste it into anything that stores history you do not control: it is a
bearer capability until it is claimed or expires.

### 2. Confirm the secret is not in the address bar

Open the link in the guest's browser. Before signing in, look at the address bar.

**Expected:** the `#kin_…` fragment is gone, replaced by a clean URL, and the page says you have
been invited and should sign in. The credential is held in memory only.

This is the property worth the most care. A fragment never reaches a server, so it stays out of
load-balancer logs, access logs and `Referer` headers — and the console scrubs it with
`replaceState` *before* the claim rather than after, so it is not left in browser history either.
If the fragment is still visible after the page has loaded, stop and report it.

### 3. Claim as the guest

Sign in with the second Google account.

**Expected:** "Invitation accepted. You are now an owner of this Kratos."

**If it fails**, record the exact message, and check which of these it is:

- *"That invitation has already been used"* — the link was claimed twice; issue another.
- *"could not be used… expired or revoked"* — check the expiry from step 1.
- *A sign-in failure before any Kratos message* — this is the ADR-011 identity path, not the
  invitation. It is the failure mode that bit us before, so capture the browser console and the
  control-plane logs rather than retrying blindly.

### 4. Confirm co-owner parity, resource by resource

Side by side, compare founder and guest. The guest must see **the same set**, not merely
"some things":

- the same workers, with the same states;
- the same jobs, including their artefacts;
- the same datasets and versions;
- both people listed under **Owners**, each seeing themselves marked "(you)".

Parity is the whole promise of ADR-017 — "the same access I have" — and a difference here means
a predicate was missed, which is precisely what the project-scoping work was for.

### 5. Confirm the guest holds real authority

Confirm the selected worker is idle and no jobs are queued. Record its approval and group membership before starting. Have the **guest** quarantine that worker, then return it to service and verify that its original approval and group membership are restored. Do not revoke or re-enrol it.

**Expected:** it works, and the founder's console reflects it. A read-only invitee would be a
different feature from the one that was asked for.

Do not schedule a job during this step if THESHED2 is mid-acceptance for anything else.

### 6. Confirm removal works and the last owner is protected

As the founder, remove the guest under **Owners**.

**Expected:** the guest's next action fails and their console shows nothing of the project.

Then confirm the rail that cannot be tested politely: the founder is offered no "remove me"
control against themselves. The server refuses the last owner independently, so this is belt and
braces, but a console that offered the button and then failed would be worse than one that does
not offer it.

### 7. Confirm attribution survives

Any job or dataset the guest created must still name them after removal, and the audit trail must
still read correctly. Membership is revoked rather than deleted precisely so that "who could see
this, and until when" stays answerable.

## Record

Append the result to this file, naming: the control-plane revision, the date, both accounts by
role rather than address, each step's expected and observed outcome, and anything that had to be
retried. A step that was skipped is recorded as skipped, not omitted.

## Result

*Not yet executed.*

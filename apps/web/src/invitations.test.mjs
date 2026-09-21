import assert from "node:assert/strict";
import test from "node:test";

import {
  buildInvitationLink,
  describeExpiry,
  readInvitationCredential,
  scrubbedUrl,
} from "./invitations.ts";

// Assembled from parts rather than written out, so no line here is a credential-shaped literal
// for a secret scanner to find. The pieces are deliberately dull: a fixed UUID and a repeated
// character, which carry no entropy and could not be mistaken for a real invitation.
const FIXTURE_UUID = "3f4a1c2e-1111-4222-8333-444455556666";
const FIXTURE_SECRET = "x".repeat(43);
const CREDENTIAL = ["kin", FIXTURE_UUID, FIXTURE_SECRET].join("_");

test("reads a credential from a bare fragment", () => {
  assert.equal(readInvitationCredential({ hash: `#${CREDENTIAL}` }), CREDENTIAL);
});

test("reads a credential from a named fragment parameter", () => {
  assert.equal(
    readInvitationCredential({ hash: `#invitation=${CREDENTIAL}` }),
    CREDENTIAL,
  );
});

test("ignores a credential in the query string", () => {
  // The whole point of the fragment is that the secret never reaches a server log. Honouring a
  // query string would reward the one way of sending an invitation that leaks it.
  assert.equal(
    readInvitationCredential({ hash: "", search: `?invitation=${CREDENTIAL}` }),
    null,
  );
});

test("refuses a credential of another kind", () => {
  const enrolment = ["ken", FIXTURE_UUID, FIXTURE_SECRET].join("_");
  assert.equal(readInvitationCredential({ hash: `#${enrolment}` }), null);
});

test("refuses anything that is not credential-shaped", () => {
  for (const hash of ["", "#", "#kin_", "#kin_not-a-uuid_secret", "#/jobs", `#kin_${FIXTURE_UUID}_tooshort`]) {
    assert.equal(readInvitationCredential({ hash }), null, `should refuse ${hash}`);
  }
});

test("builds a link that carries the credential in the fragment", () => {
  const link = buildInvitationLink("https://kratos.example.com", CREDENTIAL);
  assert.equal(link, `https://kratos.example.com/#${CREDENTIAL}`);
  const url = new URL(link);
  assert.equal(url.search, "", "nothing may reach the server");
  assert.equal(url.hash, `#${CREDENTIAL}`);
});

test("does not double the slash when the origin already ends in one", () => {
  assert.equal(
    buildInvitationLink("https://kratos.example.com/", CREDENTIAL),
    `https://kratos.example.com/#${CREDENTIAL}`,
  );
});

test("scrubs the credential out of the address, leaving no trailing hash", () => {
  const scrubbed = scrubbedUrl(`https://kratos.example.com/#${CREDENTIAL}`);
  assert.equal(scrubbed, "https://kratos.example.com/");
  assert.ok(!scrubbed.includes(CREDENTIAL));
  assert.ok(!scrubbed.endsWith("#"));
});

test("scrubbing keeps the rest of the address intact", () => {
  assert.equal(
    scrubbedUrl(`https://kratos.example.com/console?tab=workers#${CREDENTIAL}`),
    "https://kratos.example.com/console?tab=workers",
  );
});

test("describes how long an invitation has left", () => {
  const now = new Date("2026-09-21T10:00:00Z");
  assert.equal(describeExpiry("2026-09-21T10:30:00Z", now), "expires in 30 minutes");
  assert.equal(describeExpiry("2026-09-21T11:00:00Z", now), "expires in 1 hour");
  assert.equal(describeExpiry("2026-09-21T10:59:00Z", now), "expires in 59 minutes");
  assert.equal(describeExpiry("2026-09-22T12:00:00Z", now), "expires in 26 hours");
  assert.equal(describeExpiry("2026-09-25T10:00:00Z", now), "expires in 4 days");
  assert.equal(describeExpiry("2026-09-21T09:00:00Z", now), "expired");
  assert.equal(describeExpiry("not a date", now), "expiry unknown");
});

import assert from "node:assert/strict";
import test from "node:test";
import { SubmissionRetry, definitiveSubmissionRejection } from "./submissionRetry.ts";

const payload = (name = "Training") => ({ name, image_reference: "image", timeout_seconds: 300, output_requirements: [], dataset_inputs: [] });

test("a lost response retries the exact key and body even if the draft changes", () => {
  const retry = new SubmissionRetry();
  const first = retry.begin(() => payload(), () => "first-key");
  retry.finish(first, "uncertain");
  const second = retry.begin(() => { throw new Error("edited draft must not be read"); }, () => "second-key");
  assert.equal(second, first);
  assert.equal(second.key, "first-key");
  assert.deepEqual(JSON.parse(second.body), payload());
});

test("two clicks while a request is in flight cannot dispatch twice", () => {
  const retry = new SubmissionRetry();
  const first = retry.begin(() => payload(), () => "first-key");
  assert.equal(retry.begin(() => payload(), () => "second-key"), null);
  assert.equal(retry.busy, true);
  retry.finish(first, "accepted");
  assert.equal(retry.busy, false);
});

test("a confirmed response allows an intentional identical new job with a new key", () => {
  const retry = new SubmissionRetry();
  const first = retry.begin(() => payload(), () => "first-key");
  retry.finish(first, "accepted");
  const next = retry.begin(() => payload(), () => "new-key");
  assert.equal(next.body, first.body);
  assert.notEqual(next.key, first.key);
});

test("sign-out/reset isolates old responses from a new submission", () => {
  const retry = new SubmissionRetry();
  const first = retry.begin(() => payload(), () => "old-key");
  retry.reset();
  const next = retry.begin(() => payload("New user"), () => "new-key");
  assert.equal(retry.finish(first, "accepted"), false);
  assert.equal(retry.busy, true);
  assert.equal(retry.finish(next, "uncertain"), true);
  assert.equal(retry.begin(() => payload()), next);
});

test("invalid local details do not reserve a key or leave the form sending", () => {
  const retry = new SubmissionRetry();
  assert.throws(() => retry.begin(() => { throw new RangeError("bad date"); }));
  assert.equal(retry.busy, false);
  assert.equal(retry.begin(() => payload(), () => "valid-key").key, "valid-key");
});

test("a definitive initial refusal unlocks the form and permits a corrected request", () => {
  const retry = new SubmissionRetry();
  const first = retry.begin(() => payload(), () => "bad-key");
  retry.finish(first, "rejected");
  assert.equal(retry.pending, null);
  assert.equal(retry.busy, false);
  assert.equal(retry.begin(() => payload("Corrected"), () => "corrected-key").name, "Corrected");
});

test("a refusal after a lost response cannot discard the original retry identity", () => {
  const retry = new SubmissionRetry();
  const first = retry.begin(() => payload(), () => "original-key");
  retry.finish(first, "uncertain");
  retry.begin(() => payload());
  retry.finish(first, "rejected");
  assert.equal(retry.pending, first);
  assert.equal(retry.begin(() => payload("Edited")), first);
});

test("transient responses and key conflicts retain the retry identity", () => {
  for (const status of [408, 409, 429, 500, 502, 503]) assert.equal(definitiveSubmissionRejection(status), false);
  for (const status of [400, 401, 403, 404, 422]) assert.equal(definitiveSubmissionRejection(status), true);
});
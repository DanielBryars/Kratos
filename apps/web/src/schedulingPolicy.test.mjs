import assert from "node:assert/strict";
import test from "node:test";

import {
  buildPolicyUpdate,
  describePolicy,
  describeQueueWait,
  draftFromPolicy,
  queueWaitReason,
} from "./schedulingPolicy.ts";

const now = new Date("2026-09-29T12:00:00Z");
const policy = (max, active) => ({ project_id: "p", max_concurrent_jobs: max, active_jobs: active });

test("unlimited sends an explicit null and a limit sends a positive integer", () => {
  assert.deepEqual(buildPolicyUpdate({ unlimited: true, limit: "4" }), { max_concurrent_jobs: null });
  assert.deepEqual(buildPolicyUpdate({ unlimited: false, limit: " 3 " }), { max_concurrent_jobs: 3 });
});

test("an empty, zero, negative, fractional or oversized limit never becomes a clear", () => {
  for (const limit of ["", "0", "-1", "1.5", "abc", "2147483648"]) {
    assert.throws(() => buildPolicyUpdate({ unlimited: false, limit }), RangeError, limit);
  }
});

test("draft mirrors the stored policy", () => {
  assert.deepEqual(draftFromPolicy(policy(null, 0)), { unlimited: true, limit: "" });
  assert.deepEqual(draftFromPolicy(policy(2, 1)), { unlimited: false, limit: "2" });
});

test("policy summary shows the limit and active count", () => {
  assert.equal(describePolicy(policy(null, 1)), "Unlimited · 1 job active");
  assert.equal(describePolicy(policy(2, 2)), "2 of 2 jobs active");
});

test("a future earliest start explains the wait even when the project is at its limit", () => {
  const job = { status: "queued", earliest_start_at: "2026-09-30T12:00:00Z" };
  assert.equal(queueWaitReason(job, policy(1, 1), now), "earliest_start");
});

test("a due job at the project limit is waiting for a concurrency slot", () => {
  const job = { status: "queued", earliest_start_at: "2026-09-28T12:00:00Z" };
  assert.equal(queueWaitReason(job, policy(1, 1), now), "project_limit");
  assert.equal(
    describeQueueWait("project_limit", policy(1, 1)),
    "waiting for a slot · project concurrency limit reached (1 of 1 job active)",
  );
});

test("below the limit or unlimited, the wait is generic rather than a compatibility diagnosis", () => {
  const job = { status: "queued", earliest_start_at: null };
  assert.equal(queueWaitReason(job, policy(2, 1), now), "eligible_worker");
  assert.equal(queueWaitReason(job, policy(null, 9), now), "eligible_worker");
  assert.equal(describeQueueWait("eligible_worker", policy(2, 1)), "waiting for an eligible worker");
});

test("an unknown policy does not claim either reason", () => {
  assert.equal(queueWaitReason({ status: "queued" }, null, now), "unknown");
  assert.equal(describeQueueWait("unknown", null), "waiting to be scheduled");
});

test("only queued jobs have a queue explanation", () => {
  for (const status of ["assigned", "running", "cancelling", "succeeded"]) {
    assert.equal(queueWaitReason({ status }, policy(1, 1), now), null);
  }
});

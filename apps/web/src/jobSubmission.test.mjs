import assert from "node:assert/strict";
import test from "node:test";

import { buildJobSubmission, DURABLE_TRAINING_PRESET } from "./jobSubmission.ts";

test("pins the durable training preset to an immutable image", () => {
  assert.match(
    DURABLE_TRAINING_PRESET.imageReference,
    /^ghcr\.io\/danielbryars\/kratos-training-example@sha256:[0-9a-f]{64}$/,
  );
  assert.deepEqual(DURABLE_TRAINING_PRESET.output, {
    enabled: true,
    logicalPath: "model.pt",
    role: "model",
    mediaType: "application/x-pytorch",
    maxMiB: 1,
  });
});

test("adds the mandatory durable output contract when enabled", () => {
  const request = buildJobSubmission(" training ", " image@sha256:abc ", 300, {
    enabled: true,
    logicalPath: " model.pt ",
    role: " model ",
    mediaType: " application/x-pytorch ",
    maxMiB: 1,
  });

  assert.deepEqual(request, {
    name: "training",
    image_reference: "image@sha256:abc",
    timeout_seconds: 300,
    output_requirements: [{
      logical_path: "model.pt",
      role: "model",
      media_type: "application/x-pytorch",
      mandatory: true,
      max_bytes: 1_048_576,
    }],
    dataset_inputs: [],
  });
});

test("keeps jobs without durable outputs explicit", () => {
  const request = buildJobSubmission("health", "image@sha256:def", 120, {
    enabled: false,
    logicalPath: "model.pt",
    role: "model",
    mediaType: "application/x-pytorch",
    maxMiB: 1,
  });

  assert.deepEqual(request.output_requirements, []);
  assert.deepEqual(request.dataset_inputs, []);
});

test("adds an exact immutable dataset version and optional curated view", () => {
  const request = buildJobSubmission("training", "image@sha256:def", 120, {
    enabled: false,
    logicalPath: "model.pt",
    role: "model",
    mediaType: "application/x-pytorch",
    maxMiB: 1,
  }, [{
    alias: " training_data ",
    dataset_version_id: "11111111-1111-1111-1111-111111111111",
    dataset_view_id: "22222222-2222-2222-2222-222222222222",
  }]);

  assert.deepEqual(request.dataset_inputs, [{
    alias: "training_data",
    dataset_version_id: "11111111-1111-1111-1111-111111111111",
    dataset_view_id: "22222222-2222-2222-2222-222222222222",
  }]);
});


test("earliest start preserves an offset as the same UTC instant", () => {
  const request = buildJobSubmission("scheduled", "image", 120,
    DURABLE_TRAINING_PRESET.output, [], "2026-10-01T10:30:00+01:00");
  assert.equal(request.earliest_start_at, "2026-10-01T09:30:00.000Z");
});

test("invalid earliest start fails before submission", () => {
  assert.throws(() => buildJobSubmission("scheduled", "image", 120,
    DURABLE_TRAINING_PRESET.output, [], "not-a-time"), RangeError);
});

import assert from "node:assert/strict";
import test from "node:test";

import { buildJobSubmission, DURABLE_TRAINING_PRESET, parseJobParameters } from "./jobSubmission.ts";

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


test("local datetime input uses the browser timezone in summer and winter", () => {
  const old = process.env.TZ;
  process.env.TZ = "Europe/London";
  try {
    for (const [local, utc] of [["2026-07-01T10:30", "2026-07-01T09:30:00.000Z"], ["2026-12-01T10:30", "2026-12-01T10:30:00.000Z"]]) {
      assert.equal(buildJobSubmission("scheduled", "image", 120, DURABLE_TRAINING_PRESET.output, [], local).earliest_start_at, utc);
    }
  } finally {
    if (old === undefined) delete process.env.TZ;
    else process.env.TZ = old;
  }
});

const output = { enabled: false, logicalPath: "", role: "", mediaType: "", maxMiB: 1 };

test("parameters are sent only when there are some", () => {
  const without = buildJobSubmission("Run", "img@sha256:" + "a".repeat(64), 7200, output);
  assert.equal("parameters" in without, false);
  assert.equal(without.timeout_seconds, 7200);
  assert.equal("parameters" in buildJobSubmission("Run", "img", 60, output, [], "", {}), false);
  const withParameters = buildJobSubmission("Run", "img", 60, output, [], "", { epochs: 12, lr: 0.0003 });
  assert.deepEqual(withParameters.parameters, { epochs: 12, lr: 0.0003 });
});

test("the parameters box accepts what the control plane accepts", () => {
  assert.deepEqual(parseJobParameters(""), { ok: true, value: null });
  assert.deepEqual(parseJobParameters("  {}  "), { ok: true, value: null });
  assert.deepEqual(parseJobParameters('{"learning_rate": 0.0003, "epochs": 12, "model.name": "smolvla", "_amp": true}'),
    { ok: true, value: { learning_rate: 0.0003, epochs: 12, "model.name": "smolvla", _amp: true } });
});

test("the parameters box refuses, with a reason, what the control plane would refuse", () => {
  const tooMany = JSON.stringify(Object.fromEntries(Array.from({ length: 65 }, (_, i) => [`p${i}`, i])));
  for (const bad of ["{", "[1, 2]", "null", '"text"', '{"nested": {"a": 1}}', '{"list": [1]}', '{"none": null}',
    '{"1digit": 1}', '{"has space": 1}', JSON.stringify({ ["x".repeat(65)]: 1 }), tooMany,
    JSON.stringify({ big: "x".repeat(4_100) })]) {
    const parsed = parseJobParameters(bad);
    assert.equal(parsed.ok, false, bad.slice(0, 40));
    assert.ok(parsed.error.length > 0);
  }
});

test("a whole number the browser cannot hold exactly is refused rather than rounded", () => {
  const parsed = parseJobParameters('{"seed": 12345678901234567890}');
  assert.equal(parsed.ok, false);
  assert.match(parsed.error, /as a string/);
  assert.equal(parseJobParameters('{"seed": "12345678901234567890"}').ok, true);
  assert.equal(parseJobParameters('{"seed": 9007199254740991}').ok, true);
});

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { artifactRows } from "./artifactPresentation.ts";

const requirements = [
  {
    logical_path: "outputs/model.bin",
    role: "model",
    media_type: "application/octet-stream",
    mandatory: true,
    max_bytes: 2048,
  },
  {
    logical_path: "outputs/metrics.json",
    role: "metrics",
    media_type: "application/json",
    mandatory: false,
    max_bytes: 1024,
  },
];

describe("artifactRows", () => {
  it("uses the newest attempt and reports outputs that were never declared", () => {
    const response = {
      job_id: "job-1",
      artifacts: [
        {
          artifact_id: "new",
          attempt_id: "attempt-2",
          attempt_number: 2,
          logical_path: "outputs/model.bin",
          role: "model",
          media_type: "application/octet-stream",
          mandatory: true,
          max_bytes: 2048,
          byte_length: 512,
          sha256: "b".repeat(64),
          crc32c: "ImIEBA==",
          status: "verified",
          declared_at: "2026-09-20T12:00:00Z",
          verified: {
            storage_generation: 42,
            byte_length: 512,
            sha256: "b".repeat(64),
            crc32c: "ImIEBA==",
            verification_source: "gcs_metadata",
            verified_at: "2026-09-20T12:01:00Z",
          },
        },
        {
          artifact_id: "old",
          attempt_id: "attempt-1",
          attempt_number: 1,
          logical_path: "outputs/model.bin",
          role: "model",
          media_type: "application/octet-stream",
          mandatory: true,
          max_bytes: 2048,
          byte_length: 256,
          sha256: "a".repeat(64),
          crc32c: "AAAAAA==",
          status: "rejected",
          declared_at: "2026-09-20T11:00:00Z",
          verified: null,
        },
      ].reverse(),
    };

    const rows = artifactRows(requirements, response);
    assert.equal(rows[0].artifact?.artifact_id, "new");
    assert.equal(rows[0].artifact?.verified?.storage_generation, 42);
    assert.equal(rows[1].availability, "not_declared");
    assert.equal(rows[1].artifact, null);
  });

  it("distinguishes an unavailable API response from an undeclared output", () => {
    assert.deepEqual(
      artifactRows(requirements, null, true).map((row) => row.availability),
      ["request_failed", "request_failed"],
    );
  });
});

import assert from "node:assert/strict";
import test from "node:test";

import {
  buildDeclaration,
  describeVersionStatus,
  logicalPathFor,
  manifestProblems,
  mediaTypeFor,
  pathProblem,
  uploadProgress,
} from "./datasetUpload.ts";

const HASH = "a".repeat(64);

function picked(relativePath, size = 10, type = "") {
  const name = relativePath.split("/").pop();
  return { relativePath, name, size, type };
}

test("drops the folder the person happened to pick from", () => {
  // Two people downloading the same dataset into differently named folders must declare the same
  // logical paths, or the catalogue would treat them as different datasets.
  assert.equal(logicalPathFor("pick-and-place/meta/info.json"), "meta/info.json");
  assert.equal(logicalPathFor("lerobot-so101-v2/data/episode_0.parquet"), "data/episode_0.parquet");
});

test("keeps a path that already starts at the dataset root", () => {
  assert.equal(logicalPathFor("meta/info.json"), "meta/info.json");
  assert.equal(logicalPathFor("data/episode_0.parquet"), "data/episode_0.parquet");
  assert.equal(logicalPathFor("videos/observation.images.top/episode_0.mp4"),
    "videos/observation.images.top/episode_0.mp4");
});

test("never reduces a flat pick to nothing", () => {
  // Dropping the first segment unconditionally would turn a single picked file into an empty path,
  // which the server rejects for the whole folder.
  assert.equal(logicalPathFor("info.json"), "info.json");
});

test("normalises separators and leading slashes", () => {
  assert.equal(logicalPathFor("/dataset/meta/info.json"), "meta/info.json");
  assert.equal(logicalPathFor("dataset\\meta\\info.json"), "meta/info.json");
});

test("names why a path would be refused", () => {
  assert.equal(pathProblem("meta/info.json"), null);
  assert.match(pathProblem(""), /empty/);
  assert.match(pathProblem("/meta/info.json"), /slash/);
  assert.match(pathProblem("meta//info.json"), /empty segment/);
  assert.match(pathProblem("meta/../../secret"), /leave the dataset/);
  assert.match(pathProblem(`${"a".repeat(513)}`), /longer than 512/);
});

test("supplies a media type where the browser gives none", () => {
  assert.equal(mediaTypeFor(picked("data/episode_0.parquet")), "application/vnd.apache.parquet");
  assert.equal(mediaTypeFor(picked("meta/info.json")), "application/json");
  assert.equal(mediaTypeFor(picked("videos/a/episode_0.mp4")), "video/mp4");
  assert.equal(mediaTypeFor(picked("notes/readme.xyz")), "application/octet-stream");
  assert.equal(mediaTypeFor(picked("meta/info.json", 1, "application/json")), "application/json");
});

test("requires the LeRobot metadata file", () => {
  const problems = manifestProblems([picked("set/data/episode_0.parquet")]);
  assert.equal(problems.length, 1);
  assert.match(problems[0].reason, /meta\/info\.json/);
});

test("accepts a well formed LeRobot folder", () => {
  const problems = manifestProblems([
    picked("set/meta/info.json"),
    picked("set/data/episode_0.parquet", 4096),
  ]);
  assert.deepEqual(problems, []);
});

test("reports every problem, not just the first", () => {
  // Someone fixing an upload wants the list, not one error at a time across several attempts.
  const problems = manifestProblems([
    picked("set/meta/info.json"),
    picked("set/data/empty.parquet", 0),
    picked("set/data/../escape.parquet", 10),
  ]);
  assert.equal(problems.length, 2, JSON.stringify(problems));
  assert.ok(problems.some((problem) => /empty/.test(problem.reason)));
  assert.ok(problems.some((problem) => /leave the dataset/.test(problem.reason)));
});

test("catches two picked files sharing one logical path", () => {
  const problems = manifestProblems([
    picked("set/meta/info.json"),
    picked("other/meta/info.json"),
  ]);
  assert.ok(
    problems.some((problem) => /share this path/.test(problem.reason)),
    JSON.stringify(problems),
  );
});

test("refuses an empty selection", () => {
  assert.match(manifestProblems([])[0].reason, /no files/);
});

test("builds a declaration in the order it was given", () => {
  const files = [picked("set/meta/info.json", 12, "application/json"), picked("set/data/e.parquet", 99)];
  const declaration = buildDeclaration(files, [HASH, "b".repeat(64)]);
  assert.deepEqual(declaration, [
    { logical_path: "meta/info.json", media_type: "application/json", byte_length: 12, sha256: HASH },
    {
      logical_path: "data/e.parquet",
      media_type: "application/vnd.apache.parquet",
      byte_length: 99,
      sha256: "b".repeat(64),
    },
  ]);
});

test("refuses to build a declaration with a hash missing", () => {
  assert.throws(() => buildDeclaration([picked("set/meta/info.json")], []), /exactly one hash/);
});

test("weights progress by bytes rather than by file count", () => {
  // A LeRobot dataset is a few large parquet files among many small ones. Counting files would
  // show 90% while nearly all the bytes were still to go.
  const entries = [
    { byteLength: 1_000_000, state: "pending" },
    { byteLength: 10, state: "verified" },
    { byteLength: 10, state: "verified" },
  ];
  const progress = uploadProgress(entries);
  assert.ok(progress.fraction < 0.01, `expected a small fraction, got ${progress.fraction}`);
  assert.equal(progress.label, "0% uploaded");
});

test("reports completion only when every byte is verified", () => {
  const progress = uploadProgress([
    { byteLength: 100, state: "verified" },
    { byteLength: 100, state: "verified" },
  ]);
  assert.equal(progress.fraction, 1);
  assert.equal(progress.label, "all files verified");
});

test("a rejection is what the label says, not a percentage", () => {
  const progress = uploadProgress([
    { byteLength: 100, state: "verified" },
    { byteLength: 100, state: "rejected" },
  ]);
  assert.equal(progress.label, "1 file rejected");
});

test("an empty upload is not reported as complete", () => {
  assert.equal(uploadProgress([]).fraction, 0);
});

test("says what a version's status means for using it", () => {
  assert.match(describeVersionStatus("uploading"), /not yet selectable/);
  assert.match(describeVersionStatus("failed"), /did not match/);
  assert.equal(describeVersionStatus("ready"), "Ready");
  assert.equal(describeVersionStatus("something new"), "something new");
});

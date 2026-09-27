/// Turning a folder a person picked into a declaration Kratos will accept.
///
/// The rules here deliberately mirror `validate_file_declarations` in the control plane. The
/// server is the authority and rejects anything wrong, but a browser that sends an invalid
/// manifest gets one 422 for the whole folder with nothing to act on — after the person has
/// already waited for every file to be hashed. Checking the same rules here means the problem is
/// named, and named against the file that caused it, before any of that happens.

/// The same ceiling the control plane applies.
export const MAX_UPLOAD_FILES = 10_000;

/// LeRobot metadata Kratos requires before a version can become ready.
export const REQUIRED_LOGICAL_PATH = "meta/info.json";

/// Files are hashed outside the UI thread and never buffered in full. Four MiB keeps memory
/// bounded while avoiding tens of thousands of messages for normal LeRobot videos.
export const HASH_CHUNK_BYTES = 4 * 1024 * 1024;

export type PickedFile = {
  /// What the browser reports for a directory pick: `dataset/meta/info.json`.
  relativePath: string;
  name: string;
  size: number;
  type: string;
};

export type DeclaredFile = {
  logical_path: string;
  media_type: string;
  byte_length: number;
  sha256: string;
};

export type ManifestProblem = { path: string; reason: string };

/// Strip the directory the person happened to pick from, so two people uploading the same dataset
/// from differently named folders declare the same logical paths.
///
/// LeRobot datasets are identified by their internal layout — `meta/info.json`, `data/…` — and the
/// name of the enclosing folder is an accident of how someone downloaded it.
export function logicalPathFor(relativePath: string): string {
  const cleaned = relativePath.replace(/\\/g, "/").replace(/^\/+/, "");
  const segments = cleaned.split("/").filter((segment) => segment.length > 0);
  // Drop a single leading folder only when doing so still leaves a path, and only when the result
  // keeps the LeRobot shape. A flat pick of `info.json` must not become an empty path.
  if (segments.length > 1 && segments[0] !== "meta" && segments[0] !== "data" && segments[0] !== "videos") {
    return segments.slice(1).join("/");
  }
  return segments.join("/");
}

/// Why this path would be refused, or null when it is fine.
export function pathProblem(logicalPath: string): string | null {
  if (!logicalPath) return "the path is empty";
  if (logicalPath.length > 512) return "the path is longer than 512 characters";
  if (logicalPath.startsWith("/")) return "the path starts with a slash";
  if (logicalPath.includes("\\")) return "the path contains a backslash";
  const segments = logicalPath.split("/");
  if (segments.some((segment) => segment === "")) return "the path has an empty segment";
  if (segments.some((segment) => segment === "." || segment === "..")) {
    return "the path tries to leave the dataset";
  }
  return null;
}

/// A media type for a dataset file, since browsers leave `type` empty for parquet and many others.
export function mediaTypeFor(file: PickedFile): string {
  if (file.type) return file.type;
  const extension = file.name.includes(".") ? file.name.split(".").pop()!.toLowerCase() : "";
  switch (extension) {
    case "parquet":
      return "application/vnd.apache.parquet";
    case "json":
      return "application/json";
    case "jsonl":
      return "application/x-ndjson";
    case "mp4":
      return "video/mp4";
    case "png":
      return "image/png";
    case "csv":
      return "text/csv";
    case "txt":
    case "md":
      return "text/plain";
    default:
      return "application/octet-stream";
  }
}

/// Everything wrong with a folder, before a single byte is hashed.
///
/// Returns every problem rather than the first, because a person fixing an upload wants the list.
export function manifestProblems(files: PickedFile[]): ManifestProblem[] {
  const problems: ManifestProblem[] = [];
  if (files.length === 0) {
    problems.push({ path: "", reason: "no files were selected" });
    return problems;
  }
  if (files.length > MAX_UPLOAD_FILES) {
    problems.push({
      path: "",
      reason: `${files.length} files selected; the limit is ${MAX_UPLOAD_FILES}`,
    });
  }
  const seen = new Set<string>();
  for (const file of files) {
    const logicalPath = logicalPathFor(file.relativePath);
    const reason = pathProblem(logicalPath);
    if (reason) {
      problems.push({ path: file.relativePath, reason });
      continue;
    }
    if (seen.has(logicalPath)) {
      problems.push({ path: logicalPath, reason: "two selected files share this path" });
      continue;
    }
    seen.add(logicalPath);
    if (file.size <= 0) {
      problems.push({ path: logicalPath, reason: "the file is empty" });
    }
  }
  if (!seen.has(REQUIRED_LOGICAL_PATH)) {
    problems.push({
      path: REQUIRED_LOGICAL_PATH,
      reason: "a LeRobot dataset must include meta/info.json",
    });
  }
  return problems;
}

/// Assemble the declaration, given a hash per picked file in the same order.
export function buildDeclaration(files: PickedFile[], hashes: string[]): DeclaredFile[] {
  if (files.length !== hashes.length) {
    throw new Error("every file needs exactly one hash");
  }
  return files.map((file, index) => ({
    logical_path: logicalPathFor(file.relativePath),
    media_type: mediaTypeFor(file),
    byte_length: file.size,
    sha256: hashes[index],
  }));
}

/// Visit a file in bounded chunks. The worker owns the digest state; this helper owns the memory
/// contract and is kept DOM-free so the exact boundary is testable in Node.
export async function readFileChunks(
  file: Blob,
  visit: (chunk: Uint8Array) => void | Promise<void>,
  chunkBytes = HASH_CHUNK_BYTES,
): Promise<void> {
  if (!Number.isSafeInteger(chunkBytes) || chunkBytes <= 0) {
    throw new Error("chunk size must be a positive integer");
  }
  for (let offset = 0; offset < file.size; offset += chunkBytes) {
    const end = Math.min(offset + chunkBytes, file.size);
    const chunk = new Uint8Array(await file.slice(offset, end).arrayBuffer());
    await visit(chunk);
  }
}

/// GCS generations are immutable object identities. Inventing one when a response header is hidden
/// would make the control plane verify a different object (or none at all), so absence is fatal.
export async function storageGeneration(response: Response): Promise<string> {
  const header = response.headers.get("x-goog-generation");
  let candidate: unknown = header;
  if (!candidate) {
    const body = await response.clone().json().catch(() => null) as { generation?: unknown } | null;
    candidate = body?.generation;
  }
  if (typeof candidate !== "string" || !/^[1-9][0-9]*$/.test(candidate)) {
    throw new Error("Storage did not return the uploaded object's generation");
  }
  return candidate;
}

export type UploadState = "pending" | "hashing" | "uploading" | "verified" | "rejected";

/// What to show while a folder is being hashed and sent.
///
/// Byte-weighted rather than file-weighted: a LeRobot dataset is a handful of large parquet files
/// among many small ones, so counting files would sit at 90% through all the real work.
export function uploadProgress(
  entries: Array<{ byteLength: number; state: UploadState }>,
): { done: number; total: number; fraction: number; label: string } {
  const total = entries.reduce((sum, entry) => sum + entry.byteLength, 0);
  const done = entries
    .filter((entry) => entry.state === "verified")
    .reduce((sum, entry) => sum + entry.byteLength, 0);
  const rejected = entries.filter((entry) => entry.state === "rejected").length;
  const fraction = total === 0 ? 0 : done / total;
  if (rejected > 0) {
    return {
      done,
      total,
      fraction,
      label: `${rejected} ${rejected === 1 ? "file" : "files"} rejected`,
    };
  }
  if (total > 0 && done === total) {
    return { done, total, fraction: 1, label: "all files verified" };
  }
  return { done, total, fraction, label: `${Math.round(fraction * 100)}% uploaded` };
}

/// Plain words for a version's status, including what it means for using the dataset.
export function describeVersionStatus(status: string): string {
  switch (status) {
    case "uploading":
      return "Uploading — not yet selectable for training";
    case "ready":
      return "Ready";
    case "failed":
      return "Failed — a file did not match its declaration";
    case "draft":
      return "Draft";
    default:
      return status;
  }
}

import type { OutputRequirement } from "./artifactPresentation";

export type JobSubmission = {
  name: string;
  image_reference: string;
  timeout_seconds: number;
  earliest_start_at?: string;
  output_requirements: OutputRequirement[];
  dataset_inputs: JobDatasetInputDraft[];
  parameters?: JobParameters;
};

export type JobParameters = Record<string, string | number | boolean>;

/** The control plane's bounds, so the form refuses exactly what the server would. */
export const MAX_TIMEOUT_SECONDS = 86_400;
const MAX_PARAMETERS = 64;
const MAX_PARAMETER_BYTES = 4_096;
const PARAMETER_KEY = /^[A-Za-z_][A-Za-z0-9_.-]{0,63}$/;

export type ParsedParameters = { ok: true; value: JobParameters | null } | { ok: false; error: string };

/**
 * The parameters box, as typed: empty means none; otherwise one flat JSON object of named strings,
 * numbers and booleans. Whole numbers beyond 2^53 are refused rather than sent, because the browser
 * would silently round them -- a seed off by one is a different experiment.
 */
export function parseJobParameters(text: string): ParsedParameters {
  if (text.trim() === "") return { ok: true, value: null };
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return { ok: false, error: "Parameters must be a JSON object, for example {\"learning_rate\": 0.0003, \"epochs\": 12}." };
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return { ok: false, error: "Parameters must be a JSON object of names and values." };
  }
  const entries = Object.entries(parsed as Record<string, unknown>);
  if (entries.length > MAX_PARAMETERS) return { ok: false, error: `At most ${MAX_PARAMETERS} parameters.` };
  for (const [key, value] of entries) {
    if (!PARAMETER_KEY.test(key)) {
      return { ok: false, error: `"${key}" is not a valid name: start with a letter or _, then letters, digits, _ . or -, up to 64 characters.` };
    }
    if (typeof value === "number") {
      if (Number.isInteger(value) && !Number.isSafeInteger(value)) {
        return { ok: false, error: `"${key}" is too large to send exactly from a browser. Send it as a string instead.` };
      }
    } else if (typeof value !== "string" && typeof value !== "boolean") {
      return { ok: false, error: `"${key}" must be a string, number or true/false, not a list, object or null.` };
    }
  }
  if (entries.length === 0) return { ok: true, value: null };
  if (new TextEncoder().encode(JSON.stringify(parsed)).length > MAX_PARAMETER_BYTES) {
    return { ok: false, error: "Parameters are limited to 4 KiB." };
  }
  return { ok: true, value: parsed as JobParameters };
}

export type JobDatasetInputDraft = {
  alias: string;
  dataset_version_id: string;
  dataset_view_id: string | null;
};

export type DurableOutputDraft = {
  enabled: boolean;
  logicalPath: string;
  role: string;
  mediaType: string;
  maxMiB: number;
};

export const DURABLE_TRAINING_PRESET = {
  name: "Kratos Shapes durable model",
  imageReference: "ghcr.io/danielbryars/kratos-training-example@sha256:c0f8df79289f200706c5a2b19bb45f45c0e1258c9b774eba5f11c8c51fe2cc31",
  timeoutSeconds: 300,
  output: {
    enabled: true,
    logicalPath: "model.pt",
    role: "model",
    mediaType: "application/x-pytorch",
    maxMiB: 1,
  } satisfies DurableOutputDraft,
} as const;

export function buildJobSubmission(
  name: string,
  imageReference: string,
  timeoutSeconds: number,
  output: DurableOutputDraft,
  datasetInputs: JobDatasetInputDraft[] = [],
  earliestStartLocal = "",
  parameters: JobParameters | null = null,
): JobSubmission {
  const earliest = earliestStartLocal ? new Date(earliestStartLocal) : null;
  if (earliest && (Number.isNaN(earliest.getTime()) || earliest.getUTCFullYear() < 1 || earliest.getUTCFullYear() > 9999)) {
    throw new RangeError("Choose a valid earliest start date and time.");
  }
  const outputRequirements = output.enabled
    ? [{
        logical_path: output.logicalPath.trim(),
        role: output.role.trim(),
        media_type: output.mediaType.trim(),
        mandatory: true,
        max_bytes: Math.round(output.maxMiB * 1024 * 1024),
      }]
    : [];
  return {
    ...(earliest ? { earliest_start_at: earliest.toISOString() } : {}),
    ...(parameters && Object.keys(parameters).length > 0 ? { parameters } : {}),
    name: name.trim(),
    image_reference: imageReference.trim(),
    timeout_seconds: timeoutSeconds,
    output_requirements: outputRequirements,
    dataset_inputs: datasetInputs.map((input) => ({
      alias: input.alias.trim(),
      dataset_version_id: input.dataset_version_id,
      dataset_view_id: input.dataset_view_id,
    })),
  };
}

import type { OutputRequirement } from "./artifactPresentation";

export type JobSubmission = {
  name: string;
  image_reference: string;
  timeout_seconds: number;
  earliest_start_at?: string;
  output_requirements: OutputRequirement[];
  dataset_inputs: JobDatasetInputDraft[];
};

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
): JobSubmission {
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
    ...(earliestStartLocal ? { earliest_start_at: new Date(earliestStartLocal).toISOString() } : {}),
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

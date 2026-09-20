import type { OutputRequirement } from "./artifactPresentation";

export type JobSubmission = {
  name: string;
  image_reference: string;
  timeout_seconds: number;
  output_requirements: OutputRequirement[];
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
    name: name.trim(),
    image_reference: imageReference.trim(),
    timeout_seconds: timeoutSeconds,
    output_requirements: outputRequirements,
  };
}

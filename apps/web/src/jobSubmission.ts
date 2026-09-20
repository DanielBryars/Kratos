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

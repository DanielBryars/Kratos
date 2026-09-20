export type OutputRequirement = {
  logical_path: string;
  role: string;
  media_type: string;
  mandatory: boolean;
  max_bytes: number;
};

export type VerifiedArtifactEvidence = {
  storage_generation: number;
  byte_length: number;
  sha256: string;
  crc32c: string;
  verification_source: string;
  verified_at: string;
};

export type Artifact = {
  artifact_id: string;
  attempt_id: string;
  attempt_number: number;
  logical_path: string;
  role: string;
  media_type: string;
  mandatory: boolean;
  max_bytes: number;
  byte_length: number;
  sha256: string;
  crc32c: string;
  status: "declared" | "uploading" | "verifying" | "verified" | "rejected";
  declared_at: string;
  verified: VerifiedArtifactEvidence | null;
};

export type ArtifactList = { job_id: string; artifacts: Artifact[] };

export type ArtifactRow = OutputRequirement & {
  artifact: Artifact | null;
  availability: "available" | "not_declared" | "request_failed";
};

export function artifactRows(
  requirements: OutputRequirement[],
  response: ArtifactList | null,
  requestFailed = false,
): ArtifactRow[] {
  const latestByPath = new Map<string, Artifact>();
  for (const artifact of response?.artifacts ?? []) {
    const current = latestByPath.get(artifact.logical_path);
    if (!current || artifact.attempt_number > current.attempt_number) {
      latestByPath.set(artifact.logical_path, artifact);
    }
  }
  return requirements.map((requirement) => {
    const artifact = latestByPath.get(requirement.logical_path) ?? null;
    return {
      ...requirement,
      artifact,
      availability: artifact ? "available" : requestFailed ? "request_failed" : "not_declared",
    };
  });
}

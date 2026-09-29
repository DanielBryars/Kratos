/// Project concurrency policy presentation and payloads, kept out of the component so the
/// queue explanation and the "never clear by accident" rule can be tested directly.

export type SchedulingPolicy = {
  project_id: string;
  /// Null means unlimited.
  max_concurrent_jobs: number | null;
  /// Jobs assigned, running or cancelling: the jobs holding a slot.
  active_jobs: number;
};

export type PolicyDraft = { unlimited: boolean; limit: string };

export type SchedulingPolicyUpdate = { max_concurrent_jobs: number | null };

/// The column is a PostgreSQL integer.
const MAX_LIMIT = 2_147_483_647;

export function draftFromPolicy(policy: SchedulingPolicy): PolicyDraft {
  return policy.max_concurrent_jobs === null
    ? { unlimited: true, limit: "" }
    : { unlimited: false, limit: String(policy.max_concurrent_jobs) };
}

/// Always names the field: null only when Unlimited is chosen, never because the box was empty.
export function buildPolicyUpdate(draft: PolicyDraft): SchedulingPolicyUpdate {
  if (draft.unlimited) return { max_concurrent_jobs: null };
  const trimmed = draft.limit.trim();
  const value = Number(trimmed);
  if (!/^\d+$/.test(trimmed) || value < 1 || value > MAX_LIMIT) {
    throw new RangeError("Enter a whole number of jobs of at least 1, or choose Unlimited.");
  }
  return { max_concurrent_jobs: value };
}

function jobs(count: number) {
  return `${count} ${count === 1 ? "job" : "jobs"}`;
}

export function describePolicy(policy: SchedulingPolicy): string {
  return policy.max_concurrent_jobs === null
    ? `Unlimited · ${jobs(policy.active_jobs)} active`
    : `${policy.active_jobs} of ${jobs(policy.max_concurrent_jobs)} active`;
}

export function projectAtCapacity(policy: SchedulingPolicy | null): boolean {
  return policy !== null
    && policy.max_concurrent_jobs !== null
    && policy.active_jobs >= policy.max_concurrent_jobs;
}

export type QueueWaitReason = "earliest_start" | "project_limit" | "eligible_worker" | "unknown";

/// Why a queued job has not started, claiming only what Kratos actually knows. A future earliest
/// start wins; a project at its limit is reported as such; anything else is the generic wait for
/// an eligible worker, because per-worker compatibility is not diagnosed here.
export function queueWaitReason(
  job: { status: string; earliest_start_at?: string | null },
  policy: SchedulingPolicy | null,
  now: Date = new Date(),
): QueueWaitReason | null {
  if (job.status !== "queued") return null;
  if (job.earliest_start_at && new Date(job.earliest_start_at).getTime() > now.getTime()) {
    return "earliest_start";
  }
  if (policy === null) return "unknown";
  return projectAtCapacity(policy) ? "project_limit" : "eligible_worker";
}

export function describeQueueWait(reason: Exclude<QueueWaitReason, "earliest_start">, policy: SchedulingPolicy | null): string {
  if (reason === "project_limit" && policy?.max_concurrent_jobs != null) {
    return `waiting for a slot · project concurrency limit reached (${policy.active_jobs} of ${jobs(policy.max_concurrent_jobs)} active)`;
  }
  if (reason === "eligible_worker") return "waiting for an eligible worker";
  return "waiting to be scheduled";
}

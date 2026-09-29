import type { JobSubmission } from "./jobSubmission";

export type SubmissionAttempt = Readonly<{ key: string; body: string; name: string }>;
export type SubmissionOutcome = "accepted" | "uncertain" | "rejected";

export function definitiveSubmissionRejection(status: number): boolean {
  return status >= 400 && status < 500 && ![408, 409, 429].includes(status);
}

/** One tab's unresolved submission. No identity token or durable browser storage is retained. */
export class SubmissionRetry {
  private attempt: SubmissionAttempt | null = null;
  private sending = false;
  private uncertain = false;

  get busy(): boolean { return this.sending; }
  get pending(): SubmissionAttempt | null { return this.attempt; }

  isCurrent(attempt: SubmissionAttempt): boolean { return this.attempt === attempt; }

  begin(build: () => JobSubmission, newKey = () => crypto.randomUUID()): SubmissionAttempt | null {
    if (this.sending) return null;
    if (!this.attempt) {
      const payload = build();
      this.attempt = Object.freeze({ key: newKey(), body: JSON.stringify(payload), name: payload.name });
    }
    this.sending = true;
    return this.attempt;
  }

  /** A response from before sign-out/reset must not update the new session's UI. */
  finish(attempt: SubmissionAttempt, outcome: SubmissionOutcome): boolean {
    if (this.attempt !== attempt) return false;
    this.sending = false;
    if (outcome === "uncertain") this.uncertain = true;
    if (outcome === "accepted" || (outcome === "rejected" && !this.uncertain)) {
      this.attempt = null;
      this.uncertain = false;
    }
    return true;
  }

  reset(): void {
    this.attempt = null;
    this.sending = false;
    this.uncertain = false;
  }
}

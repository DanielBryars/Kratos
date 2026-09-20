import { getApp, getApps, initializeApp } from "firebase/app";
import {
  GoogleAuthProvider,
  getAuth,
  onAuthStateChanged,
  signInWithPopup,
  signOut,
  type Auth,
  type User,
} from "firebase/auth";
import { useEffect, useState } from "react";

import {
  artifactRows,
  type Artifact,
  type OutputRequirement,
} from "./artifactPresentation";

type Version = { name: string; version: string };
type AuthConfig = { apiKey: string; authDomain: string; projectId: string };
type Enrolment = { enrolment_credential: string; expires_at: string };
type PendingRegistration = {
  registration_id: string;
  display_name: string;
  confirmation_code: string;
  expires_at: string;
  capabilities: {
    hostname: string;
    logical_cpu_count: number;
    memory_total_bytes: number;
    gpus: Array<{ name: string; memory_total_bytes: number }>;
    gpu_health: {
      status: "unavailable" | "unverified" | "healthy" | "unhealthy";
      detail: string;
      evidence?: { checked_at: string; image_reference: string; duration_ms?: number } | null;
    };
  };
};
type Worker = {
  worker_id: string;
  display_name: string;
  state: string;
  connectivity: "never_seen" | "online" | "stale" | "offline";
  last_seen_at: string | null;
  capabilities: PendingRegistration["capabilities"];
  compute_groups: Array<{ id: string; name: string }>;
};
type Job = {
  job_id: string;
  name: string;
  image_reference: string;
  gpu_count: number;
  timeout_seconds: number;
  status: "queued" | "assigned" | "running" | "cancelling" | "succeeded" | "failed" | "cancelled";
  assigned_worker_id: string | null;
  submitted_at: string;
  started_at: string | null;
  finished_at: string | null;
  exit_code: number | null;
  stdout: string | null;
  stderr: string | null;
  failure_message: string | null;
  output_requirements: OutputRequirement[];
  current_attempt: { attempt_id: string; attempt_number: number } | null;
  artifacts: Artifact[];
};
type ApiError = { message?: string };

const DEMO_WORKLOAD_IMAGE = "ghcr.io/danielbryars/kratos-gpu-health-check@sha256:3ee068a54416c67c32b5d6369e9120fd4ee9b62ffd7865dcde7a688f482168a9";

function formatBytes(bytes: number) {
  return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
}

function formatArtifactBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GiB`;
}

function formatDuration(milliseconds: number) {
  const seconds = Math.max(0, Math.round(milliseconds / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return `${minutes}m ${remainder}s`;
}

function jobTiming(job: Job) {
  const submittedAt = new Date(job.submitted_at).getTime();
  const startedAt = job.started_at ? new Date(job.started_at).getTime() : null;
  const finishedAt = job.finished_at ? new Date(job.finished_at).getTime() : null;
  if (job.status === "cancelling") {
    const elapsed = startedAt === null
      ? formatDuration(Date.now() - submittedAt)
      : formatDuration(Date.now() - startedAt);
    return `Cancellation pending · waiting for the worker to stop or its lease to expire · ${elapsed}`;
  }
  if (startedAt !== null && finishedAt !== null) {
    return `Waited ${formatDuration(startedAt - submittedAt)} · ran ${formatDuration(finishedAt - startedAt)}`;
  }
  if (startedAt !== null) {
    return `Waited ${formatDuration(startedAt - submittedAt)} · running for ${formatDuration(Date.now() - startedAt)}`;
  }
  if (finishedAt !== null) {
    return `Finished after ${formatDuration(finishedAt - submittedAt)}`;
  }
  return `Queued for ${formatDuration(Date.now() - submittedAt)} · completion estimate unavailable`;
}

export function App() {
  const [service, setService] = useState<Version | null>(null);
  const [status, setStatus] = useState<"checking" | "online" | "offline">("checking");
  const [auth, setAuth] = useState<Auth | null>(null);
  const [user, setUser] = useState<User | null>(null);
  const [authStatus, setAuthStatus] = useState<"loading" | "ready" | "unavailable">("loading");
  const [expirySeconds, setExpirySeconds] = useState(900);
  const [enrolment, setEnrolment] = useState<Enrolment | null>(null);
  const [action, setAction] = useState<"idle" | "signing-in" | "creating">("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [pending, setPending] = useState<PendingRegistration[]>([]);
  const [decidingId, setDecidingId] = useState<string | null>(null);
  const [workers, setWorkers] = useState<Worker[]>([]);
  const [groupNames, setGroupNames] = useState<Record<string, string>>({});
  const [workerActionId, setWorkerActionId] = useState<string | null>(null);
  const [jobs, setJobs] = useState<Job[]>([]);
  const [jobStatusUnavailable, setJobStatusUnavailable] = useState(false);
  const [jobName, setJobName] = useState("RTX 5090 matrix check");
  const [jobImage, setJobImage] = useState(DEMO_WORKLOAD_IMAGE);
  const [jobTimeout, setJobTimeout] = useState(120);
  const [jobAction, setJobAction] = useState(false);

  useEffect(() => {
    fetch("/api/v1/version")
      .then((response) => {
        if (!response.ok) throw new Error(`API returned ${response.status}`);
        return response.json() as Promise<Version>;
      })
      .then((version) => {
        setService(version);
        setStatus("online");
      })
      .catch(() => setStatus("offline"));
  }, []);

  useEffect(() => {
    if (!user) {
      setPending([]);
      setWorkers([]);
      setJobs([]);
      setJobStatusUnavailable(false);
      return;
    }
    let stopped = false;
    let controller: AbortController | null = null;
    let nextRefresh: number | null = null;
    async function refresh() {
      controller = new AbortController();
      try {
        const idToken = await user!.getIdToken();
        if (stopped) return;
        const headers = { Authorization: `Bearer ${idToken}` };
        const request = { headers, signal: controller.signal };
        const [pendingResponse, workersResponse, jobsResponse] = await Promise.all([
          fetch("/api/v1/operator/worker-registration-requests", request),
          fetch("/api/v1/operator/workers", request),
          fetch("/api/v1/operator/jobs", request),
        ]);
        const [nextPending, nextWorkers, nextJobs] = await Promise.all([
          pendingResponse.ok ? pendingResponse.json() as Promise<PendingRegistration[]> : null,
          workersResponse.ok ? workersResponse.json() as Promise<Worker[]> : null,
          jobsResponse.ok ? jobsResponse.json() as Promise<Job[]> : null,
        ]);
        if (stopped) return;
        if (nextPending) setPending(nextPending);
        if (nextWorkers) setWorkers(nextWorkers);
        if (nextJobs) {
          setJobs(nextJobs);
          setJobStatusUnavailable(false);
        } else {
          setJobStatusUnavailable(true);
        }
      } catch (error) {
        if (!(error instanceof DOMException && error.name === "AbortError")) {
          if (!stopped) setJobStatusUnavailable(true);
        }
      } finally {
        controller = null;
        if (!stopped) nextRefresh = window.setTimeout(() => void refresh(), 5000);
      }
    }
    void refresh();
    return () => {
      stopped = true;
      controller?.abort();
      if (nextRefresh !== null) window.clearTimeout(nextRefresh);
    };
  }, [user]);

  useEffect(() => {
    let cancelled = false;
    let unsubscribe: () => void = () => {};
    fetch("/api/v1/auth/config")
      .then((response) => {
        if (!response.ok) throw new Error("Authentication is not configured.");
        return response.json() as Promise<AuthConfig>;
      })
      .then((config) => {
        if (cancelled) return;
        const firebase = getApps().length === 0 ? initializeApp(config) : getApp();
        const instance = getAuth(firebase);
        setAuth(instance);
        unsubscribe = onAuthStateChanged(instance, (currentUser) => {
          setUser(currentUser);
          setAuthStatus("ready");
        });
      })
      .catch(() => {
        if (!cancelled) setAuthStatus("unavailable");
      });
    return () => {
      cancelled = true;
      unsubscribe();
    };
  }, []);

  async function signIn() {
    if (!auth) return;
    setAction("signing-in");
    setMessage(null);
    try {
      await signInWithPopup(auth, new GoogleAuthProvider());
    } catch {
      setMessage("Google sign-in did not complete. Check the provider setup and try again.");
    } finally {
      setAction("idle");
    }
  }

  async function createEnrolment() {
    if (!user) return;
    setAction("creating");
    setMessage(null);
    setEnrolment(null);
    setCopied(false);
    try {
      const idToken = await user.getIdToken();
      const response = await fetch("/api/v1/operator/worker-enrolments", {
        method: "POST",
        headers: { Authorization: `Bearer ${idToken}`, "Content-Type": "application/json" },
        body: JSON.stringify({ expires_in_seconds: expirySeconds }),
      });
      if (!response.ok) {
        const error = (await response.json().catch(() => ({}))) as ApiError;
        throw new Error(error.message ?? `Request failed with ${response.status}`);
      }
      setEnrolment((await response.json()) as Enrolment);
    } catch (error) {
      setMessage(error instanceof Error ? error.message : "The enrolment could not be created.");
    } finally {
      setAction("idle");
    }
  }

  async function copyCredential() {
    if (!enrolment) return;
    await navigator.clipboard.writeText(enrolment.enrolment_credential);
    setCopied(true);
  }

  async function decideRegistration(registrationId: string, decision: "approve" | "reject") {
    if (!user) return;
    setDecidingId(registrationId);
    setMessage(null);
    try {
      const idToken = await user.getIdToken();
      const response = await fetch(
        `/api/v1/operator/worker-registration-requests/${registrationId}/${decision}`,
        { method: "POST", headers: { Authorization: `Bearer ${idToken}` } },
      );
      if (!response.ok) {
        const error = (await response.json().catch(() => ({}))) as ApiError;
        throw new Error(error.message ?? `Request failed with ${response.status}`);
      }
      setPending((current) => current.filter((item) => item.registration_id !== registrationId));
    } catch (error) {
      setMessage(error instanceof Error ? error.message : "The registration could not be updated.");
    } finally {
      setDecidingId(null);
    }
  }

  async function updateWorker(workerId: string, actionName: "approve" | "quarantine" | "revoke") {
    if (!user) return;
    if (actionName === "revoke" && !window.confirm("Revoke this worker and its credential? It will need to register again.")) return;
    setWorkerActionId(workerId);
    setMessage(null);
    try {
      const idToken = await user.getIdToken();
      const response = await fetch(`/api/v1/operator/workers/${workerId}/${actionName}`, {
        method: "POST",
        headers: { Authorization: `Bearer ${idToken}`, "Content-Type": "application/json" },
        body: actionName === "approve" ? JSON.stringify({ compute_group_name: groupNames[workerId] ?? "Home" }) : undefined,
      });
      if (!response.ok) {
        const error = (await response.json().catch(() => ({}))) as ApiError;
        throw new Error(error.message ?? `Request failed with ${response.status}`);
      }
      const result = (await response.json()) as { state: string };
      setWorkers((current) => current.map((worker) => worker.worker_id === workerId
        ? { ...worker, state: result.state }
        : worker));
      const refreshed = await fetch("/api/v1/operator/workers", { headers: { Authorization: `Bearer ${idToken}` } });
      if (refreshed.ok) setWorkers((await refreshed.json()) as Worker[]);
    } catch (error) {
      setMessage(error instanceof Error ? error.message : "The worker could not be updated.");
    } finally {
      setWorkerActionId(null);
    }
  }

  async function submitJob() {
    if (!user) return;
    setJobAction(true);
    setMessage(null);
    try {
      const idToken = await user.getIdToken();
      const response = await fetch("/api/v1/operator/jobs", {
        method: "POST",
        headers: { Authorization: `Bearer ${idToken}`, "Content-Type": "application/json" },
        body: JSON.stringify({ name: jobName, image_reference: jobImage, timeout_seconds: jobTimeout }),
      });
      if (!response.ok) {
        const error = (await response.json().catch(() => ({}))) as ApiError;
        throw new Error(error.message ?? `Request failed with ${response.status}`);
      }
      const created = (await response.json()) as Job;
      setJobs((current) => [created, ...current]);
    } catch (error) {
      setMessage(error instanceof Error ? error.message : "The job could not be queued.");
    } finally {
      setJobAction(false);
    }
  }

  async function cancelJob(jobId: string) {
    if (!user) return;
    setJobAction(true);
    setMessage(null);
    try {
      const idToken = await user.getIdToken();
      const response = await fetch(`/api/v1/operator/jobs/${jobId}/cancel`, {
        method: "POST",
        headers: { Authorization: `Bearer ${idToken}` },
      });
      if (!response.ok) {
        const error = (await response.json().catch(() => ({}))) as ApiError;
        throw new Error(error.message ?? `Request failed with ${response.status}`);
      }
      const cancelled = (await response.json()) as Job;
      setJobs((current) => current.map((job) => job.job_id === jobId ? cancelled : job));
    } catch (error) {
      setMessage(error instanceof Error ? error.message : "The job could not be cancelled.");
    } finally {
      setJobAction(false);
    }
  }

  return (
    <main>
      <header>
        <div><p className="eyebrow">GPU training platform</p><h1>KRATOS</h1></div>
        <span className={`status status--${status}`}>{status}</span>
      </header>

      <section className="hero">
        <p>Private compute. Cloud control.</p>
        <h2>Bring the fleet online.</h2>
        <p className="muted">Register trusted GPU workers, inspect their capabilities, and keep control of where training runs.</p>
      </section>

      <section className="grid">
        <article className="card service-card" aria-labelledby="control-plane-heading">
          <div><p className="label" id="control-plane-heading">Control plane</p><p className="value">{service?.name ?? "Waiting for API"}</p></div>
          <div><p className="label">Version</p><p className="value">{service?.version ?? "—"}</p></div>
          <a href="/swagger-ui/" target="_blank" rel="noreferrer">Open API</a>
        </article>

        <article className="card operator-card" aria-labelledby="operator-heading">
          <div className="card-heading">
            <div><p className="label">Operator access</p><h3 id="operator-heading">Worker registration</h3></div>
            {user && <span className="identity">{user.email}</span>}
          </div>
          {authStatus === "loading" && <p className="muted compact">Loading secure sign-in…</p>}
          {authStatus === "unavailable" && <p className="notice notice--error">Operator sign-in is not available in this environment.</p>}
          {authStatus === "ready" && !user && (
            <div className="operator-action">
              <p className="muted compact">Sign in with the approved Google account to issue a one-time credential.</p>
              <button type="button" onClick={signIn} disabled={action !== "idle"}>{action === "signing-in" ? "Signing in…" : "Sign in with Google"}</button>
            </div>
          )}
          {authStatus === "ready" && user && (
            <>
              <div className="registration-section">
                <div><p className="label">Fleet</p><h3>Registered machines</h3></div>
                {workers.length === 0 && <p className="muted compact">No machines have registered yet.</p>}
                <div className="worker-grid">
                  {workers.map((worker) => (
                    <div className="worker" key={worker.worker_id}>
                      <div className="worker-heading">
                        <div><strong>{worker.display_name}</strong><p>{worker.capabilities.hostname}</p></div>
                        <div className="worker-badges"><span className={`badge badge--${worker.connectivity}`}>{worker.connectivity.replace("_", " ")}</span><span className="badge">{worker.state}</span></div>
                      </div>
                      <p className="worker-hardware">{worker.capabilities.gpus.map((gpu) => `${gpu.name} · ${formatBytes(gpu.memory_total_bytes)}`).join(", ") || "No GPU detected"}</p>
                      <p>{worker.capabilities.logical_cpu_count} CPUs · {formatBytes(worker.capabilities.memory_total_bytes)} RAM</p>
                      <p className={`gpu-health gpu-health--${worker.capabilities.gpu_health.status}`}>GPU check: {worker.capabilities.gpu_health.detail}</p>
                      <p>Last heartbeat: {worker.last_seen_at ? new Date(worker.last_seen_at).toLocaleString() : "never"}</p>
                      <div className="worker-groups">{worker.compute_groups.map((group) => <span className="badge" key={group.id}>{group.name}</span>)}</div>
                      {worker.state !== "revoked" && (
                        <div className="worker-controls">
                          <input aria-label={`Compute group for ${worker.display_name}`} value={groupNames[worker.worker_id] ?? "Home"} maxLength={100} onChange={(event) => setGroupNames((current) => ({ ...current, [worker.worker_id]: event.target.value }))} />
                          <button type="button" disabled={workerActionId !== null} onClick={() => void updateWorker(worker.worker_id, "approve")}>{worker.state === "unapproved" || worker.state === "quarantined" ? "Approve and add" : "Add group"}</button>
                          {worker.state !== "quarantined" && <button className="button-secondary" type="button" disabled={workerActionId !== null} onClick={() => void updateWorker(worker.worker_id, "quarantine")}>Quarantine</button>}
                          <button className="button-danger" type="button" disabled={workerActionId !== null} onClick={() => void updateWorker(worker.worker_id, "revoke")}>Revoke</button>
                        </div>
                      )}
                    </div>
                  ))}
                </div>
              </div>
              <div className="registration-section">
                <div><p className="label">Work queue</p><h3>Schedule GPU work</h3></div>
                <p className="muted compact">Submit one immutable container image. Kratos assigns it to the next online, approved worker with a healthy GPU.</p>
                <div className="job-form">
                  <label>Job name<input value={jobName} maxLength={120} onChange={(event) => setJobName(event.target.value)} /></label>
                  <label>Immutable image<input value={jobImage} onChange={(event) => setJobImage(event.target.value)} /></label>
                  <label>Maximum runtime (seconds)<input type="number" min={30} max={3600} value={jobTimeout} onChange={(event) => setJobTimeout(Number(event.target.value))} /></label>
                  <button type="button" disabled={jobAction || !jobName.trim() || !jobImage.trim()} onClick={() => void submitJob()}>{jobAction ? "Updating…" : "Queue job"}</button>
                </div>
                {jobs.length === 0 && <p className="muted compact">No jobs have been submitted.</p>}
                {jobStatusUnavailable && <p className="notice notice--error" role="status">Live job and output status is temporarily unavailable. Showing the last complete snapshot.</p>}
                <div className="job-list">
                  {jobs.map((job) => (
                    <div className="job" key={job.job_id}>
                      <div className="job-heading"><div><strong>{job.name}</strong><p>{new Date(job.submitted_at).toLocaleString()}</p></div><span className={`badge badge--job-${job.status}`}>{job.status === "cancelling" ? "cancellation pending" : job.status}</span></div>
                      <p className="job-identity">Run {job.job_id}</p>
                      <p className="job-image">{job.image_reference}</p>
                      <p>1 GPU · {job.timeout_seconds}s limit{job.assigned_worker_id ? ` · worker ${job.assigned_worker_id.slice(0, 8)}` : ""}</p>
                      <p>{jobTiming(job)}</p>
                      {job.failure_message && <p className="job-failure">{job.failure_message}</p>}
                      {job.stdout && <pre>{job.stdout}</pre>}
                      {job.stderr && <pre className="job-failure">{job.stderr}</pre>}
                      <div className="artifact-list" aria-label={`Outputs for ${job.name}`}>
                        <div className="artifact-list-heading"><strong>Outputs</strong><span>{job.output_requirements.length} requested</span></div>
                        {job.output_requirements.length === 0 && <p className="artifact-unavailable">No durable outputs were requested for this job.</p>}
                        {job.output_requirements.length > 0 && artifactRows(
                          job.output_requirements,
                          { job_id: job.job_id, artifacts: job.artifacts },
                        ).map((row) => (
                          <div className="artifact" key={row.logical_path}>
                            <div className="artifact-heading">
                              <div><strong>{row.logical_path}</strong><span>{row.mandatory ? "Required" : "Optional"} · {row.role}</span></div>
                              <span className={`badge badge--artifact-${row.artifact?.status ?? "unavailable"}`}>
                                {row.artifact?.status ?? "unavailable"}
                              </span>
                            </div>
                            {!row.artifact && <p className="artifact-unavailable">{job.current_attempt ? `Attempt ${job.current_attempt.attempt_number} has not declared this output.` : "No attempt has started, so this output is not declared."}</p>}
                            {row.artifact && (
                              <div className="artifact-evidence">
                                <p>Attempt {row.artifact.attempt_number} · {formatArtifactBytes(row.artifact.byte_length)} of {formatArtifactBytes(row.max_bytes)}</p>
                                <p><span>Declared SHA-256</span><code>{row.artifact.sha256}</code></p>
                                <p><span>Declared CRC32C</span><code>{row.artifact.crc32c}</code></p>
                                {row.artifact.verified ? (
                                  <div className="artifact-verified">
                                    <p>Storage verified {new Date(row.artifact.verified.verified_at).toLocaleString()} · generation {row.artifact.verified.storage_generation}</p>
                                    <p><span>Verified SHA-256</span><code>{row.artifact.verified.sha256}</code></p>
                                    <p><span>Verified CRC32C</span><code>{row.artifact.verified.crc32c}</code></p>
                                  </div>
                                ) : <p className="artifact-unavailable">Verified storage evidence is unavailable until verification succeeds.</p>}
                              </div>
                            )}
                          </div>
                        ))}
                      </div>
                      {job.status === "cancelling" && <p className="job-cancellation" role="status">Cancellation has been requested. The job will be cancelled when the worker stops or its lease expires.</p>}
                      {(job.status === "queued" || job.status === "assigned" || job.status === "running") && (
                        <button className="button-secondary" type="button" disabled={jobAction} onClick={() => void cancelJob(job.job_id)}>
                          {job.status === "queued" ? "Cancel queued job" : "Request cancellation"}
                        </button>
                      )}
                    </div>
                  ))}
                </div>
              </div>
              <div className="registration-section">
                <div><p className="label">Requests</p><h3>Machines waiting for approval</h3></div>
                {pending.length === 0 && <p className="muted compact">No machines are waiting.</p>}
                {pending.map((request) => (
                  <div className="registration" key={request.registration_id}>
                    <div>
                      <strong>{request.display_name}</strong>
                      <p>{request.capabilities.gpus.map((gpu) => gpu.name).join(", ") || "No GPU detected"} · {request.capabilities.logical_cpu_count} CPUs</p>
                      <p className="registration-code">Code {request.confirmation_code}</p>
                    </div>
                    <div className="registration-actions">
                      <button type="button" disabled={decidingId !== null} onClick={() => void decideRegistration(request.registration_id, "approve")}>Approve</button>
                      <button className="button-secondary" type="button" disabled={decidingId !== null} onClick={() => void decideRegistration(request.registration_id, "reject")}>Reject</button>
                    </div>
                  </div>
                ))}
              </div>
              <div className="registration-section">
                <div><p className="label">Automation</p><h3>Create a one-time enrolment</h3></div>
              <div className="form-row">
                <label htmlFor="expiry">Credential lifetime</label>
                <select id="expiry" value={expirySeconds} onChange={(event) => setExpirySeconds(Number(event.target.value))}>
                  <option value={900}>15 minutes</option><option value={1800}>30 minutes</option><option value={3600}>60 minutes</option>
                </select>
                <button type="button" onClick={createEnrolment} disabled={action !== "idle"}>{action === "creating" ? "Creating…" : "Create enrolment"}</button>
                <button className="button-secondary" type="button" onClick={() => void signOut(auth!)}>Sign out</button>
              </div>
              {enrolment && (
                <div className="credential" aria-live="polite">
                  <div><p className="label">Shown once</p><code>{enrolment.enrolment_credential}</code></div>
                  <button type="button" onClick={copyCredential}>{copied ? "Copied" : "Copy"}</button>
                  <p>Expires {new Date(enrolment.expires_at).toLocaleString()}.</p>
                </div>
              )}
              </div>
            </>
          )}
          {message && <p className="notice notice--error" role="alert">{message}</p>}
        </article>
      </section>
    </main>
  );
}

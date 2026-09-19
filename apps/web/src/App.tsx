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
type ApiError = { message?: string };

function formatBytes(bytes: number) {
  return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
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
      return;
    }
    let cancelled = false;
    async function refresh() {
      const idToken = await user!.getIdToken();
      const headers = { Authorization: `Bearer ${idToken}` };
      const [pendingResponse, workersResponse] = await Promise.all([
        fetch("/api/v1/operator/worker-registration-requests", { headers }),
        fetch("/api/v1/operator/workers", { headers }),
      ]);
      if (!cancelled) {
        if (pendingResponse.ok) setPending((await pendingResponse.json()) as PendingRegistration[]);
        if (workersResponse.ok) setWorkers((await workersResponse.json()) as Worker[]);
      }
    }
    void refresh();
    const interval = window.setInterval(() => void refresh(), 5000);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
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

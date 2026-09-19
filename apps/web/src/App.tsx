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
type ApiError = { message?: string };

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
            <div><p className="label">Operator access</p><h3 id="operator-heading">Create a worker enrolment</h3></div>
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
            </>
          )}
          {message && <p className="notice notice--error" role="alert">{message}</p>}
        </article>
      </section>
    </main>
  );
}

import { useEffect, useState } from "react";

type Version = {
  name: string;
  version: string;
};

export function App() {
  const [service, setService] = useState<Version | null>(null);
  const [status, setStatus] = useState<"checking" | "online" | "offline">("checking");

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

  return (
    <main>
      <header>
        <div>
          <p className="eyebrow">GPU training platform</p>
          <h1>KRATOS</h1>
        </div>
        <span className={`status status--${status}`}>{status}</span>
      </header>

      <section className="hero">
        <p>Control plane scaffold</p>
        <h2>The fleet starts here.</h2>
        <p className="muted">
          Worker registration, capability discovery and the home compute group arrive in R0.1.
        </p>
      </section>

      <section className="card" aria-labelledby="control-plane-heading">
        <div>
          <p className="label" id="control-plane-heading">Control plane</p>
          <p className="value">{service?.name ?? "Waiting for API"}</p>
        </div>
        <div>
          <p className="label">Version</p>
          <p className="value">{service?.version ?? "—"}</p>
        </div>
        <a href="/swagger-ui/" target="_blank" rel="noreferrer">
          Open API
        </a>
      </section>
    </main>
  );
}

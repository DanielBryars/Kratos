import { useEffect, useState, type FormEvent } from "react";
import type { User } from "firebase/auth";
import {
  CREDIT_ENTRIES_URL, creditGrantRequest, creditHistoryUrl, formatCreditUnits, parseCreditAccount, poundsToPence,
  type CreditAccount,
} from "./creditLedger";

/**
 * One idempotency key per intended grant. Resubmitting the same amount and reason reuses it, so a
 * request whose answer was lost can be retried without adding the credit twice; changing either
 * value starts a new grant with a new key.
 */
type PendingGrant = { key: string; pence: string; reason: string };

function AddCreditForm({ user, onAdded }: { user: User; onAdded: () => void }) {
  const [amount, setAmount] = useState("");
  const [reason, setReason] = useState("");
  const [pending, setPending] = useState<PendingGrant | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ kind: "error" | "success"; text: string } | null>(null);

  const pence = poundsToPence(amount);
  const request = pence === null ? null : creditGrantRequest(pence, reason);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (request === null || busy) return;
    const same = pending !== null && pending.pence === request.amount_units && pending.reason === request.reason;
    const grant = same ? pending : { key: crypto.randomUUID(), pence: request.amount_units, reason: request.reason };
    setPending(grant);
    setBusy(true);
    setMessage(null);
    try {
      const token = await user.getIdToken();
      const response = await fetch(CREDIT_ENTRIES_URL, {
        method: "POST",
        headers: { Authorization: `Bearer ${token}`, "content-type": "application/json", "Idempotency-Key": grant.key },
        body: JSON.stringify(request),
      });
      if (response.ok) {
        setPending(null);
        setAmount("");
        setReason("");
        setMessage({ kind: "success", text: `Added ${formatCreditUnits(grant.pence)}.` });
        onAdded();
        return;
      }
      const body = await response.json().catch(() => null) as { message?: string } | null;
      // A definite refusal: nothing was written, so the next attempt should not replay this key.
      setPending(null);
      setMessage({ kind: "error", text: body?.message ?? "The credit could not be added." });
    } catch {
      // No answer: the grant may or may not have been recorded. Keeping the key makes a retry safe.
      setMessage({ kind: "error", text: "No answer from Kratos, so this may or may not have been added. Submitting the same amount and reason again is safe: it will not be added twice." });
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="credit-add" onSubmit={submit} aria-label="Add credit">
      <label>Amount (£)<input inputMode="decimal" autoComplete="off" placeholder="25.00" value={amount} onChange={(event) => setAmount(event.target.value)} /></label>
      <label>Reason<input maxLength={500} autoComplete="off" placeholder="What this credit is for" value={reason} onChange={(event) => setReason(event.target.value)} /></label>
      <button type="submit" disabled={request === null || busy}>{busy ? "Adding…" : request ? `Add ${formatCreditUnits(request.amount_units)}` : "Add credit"}</button>
      {amount !== "" && pence === null && <p className="muted compact" role="status">Enter a positive amount in pounds, with at most two decimal places.</p>}
      {message && <p className={message.kind === "error" ? "notice notice--error" : "notice"} role={message.kind === "error" ? "alert" : "status"}>{message.text}</p>}
    </form>
  );
}

/** Mounted with a user-ID key so neither history nor an in-flight read crosses sign-in. */
export function CreditLedgerPanel({ user }: { user: User }) {
  const [account, setAccount] = useState<CreditAccount | null>(null);
  const [before, setBefore] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    let current = true;
    setLoading(true);
    setError(null);
    setAccount(null);
    async function load() {
      try {
        const token = await user.getIdToken();
        if (!current) return;
        const response = await fetch(creditHistoryUrl(before), {
          headers: { Authorization: `Bearer ${token}` },
          signal: controller.signal,
        });
        if (!response.ok) throw new Error("Credit history is unavailable. Try refreshing.");
        const next = parseCreditAccount(await response.json());
        if (current) setAccount(next);
      } catch (failure) {
        if (current) setError(failure instanceof Error ? failure.message : "Credit history is unavailable.");
      } finally {
        if (current) setLoading(false);
      }
    }
    void load();
    return () => { current = false; controller.abort(); };
  }, [user, before, refresh]);

  return (
    <section className="registration-section" id="credit-ledger" aria-labelledby="credit-ledger-heading">
      <div className="section-heading">
        <div><p className="label">Project accounting</p><h3 id="credit-ledger-heading">Internal credit ledger</h3></div>
        <button className="button-secondary button-compact" type="button" disabled={loading} onClick={() => { setBefore(null); setRefresh((value) => value + 1); }}>Refresh latest</button>
      </div>
      <p className="muted compact">Bookkeeping only. Credits do not start or stop jobs. Rates, usage charges and reserved budgets are not enabled.</p>
      {account && (account.can_manage
        ? <p className="muted compact">Any member of this project can add credit.</p>
        : <p className="muted compact">Only members of this project can add credit.</p>)}
      {loading && <p role="status">Loading credit history…</p>}
      {error && <p className="notice notice--error" role="alert">{error}</p>}
      {account && <>
        <p className="credit-balance">{account.account_exists ? <><strong>{formatCreditUnits(account.balance_units)}</strong><span>Balance at last refresh</span></> : <><strong>No credit account yet</strong><span>No credits have been granted.</span></>}</p>
        {account.can_manage && <AddCreditForm user={user} onAdded={() => { setBefore(null); setRefresh((value) => value + 1); }} />}
        {before && <p className="muted compact">Showing older entries. Refresh latest to return to recent activity.</p>}
        {account.entries.length === 0 ? <p className="muted compact">No entries on this page.</p> : (
          <ol className="credit-entries" aria-label="Credit ledger entries">
            {account.entries.map((entry) => <li key={entry.id}>
              <div className="worker-heading"><strong>{entry.kind[0].toUpperCase() + entry.kind.slice(1)}</strong><strong>{formatCreditUnits(entry.amount_units, true)}</strong></div>
              <p>{entry.reason}</p>
              <p className="muted compact">{new Date(entry.occurred_at).toLocaleString()} · Balance after entry: {formatCreditUnits(entry.balance_after_units)}</p>
              <details><summary>Entry details</summary><p className="credit-identifiers">Entry {entry.id}<br />Sequence {entry.sequence}<br />Recorded by {entry.actor_identity_id}{entry.reverses_entry_id && <><br />Reverses {entry.reverses_entry_id}</>}</p></details>
            </li>)}
          </ol>
        )}
        {account.next_before_sequence && <button className="button-secondary" type="button" disabled={loading} onClick={() => setBefore(account.next_before_sequence)}>Older entries</button>}
      </>}
    </section>
  );
}

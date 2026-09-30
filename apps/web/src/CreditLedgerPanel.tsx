import { useEffect, useState } from "react";
import type { User } from "firebase/auth";
import { creditHistoryUrl, formatCreditUnits, parseCreditAccount, type CreditAccount } from "./creditLedger";

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
      <p className="muted compact">Credit-writing controls are not enabled in this release.</p>
      {loading && <p role="status">Loading credit history…</p>}
      {error && <p className="notice notice--error" role="alert">{error}</p>}
      {account && <>
        <p className="credit-balance">{account.account_exists ? <><strong>{formatCreditUnits(account.balance_units)}</strong><span>Balance at last refresh</span></> : <><strong>No credit account yet</strong><span>No credits have been granted.</span></>}</p>
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

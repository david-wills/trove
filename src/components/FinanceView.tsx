import { useCallback, useEffect, useMemo, useState } from "react";
import {
  api,
  FinanceAccount,
  FinanceOverview,
  FinanceTransaction,
} from "../api";

// The Bridge refreshes ~daily and the vault only changes on sync; slow poll.
const REFRESH_MS = 30_000;

/** How many transactions the browser shows at once. */
const PAGE = 200;

/** Display-only: amounts are decimal strings in the vault; parsing here is
 *  purely for formatting and never written back. */
function fmtAmount(amount: string, currency: string): string {
  const n = Number(amount);
  if (!Number.isFinite(n)) return amount;
  if (/^[A-Z]{3}$/.test(currency)) {
    return n.toLocaleString([], { style: "currency", currency });
  }
  return n.toLocaleString([], { minimumFractionDigits: 2 });
}

function fmtSynced(rfc3339: string): string {
  const mins = Math.round((Date.now() - new Date(rfc3339).getTime()) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  if (mins < 48 * 60) return `${Math.floor(mins / 60)}h ago`;
  return `${Math.floor(mins / (60 * 24))}d ago`;
}

/** Net worth per currency (accounts without a snapshot yet are skipped). */
function totalsByCurrency(accounts: FinanceAccount[]): [string, number][] {
  const sums = new Map<string, number>();
  for (const a of accounts) {
    if (a.balance === null) continue;
    const n = Number(a.balance);
    if (!Number.isFinite(n)) continue;
    sums.set(a.currency, (sums.get(a.currency) ?? 0) + n);
  }
  return [...sums.entries()];
}

export default function FinanceView() {
  const [overview, setOverview] = useState<FinanceOverview | null>(null);
  const [txns, setTxns] = useState<FinanceTransaction[]>([]);
  const [account, setAccount] = useState<string>("");
  const [filter, setFilter] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    const [o, t] = await Promise.all([
      api.financeOverview(),
      api.financeTransactions(account || null, PAGE),
    ]);
    setOverview(o);
    setTxns(t);
  }, [account]);

  useEffect(() => {
    let active = true;
    const tick = () => {
      if (active) refresh().catch((e) => setError(String(e)));
    };
    tick();
    const id = setInterval(tick, REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [refresh]);

  const syncNow = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const outcome = await api.integrationPull("bank-sync");
      setNotice(outcome.headline);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const accountName = useMemo(() => {
    const names = new Map<string, string>();
    for (const a of overview?.accounts ?? []) names.set(a.id, a.name);
    return (id: string) => names.get(id) ?? id;
  }, [overview]);

  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return txns;
    return txns.filter(
      (t) =>
        t.description.toLowerCase().includes(q) ||
        (t.payee ?? "").toLowerCase().includes(q) ||
        t.amount.includes(q)
    );
  }, [txns, filter]);

  const totals = totalsByCurrency(overview?.accounts ?? []);

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Finance</h2>
          <div className="activity-sub">
            {overview?.connected ? (
              overview.state?.updated ? (
                <>SimpleFIN · synced {fmtSynced(overview.state.updated)}</>
              ) : (
                <>SimpleFIN connected · first sync pending</>
              )
            ) : (
              <>Not connected</>
            )}
          </div>
        </div>
        {overview?.connected && (
          <button className="btn-primary" onClick={syncNow} disabled={busy}>
            {busy ? "Syncing…" : "Sync now"}
          </button>
        )}
      </div>

      {error && <div className="sync-error">{error}</div>}
      {notice && <div className="sync-notice">{notice}</div>}
      {overview?.state?.error && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Bank sync needs attention.</strong> {overview.state.error}{" "}
            Connection repairs happen at bridge.simplefin.org, not in Trove.
          </div>
        </div>
      )}

      {overview && !overview.connected && overview.accounts.length === 0 && (
        <div className="activity-empty">
          No financial data yet. Connect your banks on the Integrations tab —
          Trove pulls balances and transactions once a day through your own
          SimpleFIN Bridge credential, into plain files under{" "}
          <code>~/Trove/finance/</code>.
        </div>
      )}

      {totals.length > 0 && (
        <div className="activity-stats">
          {totals.map(([currency, total]) => (
            <div className="stat" key={currency}>
              <div className="stat-value">
                {fmtAmount(total.toFixed(2), currency)}
              </div>
              <div className="stat-label">across accounts ({currency})</div>
            </div>
          ))}
          <div className="stat muted">
            <div className="stat-value">{overview?.accounts.length ?? 0}</div>
            <div className="stat-label">accounts</div>
          </div>
        </div>
      )}

      {overview && overview.accounts.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">Accounts</div>
          {overview.accounts.map((a) => (
            <div key={a.id} className="tl-row">
              <div className="tl-time">{a.balance_date ?? "—"}</div>
              <div className="tl-body">
                <span className="tl-app">{a.name}</span>
                <span className="tl-title">{a.org}</span>
              </div>
              <div className="fin-amount">
                {a.balance !== null ? fmtAmount(a.balance, a.currency) : "—"}
              </div>
            </div>
          ))}
        </div>
      )}

      {(txns.length > 0 || account || filter) && (
        <div className="activity-timeline">
          <div className="activity-section-title fin-txn-head">
            <span>Transactions · newest {PAGE}</span>
            <span className="fin-controls">
              <select
                className="fin-select"
                value={account}
                onChange={(e) => setAccount(e.target.value)}
              >
                <option value="">All accounts</option>
                {(overview?.accounts ?? []).map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name}
                  </option>
                ))}
              </select>
              <input
                className="fin-filter"
                type="text"
                placeholder="filter…"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                spellCheck={false}
              />
            </span>
          </div>
          {shown.map((t) => (
            <div key={`${t.account}/${t.id}`} className="tl-row">
              <div className="tl-time">{t.posted.slice(5)}</div>
              <div className="tl-body">
                <span className="tl-app">
                  {t.payee ?? t.description}
                  {t.pending && <span className="fin-pending">pending</span>}
                </span>
                <span className="tl-title">{accountName(t.account)}</span>
              </div>
              <div
                className={`fin-amount ${
                  t.amount.startsWith("-") ? "" : "fin-amount-pos"
                }`}
              >
                {fmtAmount(t.amount, t.currency)}
              </div>
            </div>
          ))}
          {shown.length === 0 && (
            <div className="activity-empty">Nothing matches.</div>
          )}
        </div>
      )}

      <div className="activity-footnote">
        Files are the source of truth: accounts in{" "}
        <code>~/Trove/finance/accounts.jsonl</code>, transactions per account
        and year under <code>finance/transactions/</code>, daily balance
        snapshots under <code>finance/balances/</code>. Read-only — Trove can
        never move money.
      </div>
    </div>
  );
}

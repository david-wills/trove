import { useState } from "react";
import { api } from "../api";
import type {
  ConnectMethodInfo,
  ConnectStatus,
  ConnectedAccount,
  ConnectionStatusRow,
  PullOutcome,
} from "../bindings";

/** "2026-06-12T09:30:00…" → "2026-06-12 09:30" (same trim the hub uses). */
function fmtWhen(s: string): string {
  return s.slice(0, 16).replace("T", " ");
}

/** Epoch seconds → local "YYYY-MM-DD HH:MM". */
function fmtEpoch(secs: number): string {
  const d = new Date(secs * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(
    d.getDate()
  )} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** Generic connect section for one registered connection — rendered straight
 *  off a `ConnectionStatusRow`, zero service-specific branches. Covers the
 *  affordances of the old per-service sections (TickTick/Oura/SimpleFIN/
 *  Google): OAuth login + bring-your-own-credentials form, token paste,
 *  multi-account rows with reconnect/disconnect, and post-connect auto-pull
 *  of the connection's enabled integrations. */
export function ConnectCard({
  row,
  enabledIds,
  onStatusChange,
  onPulled,
}: {
  row: ConnectionStatusRow;
  enabledIds: Set<string>;
  onStatusChange: (s: ConnectStatus) => void;
  onPulled?: (id: string, outcome: PullOutcome) => void;
}) {
  // Busy keys are local to this card: "oauth", "token:<i>", "disconnect".
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Token-paste inputs, keyed by method index (a row may carry several).
  const [tokens, setTokens] = useState<Record<number, string>>({});
  // Bring-your-own-credentials form (oauth methods share one client app).
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [showCredsForm, setShowCredsForm] = useState(false);
  // Two-step disconnect: first click arms, second click runs.
  const [confirmKey, setConfirmKey] = useState<string | null>(null);
  // Registry-supplied BYO setup steps (redirect URIs, dev-console clicks).
  const [showSetup, setShowSetup] = useState(false);
  // Auto-pulls in flight after a connect (integration ids).
  const [syncing, setSyncing] = useState<Set<string>>(new Set());

  // Bindings can claim fields are required that arrive undefined at runtime —
  // optional-chain every record access (status, accounts, extra, …).
  const accounts: ConnectedAccount[] = row.status?.accounts ?? [];
  const configured = row.status?.configured ?? false;
  const methods: ConnectMethodInfo[] = row.methods ?? [];

  /** Fire-and-forget pulls of the connection's enabled integrations after a
   *  successful connect, so the first backfill happens on the visible path. */
  const startAutoPulls = () => {
    for (const id of row.auto_pull ?? []) {
      if (!enabledIds.has(id)) continue;
      setSyncing((prev) => new Set(prev).add(id));
      api
        .integrationPull(id)
        .then((outcome) => onPulled?.(id, outcome))
        .catch((e) => setError(String(e)))
        .finally(() =>
          setSyncing((prev) => {
            const next = new Set(prev);
            next.delete(id);
            return next;
          })
        );
    }
  };

  /** Run one connect method; on success report the new status up and kick
   *  off the auto-pulls. */
  const connect = async (
    method: string,
    params: Record<string, string>,
    busyKey: string
  ) => {
    setError(null);
    setNotice(null);
    setBusy(busyKey);
    try {
      const status = await api.connectRun(row.id, method, params);
      onStatusChange(status);
      setNotice(`${row.display_name} connected.`);
      setTokens({});
      setClientId("");
      setClientSecret("");
      setShowCredsForm(false);
      startAutoPulls();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  };

  const disconnect = async (key: string) => {
    setError(null);
    setNotice(null);
    setConfirmKey(null);
    setBusy("disconnect");
    try {
      const status = await api.connectDisconnect(row.id, key);
      onStatusChange(status);
      setNotice("Disconnected. Synced data stays in the vault.");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  };

  const oauth = methods.find((m) => m?.kind === "oauth");

  const renderOauth = (m: Extract<ConnectMethodInfo, { kind: "oauth" }>) => {
    if (busy === "oauth") {
      return (
        <div className="sync-waiting">
          Waiting for you to approve {row.display_name} in the browser… (the
          sign-in page just opened; this times out after 5 minutes)
        </div>
      );
    }

    // No app credentials saved or compiled in: bring-your-own first.
    if (!configured || showCredsForm) {
      const ready = clientId.trim().length > 0;
      return (
        <>
          <div className="sync-setup-hint">
            One-time setup — create an OAuth app with {row.display_name} and
            paste its credentials below; they're saved, so every future
            connect is just a login.
            {(row.setup?.length ?? 0) > 0 && (
              <>
                {" "}
                <button
                  className="sync-link"
                  onClick={() => setShowSetup(!showSetup)}
                >
                  {showSetup ? "hide setup steps" : "show setup steps"}
                </button>
              </>
            )}
          </div>
          <div className="sync-form">
            <input
              type="text"
              placeholder="Client ID"
              value={clientId}
              onChange={(e) => setClientId(e.target.value)}
              spellCheck={false}
            />
            <input
              type="password"
              placeholder="Client Secret (if your app has one)"
              value={clientSecret}
              onChange={(e) => setClientSecret(e.target.value)}
            />
            <button
              className="btn-primary"
              disabled={busy !== null || !ready}
              onClick={() => {
                const params: Record<string, string> = {
                  client_id: clientId.trim(),
                };
                if (clientSecret.trim()) params.client_secret = clientSecret.trim();
                connect(m.kind, params, "oauth");
              }}
            >
              Connect…
            </button>
            {!ready && busy === null && (
              <span className="int-actions-note">
                paste the client ID first
              </span>
            )}
            {configured && (
              <button
                className="sync-link"
                onClick={() => setShowCredsForm(false)}
              >
                cancel
              </button>
            )}
          </div>
        </>
      );
    }

    // Credentials in place: connecting is just the button.
    return (
      <div className="sync-form">
        <button
          className="btn-primary"
          disabled={busy !== null}
          onClick={() => connect(m.kind, {}, "oauth")}
        >
          {m.multi_account && accounts.length > 0
            ? "Add another account…"
            : `Connect ${row.display_name} account…`}
        </button>
        <button className="sync-link" onClick={() => setShowCredsForm(true)}>
          use different credentials
        </button>
      </div>
    );
  };

  const renderTokenPaste = (
    m: Extract<ConnectMethodInfo, { kind: "token-paste" }>,
    i: number
  ) => {
    const value = tokens[i] ?? "";
    const ready = value.trim().length > 0;
    const busyKey = `token:${i}`;
    return (
      <div key={busyKey}>
        {/* The help line doubles as the affordance hint for the gated button. */}
        <div className="sync-setup-hint">
          <strong>{m.label ?? "Token"}</strong>
          {m.help ? ` — ${m.help}` : ""}
          {!oauth && (row.setup?.length ?? 0) > 0 && (
            <>
              {" "}
              <button
                className="sync-link"
                onClick={() => setShowSetup(!showSetup)}
              >
                {showSetup ? "hide setup steps" : "show setup steps"}
              </button>
            </>
          )}
        </div>
        <div className="sync-form">
          <input
            type="password"
            placeholder={m.placeholder ?? ""}
            value={value}
            onChange={(e) => setTokens({ ...tokens, [i]: e.target.value })}
            spellCheck={false}
          />
          <button
            className="btn-primary"
            disabled={busy !== null || !ready}
            onClick={() => connect(m.kind, { token: value.trim() }, busyKey)}
          >
            {busy === busyKey ? "Connecting…" : "Connect"}
          </button>
        </div>
      </div>
    );
  };

  return (
    <div className="sync-card int-card connect-card">
      {/* Connected accounts */}
      {accounts.length > 0 && (
        <div className="connect-accounts">
          {accounts.map((a, i) => (
            <div className="int-actions connect-account" key={a?.key ?? i}>
              <span className="int-actions-note connect-account-label">
                <strong>{a?.label ?? a?.key}</strong>
                {a?.connected_at && (
                  <span className="connect-account-meta">
                    {" "}
                    · connected {fmtWhen(a.connected_at)}
                  </span>
                )}
                {a?.expires_at != null && !a?.needs_reconnect && (
                  <span className="connect-account-meta">
                    {" "}
                    · expires {fmtEpoch(a.expires_at)}
                  </span>
                )}
                {Object.entries(a?.extra ?? {}).map(
                  ([k, v]) =>
                    v && (
                      <span className="connect-account-meta" key={k} title={k}>
                        {" "}
                        {/* Timestamp-shaped extras get the key as a label so
                            a bare RFC3339 isn't cryptic. */}
                        · {/^\d{4}-\d{2}-\d{2}T/.test(v)
                          ? `${k.replace(/_/g, " ")} ${fmtWhen(v)}`
                          : v}
                      </span>
                    )
                )}
                {a?.needs_reconnect && (
                  <span className="int-badge int-badge-warn">reconnect</span>
                )}
              </span>
              {a?.needs_reconnect && oauth && busy !== "oauth" && (
                <button
                  className="btn-primary"
                  disabled={busy !== null}
                  onClick={() => connect(oauth.kind, {}, "oauth")}
                >
                  Reconnect
                </button>
              )}
              {confirmKey === a?.key ? (
                <>
                  <button
                    className="btn-ghost connect-confirm"
                    disabled={busy !== null}
                    onClick={() => disconnect(a?.key ?? "")}
                  >
                    {busy === "disconnect"
                      ? "Disconnecting…"
                      : "Really disconnect?"}
                  </button>
                  <button
                    className="sync-link"
                    onClick={() => setConfirmKey(null)}
                  >
                    keep it
                  </button>
                </>
              ) : (
                <button
                  className="btn-ghost"
                  disabled={busy !== null}
                  onClick={() => setConfirmKey(a?.key ?? null)}
                >
                  Disconnect
                </button>
              )}
            </div>
          ))}
        </div>
      )}

      {/* Connect methods. Single-login connections hide them once connected
          (the account row's Reconnect/Disconnect covers the rest); only
          multi-account OAuth keeps offering "Add another account…". */}
      {methods.map((m, i) =>
        m?.kind === "oauth" && (m.multi_account || accounts.length === 0) ? (
          <div key={`oauth:${i}`}>{renderOauth(m)}</div>
        ) : m?.kind === "token-paste" && accounts.length === 0 ? (
          renderTokenPaste(m, i)
        ) : null
      )}

      {showSetup && (
        <ol className="sync-steps">
          {(row.setup ?? []).map((step, i) => (
            <li key={i}>{step}</li>
          ))}
        </ol>
      )}

      {/* Post-connect auto-pulls in flight */}
      {syncing.size > 0 && (
        <div className="connect-syncing">
          syncing {Array.from(syncing).join(", ")}…
        </div>
      )}

      {error && <div className="connect-error">{error}</div>}
      {notice && !error && <div className="connect-notice">{notice}</div>}
    </div>
  );
}

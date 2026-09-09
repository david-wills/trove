import { useCallback, useEffect, useRef, useState } from "react";
import { api, GmailSyncState, Message } from "../api";

type Range = "7d" | "30d" | "90d" | "all";

const RANGES: { id: Range; label: string; days: number | null }[] = [
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
  { id: "90d", label: "90 Days", days: 90 },
  { id: "all", label: "All", days: null },
];

/** Page size for the list; "Load more" appends another. */
const PAGE = 50;
// Gmail incremental sync runs every 15 minutes; refresh well under that so
// a manual "Sync now" shows up quickly.
const REFRESH_MS = 30000;

/** YYYY-MM-DD in local time (messages are stored in local time). */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function fmtWhen(rfc3339: string): string {
  const d = new Date(rfc3339);
  const today = localDate(new Date());
  if (rfc3339.startsWith(today)) {
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  return d.toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" });
}

function fmtSynced(rfc3339: string): string {
  const mins = Math.round((Date.now() - new Date(rfc3339).getTime()) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m ago`;
}

// Fields the Rust side skips serializing when empty (to, labels,
// attachments, …) arrive as undefined despite the generated type — always
// access them defensively (the MessagesView `m.attachments?.length` rule).

/** Best display for who an email is from / to. */
function senderLabel(m: Message): string {
  if (m.from_me) return m.to?.length ? `→ ${m.to[0]}` : "→ (sent)";
  return m.sender_name || m.sender || "(unknown)";
}

/** First line of the body, for the collapsed row. */
function snippet(m: Message): string {
  const line = m.text.split("\n").find((l) => l.trim().length > 0) ?? "";
  return line.length > 160 ? `${line.slice(0, 160)}…` : line;
}

/** User-meaningful labels only — hide Gmail's plumbing labels. */
function displayLabels(labels: string[] | undefined): string[] {
  return (labels ?? [])
    .filter((l) => !["INBOX", "UNREAD", "IMPORTANT", "SENT"].includes(l))
    .map((l) =>
      l.startsWith("CATEGORY_")
        ? l.slice("CATEGORY_".length).toLowerCase()
        : l.toLowerCase()
    );
}

export default function EmailView() {
  const [range, setRange] = useState<Range>("30d");
  const [account, setAccount] = useState<string | null>(null);
  const [queryInput, setQueryInput] = useState("");
  const [query, setQuery] = useState("");
  const [messages, setMessages] = useState<Message[]>([]);
  const [total, setTotal] = useState(0);
  const [accounts, setAccounts] = useState<Record<string, number>>({});
  const [sync, setSync] = useState<GmailSyncState | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // Guards against a stale slow response landing after a newer one.
  const fetchSeq = useRef(0);

  const dates = useCallback((r: Range): [string, string] => {
    const today = new Date();
    const to = localDate(today);
    const days = RANGES.find((x) => x.id === r)!.days;
    if (days === null) return ["1970-01-01", to];
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (days - 1));
    return [localDate(fromDate), to];
  }, []);

  const refresh = useCallback(
    async (r: Range, acct: string | null, q: string) => {
      const seq = ++fetchSeq.current;
      const [from, to] = dates(r);
      const [page, info] = await Promise.all([
        api.emailList(from, to, acct, q || null, PAGE, 0),
        api.gmailSyncInfo(),
      ]);
      if (seq !== fetchSeq.current) return;
      setMessages(page.messages);
      setTotal(Number(page.total));
      setAccounts(page.accounts);
      setSync(info);
    },
    [dates]
  );

  // Debounce typing into one applied query.
  useEffect(() => {
    const id = setTimeout(() => setQuery(queryInput.trim()), 300);
    return () => clearTimeout(id);
  }, [queryInput]);

  useEffect(() => {
    let active = true;
    const tick = () => {
      if (active) refresh(range, account, query).catch(() => {});
    };
    tick();
    const id = setInterval(tick, REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [range, account, query, refresh]);

  const loadMore = async () => {
    setLoading(true);
    try {
      const [from, to] = dates(range);
      const page = await api.emailList(
        from,
        to,
        account,
        query || null,
        PAGE,
        messages.length
      );
      setMessages((prev) => [...prev, ...page.messages]);
      setTotal(Number(page.total));
    } finally {
      setLoading(false);
    }
  };

  const syncAccounts = sync ? Object.values(sync.accounts) : [];
  const backfilling = syncAccounts.some((a) => !a.backfill_done);
  const syncLine = !sync || !sync.updated
    ? "Waiting for the first Gmail sync — connect a Google account in Integrations, or import an .mbox in Messages."
    : `Gmail synced ${fmtSynced(sync.updated)}${backfilling ? " · backfilling history…" : ""}`;
  const accountEntries = Object.entries(accounts).sort();
  const haveAll = messages.length >= total;

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Email</h2>
          <div className="activity-sub">{syncLine}</div>
        </div>
        <div className="segmented">
          {RANGES.map((r) => (
            <button
              key={r.id}
              className={range === r.id ? "active" : ""}
              onClick={() => setRange(r.id)}
            >
              {r.label}
            </button>
          ))}
        </div>
      </div>

      <div className="email-controls">
        <input
          type="search"
          spellCheck={false}
          placeholder="Search sender, subject, or text…"
          value={queryInput}
          onChange={(e) => setQueryInput(e.target.value)}
        />
        {accountEntries.length > 1 && (
          <div className="segmented">
            <button
              className={account === null ? "active" : ""}
              onClick={() => setAccount(null)}
            >
              All accounts
            </button>
            {accountEntries.map(([a, n]) => (
              <button
                key={a}
                className={account === a ? "active" : ""}
                onClick={() => setAccount(a)}
                title={a}
              >
                {a.split("@")[0]} · {n}
              </button>
            ))}
          </div>
        )}
      </div>

      <div className="activity-section-title">
        {total === 0
          ? "No email in this range"
          : `${total} email${total === 1 ? "" : "s"}${query ? ` matching “${query}”` : ""}, newest first`}
      </div>

      {total === 0 && (
        <div className="activity-empty">
          Nothing here yet. Gmail pulls automatically every 15 minutes once a
          Google account is connected (Integrations tab); older archives can
          be imported as .mbox from the Messages tab. Try a wider range — the
          full history lives under “All”.
        </div>
      )}

      <div className="email-list">
        {messages.map((m) => {
          const key = m.guid || `${m.ts}/${m.sender}`;
          const open = expanded === key;
          return (
            <div
              key={key}
              className={`email-row ${open ? "open" : ""}`}
              onClick={() => setExpanded(open ? null : key)}
            >
              <div className="email-row-head">
                <span className={`email-sender ${m.from_me ? "sent" : ""}`}>
                  {senderLabel(m)}
                </span>
                <span className="email-subject">
                  {m.subject || "(no subject)"}
                </span>
                {!open && <span className="email-snippet">{snippet(m)}</span>}
                <span className="email-when">{fmtWhen(m.ts)}</span>
              </div>
              {open && (
                <div className="email-detail" onClick={(e) => e.stopPropagation()}>
                  <div className="email-meta">
                    <div>
                      <strong>From:</strong>{" "}
                      {m.from_me ? `me (${m.service})` : `${m.sender_name ? `${m.sender_name} <${m.sender}>` : m.sender}`}
                    </div>
                    {(m.to?.length ?? 0) > 0 && (
                      <div>
                        <strong>To:</strong> {m.to.join(", ")}
                      </div>
                    )}
                    <div>
                      <strong>Account:</strong> {m.service}
                      {displayLabels(m.labels).length > 0 && (
                        <> · {displayLabels(m.labels).join(", ")}</>
                      )}
                    </div>
                    {(m.attachments?.length ?? 0) > 0 && (
                      <div>
                        <strong>Attachments:</strong>{" "}
                        {m.attachments
                          .map((a) => a.name || a.mime || "attachment")
                          .join(", ")}{" "}
                        (metadata only)
                      </div>
                    )}
                  </div>
                  <pre className="email-body">{m.text || "(no text body)"}</pre>
                </div>
              )}
            </div>
          );
        })}
      </div>

      {!haveAll && (
        <div className="perm-actions email-more">
          <button className="btn-ghost" disabled={loading} onClick={loadMore}>
            {loading
              ? "Loading…"
              : `Load more (${messages.length} of ${total})`}
          </button>
        </div>
      )}

      <div className="activity-footnote">
        Email is stored complete in your vault —{" "}
        <code>~/Trove/correspondence/email/</code>, one JSONL file per month,
        shared by Gmail sync and .mbox imports and deduped by Message-ID.
        Bodies are full text; attachments are metadata only unless opted in.
      </div>
    </div>
  );
}

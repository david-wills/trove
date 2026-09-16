import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  api,
  CorrespondenceSummary,
  IMessageSyncState,
  Message,
  SeriesPoint,
} from "../api";
import Chart from "./Chart";

// macOS deep-link to the Full Disk Access privacy pane (no programmatic
// prompt exists for FDA) — same banner pattern as Safari history.
const FDA_SETTINGS_URL =
  "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

type Range = "today" | "7d" | "30d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "today", label: "Today", days: 1 },
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
];

// iMessage syncs every 15 minutes from the watcher loop.
const REFRESH_MS = 15000;

const SOURCE_LABEL: Record<string, string> = {
  imessage: "iMessage",
  email: "Email",
  slack: "Slack",
};

/** YYYY-MM-DD in local time (messages are stored in local time). */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function fmtClock(rfc3339: string): string {
  return new Date(rfc3339).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function fmtSynced(rfc3339: string): string {
  const mins = Math.round((Date.now() - new Date(rfc3339).getTime()) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m ago`;
}

/** Who/where a message line is best labeled as. */
function chatLabel(m: { chat: string; chat_name?: string }): string {
  return m.chat_name || m.chat || "(unknown)";
}

export default function MessagesView() {
  const [range, setRange] = useState<Range>("7d");
  const [summary, setSummary] = useState<CorrespondenceSummary | null>(null);
  const [daily, setDaily] = useState<SeriesPoint[]>([]);
  const [timeline, setTimeline] = useState<Message[]>([]);
  const [sync, setSync] = useState<IMessageSyncState | null>(null);
  const [hasPerm, setHasPerm] = useState(true);
  const [dismissedPerm, setDismissedPerm] = useState(false);
  const [importNote, setImportNote] = useState<string>("");
  const [importing, setImporting] = useState(false);
  const [mboxAccount, setMboxAccount] = useState("");
  const [askAccount, setAskAccount] = useState<"" | "mbox" | "slack">("");

  const refresh = useCallback(async (r: Range) => {
    const today = new Date();
    const to = localDate(today);
    const days = RANGES.find((x) => x.id === r)!.days;
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (days - 1));
    const from = localDate(fromDate);

    const [s, info, perm] = await Promise.all([
      api.correspondenceSummary(from, to),
      api.imessageSyncInfo(),
      api.imessagePermission(),
    ]);
    setSummary(s);
    setSync(info);
    setHasPerm(perm);
    if (r === "today") {
      setTimeline(await api.correspondenceTimeline(to));
    } else {
      setDaily(await api.correspondenceDaily(from, to));
    }
  }, []);

  useEffect(() => {
    let active = true;
    const tick = () => {
      if (active) refresh(range).catch(() => {});
    };
    tick();
    const id = setInterval(tick, REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [range, refresh]);

  const pickMbox = async () => {
    if (!mboxAccount.includes("@")) {
      setImportNote("Enter the mailbox's email address first.");
      return;
    }
    const file = await open({
      multiple: false,
      filters: [{ name: "Mailbox", extensions: ["mbox"] }],
    });
    if (!file) return;
    setImporting(true);
    setImportNote("Importing mbox…");
    try {
      const outcome = await api.runImport("email", file as string, {
        account: mboxAccount.trim(),
      });
      setImportNote(outcome.headline);
      refresh(range).catch(() => {});
    } catch (e) {
      setImportNote(`Import failed: ${e}`);
    } finally {
      setImporting(false);
      setAskAccount("");
    }
  };

  const pickSlack = async () => {
    const file = await open({
      multiple: false,
      filters: [{ name: "Slack export", extensions: ["zip"] }],
    });
    if (!file) return;
    setImporting(true);
    setImportNote("Importing Slack export…");
    try {
      const me = mboxAccount.trim();
      const outcome = await api.runImport(
        "slack",
        file as string,
        me ? { me } : {}
      );
      setImportNote(outcome.headline);
      refresh(range).catch(() => {});
    } catch (e) {
      setImportNote(`Import failed: ${e}`);
    } finally {
      setImporting(false);
      setAskAccount("");
    }
  };

  const maxChat =
    summary && summary.chats.length > 0 ? summary.chats[0].messages : 1;
  const neverSynced = !sync || !sync.updated;
  const sourceLine = summary
    ? Object.entries(summary.sources)
        .map(([s, n]) => `${SOURCE_LABEL[s] ?? s} ${n}`)
        .join(" · ")
    : "";

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Messages</h2>
          <div className="activity-sub">
            {neverSynced ? (
              <>Waiting for the first message sync…</>
            ) : (
              <>iMessage synced {fmtSynced(sync.updated)}</>
            )}
            {sourceLine && <> · {sourceLine}</>}
          </div>
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

      {!hasPerm && !dismissedPerm && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Messages history is locked.</strong> Reading the iMessage
            database needs <strong>Full Disk Access</strong>. Grant it to
            Trove in System Settings, then restart — the full history
            imports automatically.
          </div>
          <div className="perm-actions">
            <button
              className="btn-primary"
              onClick={() => openUrl(FDA_SETTINGS_URL).catch(() => {})}
            >
              Open Settings
            </button>
            <button className="btn-ghost" onClick={() => setDismissedPerm(true)}>
              Not now
            </button>
          </div>
        </div>
      )}

      {summary && (
        <div className="activity-stats">
          <Stat label="Messages" value={String(summary.messages)} />
          <Stat label="Sent" value={String(summary.sent)} muted />
          <Stat label="Received" value={String(summary.received)} muted />
          <Stat
            label="Conversations"
            value={String(summary.chats.length)}
            muted
          />
        </div>
      )}

      {summary && summary.messages === 0 && (
        <div className="activity-empty">
          No messages in this range yet. iMessage imports automatically every
          15 minutes once Full Disk Access is granted (the first sync pulls
          the entire history). Email arrives via Gmail sync (connect a Google
          account in Integrations) or the .mbox import below; Slack via its
          export import.
        </div>
      )}

      {summary && summary.chats.length > 0 && (
        <div className="app-bars">
          {summary.chats.slice(0, 12).map((c) => (
            <div key={`${c.source}/${c.chat}`} className="app-bar">
              <div className="app-bar-name" title={c.chat}>
                {chatLabel(c)}
              </div>
              <div className="app-bar-track">
                <div
                  className="app-bar-fill"
                  style={{ width: `${(c.messages / maxChat) * 100}%` }}
                />
              </div>
              <div className="app-bar-time">{c.messages}</div>
            </div>
          ))}
        </div>
      )}

      {range !== "today" && daily.length > 0 && (
        <div className="activity-trend">
          <div className="activity-section-title">Messages per day</div>
          <Chart points={daily} name="Messages" unit="" kind="sum" />
        </div>
      )}

      {range === "today" && timeline.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">Today, most recent first</div>
          {[...timeline]
            .filter((m) => m.kind === "message")
            .sort((a, b) => (a.ts < b.ts ? 1 : -1))
            .slice(0, 60)
            .map((m, i) => (
              <div key={i} className="tl-row">
                <div className="tl-time">{fmtClock(m.ts)}</div>
                <div className="tl-body">
                  <span className="tl-app">
                    {m.from_me ? `→ ${chatLabel(m)}` : m.sender_name || m.sender}
                  </span>
                  <span className="tl-title">
                    {m.subject ? `${m.subject} — ` : ""}
                    {m.text ||
                      (m.attachments?.length
                        ? `📎 ${m.attachments[0].name || "attachment"}`
                        : "")}
                  </span>
                </div>
              </div>
            ))}
        </div>
      )}

      <div className="activity-trend">
        <div className="activity-section-title">Import archives</div>
        {askAccount === "" ? (
          <div className="perm-actions">
            <button
              className="btn-ghost"
              disabled={importing}
              onClick={() => setAskAccount("mbox")}
            >
              Import email (.mbox)…
            </button>
            <button
              className="btn-ghost"
              disabled={importing}
              onClick={() => setAskAccount("slack")}
            >
              Import Slack export (.zip)…
            </button>
          </div>
        ) : (
          <div className="sync-form">
            <input
              type="text"
              spellCheck={false}
              placeholder={
                askAccount === "mbox"
                  ? "Mailbox address (decides which mail is “sent”)"
                  : "Your Slack handle (optional, marks your messages)"
              }
              value={mboxAccount}
              onChange={(e) => setMboxAccount(e.target.value)}
              autoFocus
            />
            <button
              className="btn-primary"
              disabled={importing}
              onClick={askAccount === "mbox" ? pickMbox : pickSlack}
            >
              Choose file…
            </button>
            <button
              className="btn-ghost"
              disabled={importing}
              onClick={() => setAskAccount("")}
            >
              Cancel
            </button>
          </div>
        )}
        {importNote && <div className="activity-sub">{importNote}</div>}
      </div>

      <div className="activity-footnote">
        iMessage is imported read-only from the Messages database — Messages
        is never modified. Email arrives via Gmail sync and .mbox archives
        (browse it in the Email tab); Slack from exported archives. Raw
        messages: <code>~/Documents/Trove/correspondence/</code> — one JSONL file per
        source per month.
      </div>
    </div>
  );
}

function Stat({
  label,
  value,
  muted,
}: {
  label: string;
  value: string;
  muted?: boolean;
}) {
  return (
    <div className={`stat ${muted ? "muted" : ""}`}>
      <div className="stat-value">{value}</div>
      <div className="stat-label">{label}</div>
    </div>
  );
}

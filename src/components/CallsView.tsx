import { useCallback, useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  api,
  CallsSummary,
  CallsSyncState,
  Message,
  SeriesPoint,
} from "../api";
import Chart from "./Chart";

// macOS deep-link to the Full Disk Access privacy pane (no programmatic
// prompt exists for FDA) — same banner pattern as Messages.
const FDA_SETTINGS_URL =
  "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

// Calls are sparse (a handful per day at most), so ranges start at a week —
// a "today" view would usually be empty.
type Range = "7d" | "30d" | "90d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
  { id: "90d", label: "90 Days", days: 90 },
];

// Call history syncs every 15 minutes from the watcher loop.
const REFRESH_MS = 15000;

const RECENT_LIMIT = 60;

/** YYYY-MM-DD in local time (calls are stored in local time). */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

/** "Jun 10, 9:44 PM" — the list spans days, so the date matters. */
function fmtWhen(rfc3339: string): string {
  return new Date(rfc3339).toLocaleString([], {
    month: "short",
    day: "numeric",
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

/** Seconds → "3h 12m" / "29m" / "45s" / "0m". */
function fmtTalk(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const h = Math.floor(secs / 3600);
  const m = Math.round((secs % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

function callerLabel(c: { contact: string; contact_name: string }): string {
  return c.contact_name || c.contact || "(unknown)";
}

/** Direction/outcome glyph for a call row. */
function callGlyph(m: Message): string {
  if (m.from_me) return "↗";
  return (m.duration_secs ?? 0) > 0 ? "↘" : "✕";
}

export default function CallsView() {
  const [range, setRange] = useState<Range>("30d");
  const [summary, setSummary] = useState<CallsSummary | null>(null);
  const [daily, setDaily] = useState<SeriesPoint[]>([]);
  const [recent, setRecent] = useState<Message[]>([]);
  const [sync, setSync] = useState<CallsSyncState | null>(null);
  const [hasPerm, setHasPerm] = useState(true);
  const [dismissedPerm, setDismissedPerm] = useState(false);

  const refresh = useCallback(async (r: Range) => {
    const today = new Date();
    const to = localDate(today);
    const days = RANGES.find((x) => x.id === r)!.days;
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (days - 1));
    const from = localDate(fromDate);

    const [s, d, list, info, perm] = await Promise.all([
      api.callsSummary(from, to),
      api.callsDaily(from, to),
      api.callsRecent(from, to, RECENT_LIMIT),
      api.callsSyncInfo(),
      api.callsPermission(),
    ]);
    setSummary(s);
    setDaily(d);
    setRecent(list);
    setSync(info);
    setHasPerm(perm);
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

  const maxCaller =
    summary && summary.callers.length > 0 ? summary.callers[0].calls : 1;
  const neverSynced = !sync || !sync.updated;

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Calls</h2>
          <div className="activity-sub">
            {neverSynced ? (
              <>Waiting for the first call history sync…</>
            ) : (
              <>Call history synced {fmtSynced(sync.updated)}</>
            )}
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
            <strong>Call history is locked.</strong> Reading the call history
            database needs <strong>Full Disk Access</strong> — the same grant
            Messages uses. Grant it to Trove in System Settings (and to
            troved, if installed), then restart — the full call log imports
            automatically.
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
          <Stat label="Calls" value={String(summary.calls)} />
          <Stat label="Talk time" value={fmtTalk(summary.talk_secs)} />
          <Stat label="Outgoing" value={String(summary.outgoing)} muted />
          <Stat label="Incoming" value={String(summary.incoming)} muted />
          <Stat label="Missed" value={String(summary.missed)} muted />
        </div>
      )}

      {summary && summary.calls === 0 && (
        <div className="activity-empty">
          No calls in this range yet. Phone and FaceTime history imports
          automatically every 15 minutes once Full Disk Access is granted
          (the first sync pulls the entire retained log).
        </div>
      )}

      {summary && summary.callers.length > 0 && (
        <div className="app-bars">
          {summary.callers.slice(0, 12).map((c) => (
            <div key={c.contact} className="app-bar">
              <div className="app-bar-name" title={c.contact}>
                {callerLabel(c)}
              </div>
              <div className="app-bar-track">
                <div
                  className="app-bar-fill"
                  style={{ width: `${(c.calls / maxCaller) * 100}%` }}
                />
              </div>
              <div className="app-bar-time" title={`${c.calls} calls`}>
                {c.calls} · {fmtTalk(c.talk_secs)}
              </div>
            </div>
          ))}
        </div>
      )}

      {daily.length > 0 && (
        <div className="activity-trend">
          <div className="activity-section-title">Calls per day</div>
          <Chart points={daily} name="Calls" unit="" kind="sum" />
        </div>
      )}

      {recent.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">
            Call log, most recent first
          </div>
          {recent.map((m, i) => (
            <div key={m.guid || i} className="tl-row">
              <div className="tl-time">{fmtWhen(m.ts)}</div>
              <div className="tl-body">
                <span className="tl-app">
                  {callGlyph(m)} {callerLabel({
                    contact: m.chat,
                    contact_name: m.chat_name ?? "",
                  })}
                </span>
                <span className="tl-title">
                  {m.service ? `${m.service} — ` : ""}
                  {m.text}
                </span>
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="activity-footnote">
        Calls are imported read-only from the system call history database —
        nothing is ever modified. Raw records:{" "}
        <code>~/Documents/Trove/correspondence/calls/</code> — one JSONL file per month.
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

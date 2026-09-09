import { useCallback, useEffect, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  api,
  BrowserSummary,
  BrowserSyncState,
  BrowserVisit,
  LiveSpan,
  SeriesPoint,
} from "../api";
import Chart from "./Chart";

// macOS deep-link to the Full Disk Access privacy pane (no programmatic
// prompt exists for FDA).
const FDA_SETTINGS_URL =
  "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

type Range = "today" | "7d" | "30d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "today", label: "Today", days: 1 },
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
];

// History syncs every 15 minutes from the watcher loop; polling faster than
// the activity view buys nothing.
const REFRESH_MS = 15000;

// "Watching now" is live, open-span state the extension publishes every ~5s
// (its snapshot cadence). Poll it on the same beat so the elapsed time ticks
// up smoothly while a video plays, instead of only appearing once it closes.
const LIVE_REFRESH_MS = 5000;

/** YYYY-MM-DD in local time (visits are stored in local time). */
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

/** Compact engaged time: "45s", "12m", "1h 4m". */
function fmtDuration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  const h = Math.floor(secs / 3600);
  const m = Math.round((secs % 3600) / 60);
  return m > 0 ? `${h}h ${m}m` : `${h}h`;
}

/** Bare host for a referrer hint, e.g. "https://www.google.com/x" → "google.com". */
function hostOf(url: string): string {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return url;
  }
}

function fmtSynced(rfc3339: string): string {
  const mins = Math.round((Date.now() - new Date(rfc3339).getTime()) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m ago`;
}

export default function WebView() {
  const [range, setRange] = useState<Range>("today");
  const [summary, setSummary] = useState<BrowserSummary | null>(null);
  const [daily, setDaily] = useState<SeriesPoint[]>([]);
  const [timeline, setTimeline] = useState<BrowserVisit[]>([]);
  const [sync, setSync] = useState<BrowserSyncState | null>(null);
  const [live, setLive] = useState<LiveSpan[]>([]);
  const [hasSafariPerm, setHasSafariPerm] = useState(true);
  const [dismissedPerm, setDismissedPerm] = useState(false);

  const refresh = useCallback(async (r: Range) => {
    const today = new Date();
    const to = localDate(today);
    const days = RANGES.find((x) => x.id === r)!.days;
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (days - 1));
    const from = localDate(fromDate);

    const [s, info, safariPerm] = await Promise.all([
      api.browserSummary(from, to),
      api.browserSyncInfo(),
      api.browserSafariPermission(),
    ]);
    setSummary(s);
    setSync(info);
    setHasSafariPerm(safariPerm);
    if (r === "today") {
      setTimeline(await api.browserTimeline(to));
    } else {
      setDaily(await api.browserDaily(from, to));
    }
  }, []);

  // Initial + range-change load, then poll so new syncs show up.
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

  // Live open spans, polled independently of the history refresh so the
  // "watching now" elapsed time advances on the extension's own cadence.
  useEffect(() => {
    let active = true;
    const tick = () => {
      api
        .browserLive()
        .then((spans) => {
          if (active) setLive(spans);
        })
        .catch(() => {});
    };
    tick();
    const id = setInterval(tick, LIVE_REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, []);

  const maxDomain =
    summary && summary.domains.length > 0 ? summary.domains[0].visits : 1;
  // Foreground tab first, then longest-engaged — the most relevant on top.
  const liveSorted = [...live].sort((a, b) =>
    a.foreground !== b.foreground
      ? a.foreground
        ? -1
        : 1
      : b.duration_secs - a.duration_secs,
  );
  const neverSynced = !sync || !sync.updated;
  // A live open span (something engaged right now) or any closed extension row
  // today both mean the extension is connected and capturing.
  const extensionLive =
    live.length > 0 || timeline.some((v) => v.source === "extension");

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Web</h2>
          <div className="activity-sub">
            {neverSynced ? (
              <>Waiting for the first history sync…</>
            ) : (
              <>
                Browser history · synced {fmtSynced(sync.updated)}
                {extensionLive && <> · extension live today</>}
              </>
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

      {!hasSafariPerm && !dismissedPerm && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Safari history is locked.</strong> Chrome imports work
            without permissions, but reading Safari's history needs{" "}
            <strong>Full Disk Access</strong>. Grant it to Trove in System
            Settings (and to troved, if installed), then restart — syncing
            resumes automatically.
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

      {liveSorted.length > 0 && (
        <div className="live-now">
          <div className="activity-section-title">
            <span className="live-dot" /> Watching now
          </div>
          {liveSorted.map((s) => (
            <div key={s.url} className="tl-row live-row">
              <div className="tl-body">
                <span className="tl-app">
                  {s.favicon && (
                    <img
                      className="tl-favicon"
                      src={s.favicon}
                      alt=""
                      loading="lazy"
                      onError={(e) => {
                        e.currentTarget.style.visibility = "hidden";
                      }}
                    />
                  )}
                  {s.title || s.url}
                  {s.audible && <span className="tl-badge">🔊 audio</span>}
                  {!s.foreground && s.audible && (
                    <span className="tl-badge">background</span>
                  )}
                </span>
                <span className="tl-title">{s.url}</span>
              </div>
              <div className="tl-dur">
                {fmtDuration(s.duration_secs)}
                {s.foreground_secs > 0 && s.foreground_secs < s.duration_secs && (
                  <span className="tl-dur-sub">
                    {fmtDuration(s.foreground_secs)} focus
                  </span>
                )}
              </div>
            </div>
          ))}
        </div>
      )}

      {summary && (
        <div className="activity-stats">
          <Stat label="Visits" value={String(summary.visits)} />
          <Stat label="Sites" value={String(summary.domains.length)} muted />
        </div>
      )}

      {summary && summary.visits === 0 && (
        <div className="activity-empty">
          No browsing history here yet. Trove imports Chrome and Safari
          history automatically every 15 minutes (the first import pulls the
          full retained history). Chrome needs no permissions; Safari needs
          Full Disk Access.
        </div>
      )}

      {summary && summary.domains.length > 0 && (
        <div className="app-bars">
          {summary.domains.slice(0, 12).map((d) => (
            <div key={d.domain} className="app-bar">
              <div className="app-bar-name" title={d.domain}>
                {d.domain}
              </div>
              <div className="app-bar-track">
                <div
                  className="app-bar-fill"
                  style={{ width: `${(d.visits / maxDomain) * 100}%` }}
                />
              </div>
              <div className="app-bar-time">{d.visits}</div>
            </div>
          ))}
        </div>
      )}

      {range !== "today" && daily.length > 0 && (
        <div className="activity-trend">
          <div className="activity-section-title">Visits per day</div>
          <Chart points={daily} name="Visits" unit="" kind="sum" />
        </div>
      )}

      {range === "today" && timeline.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">Today, most recent first</div>
          {[...timeline]
            .sort((a, b) => (a.time < b.time ? 1 : -1))
            .slice(0, 60)
            .map((v, i) => (
              <div key={i} className="tl-row">
                <div className="tl-time">{fmtClock(v.time)}</div>
                <div className="tl-body">
                  <span className="tl-app">
                    {v.favicon && (
                      <img
                        className="tl-favicon"
                        src={v.favicon}
                        alt=""
                        loading="lazy"
                        onError={(e) => {
                          e.currentTarget.style.visibility = "hidden";
                        }}
                      />
                    )}
                    {v.title || v.url}
                    {v.source === "extension" && (
                      <span className="tl-badge">live</span>
                    )}
                  </span>
                  <span className="tl-title">{v.url}</span>
                  {v.referrer && (
                    <span className="tl-from">from {hostOf(v.referrer)}</span>
                  )}
                </div>
                <div className="tl-dur">
                  {v.duration_secs > 0 ? (
                    fmtDuration(v.duration_secs)
                  ) : (
                    <span className="tl-dur-none">—</span>
                  )}
                  {v.source === "extension" &&
                    (v.audible || (v.foreground_secs ?? 0) > 0) && (
                      <span className="tl-dur-sub">
                        {v.audible && "🔊 "}
                        {(v.foreground_secs ?? 0) > 0 &&
                          `${fmtDuration(v.foreground_secs!)} focus`}
                      </span>
                    )}
                </div>
              </div>
            ))}
        </div>
      )}

      <div className="activity-footnote">
        Imported read-only from Chrome's and Safari's local history databases
        — the browsers are never modified. Raw visits:{" "}
        <code>~/Trove/browser/</code> — one JSONL file per day.
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

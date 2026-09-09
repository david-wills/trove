import { useCallback, useEffect, useMemo, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  ActivityEvent,
  ActivitySummary,
  api,
  AppUsage,
  SeriesPoint,
  WatcherStatus,
} from "../api";
import Chart from "./Chart";

type Range = "today" | "7d" | "30d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "today", label: "Today", days: 1 },
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
];

// macOS deep-link to the Screen Recording privacy pane.
const SETTINGS_URL =
  "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

const REFRESH_MS = 7000;

/** YYYY-MM-DD in local time (events are stored in local time). */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function fmtDuration(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const m = Math.round(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

function fmtClock(rfc3339: string): string {
  return new Date(rfc3339).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Fold the still-open event into the logged summary (no double-count). */
function withCurrent(
  summary: ActivitySummary,
  current: ActivityEvent | null
): ActivitySummary {
  if (!current || current.seconds <= 0) return summary;
  if (current.afk) {
    return { ...summary, afk_seconds: summary.afk_seconds + current.seconds };
  }
  const apps: AppUsage[] = summary.apps.map((a) => ({ ...a }));
  const hit = apps.find((a) => a.app === current.app);
  if (hit) hit.seconds += current.seconds;
  else apps.push({ app: current.app, seconds: current.seconds });
  apps.sort((a, b) => b.seconds - a.seconds || a.app.localeCompare(b.app));
  return {
    apps,
    active_seconds: summary.active_seconds + current.seconds,
    afk_seconds: summary.afk_seconds,
  };
}

export default function ActivityView() {
  const [range, setRange] = useState<Range>("today");
  const [summary, setSummary] = useState<ActivitySummary | null>(null);
  const [current, setCurrent] = useState<ActivityEvent | null>(null);
  const [daily, setDaily] = useState<SeriesPoint[]>([]);
  const [timeline, setTimeline] = useState<ActivityEvent[]>([]);
  const [hasTitlePerm, setHasTitlePerm] = useState(true);
  const [dismissedPerm, setDismissedPerm] = useState(false);
  const [watcher, setWatcher] = useState<WatcherStatus | null>(null);

  const refresh = useCallback(async (r: Range) => {
    const today = new Date();
    const to = localDate(today);
    const days = RANGES.find((x) => x.id === r)!.days;
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (days - 1));
    const from = localDate(fromDate);

    const [s, cur, ws] = await Promise.all([
      api.activitySummary(from, to),
      api.activityCurrent(),
      api.watcherStatus(),
    ]);
    setSummary(s);
    setCurrent(cur);
    setWatcher(ws);
    if (r === "today") {
      setTimeline(await api.activityTimeline(to));
    } else {
      setDaily(await api.activityDaily(from, to));
    }
  }, []);

  // Initial + range-change load, then poll so it stays live.
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

  useEffect(() => {
    api.activityPermission().then(setHasTitlePerm);
  }, []);

  const requestPerm = useCallback(async () => {
    const granted = await api.requestActivityPermission();
    setHasTitlePerm(granted);
    if (!granted) openUrl(SETTINGS_URL).catch(() => {});
  }, []);

  const view = useMemo(
    () => (summary ? withCurrent(summary, current) : null),
    [summary, current]
  );

  const liveApp = current && !current.afk ? current.app : null;
  const maxApp = view && view.apps.length > 0 ? view.apps[0].seconds : 1;

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Activity</h2>
          <div className="activity-sub">
            {liveApp ? (
              <>
                <span className="live-dot" /> Now in <strong>{liveApp}</strong>
              </>
            ) : (
              <>
                <span className="live-dot idle" /> Away
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

      {!hasTitlePerm && !dismissedPerm && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Window titles are hidden.</strong> Trove tracks which app
            you use without any permission. To also record window titles (the
            document or page you're looking at), grant{" "}
            <strong>Screen Recording</strong>.
          </div>
          <div className="perm-actions">
            <button className="btn-primary" onClick={requestPerm}>
              Grant access
            </button>
            <button className="btn-ghost" onClick={() => setDismissedPerm(true)}>
              Not now
            </button>
          </div>
        </div>
      )}

      {view && (
        <div className="activity-stats">
          <Stat label="Active" value={fmtDuration(view.active_seconds)} />
          <Stat label="Away" value={fmtDuration(view.afk_seconds)} muted />
          <Stat label="Apps" value={String(view.apps.length)} muted />
        </div>
      )}

      {view && view.apps.length === 0 && (
        <div className="activity-empty">
          No activity recorded yet. Trove logs the app you're using every few
          seconds while it's open — switch around and check back.
        </div>
      )}

      {view && view.apps.length > 0 && (
        <div className="app-bars">
          {view.apps.slice(0, 12).map((a) => (
            <div
              key={a.app}
              className={`app-bar ${a.app === liveApp ? "live" : ""}`}
            >
              <div className="app-bar-name" title={a.app}>
                {a.app || "Unknown"}
              </div>
              <div className="app-bar-track">
                <div
                  className="app-bar-fill"
                  style={{ width: `${(a.seconds / maxApp) * 100}%` }}
                />
              </div>
              <div className="app-bar-time">{fmtDuration(a.seconds)}</div>
            </div>
          ))}
        </div>
      )}

      {range !== "today" && daily.length > 0 && (
        <div className="activity-trend">
          <div className="activity-section-title">Active hours per day</div>
          <Chart points={daily} name="Active time" unit="hr" kind="sum" />
        </div>
      )}

      {range === "today" && timeline.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">Today, most recent first</div>
          {[...timeline]
            .reverse()
            .slice(0, 60)
            .map((e, i) => (
              <div key={i} className={`tl-row ${e.afk ? "afk" : ""}`}>
                <div className="tl-time">
                  {fmtClock(e.start)} – {fmtClock(e.end)}
                </div>
                <div className="tl-body">
                  <span className="tl-app">{e.afk ? "Away" : e.app}</span>
                  {e.title && <span className="tl-title">{e.title}</span>}
                </div>
                <div className="tl-dur">{fmtDuration(e.seconds)}</div>
              </div>
            ))}
        </div>
      )}

      <div className="activity-footnote">
        {watcher &&
          (watcher.collector === "daemon" ? (
            <>
              Recorded by the background collector (troved) — tracking
              continues when Trove is closed.{" "}
            </>
          ) : (
            <>
              Recording in-app — tracking stops when Trove closes.
              {!watcher.daemon_installed && (
                <>
                  {" "}
                  Install the always-on collector with{" "}
                  <code>troved install</code>.
                </>
              )}{" "}
            </>
          ))}
        Raw events: <code>~/Documents/Trove/activity/</code> — one JSONL file per day.
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

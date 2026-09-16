import { useCallback, useEffect, useMemo, useState } from "react";
import {
  ActivityEvent,
  ActivitySummary,
  api,
  AppUsage,
  CollectorStatus,
  SeriesPoint,
} from "../api";
import Chart from "./Chart";

type Range = "today" | "7d" | "30d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "today", label: "Today", days: 1 },
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
];

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
  const [collector, setCollector] = useState<CollectorStatus | null>(null);

  const refresh = useCallback(async (r: Range) => {
    const today = new Date();
    const to = localDate(today);
    const days = RANGES.find((x) => x.id === r)!.days;
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (days - 1));
    const from = localDate(fromDate);

    const [s, cur, cs] = await Promise.all([
      api.activitySummary(from, to),
      api.activityCurrent(),
      api.collectorStatus(),
    ]);
    setSummary(s);
    setCurrent(cur);
    setCollector(cs);
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

  const view = useMemo(
    () => (summary ? withCurrent(summary, current) : null),
    [summary, current]
  );

  const liveApp = current && !current.afk ? current.app : null;
  const maxApp = view && view.apps.length > 0 ? view.apps[0].seconds : 1;

  return (
    <div className="view view--scroll">
      <div className="view-header">
        <div>
          <h2>Activity</h2>
          <div className="view-sub">
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

      {collector && !collector.running && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Nothing is recording right now.</strong> App activity is
            written by trove-collector, a separate always-on program.{" "}
            {collector.installed
              ? "It is installed but not running — check `trove-collector status` in a terminal."
              : "Install it from github.com/david-wills/trove-collector to track app activity 24/7."}
          </div>
        </div>
      )}

      {view && (
        <div className="view-stats">
          <Stat label="Active" value={fmtDuration(view.active_seconds)} />
          <Stat label="Away" value={fmtDuration(view.afk_seconds)} muted />
          <Stat label="Apps" value={String(view.apps.length)} muted />
        </div>
      )}

      {view && view.apps.length === 0 && (
        <div className="view-empty">
          No activity recorded yet. trove-collector logs the app you're using
          every few seconds while it runs — switch around and check back.
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
        <div className="view-trend">
          <div className="view-section-title">Active hours per day</div>
          <Chart points={daily} name="Active time" unit="hr" kind="sum" />
        </div>
      )}

      {range === "today" && timeline.length > 0 && (
        <div className="view-timeline">
          <div className="view-section-title">Today, most recent first</div>
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

      <div className="view-footnote">
        {collector?.running ? (
          <>
            Recorded by trove-collector
            {collector.rss_mb != null ? ` (${collector.rss_mb} MB)` : ""} —
            tracking continues when Trove is closed.{" "}
          </>
        ) : (
          <>Not recording — trove-collector is not running. </>
        )}
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

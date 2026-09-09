import { useCallback, useEffect, useState } from "react";
import {
  api,
  CalendarChange,
  CalendarOccurrence,
  CalendarSummary,
  CalendarSyncState,
  SeriesPoint,
} from "../api";
import Chart from "./Chart";

// The calendar syncs every 15 minutes from the watcher loop.
const REFRESH_MS = 15000;

/** Days of the upcoming-events list (today inclusive). */
const UPCOMING_DAYS = 7;

type Range = "7d" | "30d";

/** YYYY-MM-DD in local time. */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function addDays(date: Date, days: number): Date {
  const d = new Date(date);
  d.setDate(d.getDate() + days);
  return d;
}

function fmtTime(rfc3339: string): string {
  return new Date(rfc3339).toLocaleTimeString([], {
    hour: "numeric",
    minute: "2-digit",
  });
}

function fmtDay(day: string): string {
  return new Date(`${day}T12:00:00`).toLocaleDateString([], {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
}

function fmtSynced(rfc3339: string): string {
  const mins = Math.round((Date.now() - new Date(rfc3339).getTime()) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m ago`;
}

/** A change-stream entry as one readable line. */
function describeChange(c: CalendarChange): string {
  if (c.kind === "added") return "added";
  if (c.kind === "removed") return "removed";
  const fields = (c.changes ?? []).map((f) => f.field);
  if (fields.includes("start") || fields.includes("end")) return "rescheduled";
  if (fields.includes("status")) return "status changed";
  return `changed (${fields.join(", ")})`;
}

export default function CalendarView() {
  const [range, setRange] = useState<Range>("7d");
  const [summary, setSummary] = useState<CalendarSummary | null>(null);
  const [daily, setDaily] = useState<SeriesPoint[]>([]);
  const [upcoming, setUpcoming] = useState<Map<string, CalendarOccurrence[]>>(
    new Map(),
  );
  const [changes, setChanges] = useState<CalendarChange[]>([]);
  const [sync, setSync] = useState<CalendarSyncState | null>(null);
  const [permission, setPermission] = useState<[string, string] | null>(null);
  const [requesting, setRequesting] = useState(false);

  const refresh = useCallback(async () => {
    const now = new Date();
    const today = localDate(now);
    const days = range === "7d" ? 7 : 30;
    const from = localDate(addDays(now, -(days - 1)));
    const upcomingDates = Array.from({ length: UPCOMING_DAYS }, (_, i) =>
      localDate(addDays(now, i)),
    );

    const [s, d, ch, info, perm, ...timelines] = await Promise.all([
      api.calendarSummary(from, today),
      api.calendarDaily(from, today),
      api.calendarChanges(localDate(addDays(now, -6)), today),
      api.calendarSyncInfo(),
      api.calendarPermission(),
      ...upcomingDates.map((date) => api.calendarTimeline(date)),
    ]);
    setSummary(s);
    setDaily(d);
    setChanges(ch.slice(-20).reverse());
    setSync(info);
    setPermission(perm);
    setUpcoming(
      new Map(upcomingDates.map((date, i) => [date, timelines[i] ?? []])),
    );
  }, [range]);

  useEffect(() => {
    let active = true;
    const tick = () => {
      if (active) refresh().catch(() => {});
    };
    tick();
    const id = setInterval(tick, REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [refresh]);

  const requestAccess = async () => {
    setRequesting(true);
    try {
      await api.requestCalendarPermission();
      await refresh();
    } finally {
      setRequesting(false);
    }
  };

  const needsPermission =
    permission !== null &&
    (permission[0] !== "granted" || permission[1] !== "granted");
  const maxCal =
    summary && summary.calendars.length > 0 ? summary.calendars[0].count : 1;
  const hasUpcoming = [...upcoming.values()].some((v) => v.length > 0);

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Calendar</h2>
          <div className="activity-sub">
            {sync?.updated ? (
              <>
                synced {fmtSynced(sync.updated)}
                {sync.backfilled_to ? ` · history back to ${sync.backfilled_to}` : ""}
              </>
            ) : (
              <>Waiting for the first calendar sync…</>
            )}
          </div>
        </div>
        <div className="segmented">
          {(["7d", "30d"] as Range[]).map((r) => (
            <button
              key={r}
              className={range === r ? "active" : ""}
              onClick={() => setRange(r)}
            >
              {r}
            </button>
          ))}
        </div>
      </div>

      {needsPermission && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Calendar access needed.</strong> Trove reads events and
            reminders through the system calendar store (every account macOS
            syncs — iCloud, Google, Exchange). macOS only shows the consent
            prompt when the app asks
            {permission[0] === "denied" || permission[1] === "denied"
              ? " — access was declined; re-enable it under System Settings → Privacy & Security → Calendars and Reminders."
              : "."}
          </div>
          <div className="perm-actions">
            <button className="btn-primary" onClick={requestAccess} disabled={requesting}>
              {requesting ? "Asking…" : "Grant access"}
            </button>
          </div>
        </div>
      )}

      {summary && (
        <div className="activity-stats">
          <Stat label={`Events (${range})`} value={String(summary.events)} />
          <Stat label="Scheduled hours" value={summary.hours.toFixed(1)} />
          <Stat label="All-day" value={String(summary.all_day)} muted />
          <Stat
            label="Calendars"
            value={String(summary.calendars.length)}
            muted
          />
        </div>
      )}

      {summary && summary.events === 0 && !needsPermission && (
        <div className="activity-empty">
          No events in range yet. The first sync backfills full calendar
          history into <code>~/Documents/Trove/calendar/</code> within a few minutes of
          access being granted.
        </div>
      )}

      {summary && summary.calendars.length > 0 && (
        <div className="app-bars">
          {summary.calendars.slice(0, 12).map((c) => (
            <div key={`${c.account}/${c.calendar}`} className="app-bar">
              <div className="app-bar-name" title={`${c.calendar} (${c.account})`}>
                {c.calendar}
              </div>
              <div className="app-bar-track">
                <div
                  className="app-bar-fill"
                  style={{ width: `${(c.count / maxCal) * 100}%` }}
                />
              </div>
              <div className="app-bar-time">{c.count}</div>
            </div>
          ))}
        </div>
      )}

      {daily.length > 0 && (
        <div className="activity-trend">
          <div className="activity-section-title">
            Scheduled hours per day, last {range}
          </div>
          <Chart points={daily} name="Scheduled" unit="h" kind="sum" />
        </div>
      )}

      {hasUpcoming && (
        <div className="activity-timeline">
          <div className="activity-section-title">
            Next {UPCOMING_DAYS} days
          </div>
          {[...upcoming.entries()].map(([date, events]) =>
            events.length === 0 ? null : (
              <div key={date}>
                <div className="tl-day">{fmtDay(date)}</div>
                {events.map((e) => (
                  <div key={`${e.id}/${e.occurrence}`} className="tl-row">
                    <div className="tl-time">
                      {e.all_day ? "all day" : fmtTime(e.start)}
                    </div>
                    <div className="tl-body">
                      <span className="tl-app">
                        {e.title}
                        {e.status === "canceled" ? " (canceled)" : ""}
                        {e.recurring ? " ↻" : ""}
                      </span>
                      <span className="tl-title">
                        {e.calendar}
                        {e.location ? ` · ${e.location}` : ""}
                      </span>
                    </div>
                  </div>
                ))}
              </div>
            ),
          )}
        </div>
      )}

      {changes.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">
            Schedule changes, last 7 days
          </div>
          {changes.map((c, i) => (
            <div key={`${c.id}/${c.occurrence}/${c.ts}/${i}`} className="tl-row">
              <div className="tl-time">{fmtDay(c.ts.slice(0, 10))}</div>
              <div className="tl-body">
                <span className="tl-app">{c.title}</span>
                <span className="tl-title">
                  {describeChange(c)} · {c.calendar}
                </span>
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="activity-footnote">
        Read from the system calendar store (EventKit), which carries every
        account macOS syncs. Files: <code>~/Documents/Trove/calendar/</code> — events by
        month in <code>events/</code>, the reschedule/cancellation stream in{" "}
        <code>changes/</code>. Reminders land in{" "}
        <code>~/Documents/Trove/tasks/apple-reminders/</code> (see the Tasks tab).
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

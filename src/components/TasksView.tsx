import { useCallback, useEffect, useState } from "react";
import { api, SeriesPoint, Task, TasksOverview, TasksSyncState } from "../api";
import Chart from "./Chart";

// Tasks sync every 15 minutes from the watcher loop; polling faster than
// the other views buys nothing.
const REFRESH_MS = 15000;

/** Days of completion history shown in the trend chart. */
const TREND_DAYS = 30;

/** YYYY-MM-DD in local time (tasks are stored in local time). */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function fmtDue(rfc3339: string): string {
  return new Date(rfc3339).toLocaleDateString([], {
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

/** Priority marker on the TickTick scale (0 none, 1 low, 3 medium, 5 high). */
function priorityMark(p: number): string {
  if (p >= 5) return "‼ ";
  if (p >= 3) return "! ";
  return "";
}

interface Group {
  title: string;
  tasks: Task[];
}

/** Open tasks bucketed for the list: overdue, today, upcoming, undated. */
function groupTasks(tasks: Task[], today: string): Group[] {
  const overdue: Task[] = [];
  const dueToday: Task[] = [];
  const upcoming: Task[] = [];
  const undated: Task[] = [];
  for (const t of tasks) {
    if (t.status !== "open") continue;
    const day = t.due?.slice(0, 10);
    if (!day) undated.push(t);
    else if (day < today) overdue.push(t);
    else if (day === today) dueToday.push(t);
    else upcoming.push(t);
  }
  const byDue = (a: Task, b: Task) => (a.due! < b.due! ? -1 : 1);
  overdue.sort(byDue);
  dueToday.sort(byDue);
  upcoming.sort(byDue);
  undated.sort((a, b) => a.project.localeCompare(b.project));
  return [
    { title: "Overdue", tasks: overdue },
    { title: "Due today", tasks: dueToday },
    { title: "Upcoming", tasks: upcoming },
    { title: "No date", tasks: undated },
  ].filter((g) => g.tasks.length > 0);
}

export default function TasksView() {
  const [overview, setOverview] = useState<TasksOverview | null>(null);
  const [tasks, setTasks] = useState<Task[]>([]);
  const [daily, setDaily] = useState<SeriesPoint[]>([]);
  const [sync, setSync] = useState<TasksSyncState | null>(null);
  const [dismissedError, setDismissedError] = useState(false);

  const refresh = useCallback(async () => {
    const today = new Date();
    const to = localDate(today);
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (TREND_DAYS - 1));
    const from = localDate(fromDate);

    const [o, list, d, info] = await Promise.all([
      api.tasksOverview(),
      api.tasksList(),
      api.tasksDaily(from, to),
      api.tasksSyncInfo(),
    ]);
    setOverview(o);
    setTasks(list);
    setDaily(d);
    setSync(info);
  }, []);

  // Initial load, then poll so new syncs show up.
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

  const today = localDate(new Date());
  const groups = groupTasks(tasks, today);
  const maxProject =
    overview && overview.projects.length > 0 ? overview.projects[0].open : 1;
  // Most recent successful sync across sources; standing error, if any.
  const lastSynced = Object.values(sync?.sources ?? {})
    .map((s) => s.updated)
    .filter(Boolean)
    .sort()
    .pop();
  const syncError = Object.values(sync?.sources ?? {})
    .map((s) => s.error)
    .find(Boolean);

  return (
    <div className="view view--scroll">
      <div className="view-header">
        <div>
          <h2>Tasks</h2>
          <div className="view-sub">
            {lastSynced ? (
              <>TickTick · synced {fmtSynced(lastSynced)}</>
            ) : (
              <>Waiting for the first task sync…</>
            )}
          </div>
        </div>
      </div>

      {syncError && !dismissedError && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Task sync is failing.</strong> {syncError}
          </div>
          <div className="perm-actions">
            <button className="btn-ghost" onClick={() => setDismissedError(true)}>
              Dismiss
            </button>
          </div>
        </div>
      )}

      {overview && (
        <div className="view-stats">
          <Stat label="Open" value={String(overview.open)} />
          <Stat label="Due today" value={String(overview.due_today)} />
          <Stat label="Overdue" value={String(overview.overdue)} />
          <Stat label="Done (7d)" value={String(overview.completed_7d)} muted />
        </div>
      )}

      {overview && overview.open === 0 && (
        <div className="view-empty">
          No tasks here yet. Trove syncs TickTick every 15 minutes once a
          token is provisioned at{" "}
          <code>~/Documents/Trove/.trove/sync/ticktick-token.json</code> — and any other
          to-do app can plug in by writing the open task format under{" "}
          <code>~/Documents/Trove/tasks/</code>.
        </div>
      )}

      {overview && overview.projects.length > 0 && (
        <div className="app-bars">
          {overview.projects.slice(0, 12).map((p) => (
            <div key={`${p.source}/${p.project}`} className="app-bar">
              <div className="app-bar-name" title={p.project}>
                {p.project}
              </div>
              <div className="app-bar-track">
                <div
                  className="app-bar-fill"
                  style={{ width: `${(p.open / maxProject) * 100}%` }}
                />
              </div>
              <div className="app-bar-time">{p.open}</div>
            </div>
          ))}
        </div>
      )}

      {daily.length > 0 && (
        <div className="view-trend">
          <div className="view-section-title">
            Completed per day, last {TREND_DAYS} days
          </div>
          <Chart points={daily} name="Completed" unit="" kind="sum" />
        </div>
      )}

      {groups.map((g) => (
        <div key={g.title} className="view-timeline">
          <div className="view-section-title">
            {g.title} · {g.tasks.length}
          </div>
          {g.tasks.map((t) => (
            <div key={`${t.source}/${t.id}`} className="tl-row">
              <div className="tl-time">{t.due ? fmtDue(t.due) : "—"}</div>
              <div className="tl-body">
                <span className="tl-app">
                  {priorityMark(t.priority)}
                  {t.title}
                  {t.recurrence ? " ↻" : ""}
                </span>
                <span className="tl-title">{t.project}</span>
              </div>
            </div>
          ))}
        </div>
      ))}

      <div className="view-footnote">
        Synced read-only from TickTick — completions are detected by diffing
        snapshots, building a history TickTick itself doesn't expose. Files:{" "}
        <code>~/Documents/Trove/tasks/</code> — open tasks in <code>tasks.jsonl</code>,
        completions in <code>events/</code>, one source per folder.
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

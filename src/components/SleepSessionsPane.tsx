import { useEffect, useMemo, useState } from "react";
import { api, HeartratePoint, SleepSession } from "../api";
import { IntradayChart } from "./MultiChart";
import Segmented from "./Segmented";
import {
  asleepHours,
  fmtDay,
  fmtHours,
  fmtTime,
  rangeFor,
  sessionColor,
  sessionLabel,
  SOURCE_META,
  titleCase,
} from "./healthShared";

// Sleep over the health-sleep contract (`health/sleep/<source>/`), shared
// by both read-side shapes. The merged shape shows every source with relay
// rows deduped; the source-native shape passes one source and no dedupe.
// Sessions are grouped by their `day` (the source's attribution, not the
// date they ended — an Oura nap after 18:00 belongs to the next day).

type Span = 30 | 90 | 365;
const SPANS: { id: `${Span}`; label: string }[] = [
  { id: "30", label: "30d" },
  { id: "90", label: "90d" },
  { id: "365", label: "1Y" },
];

const STAGES: { key: keyof SleepSession; label: string; color: string }[] = [
  { key: "deep_seconds", label: "Deep", color: "#2e4a76" },
  { key: "rem_seconds", label: "REM", color: "#5b84b8" },
  { key: "light_seconds", label: "Light", color: "#9db8d9" },
  { key: "awake_seconds", label: "Awake", color: "#5e616b" },
];

function secs(s: SleepSession, key: keyof SleepSession): number {
  const v = s[key];
  return typeof v === "number" ? v : 0;
}

/** Scalars a source left under `extra` that read well as stats. */
const EXTRA_STATS: { key: string; label: string; fmt: (v: number) => string }[] = [
  { key: "efficiency", label: "Efficiency", fmt: (v) => `${Math.round(v)}%` },
  { key: "latency", label: "Latency", fmt: (v) => `${Math.round(v / 60)}m` },
  { key: "average_hrv", label: "Avg HRV", fmt: (v) => `${Math.round(v)} ms` },
  { key: "lowest_heart_rate", label: "Lowest HR", fmt: (v) => `${Math.round(v)} bpm` },
  { key: "average_heart_rate", label: "Avg HR", fmt: (v) => `${Math.round(v)} bpm` },
  { key: "average_breath", label: "Breath rate", fmt: (v) => `${v.toFixed(1)}/min` },
  { key: "restless_periods", label: "Restless", fmt: (v) => `${Math.round(v)}` },
];

export default function SleepSessionsPane({
  sources,
  dedupe,
  embedded,
}: {
  /** Show only these `source` ids (contract folder names); all when absent. */
  sources?: string[];
  /** Hide relay rows whose device writes its own folder. */
  dedupe: boolean;
  /** Inside a metric pane: the pane owns the heading, keep only the stats line. */
  embedded?: boolean;
}) {
  const [span, setSpan] = useState<`${Span}`>("90");
  const [sessions, setSessions] = useState<SleepSession[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [hr, setHr] = useState<HeartratePoint[]>([]);

  useEffect(() => {
    let stale = false;
    const { from, to } = rangeFor(Number(span));
    api
      .sleepSessions(from, to, dedupe)
      .then((rows) => {
        if (stale) return;
        const kept = sources ? rows.filter((r) => sources.includes(r.source)) : rows;
        setSessions(kept);
      })
      .catch(() => !stale && setSessions([]));
    return () => {
      stale = true;
    };
  }, [span, dedupe, sources]);

  // Newest day first; within a day, the long sleep before naps.
  const days = useMemo(() => {
    const byDay = new Map<string, SleepSession[]>();
    for (const s of sessions ?? []) {
      byDay.set(s.day, [...(byDay.get(s.day) ?? []), s]);
    }
    return Array.from(byDay.entries())
      .sort((a, b) => b[0].localeCompare(a[0]))
      .map(([day, list]) => ({
        day,
        sessions: list.sort((a, b) => (asleepHours(b) ?? 0) - (asleepHours(a) ?? 0)),
      }));
  }, [sessions]);

  const current = useMemo(() => {
    const all = sessions ?? [];
    return all.find((s) => s.guid === selected) ?? days[0]?.sessions[0] ?? null;
  }, [sessions, selected, days]);

  useEffect(() => {
    if (!current || current.source !== "oura") {
      setHr([]);
      return;
    }
    let stale = false;
    api
      .ouraHeartrateRange(current.start, current.end, 600)
      .then((pts) => !stale && setHr(pts))
      .catch(() => !stale && setHr([]));
    return () => {
      stale = true;
    };
  }, [current]);

  if (sessions === null) return null;
  if (sessions.length === 0) {
    return (
      <div className="health-section">
        <div className="oura-hint">
          No sleep sessions in this range. Sessions arrive from an Oura sync or an
          Apple Health import and land in <code>~/Documents/Trove/health/sleep/</code>.
        </div>
      </div>
    );
  }

  const nights = days.length;
  const totalHours = days.reduce(
    (t, d) => t + Math.max(...d.sessions.map((s) => asleepHours(s) ?? 0)),
    0
  );
  const stageTotal = current ? STAGES.reduce((t, s) => t + secs(current, s.key), 0) : 0;

  return (
    <div className="health-section sleep-section">
      <div className={`view-header ${embedded ? "view-header--compact" : ""}`}>
        <div>
          {!embedded && <h2>Sleep</h2>}
          <div className="health-header-sub">
            {nights} nights · avg {fmtHours(nights ? totalHours / nights : 0)} asleep ·{" "}
            {sessions.length} sessions
          </div>
        </div>
        <Segmented options={SPANS} value={span} onChange={setSpan} small />
      </div>

      {current && (
        <div className="sleep-detail">
          <div className="view-header">
            <div>
              <h2>
                {fmtDay(current.day)}
                {current.kind && current.kind !== "sleep" && (
                  <span className="kind-chip">{titleCase(current.kind)}</span>
                )}
              </h2>
              <div className="health-header-sub">
                {fmtTime(current.start)} → {fmtTime(current.end)} ·{" "}
                <span style={{ color: sessionColor(current) }}>{sessionLabel(current)}</span>
              </div>
            </div>
          </div>
          {stageTotal > 0 && (
            <>
              <div className="stage-bar">
                {STAGES.map((s) => {
                  const v = secs(current, s.key);
                  return v > 0 ? (
                    <div
                      key={s.label}
                      className="stage-seg"
                      title={`${s.label} ${fmtHours(v / 3600)}`}
                      style={{ width: `${(v / stageTotal) * 100}%`, background: s.color }}
                    />
                  ) : null;
                })}
              </div>
              <div className="stage-legend">
                {STAGES.map((s) => (
                  <span key={s.label} className="stage-key">
                    <span className="source-dot" style={{ background: s.color }} />
                    {s.label} {fmtHours(secs(current, s.key) / 3600)}
                  </span>
                ))}
              </div>
            </>
          )}
          <div className="stats-grid">
            {current.asleep_seconds != null && (
              <Stat label="Asleep" value={fmtHours(current.asleep_seconds / 3600)} />
            )}
            {current.in_bed_seconds != null && (
              <Stat label="In bed" value={fmtHours(current.in_bed_seconds / 3600)} />
            )}
            {EXTRA_STATS.map((e) => {
              const v = current.extra?.[e.key];
              return typeof v === "number" ? <Stat key={e.key} label={e.label} value={e.fmt(v)} /> : null;
            })}
          </div>
          {current.source === "oura" && (
            <IntradayChart points={hr} color={SOURCE_META.oura.color} />
          )}
        </div>
      )}

      <div className="sleep-nights">
        {days.map((d) =>
          d.sessions.map((s, i) => {
            const hours = asleepHours(s);
            const stageSum = STAGES.slice(0, 3).reduce((t, st) => t + secs(s, st.key), 0);
            return (
              <div
                key={s.guid}
                className={`sleep-night ${current?.guid === s.guid ? "active" : ""}`}
                onClick={() => setSelected(s.guid)}
              >
                <span className="sleep-night-day">{i === 0 ? fmtDay(d.day) : ""}</span>
                <span className="source-chip" style={{ color: sessionColor(s) }}>
                  {sessionLabel(s)}
                </span>
                {s.kind && s.kind !== "sleep" && <span className="kind-chip">{titleCase(s.kind)}</span>}
                <span className="sleep-night-hours">{hours != null ? fmtHours(hours) : "—"}</span>
                <span className="sleep-night-bar">
                  {stageSum > 0 &&
                    STAGES.slice(0, 3).map((st) => {
                      const v = secs(s, st.key);
                      return v > 0 ? (
                        <span key={st.label} style={{ width: `${(v / stageSum) * 100}%`, background: st.color }} />
                      ) : null;
                    })}
                </span>
                <span className="sleep-night-eff">
                  {typeof s.extra?.efficiency === "number" ? `${Math.round(s.extra.efficiency)}%` : ""}
                </span>
              </div>
            );
          })
        )}
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="stat">
      <div className="stat-value">{value}</div>
      <div className="stat-label">{label}</div>
    </div>
  );
}

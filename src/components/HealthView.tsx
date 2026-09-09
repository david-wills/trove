import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  api,
  Bucket,
  HealthSource,
  HeartratePoint,
  ImportProgress,
  OuraDayScore,
  SeriesPoint,
  SleepNight,
  SourceSeries,
  UnifiedMetric,
  WorkoutItem,
} from "../api";
import MultiChart, { ChartSeries, IntradayChart } from "./MultiChart";

type Range = "3m" | "1y" | "all";
type SectionTab = "overview" | "metrics" | "sleep" | "workouts";

const SOURCE_META: Record<HealthSource, { label: string; color: string }> = {
  "apple-health": { label: "Apple Health", color: "#d4a847" },
  oura: { label: "Oura", color: "#6f9fd8" },
};

const TABS: { id: SectionTab; label: string }[] = [
  { id: "overview", label: "Overview" },
  { id: "metrics", label: "Metrics" },
  { id: "sleep", label: "Sleep" },
  { id: "workouts", label: "Workouts" },
];

const BUCKETS: { id: Bucket; label: string }[] = [
  { id: "day", label: "Day" },
  { id: "week", label: "Week" },
  { id: "month", label: "Month" },
];

const RANGES: { id: Range; label: string }[] = [
  { id: "3m", label: "3M" },
  { id: "1y", label: "1Y" },
  { id: "all", label: "All" },
];

/** Oura daily scores that also chart as a trend. */
const TREND_SCORES = [
  "readiness-score",
  "sleep-score",
  "activity-score",
  "stress-high",
  "cardiovascular-age",
];

function isHealthExport(path: string): boolean {
  return /\.(zip|xml)$/i.test(path);
}

function inRange(points: SeriesPoint[], range: Range): SeriesPoint[] {
  if (range === "all" || points.length === 0) return points;
  const last = new Date(`${points[points.length - 1].date}T00:00:00`);
  const cutoff = new Date(last);
  if (range === "3m") cutoff.setMonth(cutoff.getMonth() - 3);
  else cutoff.setFullYear(cutoff.getFullYear() - 1);
  const cut = cutoff.toISOString().slice(0, 10);
  return points.filter((p) => p.date >= cut);
}

/** Clip every source's series to the chosen range using a shared cutoff
 *  (the newest date across sources), so they stay aligned. */
function seriesInRange(series: SourceSeries[], range: Range): ChartSeries[] {
  const all = series.flatMap((s) => s.points);
  const clipped = inRange(
    all.slice().sort((a, b) => a.date.localeCompare(b.date)),
    range
  );
  const cut = clipped[0]?.date ?? "";
  return series.map((s) => ({
    label: SOURCE_META[s.source].label,
    color: SOURCE_META[s.source].color,
    points: range === "all" ? s.points : s.points.filter((p) => p.date >= cut),
  }));
}

function fmtHours(h: number): string {
  const mins = Math.round(h * 60);
  return `${Math.floor(mins / 60)}h ${String(mins % 60).padStart(2, "0")}m`;
}

function fmtDay(day: string): string {
  return new Date(`${day}T00:00:00`).toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
}

function fmtTime(ts: string): string {
  return new Date(ts).toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
}

/** "running" / "late_nap" → "Running" / "Late nap". */
function titleCase(s: string): string {
  const clean = s.replace(/_/g, " ");
  return clean.charAt(0).toUpperCase() + clean.slice(1);
}

export default function HealthView() {
  const [metrics, setMetrics] = useState<UnifiedMetric[]>([]);
  const [scores, setScores] = useState<OuraDayScore[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [tab, setTab] = useState<SectionTab | null>(null);
  const [importing, setImporting] = useState<ImportProgress | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    const [ms, sc] = await Promise.all([
      api.healthMetricsUnified(),
      api.ouraOverview().catch(() => [] as OuraDayScore[]),
    ]);
    setMetrics(ms);
    setScores(sc);
    setLoaded(true);
    setTab((t) => t ?? (sc.length > 0 ? "overview" : "metrics"));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Progress events stream in while an import runs (any importer emits on
  // the shared "import-progress" event — only health's are ours).
  useEffect(() => {
    const unlisten = listen<ImportProgress>("import-progress", (e) => {
      if (e.payload.integration_id !== "health") return;
      setImporting(e.payload);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  const runImport = useCallback(
    async (path: string) => {
      setError(null);
      setImporting({ integration_id: "health", records: 0, percent: 0 });
      try {
        await api.runImport("health", path, {});
        await refresh();
      } catch (e) {
        setError(String(e));
      } finally {
        setImporting(null);
      }
    },
    [refresh]
  );

  const pickAndImport = useCallback(async () => {
    const file = await open({
      multiple: false,
      title: "Choose your Apple Health export",
      filters: [{ name: "Apple Health export", extensions: ["zip", "xml"] }],
    });
    if (typeof file === "string") runImport(file);
  }, [runImport]);

  // Dropping an export.zip anywhere on the window imports it.
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type !== "drop" || importing) return;
      const path = event.payload.paths.find(isHealthExport);
      if (path) runImport(path);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, [runImport, importing]);

  const hasOura = scores.length > 0;

  if (loaded && metrics.length === 0 && !hasOura) {
    return (
      <div className="health-empty">
        {importing ? (
          <ImportingPanel progress={importing} />
        ) : (
          <>
            <h2>Bring your health data home</h2>
            <p>
              On your iPhone, open <strong>Health</strong>, tap your picture,
              then <strong>Export All Health Data</strong>. AirDrop the{" "}
              <code>export.zip</code> to this Mac and import it here — every
              metric becomes plain CSV files in <code>~/Trove/health</code>.
            </p>
            <button className="btn-primary" onClick={pickAndImport}>
              Import export.zip…
            </button>
            <p className="health-hint">or drop the file anywhere in this window</p>
            <p className="health-hint">
              Wear an Oura Ring? Connect it in the Integrations tab and its
              data lands here too.
            </p>
            {error && <div className="health-error">{error}</div>}
          </>
        )}
      </div>
    );
  }

  if (!loaded || !tab) return null;

  return (
    <div className="health-shell">
      <div className="health-tabs">
        <Segmented options={TABS} value={tab} onChange={setTab} />
        {importing && <ImportingPanel progress={importing} compact />}
      </div>
      {error && <div className="health-error">{error}</div>}
      {tab === "overview" && (
        <OverviewSection scores={scores} metrics={metrics} hasOura={hasOura} />
      )}
      {tab === "metrics" && (
        <MetricsSection
          metrics={metrics}
          onImport={pickAndImport}
          importing={!!importing}
        />
      )}
      {tab === "sleep" && <SleepSection hasOura={hasOura} />}
      {tab === "workouts" && <WorkoutsSection />}
    </div>
  );
}

function OuraHint({ children }: { children: React.ReactNode }) {
  return <div className="oura-hint">{children}</div>;
}

function scoreDisplay(s: OuraDayScore): string {
  if (s.slug === "stress-high")
    return s.value != null ? `${Math.round(s.value)}m` : (s.label ?? "—");
  if (s.value != null) return `${Math.round(s.value)}`;
  return s.label ? titleCase(s.label) : "—";
}

function scoreSub(s: OuraDayScore): string {
  if (s.slug === "stress-high" && s.label) return `high · ${s.label}`;
  if (s.slug === "cardiovascular-age") return "years";
  if (s.value != null && ["readiness-score", "sleep-score", "activity-score"].includes(s.slug))
    return "of 100";
  return "";
}

function OverviewSection({
  scores,
  metrics,
  hasOura,
}: {
  scores: OuraDayScore[];
  metrics: UnifiedMetric[];
  hasOura: boolean;
}) {
  const trendOptions = TREND_SCORES.filter((slug) =>
    metrics.some((m) => m.slug === slug)
  ).map((slug) => ({
    id: slug,
    label: metrics.find((m) => m.slug === slug)!.name.replace(" Score", ""),
  }));
  const [trend, setTrend] = useState<string>(trendOptions[0]?.id ?? "");
  const [range, setRange] = useState<Range>("3m");
  const [series, setSeries] = useState<SourceSeries[]>([]);

  useEffect(() => {
    if (!trend) return;
    let stale = false;
    api.healthSeriesUnified(trend, "day").then((s) => {
      if (!stale) setSeries(s);
    });
    return () => {
      stale = true;
    };
  }, [trend]);

  if (!hasOura) {
    return (
      <div className="health-section">
        <OuraHint>
          Daily scores come from the Oura Ring. Connect it in the{" "}
          <strong>Integrations</strong> tab to see readiness, sleep and
          activity scores here — your Apple Health metrics live under{" "}
          <strong>Metrics</strong>.
        </OuraHint>
      </div>
    );
  }

  const metric = metrics.find((m) => m.slug === trend);
  return (
    <div className="health-section">
      <div className="score-cards">
        {scores.map((s) => (
          <div key={s.slug} className="score-card">
            <div className="score-name">{s.name}</div>
            <div className="score-value">{scoreDisplay(s)}</div>
            <div className="score-sub">{scoreSub(s)}</div>
            <div className="score-day">{fmtDay(s.day)}</div>
          </div>
        ))}
      </div>
      {trendOptions.length > 0 && (
        <>
          <div className="health-header">
            <div>
              <h2>{metric?.name ?? ""}</h2>
              <div className="health-header-sub">Oura · daily</div>
            </div>
            <div className="health-controls">
              <Segmented options={trendOptions} value={trend} onChange={setTrend} />
              <Segmented options={RANGES} value={range} onChange={setRange} />
            </div>
          </div>
          <MultiChart
            series={seriesInRange(series, range)}
            unit={metric?.unit ?? ""}
            kind={metric?.kind ?? "avg"}
          />
        </>
      )}
    </div>
  );
}

function MetricsSection({
  metrics,
  onImport,
  importing,
}: {
  metrics: UnifiedMetric[];
  onImport: () => void;
  importing: boolean;
}) {
  const [selected, setSelected] = useState<string | null>(
    metrics[0]?.slug ?? null
  );
  const [bucket, setBucket] = useState<Bucket>("day");
  const [range, setRange] = useState<Range>("1y");
  const [series, setSeries] = useState<SourceSeries[]>([]);

  useEffect(() => {
    if (!selected) return;
    let stale = false;
    api.healthSeriesUnified(selected, bucket).then((s) => {
      if (!stale) setSeries(s);
    });
    return () => {
      stale = true;
    };
  }, [selected, bucket]);

  const metric = metrics.find((m) => m.slug === selected) ?? null;
  const span = metric
    ? {
        first: metric.sources.map((s) => s.first_date).sort()[0],
        last: metric.sources.map((s) => s.last_date).sort().slice(-1)[0],
      }
    : null;

  return (
    <div className="health-view">
      <div className="metric-list">
        <div className="metric-list-header">
          <span className="metric-list-title">Metrics</span>
          <button
            className="btn-new"
            onClick={onImport}
            title="Import a new Apple Health export"
            disabled={importing}
          >
            +
          </button>
        </div>
        <div className="metric-list-items">
          {metrics.map((m) => (
            <div
              key={m.slug}
              className={`metric-item ${selected === m.slug ? "active" : ""}`}
              onClick={() => setSelected(m.slug)}
            >
              <div className="metric-item-name">{m.name}</div>
              <div className="metric-item-sub">
                {m.sources
                  .reduce((n, s) => n + s.records, 0)
                  .toLocaleString()}{" "}
                records
                <span className="metric-item-dots">
                  {m.sources.map((s) => (
                    <span
                      key={s.source}
                      className="source-dot"
                      title={SOURCE_META[s.source].label}
                      style={{ background: SOURCE_META[s.source].color }}
                    />
                  ))}
                </span>
              </div>
            </div>
          ))}
        </div>
      </div>
      <div className="health-main">
        {metric && span && (
          <>
            <div className="health-header">
              <div>
                <h2>{metric.name}</h2>
                <div className="health-header-sub">
                  {span.first} → {span.last}
                  {metric.unit && <span className="unit-chip">{metric.unit}</span>}
                </div>
              </div>
              <div className="health-controls">
                <Segmented options={BUCKETS} value={bucket} onChange={setBucket} />
                <Segmented options={RANGES} value={range} onChange={setRange} />
              </div>
            </div>
            <MultiChart
              series={seriesInRange(series, range)}
              unit={metric.unit}
              kind={metric.kind}
            />
            {metric.note && <div className="health-footnote">{metric.note}</div>}
            <div className="health-footnote">
              Raw data:{" "}
              {metric.sources.map((s, i) => (
                <span key={s.source}>
                  {i > 0 && " · "}
                  {SOURCE_META[s.source].label}{" "}
                  <code>
                    {s.source === "oura"
                      ? "~/Trove/health/oura/"
                      : `~/Trove/health/${metric.slug}/`}
                  </code>
                </span>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

const STAGES: { key: keyof SleepNight; label: string; color: string }[] = [
  { key: "deep_hours", label: "Deep", color: "#2e4a76" },
  { key: "rem_hours", label: "REM", color: "#5b84b8" },
  { key: "light_hours", label: "Light", color: "#9db8d9" },
  { key: "awake_hours", label: "Awake", color: "#5e616b" },
];

function SleepSection({ hasOura }: { hasOura: boolean }) {
  const [nights, setNights] = useState<SleepNight[] | null>(null);
  const [selected, setSelected] = useState(0);
  const [hr, setHr] = useState<HeartratePoint[]>([]);

  useEffect(() => {
    api.ouraSleepNights(90).then(setNights);
  }, []);

  const night = nights?.[selected] ?? null;

  useEffect(() => {
    if (!night || !night.bedtime_start || !night.bedtime_end) {
      setHr([]);
      return;
    }
    let stale = false;
    api
      .ouraHeartrateRange(night.bedtime_start, night.bedtime_end, 600)
      .then((pts) => {
        if (!stale) setHr(pts);
      })
      .catch(() => setHr([]));
    return () => {
      stale = true;
    };
  }, [night]);

  if (nights === null) return null;
  if (nights.length === 0) {
    return (
      <div className="health-section">
        <OuraHint>
          Per-night sleep detail comes from the Oura Ring
          {hasOura
            ? " — no sleep sessions have synced yet."
            : ". Connect it in the Integrations tab."}{" "}
          Apple Health sleep hours are charted under <strong>Metrics</strong>.
        </OuraHint>
      </div>
    );
  }

  const stageTotal = night
    ? STAGES.reduce((t, s) => t + (night[s.key] as number), 0)
    : 0;

  return (
    <div className="health-section sleep-section">
      {night && (
        <div className="sleep-detail">
          <div className="health-header">
            <div>
              <h2>
                {fmtDay(night.day)}
                {night.kind !== "long_sleep" && (
                  <span className="kind-chip">{titleCase(night.kind)}</span>
                )}
              </h2>
              <div className="health-header-sub">
                {fmtTime(night.bedtime_start)} → {fmtTime(night.bedtime_end)}
              </div>
            </div>
          </div>
          {stageTotal > 0 && (
            <>
              <div className="stage-bar">
                {STAGES.map((s) => {
                  const v = night[s.key] as number;
                  return v > 0 ? (
                    <div
                      key={s.label}
                      className="stage-seg"
                      title={`${s.label} ${fmtHours(v)}`}
                      style={{
                        width: `${(v / stageTotal) * 100}%`,
                        background: s.color,
                      }}
                    />
                  ) : null;
                })}
              </div>
              <div className="stage-legend">
                {STAGES.map((s) => (
                  <span key={s.label} className="stage-key">
                    <span className="source-dot" style={{ background: s.color }} />
                    {s.label} {fmtHours(night[s.key] as number)}
                  </span>
                ))}
              </div>
            </>
          )}
          <div className="stats-grid">
            <Stat label="Total sleep" value={fmtHours(night.total_hours)} />
            {night.efficiency != null && (
              <Stat label="Efficiency" value={`${Math.round(night.efficiency)}%`} />
            )}
            {night.latency_min != null && (
              <Stat label="Latency" value={`${Math.round(night.latency_min)}m`} />
            )}
            {night.average_hrv != null && (
              <Stat label="Avg HRV" value={`${Math.round(night.average_hrv)} ms`} />
            )}
            {night.lowest_heart_rate != null && (
              <Stat label="Lowest HR" value={`${Math.round(night.lowest_heart_rate)} bpm`} />
            )}
            {night.average_heart_rate != null && (
              <Stat label="Avg HR" value={`${Math.round(night.average_heart_rate)} bpm`} />
            )}
            {night.respiratory_rate != null && (
              <Stat label="Breath rate" value={`${night.respiratory_rate.toFixed(1)}/min`} />
            )}
          </div>
          <IntradayChart points={hr} color={SOURCE_META.oura.color} />
        </div>
      )}
      <div className="sleep-nights">
        {nights.map((n, i) => (
          <div
            key={`${n.day}-${n.bedtime_start}`}
            className={`sleep-night ${i === selected ? "active" : ""}`}
            onClick={() => setSelected(i)}
          >
            <span className="sleep-night-day">{fmtDay(n.day)}</span>
            {n.kind !== "long_sleep" && (
              <span className="kind-chip">{titleCase(n.kind)}</span>
            )}
            <span className="sleep-night-hours">{fmtHours(n.total_hours)}</span>
            <span className="sleep-night-bar">
              {STAGES.slice(0, 3).map((s) => {
                const v = n[s.key] as number;
                return n.total_hours > 0 && v > 0 ? (
                  <span
                    key={s.label}
                    style={{
                      width: `${(v / n.total_hours) * 100}%`,
                      background: s.color,
                    }}
                  />
                ) : null;
              })}
            </span>
            <span className="sleep-night-eff">
              {n.efficiency != null ? `${Math.round(n.efficiency)}%` : ""}
            </span>
          </div>
        ))}
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

function WorkoutsSection() {
  const [items, setItems] = useState<WorkoutItem[] | null>(null);

  useEffect(() => {
    api.healthWorkouts(200).then(setItems);
  }, []);

  if (items === null) return null;
  if (items.length === 0) {
    return (
      <div className="health-section">
        <OuraHint>
          No workouts yet — they arrive with an Apple Health import or an Oura
          Ring sync (Integrations tab).
        </OuraHint>
      </div>
    );
  }

  let lastDay = "";
  return (
    <div className="health-section">
      <div className="workout-list">
        {items.map((w, i) => {
          const dayHead = w.day !== lastDay ? fmtDay(w.day) : null;
          lastDay = w.day;
          return (
            <div key={`${w.source}-${w.start}-${i}`}>
              {dayHead && <div className="tl-day">{dayHead}</div>}
              <div className="workout-row">
                <span className="workout-time">{fmtTime(w.start)}</span>
                <span className="workout-activity">
                  {titleCase(w.activity)}
                  {w.label && <span className="workout-label"> · {w.label}</span>}
                </span>
                {w.kind === "session" && <span className="kind-chip">Session</span>}
                <span
                  className="source-chip"
                  style={{ color: SOURCE_META[w.source].color }}
                >
                  {SOURCE_META[w.source].label}
                </span>
                <span className="workout-stats">
                  {w.duration_min != null && `${Math.round(w.duration_min)} min`}
                  {w.calories != null && ` · ${Math.round(w.calories)} kcal`}
                  {w.distance_km != null &&
                    w.distance_km > 0 &&
                    ` · ${w.distance_km.toFixed(1)} km`}
                  {w.intensity && ` · ${w.intensity}`}
                </span>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function Segmented<T extends string>({
  options,
  value,
  onChange,
}: {
  options: { id: T; label: string }[];
  value: T;
  onChange: (v: T) => void;
}) {
  return (
    <div className="segmented">
      {options.map((o) => (
        <button
          key={o.id}
          className={value === o.id ? "active" : ""}
          onClick={() => onChange(o.id)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

function ImportingPanel({
  progress,
  compact,
}: {
  progress: ImportProgress;
  compact?: boolean;
}) {
  return (
    <div className={`import-panel ${compact ? "compact" : ""}`}>
      <div className="import-label">
        Importing… {progress.records.toLocaleString()} records
      </div>
      <div className="progress-track">
        <div
          className="progress-fill"
          style={{ width: `${Math.max(progress.percent, 1)}%` }}
        />
      </div>
    </div>
  );
}

import { useEffect, useState } from "react";
import { api, Bucket, HealthSource, Panel, SeriesPoint, SourceSeries, UnifiedMetric } from "../api";
import MultiChart, { ChartSeries } from "./MultiChart";
import Segmented from "./Segmented";
import SleepSessionsPane from "./SleepSessionsPane";
import SourceChips from "./SourceChips";
import { PinDialog } from "./ChartAnything";
import { SOURCE_META } from "./healthShared";

// One metric from the catalog. Every source the metric reports draws as its
// own line, filtered by the effective sources (the global filter, or this
// metric's override). Some metrics have a designed view instead of the
// chart — Sleep shows the sessions pane over the contract. "Pin" puts the
// metric on a board.

export type Range = "3m" | "1y" | "all";
const RANGES: { id: Range; label: string }[] = [
  { id: "3m", label: "3M" },
  { id: "1y", label: "1Y" },
  { id: "all", label: "All" },
];
const BUCKETS: { id: Bucket; label: string }[] = [
  { id: "day", label: "Day" },
  { id: "week", label: "Week" },
  { id: "month", label: "Month" },
];
const RANGE_DAYS: Record<Range, number> = { "3m": 90, "1y": 365, all: 1095 };

/** Metric slugs with a designed view in place of the chart. */
export const DESIGNED: Record<string, string> = { sleep: "sessions" };

function inRange(points: SeriesPoint[], range: Range): SeriesPoint[] {
  if (range === "all" || points.length === 0) return points;
  const last = new Date(`${points[points.length - 1].date}T00:00:00`);
  const cutoff = new Date(last);
  if (range === "3m") cutoff.setMonth(cutoff.getMonth() - 3);
  else cutoff.setFullYear(cutoff.getFullYear() - 1);
  const cut = cutoff.toISOString().slice(0, 10);
  return points.filter((p) => p.date >= cut);
}

/** Clip every source's series to the chosen range using a shared cutoff. */
export function seriesInRange(series: SourceSeries[], range: Range): ChartSeries[] {
  const all = series.flatMap((s) => s.points);
  const clipped = inRange(all.slice().sort((a, b) => a.date.localeCompare(b.date)), range);
  const cut = clipped[0]?.date ?? "";
  return series.map((s) => ({
    label: SOURCE_META[s.source].label,
    color: SOURCE_META[s.source].color,
    points: range === "all" ? s.points : s.points.filter((p) => p.date >= cut),
  }));
}

export default function MetricPane({
  metric,
  globalSources,
  override,
  onOverride,
  boards,
  onBoardsChanged,
}: {
  metric: UnifiedMetric;
  /** The global source filter. */
  globalSources: HealthSource[];
  /** This metric's override of the filter, if any. */
  override: HealthSource[] | null;
  onOverride: (sources: HealthSource[] | null) => void;
  boards: import("../api").Board[];
  onBoardsChanged: () => void;
}) {
  const [bucket, setBucket] = useState<Bucket>("day");
  const [range, setRange] = useState<Range>("1y");
  const [series, setSeries] = useState<SourceSeries[]>([]);
  const [pinning, setPinning] = useState(false);

  const reported = metric.sources.map((s) => s.source);
  const effective = (override ?? globalSources).filter((s) => reported.includes(s));
  const shownSources = effective.length ? effective : reported;

  useEffect(() => {
    let stale = false;
    api
      .healthSeriesUnified(metric.slug, bucket)
      .then((s) => !stale && setSeries(s))
      .catch(() => !stale && setSeries([]));
    return () => {
      stale = true;
    };
  }, [metric.slug, bucket]);

  const span = {
    first: metric.sources.map((s) => s.first_date).sort()[0],
    last: metric.sources.map((s) => s.last_date).sort().slice(-1)[0],
  };
  const shown = series.filter((s) => shownSources.includes(s.source));
  const designed = DESIGNED[metric.slug];

  const panel = (): Panel => ({
    title: metric.name,
    kind: "line",
    bucket,
    days: RANGE_DAYS[range],
    to: null,
    series: shownSources.map((s) => ({
      metric: metric.slug,
      source: s,
      agg: "avg",
      label: shownSources.length > 1 ? `${metric.name} · ${SOURCE_META[s].label}` : metric.name,
      unit: metric.unit,
      divide: null,
      table: "",
      column: "",
    })),
  });

  return (
    <>
      <div className="view-header">
        <div>
          <h2>{metric.name}</h2>
          <div className="health-header-sub">
            {span.first} → {span.last}
            {metric.unit && <span className="unit-chip">{metric.unit}</span>}
          </div>
        </div>
        <div className="health-controls">
          {reported.length > 1 && (
            <div className="override">
              <SourceChips
                available={reported as HealthSource[]}
                selected={shownSources as HealthSource[]}
                onChange={(next) => onOverride(next)}
                small
              />
              {override && (
                <button className="btn-link" onClick={() => onOverride(null)} title="Follow the global source filter again">
                  follow filter
                </button>
              )}
            </div>
          )}
          {!designed && <Segmented options={BUCKETS} value={bucket} onChange={setBucket} small />}
          {!designed && <Segmented options={RANGES} value={range} onChange={setRange} small />}
          <button className="btn-ghost" onClick={() => setPinning(true)}>
            Pin…
          </button>
        </div>
      </div>

      {designed === "sessions" ? (
        <SleepSessionsPane sources={shownSources} dedupe={shownSources.length > 1} embedded />
      ) : (
        <>
          <MultiChart series={seriesInRange(shown, range)} unit={metric.unit} kind={metric.kind} />
          {metric.note && <div className="health-footnote">{metric.note}</div>}
        </>
      )}
      <div className="health-footnote">
        Raw data:{" "}
        {metric.sources.map((s, i) => (
          <span key={s.source}>
            {i > 0 && " · "}
            {SOURCE_META[s.source].label}{" "}
            <code>{s.source === "oura" ? "~/Documents/Trove/health/oura/" : `~/Documents/Trove/health/${metric.slug}/`}</code>
          </span>
        ))}
      </div>

      {pinning && (
        <PinDialog
          boards={boards}
          panel={panel()}
          allowTile
          onClose={() => setPinning(false)}
          onPinned={() => {
            setPinning(false);
            onBoardsChanged();
          }}
        />
      )}
    </>
  );
}

import { useEffect, useState } from "react";
import { api, Board, BoardSeries, HealthSource, Panel, SeriesPoint, UnifiedMetric } from "../api";
import PanelChart, { PanelSeriesData } from "./PanelChart";
import { PALETTE, rangeFor, SOURCE_META } from "./healthShared";
import { columnLabel } from "./ChartAnything";

// One board: the merged view, made of pins. A panel's series is a catalog
// metric (typed read, per-source lines) or a table column (generic index).
// Files live at boards/<slug>.md (docs/vault-spec/boards.md). "overview"
// is the board the rail shows first; it is seeded on request, never
// silently.

export const OVERVIEW_SLUG = "overview";

const metricSeries = (metric: string, label: string, extra: Partial<BoardSeries> = {}): BoardSeries => ({
  metric,
  label,
  agg: "avg",
  source: "",
  table: "",
  column: "",
  divide: null,
  unit: "",
  ...extra,
});
const tableSeries = (table: string, column: string, agg: BoardSeries["agg"], label: string, extra: Partial<BoardSeries> = {}): BoardSeries => ({
  metric: "",
  source: "",
  table,
  column,
  agg,
  label,
  divide: null,
  unit: "",
  ...extra,
});

/** The Overview seed: the day's scores as tiles and the sleep trend. */
export function overviewBoard(metrics: UnifiedMetric[]): Board {
  const has = (slug: string) => metrics.some((m) => m.slug === slug);
  const tiles = ["readiness-score", "sleep-score", "activity-score", "stress-high", "steps", "resting-heart-rate"]
    .filter(has)
    .map((slug) => metricSeries(slug, metrics.find((m) => m.slug === slug)!.name, { unit: metrics.find((m) => m.slug === slug)!.unit }));
  const panels: Panel[] = [];
  if (tiles.length) panels.push({ title: "Latest", kind: "tile", bucket: "day", days: 14, to: null, series: tiles });
  if (has("sleep")) panels.push({ title: "Sleep", kind: "line", bucket: "day", days: 90, to: null, series: [metricSeries("sleep", "Sleep", { unit: "hr" })] });
  if (has("sleep-score")) panels.push({ title: "Sleep score", kind: "line", bucket: "day", days: 90, to: null, series: [metricSeries("sleep-score", "Sleep score")] });
  return { slug: OVERVIEW_SLUG, title: "Overview", panels, notes: "Pinned from any metric with “Pin…”. Edit this file by hand if you like." };
}

/** The five charts the read-side decision was judged on, as one board. */
export function starterBoard(): Board {
  const score = metricSeries("sleep-score", "Sleep score", { source: "oura" });
  const events = tableSeries("calendar/events", "@records", "sum", "Events");
  const asleep = tableSeries("health/sleep/oura", "asleep_seconds", "sum", "Hours asleep", { divide: 3600, unit: "h" });
  const panels: Panel[] = [
    { title: "Sleep score, gaps visible", kind: "line", bucket: "day", days: 90, to: null, series: [score] },
    { title: "Sleep score vs events per day", kind: "dual", bucket: "day", days: 90, to: null, series: [score, events] },
    { title: "Hours asleep vs events, by week", kind: "dual", bucket: "week", days: 182, to: null, series: [{ ...asleep, label: "Hours asleep / week" }, events] },
    { title: "Events per day, last year", kind: "heatmap", bucket: "day", days: 365, to: null, series: [events] },
    { title: "Missing nights over event bars", kind: "gaps", bucket: "day", days: 90, to: null, series: [events, asleep] },
  ];
  return {
    slug: "sleep-and-calendar",
    title: "Sleep × Calendar",
    panels,
    notes: "Does a packed calendar cost sleep? Five views over Oura sleep and the calendar.",
  };
}

export default function BoardPane({
  board,
  metrics,
  sources,
  onBoardsChanged,
}: {
  /** The board, or null for an Overview that has no file yet. */
  board: Board | null;
  metrics: UnifiedMetric[];
  /** The global source filter — metric series with no `source` follow it. */
  sources: HealthSource[];
  onBoardsChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);

  const write = async (b: Board) => {
    setBusy(true);
    try {
      await api.writeBoard(b);
      onBoardsChanged();
    } finally {
      setBusy(false);
    }
  };
  const remove = async (b: Board) => {
    if (!window.confirm(`Delete the board “${b.title}”? The file boards/${b.slug}.md is removed.`)) return;
    await api.deleteBoard(b.slug);
    onBoardsChanged();
  };

  if (!board || board.panels.length === 0) {
    const isOverview = !board || board.slug === OVERVIEW_SLUG;
    return (
      <>
        <div className="view-header">
          <div>
            <h2>{board?.title ?? "Overview"}</h2>
            <div className="health-header-sub">
              {board ? `boards/${board.slug}.md · empty` : "boards/overview.md · not created yet"}
            </div>
          </div>
          {board && (
            <button className="btn-ghost danger" onClick={() => remove(board)}>
              Delete
            </button>
          )}
        </div>
        <div className="oura-hint">
          <p>
            Nothing pinned yet. Open any metric and press <strong>Pin…</strong>, or seed this board.
          </p>
          <div className="health-controls" style={{ marginTop: 12 }}>
            {isOverview && (
              <button className="btn-primary" disabled={busy} onClick={() => write(overviewBoard(metrics))}>
                Seed Overview with today's scores
              </button>
            )}
            <button className="btn-ghost" disabled={busy} onClick={() => write(starterBoard())}>
              Add the Sleep × Calendar board
            </button>
          </div>
        </div>
      </>
    );
  }

  return (
    <>
      <div className="view-header">
        <div>
          <h2>{board.title}</h2>
          <div className="health-header-sub">
            boards/{board.slug}.md · {board.panels.length} {board.panels.length === 1 ? "panel" : "panels"}
          </div>
        </div>
        <button className="btn-ghost danger" onClick={() => remove(board)}>
          Delete
        </button>
      </div>
      {board.notes && <p className="view-intro board-notes">{board.notes}</p>}
      {board.panels.map((p, i) => (
        <BoardPanel
          key={`${board.slug}-${i}-${p.kind}`}
          panel={p}
          sources={sources}
          onRemove={() => write({ ...board, panels: board.panels.filter((_, j) => j !== i) })}
        />
      ))}
    </>
  );
}

function seriesLabel(s: BoardSeries): string {
  if (s.label) return s.label;
  if (s.metric) return s.metric;
  return columnLabel(s.column ?? "");
}

function seriesDesc(s: BoardSeries): string {
  if (s.metric) return `${s.metric}${s.source ? ` (${s.source})` : ""}`;
  return `${s.table} › ${columnLabel(s.column ?? "")} (${s.agg ?? "avg"})`;
}

/** Resolve one board series to drawable series (a metric may fan out to
 *  one line per source). */
async function loadSeries(
  s: BoardSeries,
  bucket: import("../api").Bucket,
  from: string,
  to: string,
  sources: HealthSource[],
  color: string
): Promise<PanelSeriesData[]> {
  const div = s.divide ?? null;
  const scale = (pts: SeriesPoint[]) => (div && div !== 0 ? pts.map((p) => ({ date: p.date, value: p.value / div })) : pts);
  const clip = (pts: SeriesPoint[]) => pts.filter((p) => p.date >= from && p.date <= to);
  if (s.metric) {
    const all = await api.healthSeriesUnified(s.metric, bucket).catch(() => []);
    const wanted = s.source ? all.filter((x) => x.source === s.source) : all.filter((x) => sources.includes(x.source));
    return wanted.map((x) => ({
      label: wanted.length > 1 || !s.label ? `${seriesLabel(s)} · ${SOURCE_META[x.source].label}` : seriesLabel(s),
      color: wanted.length > 1 ? SOURCE_META[x.source].color : color,
      unit: s.unit || undefined,
      points: scale(clip(x.points)),
    }));
  }
  const pts = await api.tableSeries(s.table ?? "", s.column ?? "", s.agg ?? "avg", bucket, from, to);
  return [{ label: seriesLabel(s), color, unit: s.unit || undefined, points: scale(pts) }];
}

function BoardPanel({ panel, sources, onRemove }: { panel: Panel; sources: HealthSource[]; onRemove: () => void }) {
  const [series, setSeries] = useState<PanelSeriesData[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const days = panel.days ?? 90;
  const bucket = panel.kind === "heatmap" || panel.kind === "tile" ? "day" : (panel.bucket ?? "day");
  const { from, to } = rangeFor(days, panel.to);

  useEffect(() => {
    let stale = false;
    setError(null);
    Promise.all(panel.series.map((s, i) => loadSeries(s, bucket, from, to, sources, PALETTE[i % PALETTE.length])))
      .then((groups) => !stale && setSeries(groups.flat()))
      .catch((e) => !stale && setError(String(e)));
    return () => {
      stale = true;
    };
  }, [panel, bucket, from, to, sources]);

  return (
    <div className="board-panel">
      <div className="board-panel-head">
        <div>
          <div className="view-section-title">{panel.title || seriesLabel(panel.series[0])}</div>
          <div className="health-header-sub">
            {panel.kind} · {bucket} · {from} → {to} · {panel.series.map(seriesDesc).join(" · ")}
          </div>
        </div>
        <button className="btn-link" onClick={onRemove} title="Remove this panel from the board">
          remove
        </button>
      </div>
      {error && <div className="health-error">{error}</div>}
      {series && <PanelChart kind={panel.kind} bucket={bucket} from={from} to={to} series={series} />}
    </div>
  );
}

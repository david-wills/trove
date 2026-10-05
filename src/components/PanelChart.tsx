import { useEffect, useMemo, useRef } from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import type { Bucket, PanelKind, SeriesPoint } from "../api";
import { bucketKeys, fmtDay, GAP_COLOR } from "./healthShared";

// One renderer for every board panel kind: line (gaps stay gaps; a second
// unit gets a right-hand axis), bars, dual (a forced two-axis line), gaps
// (bars with missing-day markers from a second series), heatmap (a calendar
// grid), tile (latest value per series). The x axis always spans the full
// requested range so a day with no data is a visible hole rather than a
// shorter chart.

const GRID = "#23262e";
const TICK = "#2a2d35";
const AXIS_TEXT = "#9a9da6";

export interface PanelSeriesData {
  label: string;
  color: string;
  unit?: string;
  points: SeriesPoint[];
}

function fill(color: string, alpha = 0.35): string {
  const r = parseInt(color.slice(1, 3), 16);
  const g = parseInt(color.slice(3, 5), 16);
  const b = parseInt(color.slice(5, 7), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

function epoch(day: string): number {
  return new Date(`${day}T00:00:00`).getTime() / 1000;
}

/** Lay every series over the same bucket keys; null where a series has no
 *  value in a slot. */
function align(keys: string[], series: PanelSeriesData[]): uPlot.AlignedData {
  const xs = keys.map(epoch);
  const ys = series.map((s) => {
    const byDate = new Map(s.points.map((p) => [p.date, p.value]));
    return keys.map((k) => byDate.get(k) ?? null);
  });
  return [xs, ...ys] as uPlot.AlignedData;
}

function axis(extra: Partial<uPlot.Axis> = {}): uPlot.Axis {
  return {
    stroke: AXIS_TEXT,
    grid: { stroke: GRID, width: 1 },
    ticks: { stroke: TICK, width: 1 },
    ...extra,
  };
}

function label(s: PanelSeriesData): string {
  return s.unit ? `${s.label} (${s.unit})` : s.label;
}

export default function PanelChart({
  kind,
  bucket,
  from,
  to,
  series,
  height = 300,
}: {
  kind: PanelKind;
  bucket: Bucket;
  from: string;
  to: string;
  series: PanelSeriesData[];
  height?: number;
}) {
  const keys = useMemo(() => bucketKeys(from, to, kind === "heatmap" ? "day" : bucket), [from, to, bucket, kind]);
  if (kind === "heatmap") return <Heatmap keys={keys} series={series[0]} />;
  if (kind === "tile") return <Tiles series={series} />;
  return <PlotPanel kind={kind} keys={keys} series={series} height={height} />;
}

function PlotPanel({
  kind,
  keys,
  series,
  height,
}: {
  kind: Exclude<PanelKind, "heatmap" | "tile">;
  keys: string[];
  series: PanelSeriesData[];
  height: number;
}) {
  const hostRef = useRef<HTMLDivElement>(null);
  const hasData = series.some((s) => s.points.length > 0);

  useEffect(() => {
    const host = hostRef.current;
    if (!host || !hasData || keys.length === 0) return;

    const few = keys.length <= 120;
    const bars = uPlot.paths.bars!({ size: [0.7, 100] });
    let drawn: PanelSeriesData[] = series;
    let plotSeries: uPlot.Series[] = [];
    const axes: uPlot.Axis[] = [axis(), axis({ size: 60 })];
    const scales: uPlot.Scales = {};
    const plugins: uPlot.Plugin[] = [];

    // A right-hand axis appears once for the first unit that differs from
    // the first series' (or for the second series of a forced `dual`).
    const rightAxis = (): string => {
      if (!scales.y2) {
        scales.y2 = {};
        axes.push(axis({ scale: "y2", side: 1, size: 56, grid: { show: false } }));
      }
      return "y2";
    };
    const scaleFor = (s: PanelSeriesData, i: number): string | undefined => {
      if (i === 0) return undefined;
      if (kind === "dual" && i === 1) return rightAxis();
      return (s.unit ?? "") !== (series[0].unit ?? "") ? rightAxis() : undefined;
    };

    if (kind === "line" || kind === "dual") {
      plotSeries = series.map((s, i) => {
        const scale = scaleFor(s, i);
        const asBars = kind === "dual" && i === 1;
        return {
          label: label(s),
          stroke: s.color,
          width: asBars ? 1 : 2,
          spanGaps: false,
          scale,
          fill: asBars ? fill(s.color, 0.25) : undefined,
          paths: asBars ? bars : undefined,
          points: { show: few && !asBars, size: 4, fill: s.color },
        };
      });
    } else if (kind === "bars") {
      drawn = series.slice(0, 1);
      plotSeries = drawn.map((s) => ({
        label: label(s),
        stroke: s.color,
        width: 1,
        fill: fill(s.color),
        paths: bars,
        points: { show: false },
      }));
    } else {
      // gaps: bars for the first series, a marker wherever the second has no value.
      const [a, b] = series;
      drawn = [a];
      plotSeries = [
        {
          label: label(a),
          stroke: a.color,
          width: 1,
          fill: fill(a.color),
          paths: bars,
          points: { show: false },
        },
      ];
      if (b) {
        const present = new Set(b.points.map((p) => p.date));
        const missing = keys.filter((k) => !present.has(k));
        plugins.push({
          hooks: {
            draw: (u) => {
              const ctx = u.ctx;
              ctx.save();
              ctx.fillStyle = GAP_COLOR;
              const top = u.bbox.top;
              const w = Math.max(2, (u.bbox.width / Math.max(keys.length, 1)) * 0.6);
              for (const day of missing) {
                const x = u.valToPos(epoch(day), "x", true);
                ctx.fillRect(x - w / 2, top, w, 6);
              }
              ctx.restore();
            },
          },
        });
      }
    }

    const opts: uPlot.Options = {
      width: host.clientWidth,
      height,
      series: [{}, ...plotSeries],
      axes,
      scales,
      plugins,
      cursor: { points: { size: 7 } },
    };
    const plot = new uPlot(opts, align(keys, drawn), host);
    const ro = new ResizeObserver(() => plot.setSize({ width: host.clientWidth, height }));
    ro.observe(host);
    return () => {
      ro.disconnect();
      plot.destroy();
    };
  }, [kind, keys, series, height, hasData]);

  if (!hasData) return <div className="chart-empty">No data in this range.</div>;
  const gapSeries = kind === "gaps" ? series[1] : null;
  return (
    <div>
      <div className="chart-host" ref={hostRef} />
      {gapSeries && (
        <div className="panel-note">
          <span className="gap-swatch" /> days with no {gapSeries.label.toLowerCase()} row
        </div>
      )}
    </div>
  );
}

/** One cell per day in week columns, shaded by value. */
function Heatmap({ keys, series }: { keys: string[]; series?: PanelSeriesData }) {
  if (!series || series.points.length === 0) {
    return <div className="chart-empty">No data in this range.</div>;
  }
  const byDate = new Map(series.points.map((p) => [p.date, p.value]));
  const max = Math.max(...series.points.map((p) => p.value), 0);
  // Pad the first week so columns start on Monday.
  const first = new Date(`${keys[0]}T00:00:00`);
  const lead = (first.getDay() + 6) % 7;
  const cells: (string | null)[] = [...Array(lead).fill(null), ...keys];
  const weeks: (string | null)[][] = [];
  for (let i = 0; i < cells.length; i += 7) weeks.push(cells.slice(i, i + 7));
  // A column is labelled when it holds the first days of a month.
  const monthLabels = weeks.map((w) => {
    const early = w.find((x) => x != null && x.slice(8, 10) <= "07");
    return early ? new Date(`${early}T00:00:00`).toLocaleDateString(undefined, { month: "short" }) : "";
  });
  return (
    <div className="heatmap-host">
      <div className="heatmap-months" style={{ gridTemplateColumns: `repeat(${weeks.length}, 12px)` }}>
        {monthLabels.map((m, i) => (
          <span key={i}>{i === 0 || m !== monthLabels[i - 1] ? m : ""}</span>
        ))}
      </div>
      <div className="heatmap-grid" style={{ gridTemplateColumns: `repeat(${weeks.length}, 12px)` }}>
        {weeks.map((w, wi) => (
          <div key={wi} className="heatmap-week">
            {Array.from({ length: 7 }, (_, di) => {
              const day = w[di] ?? null;
              const v = day ? byDate.get(day) : undefined;
              const alpha = v == null || max === 0 ? 0 : 0.15 + 0.85 * (v / max);
              return (
                <span
                  key={di}
                  className={`heatmap-cell ${day ? "" : "pad"} ${v == null ? "empty" : ""}`}
                  title={day ? `${day}: ${v == null ? "no data" : Math.round(v * 100) / 100}` : ""}
                  style={v != null ? { background: fill(series.color, alpha) } : undefined}
                />
              );
            })}
          </div>
        ))}
      </div>
      <div className="panel-note">
        {series.label}
        {series.unit ? ` (${series.unit})` : ""} · darker is more · max {Math.round(max * 100) / 100}
      </div>
    </div>
  );
}

export function fmtValue(v: number): string {
  if (Math.abs(v) >= 1000) return Math.round(v).toLocaleString();
  if (Math.abs(v) >= 100) return String(Math.round(v));
  return String(Math.round(v * 10) / 10);
}

/** The latest value of each series as a card. */
function Tiles({ series }: { series: PanelSeriesData[] }) {
  const cards = series.map((s) => {
    const last = s.points.length ? s.points[s.points.length - 1] : null;
    return { s, last };
  });
  if (cards.every((c) => !c.last)) return <div className="chart-empty">No data in this range.</div>;
  return (
    <div className="score-cards">
      {cards.map(({ s, last }) => (
        <div key={s.label} className="score-card" style={{ borderTopColor: s.color }}>
          <div className="score-name">{s.label}</div>
          <div className="score-value">{last ? fmtValue(last.value) : "—"}</div>
          <div className="score-sub">{s.unit ?? ""}</div>
          <div className="score-day">{last ? fmtDay(last.date) : "no data"}</div>
        </div>
      ))}
    </div>
  );
}

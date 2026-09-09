import { useEffect, useRef } from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import { HeartratePoint, MetricKind, SeriesPoint } from "../api";

const GRID = "#23262e";
const TICK = "#2a2d35";
const AXIS_TEXT = "#9a9da6";

export interface ChartSeries {
  label: string;
  color: string;
  points: SeriesPoint[];
}

/** Hex accent → translucent fill for bar bodies. */
function fill(color: string): string {
  const r = parseInt(color.slice(1, 3), 16);
  const g = parseInt(color.slice(3, 5), 16);
  const b = parseInt(color.slice(5, 7), 16);
  return `rgba(${r}, ${g}, ${b}, 0.35)`;
}

/** Union the dates of every series into one x axis, nulls where a source
 *  has no value that day. */
function align(series: ChartSeries[]): uPlot.AlignedData {
  const dates = Array.from(
    new Set(series.flatMap((s) => s.points.map((p) => p.date)))
  ).sort();
  const xs = dates.map((d) => new Date(`${d}T00:00:00`).getTime() / 1000);
  const ys = series.map((s) => {
    const byDate = new Map(s.points.map((p) => [p.date, p.value]));
    return dates.map((d) => byDate.get(d) ?? null);
  });
  return [xs, ...ys] as uPlot.AlignedData;
}

function axes(): uPlot.Axis[] {
  return [
    {
      stroke: AXIS_TEXT,
      grid: { stroke: GRID, width: 1 },
      ticks: { stroke: TICK, width: 1 },
    },
    {
      stroke: AXIS_TEXT,
      grid: { stroke: GRID, width: 1 },
      ticks: { stroke: TICK, width: 1 },
      size: 64,
    },
  ];
}

/** Daily series from one or more sources on a shared time axis. A single
 *  sum-like series reads as bars (matching Chart.tsx); overlaid sources are
 *  always lines so they never hide each other. */
export default function MultiChart({
  series,
  unit,
  kind,
  height = 380,
}: {
  series: ChartSeries[];
  unit: string;
  kind: MetricKind;
  height?: number;
}) {
  const hostRef = useRef<HTMLDivElement>(null);
  const nonEmpty = series.filter((s) => s.points.length > 0);

  useEffect(() => {
    const host = hostRef.current;
    if (!host || nonEmpty.length === 0) return;

    const bars = nonEmpty.length === 1 && kind !== "avg";
    const few = nonEmpty.every((s) => s.points.length <= 120);
    const opts: uPlot.Options = {
      width: host.clientWidth,
      height,
      series: [
        {},
        ...nonEmpty.map((s) => ({
          label: unit ? `${s.label} (${unit})` : s.label,
          stroke: s.color,
          width: 2,
          spanGaps: true,
          fill: bars ? fill(s.color) : undefined,
          paths: bars ? uPlot.paths.bars!({ size: [0.7, 100] }) : undefined,
          points: { show: !bars && few, size: 5, fill: s.color },
        })),
      ],
      axes: axes(),
      cursor: { points: { size: 7 } },
    };

    const plot = new uPlot(opts, align(nonEmpty), host);
    const ro = new ResizeObserver(() => {
      plot.setSize({ width: host.clientWidth, height });
    });
    ro.observe(host);
    return () => {
      ro.disconnect();
      plot.destroy();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [series, unit, kind, height]);

  if (nonEmpty.length === 0) {
    return <div className="chart-empty">No data in this range.</div>;
  }
  return <div className="chart-host" ref={hostRef} />;
}

/** Intraday heart-rate line (x = real timestamps, not days). */
export function IntradayChart({
  points,
  color,
  height = 220,
}: {
  points: HeartratePoint[];
  color: string;
  height?: number;
}) {
  const hostRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host || points.length === 0) return;

    const xs = points.map((p) => new Date(p.ts).getTime() / 1000);
    const ys = points.map((p) => p.bpm);
    const opts: uPlot.Options = {
      width: host.clientWidth,
      height,
      series: [
        {},
        {
          label: "Heart rate (bpm)",
          stroke: color,
          width: 2,
          spanGaps: true,
          points: { show: false },
        },
      ],
      axes: axes(),
      cursor: { points: { size: 7 } },
    };

    const plot = new uPlot(opts, [xs, ys], host);
    const ro = new ResizeObserver(() => {
      plot.setSize({ width: host.clientWidth, height });
    });
    ro.observe(host);
    return () => {
      ro.disconnect();
      plot.destroy();
    };
  }, [points, color, height]);

  if (points.length === 0) {
    return <div className="chart-empty">No heart-rate samples for this night.</div>;
  }
  return <div className="chart-host" ref={hostRef} />;
}

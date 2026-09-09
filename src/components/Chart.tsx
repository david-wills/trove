import { useEffect, useRef } from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import { MetricKind, SeriesPoint } from "../api";

interface ChartProps {
  points: SeriesPoint[];
  name: string;
  unit: string;
  kind: MetricKind;
}

const ACCENT = "#d4a847";
const GRID = "#23262e";
const TICK = "#2a2d35";
const AXIS_TEXT = "#9a9da6";

function toAligned(points: SeriesPoint[]): uPlot.AlignedData {
  const xs = points.map((p) => new Date(`${p.date}T00:00:00`).getTime() / 1000);
  const ys = points.map((p) => p.value);
  return [xs, ys];
}

export default function Chart({ points, name, unit, kind }: ChartProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const plotRef = useRef<uPlot | null>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host || points.length === 0) return;

    // Sums read best as bars, measurements as a line.
    const bars = kind !== "avg";
    const opts: uPlot.Options = {
      width: host.clientWidth,
      height: 380,
      series: [
        {},
        {
          label: unit ? `${name} (${unit})` : name,
          stroke: ACCENT,
          width: 2,
          fill: bars ? "rgba(212, 168, 71, 0.35)" : undefined,
          paths: bars ? uPlot.paths.bars!({ size: [0.7, 100] }) : undefined,
          points: { show: !bars && points.length <= 120, size: 5, fill: ACCENT },
        },
      ],
      axes: [
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
      ],
      cursor: { points: { size: 7 } },
    };

    const plot = new uPlot(opts, toAligned(points), host);
    plotRef.current = plot;

    const ro = new ResizeObserver(() => {
      plot.setSize({ width: host.clientWidth, height: 380 });
    });
    ro.observe(host);

    return () => {
      ro.disconnect();
      plot.destroy();
      plotRef.current = null;
    };
  }, [points, name, unit, kind]);

  if (points.length === 0) {
    return <div className="chart-empty">No data in this range.</div>;
  }
  return <div className="chart-host" ref={hostRef} />;
}

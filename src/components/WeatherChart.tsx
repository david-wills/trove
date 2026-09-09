import { useEffect, useRef } from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";

export interface WeatherSeries {
  label: string;
  color: string;
  values: number[];
  /** Render this series as bars (sums); lines otherwise. */
  bars?: boolean;
}

interface WeatherChartProps {
  /** YYYY-MM-DD per point, shared by every series. */
  dates: string[];
  series: WeatherSeries[];
  unit: string;
  /** Fill the area between series 1 and 2 (high/low envelope). */
  band?: boolean;
  height?: number;
}

const GRID = "#23262e";
const TICK = "#2a2d35";
const AXIS_TEXT = "#9a9da6";

function withAlpha(hex: string, alpha: number): string {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

export default function WeatherChart({
  dates,
  series,
  unit,
  band,
  height = 260,
}: WeatherChartProps) {
  const hostRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host || dates.length === 0) return;

    const xs = dates.map((d) => new Date(`${d}T00:00:00`).getTime() / 1000);
    const data: uPlot.AlignedData = [xs, ...series.map((s) => s.values)];

    const opts: uPlot.Options = {
      width: host.clientWidth,
      height,
      series: [
        {},
        ...series.map((s) => ({
          label: unit ? `${s.label} (${unit})` : s.label,
          stroke: s.color,
          width: 2,
          fill: s.bars ? withAlpha(s.color, 0.35) : undefined,
          paths: s.bars ? uPlot.paths.bars!({ size: [0.7, 100] }) : undefined,
          points: {
            show: !s.bars && dates.length <= 120,
            size: 5,
            fill: s.color,
          },
        })),
      ],
      bands:
        band && series.length >= 2
          ? [{ series: [1, 2], fill: withAlpha(series[0].color, 0.08) }]
          : undefined,
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

    const plot = new uPlot(opts, data, host);

    const ro = new ResizeObserver(() => {
      plot.setSize({ width: host.clientWidth, height });
    });
    ro.observe(host);

    return () => {
      ro.disconnect();
      plot.destroy();
    };
  }, [dates, series, unit, band, height]);

  if (dates.length === 0) {
    return <div className="chart-empty">No data in this range.</div>;
  }
  return <div className="chart-host" ref={hostRef} />;
}

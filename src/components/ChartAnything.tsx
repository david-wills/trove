import { useEffect, useMemo, useState } from "react";
import { Agg, api, Board, BoardSeries, Bucket, ColumnInfo, Panel, PanelKind, SeriesPoint, TableInfo } from "../api";
import PanelChart from "./PanelChart";
import Segmented from "./Segmented";
import { PALETTE, rangeFor } from "./healthShared";

// The generic chart: pick a table → a numeric column → how to fold it →
// see it over time, no mapping. Anything charted here can be pinned to a
// board, which is how a board's panels come to exist.

const AGGS: { id: Agg; label: string }[] = [
  { id: "avg", label: "Avg" },
  { id: "sum", label: "Sum" },
  { id: "min", label: "Min" },
  { id: "max", label: "Max" },
  { id: "count", label: "Count" },
];
const BUCKETS: { id: Bucket; label: string }[] = [
  { id: "day", label: "Day" },
  { id: "week", label: "Week" },
  { id: "month", label: "Month" },
];
const SPANS: { id: string; label: string }[] = [
  { id: "30", label: "30d" },
  { id: "90", label: "90d" },
  { id: "365", label: "1Y" },
  { id: "1095", label: "3Y" },
];
const KINDS: { id: PanelKind; label: string }[] = [
  { id: "line", label: "Line" },
  { id: "bars", label: "Bars" },
  { id: "heatmap", label: "Heatmap" },
];

export function columnLabel(name: string): string {
  return name === "@records" ? "records per day" : name;
}

export default function ChartAnything({
  tables,
  /** Restrict the table list to these ids (a source pane); all when absent. */
  only,
  boards,
  onBoardsChanged,
  initialTable,
}: {
  tables: TableInfo[];
  only?: string[];
  boards: Board[];
  onBoardsChanged: () => void;
  /** Start on this table (a source pane's "Chart →"). */
  initialTable?: string;
}) {
  const visible = useMemo(
    () => (only ? tables.filter((t) => only.includes(t.id)) : tables),
    [tables, only]
  );
  const [table, setTable] = useState<string>(initialTable ?? "");
  const [columns, setColumns] = useState<ColumnInfo[] | null>(null);
  const [column, setColumn] = useState<string>("");
  const [agg, setAgg] = useState<Agg>("avg");
  const [bucket, setBucket] = useState<Bucket>("day");
  const [span, setSpan] = useState("90");
  const [kind, setKind] = useState<PanelKind>("line");
  const [points, setPoints] = useState<SeriesPoint[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pinning, setPinning] = useState(false);

  useEffect(() => {
    if (initialTable) setTable(initialTable);
  }, [initialTable]);

  // Start on a health table when there is one — this pane lives in Health.
  useEffect(() => {
    if (table || visible.length === 0) return;
    setTable((visible.find((t) => t.id.startsWith("health/")) ?? visible[0]).id);
  }, [visible, table]);

  useEffect(() => {
    if (!table) return;
    let stale = false;
    setColumns(null);
    setError(null);
    api
      .tableColumns(table)
      .then((tc) => {
        if (stale) return;
        setColumns(tc.columns);
        setColumn((c) => (tc.columns.some((x) => x.name === c) ? c : tc.columns[1]?.name ?? tc.columns[0]?.name ?? ""));
      })
      .catch((e) => !stale && setError(String(e)));
    return () => {
      stale = true;
    };
  }, [table]);

  const { from, to } = useMemo(() => rangeFor(Number(span)), [span]);

  useEffect(() => {
    if (!table || !column) return;
    let stale = false;
    api
      .tableSeries(table, column, agg, kind === "heatmap" ? "day" : bucket, from, to)
      .then((p) => !stale && setPoints(p))
      .catch((e) => !stale && setError(String(e)));
    return () => {
      stale = true;
    };
  }, [table, column, agg, bucket, kind, from, to]);

  const col = columns?.find((c) => c.name === column) ?? null;

  const panel = (): Panel => ({
    title: `${columnLabel(column)} · ${table}`,
    kind,
    bucket: kind === "heatmap" ? "day" : bucket,
    days: Number(span),
    to: null,
    series: [{ metric: "", source: "", table, column, agg, label: columnLabel(column), divide: null, unit: "" } as BoardSeries],
  });

  return (
    <div className="health-section chart-anything">
      <div className="view-header">
        <div>
          <h2>Chart anything</h2>
          <div className="health-header-sub">any numeric column of any table, no mapping</div>
        </div>
        <button className="btn-ghost" disabled={!column || !points?.length} onClick={() => setPinning(true)}>
          Pin to a board…
        </button>
      </div>

      <div className="ca-controls">
        <label>
          <span>Table</span>
          <select value={table} onChange={(e) => setTable(e.target.value)}>
            {visible.map((t) => (
              <option key={t.id} value={t.id}>
                {t.id}
                {t.dated && t.first ? `  (${t.first} → ${t.last})` : ""}
              </option>
            ))}
          </select>
        </label>
        <label>
          <span>Column</span>
          <select value={column} onChange={(e) => setColumn(e.target.value)} disabled={!columns}>
            {(columns ?? []).map((c) => (
              <option key={c.name} value={c.name}>
                {columnLabel(c.name)}
              </option>
            ))}
          </select>
        </label>
        <Segmented options={AGGS} value={agg} onChange={setAgg} small />
        <Segmented options={BUCKETS} value={bucket} onChange={setBucket} small />
        <Segmented options={SPANS} value={span} onChange={setSpan} small />
        <Segmented options={KINDS} value={kind} onChange={setKind} small />
      </div>

      {error && <div className="health-error">{error}</div>}
      {col && (
        <div className="health-header-sub ca-coverage">
          {col.records.toLocaleString()} values over {col.days.toLocaleString()} days · {col.first} → {col.last} ·
          min {fmt(col.min)} · max {fmt(col.max)}
        </div>
      )}
      {points && column && (
        <PanelChart
          kind={kind}
          bucket={bucket}
          from={from}
          to={to}
          series={[{ label: columnLabel(column), color: PALETTE[0], points }]}
        />
      )}

      {pinning && (
        <PinDialog
          boards={boards}
          panel={panel()}
          onClose={() => setPinning(false)}
          onPinned={() => {
            setPinning(false);
            onBoardsChanged();
          }}
        />
      )}
    </div>
  );
}

function fmt(v: number): string {
  return Math.abs(v) >= 1000 ? Math.round(v).toLocaleString() : String(Math.round(v * 100) / 100);
}

/** Append a panel to an existing board or a new one. */
export function PinDialog({
  boards,
  panel,
  allowTile,
  onClose,
  onPinned,
}: {
  boards: Board[];
  panel: Panel;
  /** Offer "latest value" tiles as well as the chart (metric pins). */
  allowTile?: boolean;
  onClose: () => void;
  onPinned: (board: Board) => void;
}) {
  const overview = boards.find((b) => b.slug === "overview");
  const [target, setTarget] = useState<string>(overview ? "overview" : boards[0]?.slug ?? "overview");
  const [title, setTitle] = useState("");
  const [asTile, setAsTile] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const pin = async () => {
    setBusy(true);
    setError(null);
    try {
      const pinned: Panel = asTile ? { ...panel, kind: "tile", bucket: "day", days: 14 } : panel;
      const existing = boards.find((b) => b.slug === target);
      const board: Board = existing
        ? { ...existing, panels: [...existing.panels, pinned] }
        : target === "overview"
          ? { slug: "overview", title: "Overview", panels: [pinned], notes: "" }
          : { slug: "", title: title.trim() || "My board", panels: [pinned], notes: "" };
      const saved = await api.writeBoard(board);
      onPinned(saved);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <div className="norm-modal-backdrop" onClick={onClose}>
      <div className="norm-modal pin-dialog" onClick={(e) => e.stopPropagation()}>
        <h2 className="norm-modal-title">Pin to a board</h2>
        <p className="view-intro">
          Boards are files in <code>~/Documents/Trove/boards/</code>. This panel is appended as
          YAML you can edit by hand.
        </p>
        {allowTile && (
          <div className="pin-field">
            <span>As</span>
            <Segmented
              options={[
                { id: "chart", label: "Chart" },
                { id: "tile", label: "Latest value" },
              ]}
              value={asTile ? "tile" : "chart"}
              onChange={(v) => setAsTile(v === "tile")}
              small
            />
          </div>
        )}
        <label className="pin-field">
          <span>Board</span>
          <select value={target} onChange={(e) => setTarget(e.target.value)}>
            {!overview && <option value="overview">Overview</option>}
            {boards.map((b) => (
              <option key={b.slug} value={b.slug}>
                {b.title}
              </option>
            ))}
            <option value="__new">New board…</option>
          </select>
        </label>
        {target === "__new" && (
          <label className="pin-field">
            <span>Title</span>
            <input
              className="norm-input"
              value={title}
              placeholder="Sleep × Calendar"
              onChange={(e) => setTitle(e.target.value)}
              autoFocus
            />
          </label>
        )}
        {error && <div className="health-error">{error}</div>}
        <div className="pin-actions">
          <button className="btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="btn-primary" disabled={busy} onClick={pin}>
            {busy ? "Saving…" : "Pin"}
          </button>
        </div>
      </div>
    </div>
  );
}

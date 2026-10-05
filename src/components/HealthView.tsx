import { useCallback, useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { api, Board, HealthSource, ImportProgress, TableInfo, UnifiedMetric } from "../api";
import BoardPane, { OVERVIEW_SLUG } from "./BoardPane";
import ChartAnything from "./ChartAnything";
import MetricPane, { DESIGNED } from "./MetricPane";
import SourceChips from "./SourceChips";
import WorkoutsPane from "./WorkoutsPane";
import { SOURCE_META } from "./healthShared";

// Health: metric-first, source as a filter, boards as the merged view
// (docs/roadmap.md, S7-health decision). The rail lists the Overview board,
// other boards, every metric in the catalog, and the generic chart. A
// source multi-select at the top applies to every metric; a metric can
// override it. Some metrics open a designed view (Sleep → sessions);
// the rest open their chart. Anything can be pinned to a board.

type Pick =
  | { kind: "board"; slug: string }
  | { kind: "metric"; slug: string }
  | { kind: "workouts" }
  | { kind: "chart"; table?: string };

const SOURCES_KEY = "trove.health.sources";
const OVERRIDES_KEY = "trove.health.source-overrides";
const PICK_KEY = "trove.health.pick";
const SOURCE_ORDER: HealthSource[] = ["oura", "apple-health"];

function isSource(s: unknown): s is HealthSource {
  return s === "oura" || s === "apple-health";
}

function loadJson<T>(key: string, fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    return v ? (JSON.parse(v) as T) : fallback;
  } catch {
    return fallback;
  }
}

function isHealthExport(path: string): boolean {
  return /\.(zip|xml)$/i.test(path);
}

export default function HealthView() {
  const [metrics, setMetrics] = useState<UnifiedMetric[]>([]);
  const [tables, setTables] = useState<TableInfo[]>([]);
  const [boards, setBoards] = useState<Board[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [importing, setImporting] = useState<ImportProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  const [pick, setPick] = useState<Pick | null>(() => loadJson<Pick | null>(PICK_KEY, null));
  // Global source filter (default: every source with data) and per-metric overrides.
  const [enabled, setEnabled] = useState<HealthSource[] | null>(() => {
    const kept = loadJson<unknown[]>(SOURCES_KEY, []).filter(isSource);
    return kept.length ? kept : null;
  });
  const [overrides, setOverrides] = useState<Record<string, HealthSource[]>>(() =>
    loadJson<Record<string, HealthSource[]>>(OVERRIDES_KEY, {})
  );

  const available = useMemo(
    () => SOURCE_ORDER.filter((s) => metrics.some((m) => m.sources.some((x) => x.source === s))),
    [metrics]
  );
  const sources = useMemo(() => {
    const on = (enabled ?? available).filter((s) => available.includes(s));
    return on.length ? on : available;
  }, [enabled, available]);

  useEffect(() => {
    if (enabled) localStorage.setItem(SOURCES_KEY, JSON.stringify(enabled));
  }, [enabled]);
  useEffect(() => {
    localStorage.setItem(OVERRIDES_KEY, JSON.stringify(overrides));
  }, [overrides]);
  useEffect(() => {
    if (pick) localStorage.setItem(PICK_KEY, JSON.stringify(pick));
  }, [pick]);

  const refreshBoards = useCallback(() => {
    api.listBoards().then(setBoards).catch(() => setBoards([]));
  }, []);

  const refresh = useCallback(async () => {
    try {
      const [ms, ts] = await Promise.all([api.healthMetricsUnified(), api.listTables().catch(() => [] as TableInfo[])]);
      setMetrics(ms);
      setTables(ts);
      refreshBoards();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoaded(true);
    }
  }, [refreshBoards]);

  useEffect(() => {
    refresh();
  }, [refresh]);

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

  // Metrics the rail shows: at least one enabled source, matching the search.
  const visibleMetrics = useMemo(() => {
    const q = search.trim().toLowerCase();
    return metrics.filter(
      (m) => m.sources.some((s) => sources.includes(s.source)) && (!q || m.name.toLowerCase().includes(q) || m.slug.includes(q))
    );
  }, [metrics, sources, search]);
  const hiddenBySource = metrics.length - metrics.filter((m) => m.sources.some((s) => sources.includes(s.source))).length;

  // The pick must point at something that exists; the baseline is the first metric.
  const current: Pick | null = useMemo(() => {
    if (metrics.length === 0) return null;
    if (pick?.kind === "metric" && metrics.some((m) => m.slug === pick.slug)) return pick;
    if (pick?.kind === "board" && (pick.slug === OVERVIEW_SLUG || boards.some((b) => b.slug === pick.slug))) return pick;
    if (pick?.kind === "workouts" || pick?.kind === "chart") return pick;
    return { kind: "metric", slug: visibleMetrics[0]?.slug ?? metrics[0].slug };
  }, [pick, metrics, boards, visibleMetrics]);

  if (loaded && metrics.length === 0) {
    return (
      <div className="health-empty">
        {importing ? (
          <ImportingPanel progress={importing} />
        ) : (
          <>
            <h2>Bring your health data home</h2>
            <p>
              On your iPhone, open <strong>Health</strong>, tap your picture, then{" "}
              <strong>Export All Health Data</strong>. AirDrop the <code>export.zip</code> to this Mac and import it
              here — every metric becomes plain CSV files in <code>~/Documents/Trove/health</code>.
            </p>
            <button className="btn-primary" onClick={pickAndImport}>
              Import export.zip…
            </button>
            <p className="health-hint">or drop the file anywhere in this window</p>
            <p className="health-hint">Wear an Oura Ring? Connect it in the Integrations tab and its data lands here too.</p>
            {error && <div className="health-error">{error}</div>}
          </>
        )}
      </div>
    );
  }

  if (!loaded || !current) return null;

  const otherBoards = boards.filter((b) => b.slug !== OVERVIEW_SLUG);
  const overview = boards.find((b) => b.slug === OVERVIEW_SLUG) ?? null;
  const isActive = (p: Pick) => JSON.stringify(p) === JSON.stringify(current);
  const railItem = (p: Pick, name: React.ReactNode, sub?: React.ReactNode) => (
    <div key={JSON.stringify(p)} className={`metric-item ${isActive(p) ? "active" : ""}`} onClick={() => setPick(p)}>
      <div className="metric-item-name">{name}</div>
      {sub && <div className="metric-item-sub">{sub}</div>}
    </div>
  );

  return (
    <div className="health-shell">
      <div className="health-shape-bar">
        <span className="health-shape-label">
          Health{" "}
          <span className="health-shape-hint">
            · {metrics.length} metrics · {available.map((s) => SOURCE_META[s].label).join(" + ")}
          </span>
        </span>
        <div className="health-controls">
          {importing && <ImportingPanel progress={importing} compact />}
          {available.length > 1 && (
            <>
              <span className="health-shape-hint">Sources</span>
              <SourceChips available={available} selected={sources} onChange={setEnabled} />
            </>
          )}
        </div>
      </div>
      {error && <div className="health-error">{error}</div>}
      <div className="view view--split">
        <div className="view-rail">
          <div className="view-rail-header">
            <span className="view-rail-title">Health</span>
            <button className="btn-new" onClick={pickAndImport} title="Import a new Apple Health export" disabled={!!importing}>
              +
            </button>
          </div>
          <div className="view-rail-items">
            {railItem({ kind: "board", slug: OVERVIEW_SLUG }, "Overview", overview ? `${overview.panels.length} pinned` : "nothing pinned yet")}
            {otherBoards.length > 0 && <div className="view-rail-title rail-group">Boards</div>}
            {otherBoards.map((b) => railItem({ kind: "board", slug: b.slug }, b.title, `${b.panels.length} panels`))}

            <div className="view-rail-title rail-group">Metrics</div>
            <input
              className="artifacts-search rail-search"
              type="search"
              placeholder="Filter metrics…"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
            {visibleMetrics.map((m) =>
              railItem(
                { kind: "metric", slug: m.slug },
                <>
                  {m.name}
                  {DESIGNED[m.slug] && (
                    <span className="designed-mark" title="Has a designed view">
                      ▤
                    </span>
                  )}
                  {overrides[m.slug] && (
                    <span className="designed-mark" title="Source override set">
                      ◐
                    </span>
                  )}
                </>,
                <>
                  {m.sources.reduce((n, s) => n + s.records, 0).toLocaleString()} records
                  <span className="metric-item-dots">
                    {m.sources.map((s) => (
                      <span
                        key={s.source}
                        className="source-dot"
                        title={SOURCE_META[s.source].label}
                        style={{ background: sources.includes(s.source) ? SOURCE_META[s.source].color : "var(--text-faint)" }}
                      />
                    ))}
                  </span>
                </>
              )
            )}
            {railItem(
              { kind: "workouts" },
              <>
                Workouts<span className="designed-mark">▤</span>
              </>,
              "sessions from every source"
            )}
            {hiddenBySource > 0 && <div className="artifacts-empty">{hiddenBySource} metrics hidden by the source filter</div>}

            <div className="view-rail-title rail-group">Tools</div>
            {railItem({ kind: "chart" }, "Chart anything", `${tables.length} tables in the vault`)}
          </div>
        </div>
        <div className="view-body">
          {current.kind === "metric" && (
            <MetricPane
              key={current.slug}
              metric={metrics.find((m) => m.slug === current.slug)!}
              globalSources={sources}
              override={overrides[current.slug] ?? null}
              onOverride={(next) =>
                setOverrides((o) => {
                  const copy = { ...o };
                  if (next) copy[current.slug] = next;
                  else delete copy[current.slug];
                  return copy;
                })
              }
              boards={boards}
              onBoardsChanged={refreshBoards}
            />
          )}
          {current.kind === "board" && (
            <BoardPane
              key={current.slug}
              board={boards.find((b) => b.slug === current.slug) ?? null}
              metrics={metrics}
              sources={sources}
              onBoardsChanged={refreshBoards}
            />
          )}
          {current.kind === "workouts" && <WorkoutsPane sources={sources} />}
          {current.kind === "chart" && (
            <ChartAnything tables={tables} boards={boards} onBoardsChanged={refreshBoards} initialTable={current.table} />
          )}
        </div>
      </div>
    </div>
  );
}

function ImportingPanel({ progress, compact }: { progress: ImportProgress; compact?: boolean }) {
  return (
    <div className={`import-panel ${compact ? "compact" : ""}`}>
      <div className="import-label">Importing… {progress.records.toLocaleString()} records</div>
      <div className="progress-track">
        <div className="progress-fill" style={{ width: `${Math.max(progress.percent, 1)}%` }} />
      </div>
    </div>
  );
}

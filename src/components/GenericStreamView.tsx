import { useCallback, useEffect, useRef, useState } from "react";
import { api, JsonValue, StreamPage } from "../api";

// A compact raw-records browser over any date-partitioned JSONL folder —
// the generic floor every integration gets for free. Rows are best-effort:
// timestamp + headline guessed from common field names, click for raw JSON.

const PAGE = 30;
const TS_KEYS = ["ts", "time", "start", "day"];
const HEADLINE_KEYS = ["title", "text", "name", "summary", "track", "app", "url"];

type Rec = Record<string, JsonValue>;

function asRecord(v: JsonValue): Rec | null {
  return v !== null && typeof v === "object" && !Array.isArray(v)
    ? (v as Rec)
    : null;
}

function strField(rec: Rec, keys: string[]): string | null {
  for (const k of keys) {
    const v = rec[k];
    if (typeof v === "string" && v) return v;
  }
  return null;
}

function recTimestamp(rec: Rec): string {
  const ts = strField(rec, TS_KEYS);
  return ts ? ts.slice(0, 16).replace("T", " ") : "";
}

function recHeadline(rec: Rec): string {
  const hit = strField(rec, HEADLINE_KEYS);
  if (hit) return hit;
  // Fall back to the first reasonably short string field.
  for (const v of Object.values(rec)) {
    if (typeof v === "string" && v && v.length <= 120) return v;
  }
  return "(record)";
}

export default function GenericStreamView({
  dir,
  title,
  onFirstPage,
}: {
  /** Vault-relative folder of date-partitioned JSONL files. */
  dir: string;
  title?: string;
  /** Fired once with the first page (lets the hub fall back to the manifest
   *  when a folder turns out not to be a stream itself). */
  onFirstPage?: (page: StreamPage) => void;
}) {
  const [records, setRecords] = useState<JsonValue[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [done, setDone] = useState(false);
  const [loading, setLoading] = useState(false);
  const [expanded, setExpanded] = useState<number | null>(null);

  // Via a ref so an inline callback prop can't retrigger the load effect.
  const onFirstPageRef = useRef(onFirstPage);
  onFirstPageRef.current = onFirstPage;

  const loadMore = useCallback(
    async (offset: number) => {
      setLoading(true);
      let page: StreamPage = { records: [], partitions: [] };
      try {
        page = await api.readStream(dir, PAGE, offset);
      } catch {
        // Missing / non-stream folder: empty is fine, never crash the card.
      }
      setRecords((prev) => (offset === 0 ? page.records : [...prev, ...page.records]));
      if (page.records.length < PAGE) setDone(true);
      setLoading(false);
      setLoaded(true);
      if (offset === 0) onFirstPageRef.current?.(page);
    },
    [dir]
  );

  useEffect(() => {
    setRecords([]);
    setLoaded(false);
    setDone(false);
    setExpanded(null);
    loadMore(0);
  }, [loadMore]);

  if (!loaded) return null;

  return (
    <div className="gsv">
      {title && <div className="gsv-title">{title}</div>}
      {records.length === 0 ? (
        <div className="gsv-empty">No data yet.</div>
      ) : (
        <>
          {records.map((r, i) => {
            const rec = asRecord(r) ?? {};
            const source = typeof rec.source === "string" ? rec.source : null;
            return (
              <div key={i}>
                <div
                  className="tl-row"
                  onClick={() => setExpanded(expanded === i ? null : i)}
                >
                  <span className="tl-time">{recTimestamp(rec)}</span>
                  <span className="tl-title">{recHeadline(rec)}</span>
                  {source && <span className="source-chip">{source}</span>}
                </div>
                {expanded === i && (
                  <pre className="gsv-raw">{JSON.stringify(r, null, 2)}</pre>
                )}
              </div>
            );
          })}
          {!done && (
            <button
              className="sync-link gsv-more"
              disabled={loading}
              onClick={() => loadMore(records.length)}
            >
              {loading ? "Loading…" : "Load more"}
            </button>
          )}
        </>
      )}
    </div>
  );
}

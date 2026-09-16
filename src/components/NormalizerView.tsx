import { useCallback, useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { listen } from "@tauri-apps/api/event";
import {
  api,
  Binding,
  ContractCandidate,
  DeclinedDrop,
  Detection,
  ImportProgress,
  JsonValue,
  LlmAdvisorStatus,
  Mapping,
  PreviewResult,
  RawPage,
  SuggestCandidateMeta,
  SuggestPayloadResponse,
} from "../api";

// The drop-in normalizer (R2). Drop a CSV/JSONL file and it resolves to one of
// three fates: route to a built importer, map to a ratified contract, or land
// raw. All the smarts live in `trove-core::normalizer`; this view is the flow.

const COERCIONS = ["", "date", "number", "split", "value_map"] as const;
const DROP_EXT = /\.(csv|jsonl|json)$/i;
const RAW_PAGE = 30;

type Phase = "idle" | "detecting" | "outcome";

function baseName(path: string): string {
  const parts = path.split(/[/\\]/);
  return parts[parts.length - 1] || path;
}

function slugify(s: string): string {
  return s
    .replace(/\.[^.]+$/, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 60);
}

const isSlug = (s: string) => /^[a-z0-9]+(-[a-z0-9]+)*$/.test(s);

function cellText(v: JsonValue | undefined): string {
  if (v === undefined || v === null) return "";
  if (typeof v === "string") return v;
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

/** The header label for a column index (blank/unnamed → colN, matching core). */
function headerKey(headers: string[], i: number): string {
  const h = headers[i];
  return h && h.trim() ? h : `col${i}`;
}

// `initialFile` lets the hub (IntegrationsView) hand a dropped/picked file
// straight into the flow when the user starts an import from the hub drop zone;
// `onFileConsumed` clears the parent's handoff so it isn't re-ingested.
export default function NormalizerView({
  initialFile,
  onFileConsumed,
}: {
  initialFile?: string | null;
  onFileConsumed?: () => void;
} = {}) {
  const [phase, setPhase] = useState<Phase>("idle");
  const [filePath, setFilePath] = useState<string | null>(null);
  const [detection, setDetection] = useState<Detection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);

  const [mappings, setMappings] = useState<Mapping[]>([]);
  const [declined, setDeclined] = useState<DeclinedDrop[]>([]);
  const [advisor, setAdvisor] = useState<LlmAdvisorStatus | null>(null);

  const refreshLists = useCallback(async () => {
    try {
      const [m, d, a] = await Promise.all([
        api.normalizerListMappings(),
        api.normalizerListDeclined(),
        api.normalizerAdvisorStatus(),
      ]);
      setMappings(m);
      setDeclined(d);
      setAdvisor(a);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    refreshLists();
  }, [refreshLists]);

  const reset = () => {
    setPhase("idle");
    setFilePath(null);
    setDetection(null);
    setError(null);
  };

  const ingest = useCallback(async (path: string) => {
    setError(null);
    setNotice(null);
    setFilePath(path);
    setPhase("detecting");
    try {
      const det = await api.normalizerDetect(path);
      setDetection(det);
      setPhase("outcome");
    } catch (e) {
      setError(String(e));
      setPhase("idle");
      setFilePath(null);
    }
  }, []);

  // File picker.
  const pick = async () => {
    const picked = await open({
      multiple: false,
      filters: [{ name: "Data file", extensions: ["csv", "jsonl", "json"] }],
    });
    if (typeof picked === "string") ingest(picked);
  };

  // Drag-and-drop of CSV/JSONL onto the view (coexists with App's md/txt handler,
  // which ignores these extensions).
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload;
      if (p.type === "enter") {
        setDragging(p.paths.some((x: string) => DROP_EXT.test(x)));
      } else if (p.type === "leave") {
        setDragging(false);
      } else if (p.type === "drop") {
        setDragging(false);
        const hit = p.paths.find((x) => DROP_EXT.test(x));
        if (hit) ingest(hit);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, [ingest]);

  // A file handed in from the hub drop zone: ingest it once, then release the
  // handoff so navigating back here later doesn't re-open it.
  useEffect(() => {
    if (!initialFile) return;
    ingest(initialFile);
    onFileConsumed?.();
  }, [initialFile, ingest, onFileConsumed]);

  return (
    <div className="view view--scroll norm-view">
      <div className="view-header">
        <div>
          <h2>Import</h2>
          <p className="view-intro">
            Drop any CSV or JSONL export. Trove maps it to a contract, routes it
            to a built importer, or keeps it raw — your call, every time.
          </p>
        </div>
      </div>

      {error && <div className="norm-error">{error}</div>}
      {notice && <div className="norm-notice">{notice}</div>}

      {phase === "idle" && (
        <button
          className={`norm-dropzone ${dragging ? "drag" : ""}`}
          onClick={pick}
          type="button"
        >
          <span className="norm-dropzone-glyph">⬇</span>
          <span className="norm-dropzone-main">Drop a file, or click to choose</span>
          <span className="norm-dropzone-sub">.csv · .jsonl</span>
        </button>
      )}

      {phase === "detecting" && (
        <div className="norm-panel norm-detecting">Reading {baseName(filePath ?? "")}…</div>
      )}

      {phase === "outcome" && detection && filePath && (
        <Outcome
          key={filePath}
          path={filePath}
          detection={detection}
          advisor={advisor}
          onDone={async (msg) => {
            setNotice(msg);
            reset();
            await refreshLists();
          }}
          onCancel={reset}
          setError={setError}
        />
      )}

      <MappingsPanel
        mappings={mappings}
        onChange={refreshLists}
        setError={setError}
        setNotice={setNotice}
      />

      <DeclinedPanel declined={declined} />

      <AdvisorPanel advisor={advisor} onChange={refreshLists} setError={setError} />
    </div>
  );
}

// ===========================================================================
// Outcome — the three-fate flow for one dropped file.
// ===========================================================================

function Outcome({
  path,
  detection,
  advisor,
  onDone,
  onCancel,
  setError,
}: {
  path: string;
  detection: Detection;
  advisor: LlmAdvisorStatus | null;
  onDone: (msg: string) => void;
  onCancel: () => void;
  setError: (e: string | null) => void;
}) {
  const out = detection.outcome;
  // "manual" forces the contract binding sheet even from route / no-match.
  const [manual, setManual] = useState(false);

  if (out.kind === "route" && !manual) {
    return (
      <RouteOffer
        path={path}
        route={out}
        onDone={onDone}
        onCancel={onCancel}
        onManual={() => setManual(true)}
        setError={setError}
      />
    );
  }

  if (out.kind === "contract" || manual) {
    return (
      <BindingSheet
        path={path}
        detection={detection}
        advisor={advisor}
        onDone={onDone}
        onCancel={onCancel}
        setError={setError}
      />
    );
  }

  // no-match
  return (
    <DeclineOffer
      path={path}
      detection={detection}
      onDone={onDone}
      onCancel={onCancel}
      onManual={() => setManual(true)}
      setError={setError}
    />
  );
}

// --- Path 1: route to a built importer --------------------------------------

function RouteOffer({
  path,
  route,
  onDone,
  onCancel,
  onManual,
  setError,
}: {
  path: string;
  route: { integration_id: string; name: string; shape_label: string };
  onDone: (msg: string) => void;
  onCancel: () => void;
  onManual: () => void;
  setError: (e: string | null) => void;
}) {
  const [busy, setBusy] = useState(false);

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      const outcome = await api.runImport(route.integration_id, path, {});
      onDone(outcome.headline);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="norm-panel">
      <div className="norm-outcome-badge route">Built importer</div>
      <h2 className="norm-outcome-title">
        This looks like a <strong>{route.name}</strong> export
      </h2>
      <p className="norm-outcome-body">
        Matched shape: <span className="norm-mono">{route.shape_label}</span>. The
        built {route.name} importer already knows this format — run it as-is.
      </p>
      <div className="norm-actions">
        <button className="norm-btn primary" disabled={busy} onClick={run}>
          {busy ? "Running…" : `Run ${route.name} import`}
        </button>
        <button className="norm-btn ghost" disabled={busy} onClick={onManual}>
          Map to a contract instead
        </button>
        <button className="norm-btn ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </button>
      </div>
    </div>
  );
}

// --- Path 3: keep raw -------------------------------------------------------

function DeclineOffer({
  path,
  detection,
  onDone,
  onCancel,
  onManual,
  setError,
}: {
  path: string;
  detection: Detection;
  onDone: (msg: string) => void;
  onCancel: () => void;
  onManual: () => void;
  setError: (e: string | null) => void;
}) {
  const [source, setSource] = useState(slugify(baseName(path)));
  const [busy, setBusy] = useState(false);
  const ok = isSlug(source);

  const keep = async () => {
    if (!ok) return;
    setBusy(true);
    setError(null);
    try {
      const d = await api.normalizerDecline(source, path);
      onDone(`Kept raw under imports/${d.source}.`);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="norm-panel">
      <div className="norm-outcome-badge raw">No contract fit</div>
      <h2 className="norm-outcome-title">Keep this file raw</h2>
      <p className="norm-outcome-body">
        Nothing in the catalog fits{" "}
        <span className="norm-mono">{baseName(path)}</span> ({detection.headers.length}{" "}
        columns). Declining honestly beats inventing a fate — it lands under{" "}
        <span className="norm-mono">imports/</span>, full-fidelity and browsable.
      </p>
      <label className="norm-field">
        <span>Source name</span>
        <input
          className="norm-input"
          value={source}
          onChange={(e) => setSource(e.target.value)}
          placeholder="my-export"
        />
      </label>
      {!ok && source !== "" && (
        <div className="norm-hint warn">Use lowercase letters, digits, and dashes.</div>
      )}
      <div className="norm-actions">
        <button className="norm-btn primary" disabled={!ok || busy} onClick={keep}>
          {busy ? "Saving…" : "Keep raw"}
        </button>
        <button className="norm-btn ghost" disabled={busy} onClick={onManual}>
          Map to a contract instead
        </button>
        <button className="norm-btn ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </button>
      </div>
    </div>
  );
}

// --- Path 2: binding confirm / edit sheet -----------------------------------

/** Build a blank, valid-ish mapping for a shape with no heuristic draft. */
function blankMapping(
  detection: Detection,
  meta: SuggestCandidateMeta,
  source: string
): Mapping {
  const cols = detection.headers.filter((h) => h.trim() !== "");
  return {
    version: 1,
    source,
    domain: meta.domain,
    shape: meta.shape,
    signature: { headers: detection.headers, format: detection.format },
    bindings: [],
    constants: {},
    guid: { hash: cols.length ? cols : detection.headers },
    unbound: "extra",
    provenance: { suggested_by: "manual" },
  };
}

function BindingSheet({
  path,
  detection,
  advisor,
  onDone,
  onCancel,
  setError,
}: {
  path: string;
  detection: Detection;
  advisor: LlmAdvisorStatus | null;
  onDone: (msg: string) => void;
  onCancel: () => void;
  setError: (e: string | null) => void;
}) {
  const candidates: ContractCandidate[] =
    detection.outcome.kind === "contract" ? detection.outcome.candidates : [];

  // Field metadata catalog for every candidate shape (no network — detect +
  // from_detection). When there were no contract candidates (route / no-match
  // → "map manually"), this returns the full projectable catalog.
  const [catalog, setCatalog] = useState<SuggestCandidateMeta[]>([]);
  const [payloadResp, setPayloadResp] = useState<SuggestPayloadResponse | null>(null);
  const [mapping, setMapping] = useState<Mapping | null>(null);
  const [preview, setPreview] = useState<PreviewResult | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<number | null>(null);
  const [consent, setConsent] = useState(false);
  const [suggesting, setSuggesting] = useState(false);

  const defaultSlug =
    candidates[0]?.draft.source || slugify(baseName(path));

  // Load the field-metadata catalog + seed the initial mapping.
  useEffect(() => {
    let live = true;
    (async () => {
      try {
        const resp = await api.normalizerSuggestPayload(path);
        if (!live) return;
        setPayloadResp(resp);
        setCatalog(resp.payload.candidates);
        if (candidates.length) {
          setMapping({ ...candidates[0].draft });
        } else if (resp.payload.candidates.length) {
          setMapping(blankMapping(detection, resp.payload.candidates[0], defaultSlug));
        }
      } catch (e) {
        if (live) setError(String(e));
      }
    })();
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path]);

  const shapeMeta = useMemo(
    () =>
      mapping
        ? catalog.find((c) => c.domain === mapping.domain && c.shape === mapping.shape)
        : undefined,
    [catalog, mapping]
  );

  const update = (patch: Partial<Mapping>) =>
    setMapping((m) => (m ? { ...m, ...patch } : m));

  const bindingFor = (col: string): Binding | undefined =>
    mapping?.bindings.find((b) => b.from === col);

  const setTarget = (col: string, to: string) => {
    setMapping((m) => {
      if (!m) return m;
      const rest = m.bindings.filter((b) => b.from !== col);
      if (!to) return { ...m, bindings: rest };
      const existing = m.bindings.find((b) => b.from === col);
      return {
        ...m,
        bindings: [...rest, { from: col, to, coerce: existing?.coerce, with: existing?.with }],
      };
    });
    setPreview(null);
  };

  const setCoerce = (col: string, coerce: string) => {
    setMapping((m) => {
      if (!m) return m;
      return {
        ...m,
        bindings: m.bindings.map((b) =>
          b.from === col ? { ...b, coerce: coerce || null } : b
        ),
      };
    });
    setPreview(null);
  };

  const pickShape = (id: string) => {
    const meta = catalog.find((c) => `${c.domain}.${c.shape}` === id);
    if (!meta) return;
    // Prefer a heuristic draft for the chosen shape; else a blank mapping.
    const draft = candidates.find(
      (c) => c.domain === meta.domain && c.shape === meta.shape
    )?.draft;
    const slug = mapping?.source || defaultSlug;
    setMapping(draft ? { ...draft, source: slug } : blankMapping(detection, meta, slug));
    setPreview(null);
  };

  const runPreview = async () => {
    if (!mapping) return;
    setPreviewing(true);
    setError(null);
    try {
      setPreview(await api.normalizerPreview(path, mapping));
    } catch (e) {
      setError(String(e));
    } finally {
      setPreviewing(false);
    }
  };

  const doSuggest = async () => {
    if (!mapping) return;
    setConsent(false);
    setSuggesting(true);
    setError(null);
    try {
      const suggested = await api.normalizerSuggest(mapping.source, path);
      setMapping({ ...suggested });
      setPreview(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setSuggesting(false);
    }
  };

  const confirm = async () => {
    if (!mapping) return;
    setBusy(true);
    setError(null);
    setProgress(0);
    const unlisten = await listen<ImportProgress>("import-progress", (e) => {
      if (e.payload.integration_id === "normalizer") setProgress(e.payload.percent);
    });
    try {
      const outcome = await api.normalizerConfirm(path, mapping);
      onDone(outcome.headline);
    } catch (e) {
      setError(String(e));
    } finally {
      unlisten();
      setProgress(null);
      setBusy(false);
    }
  };

  if (!mapping) {
    return <div className="norm-panel norm-detecting">Preparing binding form…</div>;
  }

  const slugOk = isSlug(mapping.source);
  const boundTargets = new Set(mapping.bindings.map((b) => b.to));
  const constantKeys = Object.keys(mapping.constants ?? {});
  const requiredMissing = (shapeMeta?.required ?? []).filter(
    (r) =>
      r !== shapeMeta?.guid_field &&
      !boundTargets.has(r) &&
      !constantKeys.includes(r)
  );
  const confident =
    detection.outcome.kind === "contract" ? detection.outcome.confident : false;

  return (
    <div className="norm-panel norm-sheet">
      <div className="norm-outcome-badge contract">
        {confident ? "Contract match" : "Contract candidates"}
      </div>
      <h2 className="norm-outcome-title">Map to a contract</h2>

      <div className="norm-sheet-controls">
        <label className="norm-field">
          <span>Contract</span>
          <select
            className="norm-input"
            value={`${mapping.domain}.${mapping.shape}`}
            onChange={(e) => pickShape(e.target.value)}
          >
            {catalog.map((c) => (
              <option key={`${c.domain}.${c.shape}`} value={`${c.domain}.${c.shape}`}>
                {c.domain}.{c.shape}
              </option>
            ))}
          </select>
        </label>
        <label className="norm-field">
          <span>Source name</span>
          <input
            className="norm-input"
            value={mapping.source}
            onChange={(e) => update({ source: e.target.value })}
          />
        </label>
      </div>
      {!slugOk && (
        <div className="norm-hint warn">
          Source must be lowercase letters, digits, and dashes.
        </div>
      )}

      {shapeMeta && (
        <p className="norm-guid-note">
          Dedupe key: <span className="norm-mono">{shapeMeta.guid_field}</span> ←{" "}
          {"column" in mapping.guid
            ? `column "${mapping.guid.column}"`
            : `hash(${mapping.guid.hash.join(", ")})`}
        </p>
      )}

      <div className="norm-table-wrap">
        <table className="norm-bind-table">
          <thead>
            <tr>
              <th>Column</th>
              <th>Sample</th>
              <th>→ Contract field</th>
              <th>Coerce</th>
            </tr>
          </thead>
          <tbody>
            {detection.headers.map((h, i) => {
              const col = headerKey(detection.headers, i);
              const b = bindingFor(col);
              const fieldMeta = shapeMeta?.fields.find((f) => f.name === b?.to);
              const sample = detection.sample_rows[0]?.[col];
              return (
                <tr key={i}>
                  <td className="norm-col-name">
                    {h && h.trim() ? h : <em className="norm-faint">(column {i})</em>}
                  </td>
                  <td className="norm-col-sample" title={cellText(sample)}>
                    {cellText(sample)}
                  </td>
                  <td>
                    <select
                      className="norm-input sm"
                      value={b?.to ?? ""}
                      onChange={(e) => setTarget(col, e.target.value)}
                    >
                      <option value="">— extra (kept verbatim) —</option>
                      {(shapeMeta?.fields ?? []).map((f) => (
                        <option key={f.name} value={f.name}>
                          {f.name}
                          {f.required ? " *" : ""}
                        </option>
                      ))}
                    </select>
                    {fieldMeta?.description && (
                      <div className="norm-field-desc">{fieldMeta.description}</div>
                    )}
                  </td>
                  <td>
                    {b ? (
                      <select
                        className="norm-input sm"
                        value={b.coerce ?? ""}
                        onChange={(e) => setCoerce(col, e.target.value)}
                      >
                        {COERCIONS.map((c) => (
                          <option key={c} value={c}>
                            {c || "none"}
                          </option>
                        ))}
                      </select>
                    ) : (
                      <span className="norm-faint">—</span>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {constantKeys.length > 0 && (
        <div className="norm-constants">
          <span className="norm-constants-label">Constants:</span>{" "}
          {constantKeys.map((k) => (
            <span key={k} className="norm-const-chip">
              {k} = {cellText((mapping.constants ?? {})[k])}
            </span>
          ))}
        </div>
      )}

      {requiredMissing.length > 0 && (
        <div className="norm-hint warn">
          Required field{requiredMissing.length > 1 ? "s" : ""} not yet set:{" "}
          {requiredMissing.join(", ")} — bind a column or the projection will drop
          those rows.
        </div>
      )}

      <div className="norm-preview-row">
        <button className="norm-btn ghost" disabled={previewing} onClick={runPreview}>
          {previewing ? "Checking…" : "Validation preview"}
        </button>
        {preview && (
          <span className="norm-preview-stats">
            {preview.total} rows · <strong>{preview.valid}</strong> valid ·{" "}
            <span className={preview.invalid ? "norm-bad" : ""}>
              {preview.invalid} invalid
            </span>
          </span>
        )}
      </div>

      {preview && preview.invalid > 0 && preview.invalid_samples.length > 0 && (
        <ul className="norm-invalid-list">
          {preview.invalid_samples.slice(0, 5).map((iv, i) => (
            <li key={i}>
              line {iv.line}: {iv.reason}
            </li>
          ))}
        </ul>
      )}

      {preview && preview.sample_rows.length > 0 && (
        <PreviewTable rows={preview.sample_rows} />
      )}

      <div className="norm-actions">
        <button
          className="norm-btn primary"
          disabled={busy || !slugOk}
          onClick={confirm}
        >
          {busy
            ? progress !== null
              ? `Projecting… ${Math.round(progress)}%`
              : "Projecting…"
            : "Confirm & project"}
        </button>

        {advisor?.available ? (
          <button
            className="norm-btn ghost"
            disabled={suggesting || busy}
            onClick={() => setConsent(true)}
          >
            {suggesting ? "Asking AI…" : "Suggest with AI"}
          </button>
        ) : (
          <span className="norm-suggest-disabled" title={advisor?.reason ?? ""}>
            <button className="norm-btn ghost" disabled>
              Suggest with AI
            </button>
            <span className="norm-hint">
              Add an API key in AI advisor settings below to enable.
            </span>
          </span>
        )}

        <button className="norm-btn ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </button>
      </div>

      {consent && payloadResp && (
        <ConsentDialog
          payload={payloadResp}
          model={advisor?.model ?? ""}
          onApprove={doSuggest}
          onCancel={() => setConsent(false)}
        />
      )}
    </div>
  );
}

function PreviewTable({ rows }: { rows: Record<string, JsonValue>[] }) {
  const cols = useMemo(() => {
    const seen: string[] = [];
    for (const r of rows) for (const k of Object.keys(r)) if (!seen.includes(k)) seen.push(k);
    return seen;
  }, [rows]);
  return (
    <div className="norm-table-wrap">
      <table className="norm-preview-table">
        <thead>
          <tr>
            {cols.map((c) => (
              <th key={c}>{c}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.slice(0, 5).map((r, i) => (
            <tr key={i}>
              {cols.map((c) => (
                <td key={c} title={cellText(r[c])}>
                  {cellText(r[c])}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

// --- The LLM consent dialog (shows the literal payload, every time) ---------

function ConsentDialog({
  payload,
  model,
  onApprove,
  onCancel,
}: {
  payload: SuggestPayloadResponse;
  model: string;
  onApprove: () => void;
  onCancel: () => void;
}) {
  const p = payload.payload;
  return (
    <div className="norm-modal-backdrop" onClick={onCancel}>
      <div className="norm-modal" onClick={(e) => e.stopPropagation()}>
        <h3 className="norm-modal-title">Send to {model || "the AI advisor"}?</h3>
        <p className="norm-modal-body">
          This is the exact data that would leave your machine — your file's
          headers and up to 5 sample rows, plus public contract metadata. Nothing
          else is sent, and there is no “always allow”.
        </p>
        <div className="norm-modal-scroll">
          <div className="norm-modal-section-label">
            Your data ({p.headers.length} headers, {p.sample_rows.length} sample rows)
          </div>
          <pre className="norm-payload">
            {JSON.stringify({ format: p.format, headers: p.headers, sample_rows: p.sample_rows }, null, 2)}
          </pre>
          <div className="norm-modal-section-label">
            Contract metadata (public — {p.candidates.length} candidate shapes)
          </div>
          <pre className="norm-payload dim">
            {JSON.stringify(p.candidates, null, 2)}
          </pre>
        </div>
        <div className="norm-actions">
          <button className="norm-btn primary" onClick={onApprove}>
            Send
          </button>
          <button className="norm-btn ghost" onClick={onCancel}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}

// ===========================================================================
// Stored mappings — a plain management list (hub cards are R3).
// ===========================================================================

function MappingsPanel({
  mappings,
  onChange,
  setError,
  setNotice,
}: {
  mappings: Mapping[];
  onChange: () => Promise<void>;
  setError: (e: string | null) => void;
  setNotice: (n: string | null) => void;
}) {
  const [busy, setBusy] = useState<string | null>(null);

  if (mappings.length === 0) return null;

  const reproject = async (source: string) => {
    setBusy(source);
    setError(null);
    try {
      const outcome = await api.normalizerReproject(source);
      setNotice(outcome.headline);
      await onChange();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  };

  const del = async (source: string) => {
    setBusy(source);
    setError(null);
    try {
      await api.normalizerDeleteMapping(source);
      setNotice(`Removed mapping "${source}" (raw kept).`);
      await onChange();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="norm-section">
      <h2 className="norm-section-title">Your mappings</h2>
      <div className="norm-mapping-list">
        {mappings.map((m) => (
          <div key={m.source} className="norm-mapping-row">
            <div className="norm-mapping-meta">
              <span className="norm-mapping-source">{m.source}</span>
              <span className="norm-mono norm-faint">
                → {m.domain}.{m.shape}
              </span>
              {m.provenance?.suggested_by && (
                <span className="norm-tag">{m.provenance.suggested_by}</span>
              )}
            </div>
            <div className="norm-mapping-actions">
              <button
                className="norm-btn tiny"
                disabled={busy === m.source}
                onClick={() => reproject(m.source)}
              >
                {busy === m.source ? "…" : "Re-project"}
              </button>
              <button
                className="norm-btn tiny danger"
                disabled={busy === m.source}
                onClick={() => del(m.source)}
              >
                Delete
              </button>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}

// ===========================================================================
// Declined drops (imports/) — the generic raw table viewer.
// ===========================================================================

function DeclinedPanel({ declined }: { declined: DeclinedDrop[] }) {
  const [open, setOpen] = useState<DeclinedDrop | null>(null);
  if (declined.length === 0) return null;
  return (
    <section className="norm-section">
      <h2 className="norm-section-title">Raw imports</h2>
      <div className="norm-mapping-list">
        {declined.map((d, i) => (
          <button
            key={`${d.file}-${i}`}
            className="norm-mapping-row as-button"
            onClick={() => setOpen(d)}
          >
            <div className="norm-mapping-meta">
              <span className="norm-mapping-source">{d.source}</span>
              <span className="norm-mono norm-faint">{baseName(d.file)}</span>
            </div>
            <span className="norm-faint">view →</span>
          </button>
        ))}
      </div>
      {open && <RawViewer drop={open} onClose={() => setOpen(null)} />}
    </section>
  );
}

function RawViewer({ drop, onClose }: { drop: DeclinedDrop; onClose: () => void }) {
  const [page, setPage] = useState<RawPage | null>(null);
  const [offset, setOffset] = useState(0);
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setLoading(true);
    api
      .normalizerReadRaw(drop.file, offset, RAW_PAGE)
      .then((p) => {
        if (live) setPage(p);
      })
      .catch((e) => live && setErr(String(e)))
      .finally(() => live && setLoading(false));
    return () => {
      live = false;
    };
  }, [drop.file, offset]);

  const cols = useMemo(() => {
    if (!page) return [];
    const seen: string[] = [];
    page.headers.forEach((h, i) => seen.push(h && h.trim() ? h : `col${i}`));
    for (const r of page.rows) for (const k of Object.keys(r)) if (!seen.includes(k)) seen.push(k);
    return seen;
  }, [page]);

  return (
    <div className="norm-modal-backdrop" onClick={onClose}>
      <div className="norm-modal wide" onClick={(e) => e.stopPropagation()}>
        <div className="norm-raw-head">
          <h3 className="norm-modal-title">{baseName(drop.file)}</h3>
          <button className="norm-btn ghost tiny" onClick={onClose}>
            Close
          </button>
        </div>
        {err && <div className="norm-error">{err}</div>}
        <div className="norm-table-wrap tall">
          <table className="norm-preview-table">
            <thead>
              <tr>
                {cols.map((c) => (
                  <th key={c}>{c}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {(page?.rows ?? []).map((r, i) => (
                <tr key={i}>
                  {cols.map((c) => (
                    <td key={c} title={cellText(r[c])}>
                      {cellText(r[c])}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <div className="norm-actions">
          <button
            className="norm-btn ghost tiny"
            disabled={offset === 0 || loading}
            onClick={() => setOffset(Math.max(0, offset - RAW_PAGE))}
          >
            ← Prev
          </button>
          <span className="norm-faint">rows {offset + 1}–{offset + (page?.rows.length ?? 0)}</span>
          <button
            className="norm-btn ghost tiny"
            disabled={!page?.has_more || loading}
            onClick={() => setOffset(offset + RAW_PAGE)}
          >
            Next →
          </button>
        </div>
      </div>
    </div>
  );
}

// ===========================================================================
// AI advisor settings (BYO key) — the target of the disabled-suggest hint.
// ===========================================================================

function AdvisorPanel({
  advisor,
  onChange,
  setError,
}: {
  advisor: LlmAdvisorStatus | null;
  onChange: () => Promise<void>;
  setError: (e: string | null) => void;
}) {
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);

  const save = async () => {
    if (!key.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await api.normalizerSaveKey(key.trim());
      setKey("");
      await onChange();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.normalizerDeleteKey();
      await onChange();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="norm-section">
      <h2 className="norm-section-title">AI advisor settings</h2>
      <p className="norm-sub">
        Optional. When header heuristics can't map a file, an explicit, per-drop
        cloud call can pre-fill the binding form. It runs only when you approve
        the exact payload shown. Manual binding always works without a key.
      </p>
      <div className="norm-advisor-status">
        {advisor?.available ? (
          <span className="norm-tag good">
            Available · {advisor.source === "baked" ? "built-in key" : "your key"} ·{" "}
            {advisor.model}
          </span>
        ) : (
          <span className="norm-tag">{advisor?.reason ?? "No API key configured."}</span>
        )}
      </div>
      <div className="norm-key-row">
        <input
          className="norm-input"
          type="password"
          placeholder="sk-ant-…  (bring your own Anthropic key)"
          value={key}
          onChange={(e) => setKey(e.target.value)}
        />
        <button className="norm-btn primary" disabled={busy || !key.trim()} onClick={save}>
          Save key
        </button>
        {advisor?.source === "byo" && (
          <button className="norm-btn ghost" disabled={busy} onClick={remove}>
            Remove
          </button>
        )}
      </div>
    </section>
  );
}

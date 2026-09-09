import { useCallback, useEffect, useRef, useState } from "react";
import CodeMirror from "@uiw/react-codemirror";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";
import { oneDark } from "@codemirror/theme-one-dark";
import { EditorView } from "@codemirror/view";
import { api, ArtifactMeta } from "../api";

type SaveState = "saved" | "saving" | "dirty";

const extensions = [
  markdown({ base: markdownLanguage, codeLanguages: languages }),
  EditorView.lineWrapping,
];

/** A file dropped on the window, routed here by App. */
export interface OpenRequest {
  path: string;
  /** Bumped per drop so re-importing the same name still triggers the effect. */
  seq: number;
}

function formatModified(epochSecs: number): string {
  const d = new Date(epochSecs * 1000);
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  return sameDay
    ? d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })
    : d.toLocaleDateString(undefined, { month: "short", day: "numeric", year: d.getFullYear() === now.getFullYear() ? undefined : "numeric" });
}

export default function ArtifactsView({ openRequest }: { openRequest: OpenRequest | null }) {
  const [artifacts, setArtifacts] = useState<ArtifactMeta[]>([]);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [content, setContent] = useState<string>("");
  const [saveState, setSaveState] = useState<SaveState>("saved");
  const saveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Tracks which artifact the editor content belongs to, so autosave never
  // writes one file's text into another after switching.
  const editingPath = useRef<string | null>(null);

  const refreshList = useCallback(async () => {
    const q = query.trim();
    setArtifacts(q ? await api.searchArtifacts(q) : await api.listArtifacts());
  }, [query]);

  useEffect(() => {
    // Debounce so the full-text scan doesn't run per keystroke.
    const t = setTimeout(refreshList, query ? 200 : 0);
    return () => clearTimeout(t);
  }, [refreshList, query]);

  const flushSave = useCallback(async (path: string, text: string) => {
    setSaveState("saving");
    await api.writeArtifact(path, text);
    setSaveState("saved");
    refreshList();
  }, [refreshList]);

  const openArtifact = useCallback(
    async (path: string) => {
      // Flush any pending edit on the previous artifact before switching.
      if (saveTimer.current && editingPath.current) {
        clearTimeout(saveTimer.current);
        saveTimer.current = null;
      }
      const text = await api.readArtifact(path);
      editingPath.current = path;
      setSelected(path);
      setContent(text);
      setSaveState("saved");
    },
    []
  );

  // A file was dropped on the window: refresh and show it.
  useEffect(() => {
    if (!openRequest) return;
    refreshList();
    openArtifact(openRequest.path);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [openRequest]);

  const onEdit = useCallback(
    (text: string) => {
      setContent(text);
      setSaveState("dirty");
      const path = editingPath.current;
      if (!path) return;
      if (saveTimer.current) clearTimeout(saveTimer.current);
      saveTimer.current = setTimeout(() => flushSave(path, text), 700);
    },
    [flushSave]
  );

  const newArtifact = useCallback(async () => {
    const meta = await api.createArtifact("Untitled");
    setQuery("");
    await refreshList();
    await openArtifact(meta.path);
  }, [refreshList, openArtifact]);

  const removeArtifact = useCallback(
    async (path: string) => {
      if (!confirm("Delete this file? It will be removed from your vault.")) return;
      await api.deleteArtifact(path);
      if (selected === path) {
        editingPath.current = null;
        setSelected(null);
        setContent("");
      }
      refreshList();
    },
    [selected, refreshList]
  );

  return (
    <div className="artifacts-view">
      <div className="artifacts-list">
        <div className="artifacts-list-header">
          <span className="artifacts-list-title">Artifacts</span>
          <button className="btn-new" onClick={newArtifact} title="New note">
            +
          </button>
        </div>
        <input
          className="artifacts-search"
          type="search"
          placeholder="Search…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <div className="artifacts-list-items">
          {artifacts.length === 0 && (
            <div className="artifacts-empty">
              {query.trim() ? (
                <>No matches.</>
              ) : (
                <>
                  No artifacts yet.
                  <br />
                  Write a note, or drop .md / .txt files anywhere in this window.
                </>
              )}
            </div>
          )}
          {artifacts.map((a) => (
            <div
              key={a.path}
              className={`artifact-item ${selected === a.path ? "active" : ""}`}
              onClick={() => openArtifact(a.path)}
            >
              <div className="artifact-item-main">
                <div className="artifact-item-title">{a.title}</div>
                <div className="artifact-item-date">{formatModified(a.modified)}</div>
              </div>
              <button
                className="btn-delete"
                title="Delete file"
                onClick={(e) => {
                  e.stopPropagation();
                  removeArtifact(a.path);
                }}
              >
                ×
              </button>
            </div>
          ))}
        </div>
      </div>
      <div className="artifacts-editor">
        {selected ? (
          <>
            <div className="editor-statusbar">
              <span className="editor-path">{selected}</span>
              <span className={`save-state save-${saveState}`}>
                {saveState === "saved" ? "Saved" : saveState === "saving" ? "Saving…" : "Editing"}
              </span>
            </div>
            <CodeMirror
              value={content}
              onChange={onEdit}
              extensions={extensions}
              theme={oneDark}
              height="100%"
              className="editor-cm"
              basicSetup={{
                lineNumbers: false,
                foldGutter: false,
                highlightActiveLine: false,
              }}
            />
          </>
        ) : (
          <div className="editor-placeholder">
            <h2>Your documents live in your vault</h2>
            <p>
              Everything here is a plain file in <code>~/Documents/Trove/artifacts</code> —
              notes you write and documents you import, readable by you, your
              tools, and any AI you choose. Select a file, create a note, or
              drag <code>.md</code> / <code>.txt</code> files anywhere in this
              window to import them.
            </p>
          </div>
        )}
      </div>
    </div>
  );
}

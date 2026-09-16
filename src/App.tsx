import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { api } from "./api";
import ArtifactsView, { OpenRequest } from "./components/ArtifactsView";
import HealthView from "./components/HealthView";
import ActivityView from "./components/ActivityView";
import ScreenTimeView from "./components/ScreenTimeView";
import IntegrationsView from "./components/IntegrationsView";
import NormalizerView from "./components/NormalizerView";
import WebView from "./components/WebView";
import AdsView from "./components/AdsView";
import MediaView from "./components/MediaView";
import TasksView from "./components/TasksView";
import CalendarView from "./components/CalendarView";
import FinanceView from "./components/FinanceView";
import MessagesView from "./components/MessagesView";
import EmailView from "./components/EmailView";
import CallsView from "./components/CallsView";
import WeatherView from "./components/WeatherView";
import { SECTION_KEY, dropIndex, loadOrder, moveItem, saveOrder } from "./nav";
import "./App.css";

type Section =
  | "artifacts"
  | "health"
  | "activity"
  | "screen-time"
  | "web"
  | "ads"
  | "media"
  | "messages"
  | "email"
  | "calls"
  | "weather"
  | "tasks"
  | "calendar"
  | "finance"
  | "import"
  | "integrations"
  | "ask";

interface NavItem {
  id: Section;
  label: string;
  icon: string;
  ready: boolean;
}

// Sidebar order is the user's: dragged into place, kept in localStorage.
// Items are never grouped or hidden; new sections append to the end.
const NAV: NavItem[] = [
  { id: "artifacts", label: "Artifacts", icon: "✎", ready: true },
  { id: "health", label: "Health", icon: "♥", ready: true },
  { id: "activity", label: "Activity", icon: "◷", ready: true },
  { id: "screen-time", label: "Screen Time", icon: "⧉", ready: true },
  { id: "web", label: "Web", icon: "⊙", ready: true },
  { id: "ads", label: "Ads", icon: "▣", ready: true },
  { id: "media", label: "Media", icon: "♪", ready: true },
  { id: "messages", label: "Messages", icon: "✉", ready: true },
  { id: "email", label: "Email", icon: "@", ready: true },
  { id: "calls", label: "Calls", icon: "✆", ready: true },
  { id: "weather", label: "Weather", icon: "☂", ready: true },
  { id: "tasks", label: "Tasks", icon: "☑", ready: true },
  { id: "calendar", label: "Calendar", icon: "▦", ready: true },
  { id: "finance", label: "Finance", icon: "$", ready: true },
  { id: "import", label: "Import", icon: "⬇", ready: true },
  { id: "integrations", label: "Integrations", icon: "❖", ready: true },
  { id: "ask", label: "Ask", icon: "✦", ready: false },
];

const NAV_GAP = 2; // matches .nav { gap } in App.css
const DRAG_THRESHOLD = 4; // px of travel before a press becomes a drag
const NAV_IDS: Section[] = NAV.map((n) => n.id);

const isSection = (id: string): id is Section => NAV.some((n) => n.id === id);

function loadSection(): Section {
  const saved = localStorage.getItem(SECTION_KEY) ?? "";
  const item = isSection(saved) ? NAV.find((n) => n.id === saved) : undefined;
  return item?.ready ? item.id : "artifacts";
}

// A drag in progress: which slot is being moved, where it would land,
// and how far the pointer has travelled (for the live transform).
interface Drag {
  from: number;
  to: number;
  dy: number;
  step: number;
}

// What the window-level drop handler accepts. Other types (health's
// .zip/.xml) are handled by their own views; everything else is ignored
// until artifacts support arbitrary files.
const isArtifactFile = (p: string) => /\.(md|txt)$/i.test(p);

export default function App() {
  const [section, setSection] = useState<Section>(loadSection);
  const [order, setOrder] = useState<Section[]>(() => loadOrder(NAV_IDS));
  const [drag, setDrag] = useState<Drag | null>(null);
  // Pointer bookkeeping that must not re-render: where the press started,
  // whether it crossed the drag threshold, and whether the click that
  // follows a drop should be swallowed.
  const press = useRef<{ startY: number; from: number; to: number; step: number; moved: boolean } | null>(null);
  const swallowClick = useRef(false);
  const [vaultRoot, setVaultRoot] = useState<string>("");
  const [dragging, setDragging] = useState(false);
  const [dropError, setDropError] = useState<string | null>(null);
  const [openRequest, setOpenRequest] = useState<OpenRequest | null>(null);
  // A data file handed off from the hub drop zone into the Import flow.
  const [importFile, setImportFile] = useState<string | null>(null);
  const dropSeq = useRef(0);

  useEffect(() => {
    api.vaultInfo().then((info) => setVaultRoot(info.root));
  }, []);

  useEffect(() => {
    localStorage.setItem(SECTION_KEY, section);
  }, [section]);

  const navItems = order.map((id) => NAV.find((n) => n.id === id)!);

  const onNavPointerDown = (i: number) => (e: ReactPointerEvent<HTMLButtonElement>) => {
    if (e.button !== 0) return;
    const el = e.currentTarget;
    swallowClick.current = false;
    press.current = { startY: e.clientY, from: i, to: i, step: el.offsetHeight + NAV_GAP, moved: false };
    el.setPointerCapture(e.pointerId);
  };

  const onNavPointerMove = (e: ReactPointerEvent<HTMLButtonElement>) => {
    const p = press.current;
    if (!p) return;
    const dy = e.clientY - p.startY;
    if (!p.moved && Math.abs(dy) < DRAG_THRESHOLD) return;
    p.moved = true;
    p.to = dropIndex(p.from, dy, p.step, navItems.length);
    setDrag({ from: p.from, to: p.to, dy, step: p.step });
  };

  const onNavPointerUp = () => {
    const p = press.current;
    press.current = null;
    if (!p?.moved) return;
    swallowClick.current = true;
    setDrag(null);
    if (p.from === p.to) return;
    const next = moveItem(order, p.from, p.to);
    setOrder(next);
    saveOrder(next);
  };

  const onNavPointerCancel = () => {
    press.current = null;
    setDrag(null);
  };

  // Where each slot sits while a drag is live: the dragged item follows the
  // pointer; the items between its origin and destination shift one slot.
  const navTransform = (i: number): string | undefined => {
    if (!drag) return undefined;
    if (i === drag.from) return `translateY(${drag.dy}px)`;
    if (drag.from < drag.to && i > drag.from && i <= drag.to) return `translateY(-${drag.step}px)`;
    if (drag.from > drag.to && i >= drag.to && i < drag.from) return `translateY(${drag.step}px)`;
    return undefined;
  };

  // Dropping .md/.txt files anywhere on the window imports them into
  // artifacts/ and shows the first one.
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent(async (event) => {
      const payload = event.payload;
      if (payload.type === "enter") {
        setDragging(payload.paths.some(isArtifactFile));
      } else if (payload.type === "leave") {
        setDragging(false);
      } else if (payload.type === "drop") {
        setDragging(false);
        const paths = payload.paths.filter(isArtifactFile);
        if (paths.length === 0) return;
        try {
          const imported = await api.importArtifacts(paths);
          if (imported.length > 0) {
            setSection("artifacts");
            setOpenRequest({ path: imported[0].path, seq: ++dropSeq.current });
          }
        } catch (e) {
          setDropError(`Import failed: ${e}`);
          setTimeout(() => setDropError(null), 5000);
        }
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="wordmark">
          <span className="wordmark-glyph">◆</span> Trove
        </div>
        <nav className={`nav ${drag ? "reordering" : ""}`}>
          {navItems.map((item, i) => (
            <button
              key={item.id}
              className={[
                "nav-item",
                section === item.id ? "active" : "",
                item.ready ? "" : "disabled",
                drag?.from === i ? "dragging" : "",
              ].join(" ")}
              style={{ transform: navTransform(i) }}
              aria-disabled={!item.ready}
              onPointerDown={onNavPointerDown(i)}
              onPointerMove={onNavPointerMove}
              onPointerUp={onNavPointerUp}
              onPointerCancel={onNavPointerCancel}
              onClick={() => {
                if (swallowClick.current) {
                  swallowClick.current = false;
                  return;
                }
                if (item.ready) setSection(item.id);
              }}
            >
              <span className="nav-icon">{item.icon}</span>
              <span className="nav-label">{item.label}</span>
              {!item.ready && <span className="nav-soon">soon</span>}
            </button>
          ))}
        </nav>
        <div className="sidebar-footer">
          <div className="vault-path" title={vaultRoot}>
            {vaultRoot.replace(/^\/Users\/[^/]+/, "~")}
          </div>
          <div className="tagline">Your data. Your machine.</div>
        </div>
      </aside>
      <main className="main">
        {section === "artifacts" && <ArtifactsView openRequest={openRequest} />}
        {section === "health" && <HealthView />}
        {section === "activity" && <ActivityView />}
        {section === "screen-time" && <ScreenTimeView />}
        {section === "web" && <WebView />}
        {section === "ads" && (
          <AdsView onOpenIntegrations={() => setSection("integrations")} />
        )}
        {section === "media" && <MediaView />}
        {section === "messages" && <MessagesView />}
        {section === "email" && <EmailView />}
        {section === "calls" && <CallsView />}
        {section === "weather" && <WeatherView />}
        {section === "tasks" && <TasksView />}
        {section === "calendar" && <CalendarView />}
        {section === "finance" && <FinanceView />}
        {section === "import" && (
          <NormalizerView
            initialFile={importFile}
            onFileConsumed={() => setImportFile(null)}
          />
        )}
        {section === "integrations" && (
          <IntegrationsView
            onImport={(path) => {
              setImportFile(path);
              setSection("import");
            }}
          />
        )}
      </main>
      {dragging && (
        <div className="drop-overlay">
          <div className="drop-overlay-card">
            Drop to add to your vault
            <span>.md and .txt land in ~/Documents/Trove/artifacts</span>
          </div>
        </div>
      )}
      {dropError && <div className="drop-toast">{dropError}</div>}
    </div>
  );
}

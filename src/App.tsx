import { useEffect, useRef, useState } from "react";
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

// What the window-level drop handler accepts. Other types (health's
// .zip/.xml) are handled by their own views; everything else is ignored
// until artifacts support arbitrary files.
const isArtifactFile = (p: string) => /\.(md|txt)$/i.test(p);

export default function App() {
  const [section, setSection] = useState<Section>("artifacts");
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
        <nav className="nav">
          {NAV.map((item) => (
            <button
              key={item.id}
              className={`nav-item ${section === item.id ? "active" : ""} ${item.ready ? "" : "disabled"}`}
              onClick={() => item.ready && setSection(item.id)}
              disabled={!item.ready}
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

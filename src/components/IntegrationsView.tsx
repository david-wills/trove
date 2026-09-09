import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  api,
  FinanceOverview,
  GmailSyncState,
  ImportInfo,
  ImportProgress,
  IntegrationStatus,
  OuraSyncState,
  StreamPage,
  WatcherStatus,
} from "../api";
import type { ConnectionStatusRow } from "../bindings";
import { ConnectCard } from "./ConnectCard";
import GenericStreamView from "./GenericStreamView";

/** The Google services, in card display order — each a vault-side toggle
 *  (catalog id) gated on top of a connected account. */
const GOOGLE_SERVICES = [
  "google-gmail",
  "google-calendar",
  "google-contacts",
  "google-tasks",
  "google-youtube",
  "google-books",
];

/** System Settings privacy panes, keyed by PermissionInfo.kind. */
const SETTINGS_URL: Record<string, string> = {
  "screen-recording":
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
  "full-disk-access":
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
  "media-library":
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Media",
  calendars:
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Calendars",
  reminders:
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Reminders",
};

const PERMISSION_LABEL: Record<string, string> = {
  "screen-recording": "Screen Recording",
  "full-disk-access": "Full Disk Access",
  "media-library": "Media & Apple Music",
  calendars: "Calendar access",
  reminders: "Reminders access",
};

/** One entry in the master list; its detail pane shows `members` in order.
 *  An integration id may appear in more than one group (the Music scrobbler
 *  is both a Trove Collector stream and Apple Music data) — it's the same
 *  underlying toggle wherever it shows. */
interface Group {
  id: string;
  title: string;
  section: "collectors" | "apps";
  members: string[];
  /** Optional intro line under the detail-pane title. */
  intro?: string;
}

/** Taxonomy display order + labels (docs/integrations/README.md → "Domain
 *  taxonomy"). Domains not listed fall through to alphabetical, titled by
 *  their bare token — new backend domains surface without UI work. */
const DOMAIN_ORDER: string[] = [
  "correspondence",
  "meetings",
  "voice",
  "contacts",
  "calendar",
  "tasks",
  "habits",
  "notes",
  "files",
  "browser",
  "activity",
  "developer",
  "reading",
  "media",
  "gaming",
  "photos",
  "social",
  "health",
  "finance",
  "travel",
  "location",
  "home",
  "environment",
];

const DOMAIN_LABEL: Record<string, string> = {
  correspondence: "Email & messaging",
  meetings: "Meetings",
  voice: "Voice",
  contacts: "Contacts",
  calendar: "Calendar",
  tasks: "Tasks",
  habits: "Habits",
  notes: "Notes & drafts",
  files: "Files",
  browser: "Web activity",
  activity: "Computer activity",
  developer: "Developer",
  reading: "Reading",
  media: "Media",
  gaming: "Gaming",
  photos: "Photos",
  social: "Social",
  health: "Health",
  finance: "Finance",
  travel: "Travel",
  location: "Location",
  home: "Home & IoT",
  environment: "Environment",
};

function domainLabel(domain: string): string {
  return DOMAIN_LABEL[domain] ?? domain.charAt(0).toUpperCase() + domain.slice(1);
}

const GROUPS: Group[] = [
  {
    id: "trove-collector",
    title: "Trove Collector",
    section: "collectors",
    members: ["activity", "music-scrobbler"],
    intro:
      "The always-on collector — the troved daemon when installed, this app otherwise. These live streams only exist while it runs; gaps can't be backfilled.",
  },
  {
    id: "browser-extension",
    title: "Browser extension",
    section: "collectors",
    members: ["browser-extension"],
    intro:
      "The Trove Chrome extension, reporting live tabs through troved as its native host.",
  },
  {
    id: "apple-music",
    title: "Apple Music",
    section: "apps",
    members: ["music-scrobbler", "music-library"],
    intro:
      "Two complementary streams: the live scrobbler (also under Trove Collector) and the daily library snapshot that catches what it misses.",
  },
  { id: "chrome", title: "Chrome", section: "apps", members: ["chrome-history"] },
  { id: "safari", title: "Safari", section: "apps", members: ["safari-history"] },
  { id: "messages", title: "Messages", section: "apps", members: ["imessage"] },
  { id: "podcasts", title: "Apple Podcasts", section: "apps", members: ["podcasts"] },
  { id: "books", title: "Apple Books", section: "apps", members: ["books"] },
  { id: "ticktick", title: "TickTick", section: "apps", members: ["ticktick"] },
  {
    id: "google",
    title: "Google",
    section: "apps",
    members: GOOGLE_SERVICES,
    intro:
      "One connection covering Gmail, Calendar, Contacts, Tasks, YouTube, and Play Books — across any number of Google accounts. Connect an account, then pick which services to collect.",
  },
  {
    id: "banks",
    title: "Banks & cards",
    section: "apps",
    members: ["bank-sync", "csv-import"],
    intro:
      "Two complementary routes into finance/: the daily SimpleFIN sync going forward, and statement-file imports for deep history and accounts no aggregator reaches.",
  },
  { id: "health", title: "Apple Health", section: "apps", members: ["health"] },
  { id: "email", title: "Email", section: "apps", members: ["email"] },
  { id: "slack", title: "Slack", section: "apps", members: ["slack"] },
];

/** GROUPS plus an auto-generated app entry for any catalog id no group
 *  claims — new backend integrations appear in the hub without UI work. */
function allGroups(items: IntegrationStatus[]): Group[] {
  const claimed = new Set(GROUPS.flatMap((g) => g.members));
  const extras = items
    .filter((i) => !claimed.has(i.id))
    .map((i) => ({
      id: i.id,
      title: i.name,
      section: "apps" as const,
      members: [i.id],
    }));
  return [...GROUPS, ...extras];
}

/** The taxonomy domain a group sorts under — the first member's domain
 *  (members of one group share a domain in practice). `""` if unknown. */
function groupDomain(group: Group, items: IntegrationStatus[]): string {
  for (const id of group.members) {
    const d = items.find((i) => i.id === id)?.domain;
    if (d) return d;
  }
  return "";
}

/** A group is "planned" only if every member is a NotWired stub — a mixed
 *  group (one built member) is built. */
function groupPlanned(group: Group, items: IntegrationStatus[]): boolean {
  const members = group.members
    .map((id) => items.find((i) => i.id === id))
    .filter((i): i is IntegrationStatus => Boolean(i));
  return members.length > 0 && members.every((i) => i.planned);
}

/** A group is unavailable only if every member is unavailable. */
function groupUnavailable(group: Group, items: IntegrationStatus[]): boolean {
  const members = group.members
    .map((id) => items.find((i) => i.id === id))
    .filter((i): i is IntegrationStatus => Boolean(i));
  return members.length > 0 && members.every((i) => Boolean(i.unavailable_reason));
}

/** Does any of a group's members match the search query (name / id /
 *  description)? Empty query matches everything. */
function groupMatches(
  group: Group,
  items: IntegrationStatus[],
  q: string
): boolean {
  if (!q) return true;
  const needle = q.toLowerCase();
  if (group.title.toLowerCase().includes(needle)) return true;
  return group.members.some((id) => {
    const it = items.find((i) => i.id === id);
    if (!it) return false;
    return (
      it.name.toLowerCase().includes(needle) ||
      it.id.toLowerCase().includes(needle) ||
      (it.description ?? "").toLowerCase().includes(needle)
    );
  });
}

const REFRESH_MS = 15_000;

function fmtWhen(s: string): string {
  return s.slice(0, 16).replace("T", " ");
}

/** CSV/JSONL extensions the hub drop zone routes into the normalizer flow. */
const IMPORT_EXT = /\.(csv|jsonl|json)$/i;

/** The drop-in normalizer's front door, living in the hub (R2 Step 3): drop a
 *  CSV/JSONL export or pick one, and it routes into the Import flow. */
function ImportDropCard({ onImport }: { onImport: (path: string) => void }) {
  const [dragging, setDragging] = useState(false);

  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload;
      if (p.type === "enter") {
        setDragging(p.paths.some((x: string) => IMPORT_EXT.test(x)));
      } else if (p.type === "leave") {
        setDragging(false);
      } else if (p.type === "drop") {
        setDragging(false);
        const hit = p.paths.find((x) => IMPORT_EXT.test(x));
        if (hit) onImport(hit);
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, [onImport]);

  const pick = async () => {
    const picked = await open({
      multiple: false,
      filters: [{ name: "Data file", extensions: ["csv", "jsonl", "json"] }],
    });
    if (typeof picked === "string") onImport(picked);
  };

  return (
    <button
      type="button"
      className={`int-import-drop ${dragging ? "drag" : ""}`}
      onClick={pick}
    >
      <span className="int-import-glyph">⬇</span>
      <span className="int-import-text">
        <span className="int-import-main">Drop a file to import</span>
        <span className="int-import-sub">
          Any CSV or JSONL export — Trove maps it to a contract, routes it to a
          built importer, or keeps it raw. Or click to choose a file.
        </span>
      </span>
    </button>
  );
}

export default function IntegrationsView({
  onImport,
}: {
  onImport?: (path: string) => void;
} = {}) {
  const [items, setItems] = useState<IntegrationStatus[]>([]);
  const [connections, setConnections] = useState<ConnectionStatusRow[]>([]);
  const [finance, setFinance] = useState<FinanceOverview | null>(null);
  const [ouraInfo, setOuraInfo] = useState<OuraSyncState | null>(null);
  const [gmailInfo, setGmailInfo] = useState<GmailSyncState | null>(null);
  const [watcher, setWatcher] = useState<WatcherStatus | null>(null);
  const [selected, setSelected] = useState("trove-collector");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [hideUnavailable, setHideUnavailable] = useState(false);
  // Domains collapsed by the user; Collectors and the selected group's
  // domain always stay open.
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());

  const refresh = useCallback(async () => {
    const [status, conns, fin, watch, ouraState, gmailState] =
      await Promise.all([
        api.integrationsStatus(),
        api.connectStatusAll(),
        api.financeOverview(),
        api.watcherStatus(),
        api.ouraSyncInfo(),
        api.gmailSyncInfo(),
      ]);
    setItems(status);
    setConnections(conns);
    setFinance(fin);
    setWatcher(watch);
    setOuraInfo(ouraState);
    setGmailInfo(gmailState);
  }, []);

  useEffect(() => {
    let active = true;
    const tick = () => {
      if (active) refresh().catch((e) => setError(String(e)));
    };
    tick();
    const id = setInterval(tick, REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [refresh]);

  const toggle = async (id: string, enabled: boolean) => {
    setError(null);
    try {
      setItems(await api.setIntegrationEnabled(id, enabled));
    } catch (e) {
      setError(String(e));
    }
  };

  const groups = allGroups(items);
  const group = groups.find((g) => g.id === selected) ?? groups[0];
  const enabledIds = new Set(items.filter((i) => i.enabled).map((i) => i.id));

  // The "Collectors" section stays its own curated grouping, first; every
  // other group sorts into a taxonomy-domain section.
  const collectorGroups = groups.filter((g) => g.section === "collectors");

  // Apps, bucketed by taxonomy domain, after search + hide-unavailable
  // filtering. Within a domain: available first, unavailable last, each
  // run alphabetical by title.
  const byDomain = new Map<string, Group[]>();
  for (const g of groups) {
    if (g.section === "collectors") continue;
    if (!groupMatches(g, items, query)) continue;
    if (hideUnavailable && groupUnavailable(g, items)) continue;
    const d = groupDomain(g, items);
    const bucket = byDomain.get(d) ?? [];
    bucket.push(g);
    byDomain.set(d, bucket);
  }
  for (const bucket of byDomain.values()) {
    bucket.sort((a, b) => {
      const ua = groupUnavailable(a, items) ? 1 : 0;
      const ub = groupUnavailable(b, items) ? 1 : 0;
      if (ua !== ub) return ua - ub;
      return a.title.localeCompare(b.title);
    });
  }
  // Domain sections in taxonomy order, then any unknown domains alphabetical.
  const presentDomains = [...byDomain.keys()];
  const orderedDomains = [
    ...DOMAIN_ORDER.filter((d) => byDomain.has(d)),
    ...presentDomains
      .filter((d) => !DOMAIN_ORDER.includes(d))
      .sort((a, b) => domainLabel(a).localeCompare(domainLabel(b))),
  ];

  const selectedDomain = groupDomain(group, items);
  const collectorsMatch = collectorGroups.filter((g) =>
    groupMatches(g, items, query)
  );

  /** The connection shared by a group's members, if any (first member with
   *  a `connection` wins — members of one group never mix connections). */
  const connectionFor = (g: Group): ConnectionStatusRow | null => {
    for (const id of g.members) {
      const conn = items.find((i) => i.id === id)?.connection;
      if (conn) return connections.find((c) => c.id === conn) ?? null;
    }
    return null;
  };

  return (
    <div className="sync-view int-hub">
      <div className="sync-header">
        <h2>Integrations</h2>
        <p className="sync-sub">
          Everything that brings data into your vault. Pick an integration to
          see its settings, setup steps, and status — toggles take effect
          within seconds, in the app and the background daemon alike.
        </p>
      </div>
      {onImport && <ImportDropCard onImport={onImport} />}
      {error && <div className="sync-error">{error}</div>}
      {notice && <div className="sync-notice">{notice}</div>}
      <div className="int-layout">
        <nav className="int-list">
          <div className="int-list-filter">
            <input
              type="text"
              className="int-search"
              placeholder="Search integrations…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              spellCheck={false}
            />
            <label className="int-hide-toggle" title="Hide entries Trove can't collect">
              <input
                type="checkbox"
                checked={hideUnavailable}
                onChange={(e) => setHideUnavailable(e.target.checked)}
              />
              <span>Hide unavailable</span>
            </label>
          </div>

          {/* Collectors — the always-on grouping, always first. */}
          {collectorsMatch.length > 0 && (
            <div className="int-list-section">
              <div className="int-list-head">Collectors</div>
              {collectorsMatch.map((g) => (
                <IntListRow
                  key={g.id}
                  group={g}
                  items={items}
                  connections={connections}
                  selected={g.id === group?.id}
                  onSelect={() => setSelected(g.id)}
                />
              ))}
            </div>
          )}

          {/* One collapsible section per taxonomy domain. */}
          {orderedDomains.map((domain) => {
            const bucket = byDomain.get(domain) ?? [];
            // Collapse is user-controlled, but a search or the selected
            // group's own domain forces the section open.
            const open =
              !collapsed.has(domain) ||
              query.trim() !== "" ||
              domain === selectedDomain;
            return (
              <div className="int-list-section" key={domain}>
                <button
                  className="int-list-head int-list-head-toggle"
                  onClick={() =>
                    setCollapsed((prev) => {
                      const next = new Set(prev);
                      if (next.has(domain)) next.delete(domain);
                      else next.add(domain);
                      return next;
                    })
                  }
                >
                  <span className={`int-caret ${open ? "open" : ""}`}>▸</span>
                  <span>{domainLabel(domain)}</span>
                  <span className="int-list-count">{bucket.length}</span>
                </button>
                {open &&
                  bucket.map((g) => (
                    <IntListRow
                      key={g.id}
                      group={g}
                      items={items}
                      connections={connections}
                      selected={g.id === group?.id}
                      onSelect={() => setSelected(g.id)}
                    />
                  ))}
              </div>
            );
          })}

          {orderedDomains.length === 0 && collectorsMatch.length === 0 && (
            <div className="int-list-empty">No integrations match “{query}”.</div>
          )}
        </nav>
        <section className="int-detail">
          {group && (
            <>
              <h3 className="int-detail-title">{group.title}</h3>
              {group.intro && <p className="int-detail-intro">{group.intro}</p>}
              {group.id === "trove-collector" && watcher && (
                <CollectorStatus watcher={watcher} />
              )}
              {group.id === "google" && (
                <GoogleSection
                  items={items}
                  connection={connectionFor(group)}
                  gmailInfo={gmailInfo}
                  enabledIds={enabledIds}
                  busy={busy}
                  setBusy={setBusy}
                  setError={setError}
                  setNotice={setNotice}
                  onToggle={toggle}
                  refresh={refresh}
                />
              )}
              {group.id !== "google" &&
                group.members.map((id) => {
                const item = items.find((i) => i.id === id);
                if (!item) return null;
                return (
                  <IntegrationCard
                    key={id}
                    item={item}
                    connection={
                      item.connection
                        ? connections.find((c) => c.id === item.connection) ??
                          null
                        : null
                    }
                    finance={item.id === "csv-import" ? finance : null}
                    ouraInfo={item.id === "oura" ? ouraInfo : null}
                    enabledIds={enabledIds}
                    busy={busy}
                    setBusy={setBusy}
                    setError={setError}
                    setNotice={setNotice}
                    onToggle={toggle}
                    refresh={refresh}
                  />
                );
              })}
            </>
          )}
        </section>
      </div>
    </div>
  );
}

function statusBadge(
  item: IntegrationStatus,
  connection: ConnectionStatusRow | null
): { label: string; tone: "on" | "off" | "warn" | "dim" } {
  if (item.unavailable_reason) return { label: "unavailable", tone: "dim" };
  if (item.kind === "import") {
    return item.last_data
      ? { label: "imported", tone: "on" }
      : { label: "no data yet", tone: "dim" };
  }
  if (!item.enabled) return { label: "off", tone: "off" };
  // Login-bearing integrations are driven by their connection's state.
  if (item.connection && connection) {
    const accounts = connection.status?.accounts ?? [];
    if (accounts.length === 0) return { label: "not connected", tone: "dim" };
    if (accounts.some((a) => a?.needs_reconnect)) {
      return { label: "reconnect needed", tone: "warn" };
    }
    if (accounts.some((a) => a?.extra?.error)) {
      return { label: "needs attention", tone: "warn" };
    }
  }
  if (item.permission?.granted === false && item.permission.required) {
    return { label: "needs permission", tone: "warn" };
  }
  if (item.permission?.granted === false) {
    return { label: "on · limited", tone: "warn" };
  }
  return { label: "on", tone: "on" };
}

/** A group's list-row state: the most urgent of its members' badges. */
function groupBadge(
  group: Group,
  items: IntegrationStatus[],
  connections: ConnectionStatusRow[]
): { label: string; tone: "on" | "off" | "warn" | "dim" } {
  const badges = group.members
    .map((id) => items.find((i) => i.id === id))
    .filter((i): i is IntegrationStatus => Boolean(i))
    .map((i) =>
      statusBadge(
        i,
        i.connection
          ? connections.find((c) => c.id === i.connection) ?? null
          : null
      )
    );
  for (const tone of ["warn", "on", "dim", "off"] as const) {
    const hit = badges.find((b) => b.tone === tone);
    if (hit) return hit;
  }
  return { label: "off", tone: "off" };
}

/** One master-list row. Planned (NotWired) groups carry a "planned" badge
 *  instead of a status dot; unavailable groups render dim. */
function IntListRow({
  group,
  items,
  connections,
  selected,
  onSelect,
}: {
  group: Group;
  items: IntegrationStatus[];
  connections: ConnectionStatusRow[];
  selected: boolean;
  onSelect: () => void;
}) {
  const planned = groupPlanned(group, items);
  const unavailable = groupUnavailable(group, items);
  const badge = groupBadge(group, items, connections);
  return (
    <button
      className={`int-list-row ${selected ? "selected" : ""} ${
        unavailable ? "int-list-row-dim" : ""
      }`}
      onClick={onSelect}
    >
      <span className="int-list-title">{group.title}</span>
      {planned ? (
        <span className="int-badge int-badge-planned int-list-badge">
          planned
        </span>
      ) : (
        <span
          className={`int-dot int-dot-${badge.tone}`}
          title={badge.label}
        />
      )}
    </button>
  );
}

function CollectorStatus({ watcher }: { watcher: WatcherStatus }) {
  const line =
    watcher.collector === "daemon"
      ? "Collecting 24/7 — the troved daemon is running."
      : watcher.collector === "app"
        ? watcher.daemon_installed
          ? "Collecting while this app is open — troved takes over when it quits."
          : "Collecting only while this app is open."
        : "Nothing is collecting right now.";
  return (
    <div className="int-collector-status">
      <span
        className={`int-dot int-dot-${
          watcher.collector === "none" ? "off" : "on"
        }`}
      />
      <span>
        {line}
        {!watcher.daemon_installed && (
          <>
            {" "}
            Run <code>troved install</code> in a terminal to collect 24/7 in
            the background.
          </>
        )}
      </span>
    </div>
  );
}

function IntegrationCard({
  item,
  connection,
  finance,
  ouraInfo,
  enabledIds,
  busy,
  setBusy,
  setError,
  setNotice,
  onToggle,
  refresh,
}: {
  item: IntegrationStatus;
  /** The connection this integration needs, joined for it. */
  connection: ConnectionStatusRow | null;
  /** Account registry for the statement-import picker (csv-import only). */
  finance: FinanceOverview | null;
  ouraInfo: OuraSyncState | null;
  enabledIds: Set<string>;
  busy: string | null;
  setBusy: (b: string | null) => void;
  setError: (e: string | null) => void;
  setNotice: (n: string | null) => void;
  onToggle: (id: string, enabled: boolean) => void;
  refresh: () => Promise<void>;
}) {
  const badge = statusBadge(item, connection);
  const connected = (connection?.status?.accounts?.length ?? 0) > 0;
  // Setup steps start open while the integration isn't healthy yet.
  const [showSetup, setShowSetup] = useState(badge.tone === "warn");
  // Catalogued-but-unavailable: a greyed card whose whole job is the
  // honest "why not" — no toggle, no controls, no recent-data peek.
  if (item.unavailable_reason) {
    return (
      <div className="sync-card int-card int-unavailable">
        <div className="sync-card-head">
          <div>
            <div className="sync-card-name">
              {item.name}
              <span className={`int-badge int-badge-${badge.tone}`}>
                {badge.label}
              </span>
            </div>
            <div className="sync-card-sub">{item.description}</div>
            <div className="int-unavailable-reason">
              {item.unavailable_reason}
            </div>
          </div>
        </div>
      </div>
    );
  }
  // Catalogued-but-not-built (NotWired): a "planned" badge and the inline
  // queued-for-the-build-wave hint — no toggle, no controls.
  if (item.planned) {
    return (
      <div className="sync-card int-card int-planned">
        <div className="sync-card-head">
          <div>
            <div className="sync-card-name">
              {item.name}
              <span className="int-badge int-badge-planned">planned</span>
            </div>
            <div className="sync-card-sub">{item.description}</div>
            <div className="int-planned-hint">
              Catalogued, not built yet — queued for the integration build
              wave. Nothing collects until its collector is wired.
            </div>
          </div>
        </div>
      </div>
    );
  }
  return (
    <div className={`sync-card int-card ${item.enabled ? "" : "int-disabled"}`}>
      <div className="sync-card-head">
        <div>
          <div className="sync-card-name">
            {item.name}
            <span className={`int-badge int-badge-${badge.tone}`}>
              {badge.label}
            </span>
          </div>
          <div className="sync-card-sub">{item.description}</div>
          {item.caveats && (
            <div className="int-caveat">
              <span className="int-caveat-mark">!</span> {item.caveats}
            </div>
          )}
          <div className="int-meta">
            <code>~/Documents/Trove/{item.vault_path}</code>
            {item.last_data && (
              <span>
                {item.kind === "import" ? "last import" : "last data"}{" "}
                {fmtWhen(item.last_data)}
              </span>
            )}
            {item.setup.length > 0 && (
              <button
                className="sync-link"
                onClick={() => setShowSetup(!showSetup)}
              >
                {showSetup ? "hide setup steps" : "setup steps"}
              </button>
            )}
          </div>
          {showSetup && (
            <ol className="sync-steps">
              {item.setup.map((step, i) => (
                <li key={i}>{step}</li>
              ))}
            </ol>
          )}
        </div>
        {item.toggleable && (
          <label className="int-switch" title={item.enabled ? "On" : "Off"}>
            <input
              type="checkbox"
              checked={item.enabled}
              onChange={(e) => onToggle(item.id, e.target.checked)}
            />
            <span className="int-slider" />
          </label>
        )}
      </div>
      {item.enabled && <PermissionBanner item={item} />}
      {/* The connect phase, straight off the registry: connect methods,
          account rows, reconnect/disconnect. */}
      {item.enabled && connection && (
        <ConnectCard
          row={connection}
          enabledIds={enabledIds}
          onStatusChange={() => refresh().catch((e) => setError(String(e)))}
          onPulled={(_id, outcome) => {
            setNotice(outcome.headline);
            refresh().catch((e) => setError(String(e)));
          }}
        />
      )}
      {/* Manual "Sync now" for anything with a pull hook (login-bearing ones
          only once an account is connected). */}
      {item.enabled && item.pullable && (!item.connection || connected) && (
        <SyncNowRow
          id={item.id}
          note={
            item.id === "oura" && ouraInfo
              ? [
                  ouraInfo.updated && `last sync ${fmtWhen(ouraInfo.updated)}`,
                  Object.values(ouraInfo.collections ?? {}).some(
                    (c) => !c.backfill_done
                  ) && "backfilling history…",
                  ouraInfo.error,
                ]
                  .filter(Boolean)
                  .join(" · ")
              : undefined
          }
          busy={busy}
          setBusy={setBusy}
          setError={setError}
          setNotice={setNotice}
          refresh={refresh}
        />
      )}
      {item.id === "csv-import" && item.kind === "import" && (
        <CsvImportControls
          finance={finance}
          busy={busy}
          setBusy={setBusy}
          setError={setError}
          setNotice={setNotice}
          refresh={refresh}
        />
      )}
      {item.id !== "csv-import" && item.import && (
        <ImportBox
          id={item.id}
          info={item.import}
          busy={busy}
          setBusy={setBusy}
          setError={setError}
          setNotice={setNotice}
          refresh={refresh}
        />
      )}
      <RecentData vaultPath={item.vault_path} />
    </div>
  );
}

function PermissionBanner({ item }: { item: IntegrationStatus }) {
  const perm = item.permission;
  if (!perm || perm.granted === true) return null;
  const label = PERMISSION_LABEL[perm.kind] ?? perm.kind;

  const openSettings = async () => {
    // Screen Recording and Calendar/Reminders can prompt programmatically;
    // everything else is a manual grant in System Settings. For Calendar/
    // Reminders the prompt is the only path — the panes have no add button.
    if (perm.kind === "screen-recording") {
      const granted = await api.requestActivityPermission().catch(() => false);
      if (granted) return;
    }
    if (perm.kind === "calendars" || perm.kind === "reminders") {
      const [ev, rem] = await api
        .requestCalendarPermission()
        .catch(() => [false, false]);
      if (perm.kind === "calendars" ? ev : rem) return;
    }
    openUrl(SETTINGS_URL[perm.kind] ?? SETTINGS_URL["full-disk-access"]).catch(
      () => {}
    );
  };

  return (
    <div className="perm-banner int-perm">
      <span>
        {perm.granted === false ? (
          <>
            <strong>{label}</strong> not granted
            {perm.required
              ? " — nothing can be collected until it is."
              : " — collection runs, but with less detail."}
          </>
        ) : (
          <>
            Needs <strong>{label}</strong>, granted manually in System
            Settings (no prompt will appear).
          </>
        )}{" "}
        Grants are per-binary: give it to both Trove and the troved daemon.
      </span>
      <button className="btn-primary" onClick={openSettings}>
        Open System Settings
      </button>
    </div>
  );
}

/** Generic "Sync now": one button driving the registry's `integration_pull`
 *  for any integration with a pull hook, reporting the generic headline. */
function SyncNowRow({
  id,
  note,
  busy,
  setBusy,
  setError,
  setNotice,
  refresh,
}: {
  id: string;
  /** Optional service status line shown next to the button. */
  note?: string;
  busy: string | null;
  setBusy: (b: string | null) => void;
  setError: (e: string | null) => void;
  setNotice: (n: string | null) => void;
  refresh: () => Promise<void>;
}) {
  const pull = async () => {
    setError(null);
    setNotice(null);
    setBusy(`${id}-pull`);
    try {
      const outcome = await api.integrationPull(id);
      setNotice(outcome.headline);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="int-actions">
      {note && <span className="int-actions-note">{note}</span>}
      <button className="btn-primary" onClick={pull} disabled={busy !== null}>
        {busy === `${id}-pull` ? "Syncing…" : "Sync now"}
      </button>
    </div>
  );
}

/** The Google group card: the shared connection (any number of accounts,
 *  rendered by the generic ConnectCard) plus the per-service collection
 *  toggles — the layout is the only Google-specific thing left; all connect
 *  machinery comes from the registry. */
function GoogleSection({
  items,
  connection,
  gmailInfo,
  enabledIds,
  busy,
  setBusy,
  setError,
  setNotice,
  onToggle,
  refresh,
}: {
  items: IntegrationStatus[];
  connection: ConnectionStatusRow | null;
  gmailInfo: GmailSyncState | null;
  enabledIds: Set<string>;
  busy: string | null;
  setBusy: (b: string | null) => void;
  setError: (e: string | null) => void;
  setNotice: (n: string | null) => void;
  onToggle: (id: string, enabled: boolean) => Promise<void>;
  refresh: () => Promise<void>;
}) {
  const accounts = connection?.status?.accounts ?? [];
  const services = GOOGLE_SERVICES.map((id) =>
    items.find((i) => i.id === id)
  ).filter((i): i is IntegrationStatus => Boolean(i));
  const allOn = services.length > 0 && services.every((s) => s.enabled);

  const toggleAll = async (enabled: boolean) => {
    for (const s of services) {
      if (s.enabled !== enabled) await onToggle(s.id, enabled);
    }
  };

  const gmailAccts = Object.values(gmailInfo?.accounts ?? {});
  const gmailBackfilling = gmailAccts.some((a) => !a.backfill_done);
  const gmailMessages = gmailAccts.reduce((n, a) => n + a.messages, 0);
  const gmailError = gmailAccts.find((a) => a.error)?.error;

  return (
    <div className="sync-card int-card">
      {/* The connect phase, straight off the registry. */}
      {connection && (
        <ConnectCard
          row={connection}
          enabledIds={enabledIds}
          onStatusChange={() => refresh().catch((e) => setError(String(e)))}
          onPulled={(_id, outcome) => {
            setNotice(outcome.headline);
            refresh().catch((e) => setError(String(e)));
          }}
        />
      )}

      {/* Testing-mode reconnect caveat */}
      <div className="int-caveat">
        <span className="int-caveat-mark">!</span> While the OAuth consent
        screen is in Testing mode, Google expires the login after 7 days — an
        account will show <em>reconnect</em> when that happens. Publishing the
        app (a one-time Google verification) removes the weekly reconnect.
      </div>

      {/* Per-service collection toggles */}
      <div className="int-google-services">
        <div className="int-google-services-head">
          <span>Data to collect</span>
          <label className="int-switch" title={allOn ? "All on" : "Enable all"}>
            <input
              type="checkbox"
              checked={allOn}
              onChange={(e) => toggleAll(e.target.checked)}
            />
            <span className="int-slider" />
          </label>
        </div>
        <p className="int-detail-intro">
          Choose which services to pull from your connected accounts. Each
          runs on its own schedule; Sync now pulls one on demand.
        </p>
        {services.map((s) => (
          <div className="int-google-service" key={s.id}>
            <div className="int-google-service-main">
              <div>
                <div className="sync-card-name">{s.name}</div>
                <div className="sync-card-sub">{s.description}</div>
              </div>
              <label className="int-switch" title={s.enabled ? "On" : "Off"}>
                <input
                  type="checkbox"
                  checked={s.enabled}
                  onChange={(e) => onToggle(s.id, e.target.checked)}
                />
                <span className="int-slider" />
              </label>
            </div>
            {/* Per-service status + manual sync, only meaningful with a
                connected account and the toggle on. Gmail keeps its richer
                backfill/last-sync line; the rest show last-data. */}
            {s.enabled && accounts.length > 0 && s.pullable && (
              <div className="int-actions int-google-service-sync">
                <span className="int-actions-note">
                  {s.id === "google-gmail"
                    ? [
                        gmailInfo?.updated
                          ? `${gmailMessages.toLocaleString()} messages · last sync ${fmtWhen(
                              gmailInfo.updated
                            )}`
                          : "Not synced yet",
                        gmailBackfilling && "backfilling history…",
                        gmailError,
                      ]
                        .filter(Boolean)
                        .join(" · ")
                    : s.last_data
                      ? `last data ${fmtWhen(s.last_data)}`
                      : "Not synced yet"}
                </span>
                <button
                  className="btn-primary"
                  disabled={busy !== null}
                  onClick={async () => {
                    setError(null);
                    setNotice(null);
                    setBusy(`${s.id}-pull`);
                    try {
                      const outcome = await api.integrationPull(s.id);
                      setNotice(outcome.headline);
                      await refresh();
                    } catch (e) {
                      setError(String(e));
                    } finally {
                      setBusy(null);
                    }
                  }}
                >
                  {busy === `${s.id}-pull` ? "Syncing…" : "Sync now"}
                </button>
              </div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

/** The finance statement import keeps its bespoke box: picking from the
 *  existing-account registry beats a free-text id. It still runs through
 *  the generic `run_import` ("csv-import") underneath. */
function CsvImportControls({
  finance,
  busy,
  setBusy,
  setError,
  setNotice,
  refresh,
}: {
  finance: FinanceOverview | null;
  busy: string | null;
  setBusy: (b: string | null) => void;
  setError: (e: string | null) => void;
  setNotice: (n: string | null) => void;
  refresh: () => Promise<void>;
}) {
  const [account, setAccount] = useState("");
  const [newAccountName, setNewAccountName] = useState("");

  const accounts = finance?.accounts ?? [];
  const newAccount = account === "__new__";
  const copilot = account === "__copilot__";
  const targetReady = newAccount ? newAccountName.trim() !== "" : account !== "";

  const run = async () => {
    const file = await open({
      multiple: false,
      filters: [{ name: "Bank statement CSV", extensions: ["csv"] }],
    });
    if (!file) return;
    setError(null);
    setNotice(null);
    setBusy("csv-import");
    try {
      const params: Record<string, string> = newAccount
        ? { new_account: newAccountName.trim() }
        : copilot
          ? {}
          : { account };
      const outcome = await api.runImport("csv-import", file as string, params);
      setNotice(outcome.headline);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="sync-form int-actions">
      <select
        className="int-select"
        value={account}
        onChange={(e) => setAccount(e.target.value)}
      >
        <option value="">Which account is this file from?</option>
        {accounts.map((a) => (
          <option key={a.id} value={a.id}>
            {a.org ? `${a.org} — ${a.name}` : a.name}
          </option>
        ))}
        <option value="__new__">New account…</option>
        <option value="__copilot__">
          Copilot Money export (maps accounts automatically)
        </option>
      </select>
      {newAccount && (
        <input
          type="text"
          placeholder="Account name (e.g. Apple Card)"
          value={newAccountName}
          onChange={(e) => setNewAccountName(e.target.value)}
          spellCheck={false}
        />
      )}
      <button
        className="btn-primary"
        disabled={busy !== null || !targetReady}
        onClick={run}
      >
        {busy === "csv-import" ? "Importing…" : "Import CSV…"}
      </button>
      {!targetReady && busy === null && (
        <span className="int-actions-note">
          pick the account the file is from first
        </span>
      )}
    </div>
  );
}

/** The generic import box: file picker + param fields straight off the
 *  integration's `ImportInfo` — a new import-kind integration gets a
 *  working UI with zero frontend code. */
function ImportBox({
  id,
  info,
  busy,
  setBusy,
  setError,
  setNotice,
  refresh,
}: {
  id: string;
  info: ImportInfo;
  busy: string | null;
  setBusy: (b: string | null) => void;
  setError: (e: string | null) => void;
  setNotice: (n: string | null) => void;
  refresh: () => Promise<void>;
}) {
  const [file, setFile] = useState<string | null>(null);
  const [params, setParams] = useState<Record<string, string>>({});
  const [progress, setProgress] = useState<ImportProgress | null>(null);

  // What still blocks the Import button, spelled out as the inline hint.
  const missing: string[] = [];
  if (!file) missing.push(`pick a ${info.accepts.map((e) => `.${e}`).join(" / ")} file`);
  for (const p of info.params) {
    if (p.required && !(params[p.key] ?? "").trim()) {
      missing.push(`enter the ${p.label.toLowerCase()}`);
    }
  }
  const ready = missing.length === 0;

  const pick = async () => {
    const picked = await open({
      multiple: false,
      filters: [{ name: "Import file", extensions: [...info.accepts] }],
    });
    if (typeof picked === "string") setFile(picked);
  };

  const run = async () => {
    if (!file) return;
    setError(null);
    setNotice(null);
    setBusy(id);
    setProgress({ integration_id: id, records: 0, percent: 0 });
    const unlisten = await listen<ImportProgress>("import-progress", (e) => {
      if (e.payload.integration_id === id) setProgress(e.payload);
    });
    try {
      const trimmed: Record<string, string> = {};
      for (const p of info.params) {
        const v = (params[p.key] ?? "").trim();
        if (v) trimmed[p.key] = v;
      }
      const outcome = await api.runImport(id, file, trimmed);
      setNotice(outcome.headline);
      setFile(null);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      unlisten();
      setProgress(null);
      setBusy(null);
    }
  };

  return (
    <>
      <div className="sync-form int-actions">
        <button className="btn-ghost" disabled={busy !== null} onClick={pick}>
          {file ? (file.split("/").pop() ?? file) : "Choose file…"}
        </button>
        {info.params.map((p) => (
          <input
            key={p.key}
            type="text"
            title={p.label}
            placeholder={p.required ? p.label : `${p.label} (optional)`}
            value={params[p.key] ?? ""}
            onChange={(e) =>
              setParams({ ...params, [p.key]: e.target.value })
            }
            spellCheck={false}
          />
        ))}
        <button
          className="btn-primary"
          disabled={busy !== null || !ready}
          onClick={run}
        >
          {busy === id ? "Importing…" : "Import"}
        </button>
        {!ready && busy === null && (
          <span className="int-actions-note">{missing.join(" and ")} first</span>
        )}
      </div>
      {progress && (
        <div className="import-panel compact">
          <div className="import-label">
            Importing… {progress.records.toLocaleString()} records
          </div>
          <div className="progress-track">
            <div
              className="progress-fill"
              style={{ width: `${Math.max(progress.percent, 1)}%` }}
            />
          </div>
        </div>
      )}
    </>
  );
}

/** Collapsible raw-records peek under every card. Mounts lazily; when the
 *  integration's folder isn't itself a stream (no partitions), it consults
 *  the vault manifest once and renders the matching domain's source
 *  subfolders instead (e.g. correspondence/imessage). */
function RecentData({ vaultPath }: { vaultPath: string }) {
  const dir = vaultPath.replace(/\/+$/, "");
  const [show, setShow] = useState(false);
  // null = show the folder itself; [] = nothing resolved (stay empty).
  const [streams, setStreams] = useState<
    { dir: string; title?: string }[] | null
  >(null);

  const onFirstPage = useCallback(
    async (page: StreamPage) => {
      if (page.partitions.length > 0) return;
      try {
        const manifest = await api.vaultManifest();
        const matched = manifest.domains.filter(
          (d) => d.domain === dir || d.domain.startsWith(`${dir}/`)
        );
        const resolved: { dir: string; title?: string }[] = [];
        for (const d of matched) {
          if (d.sources.length > 0) {
            for (const s of d.sources) {
              resolved.push({ dir: `${d.domain}/${s}`, title: s });
            }
          } else if (d.domain !== dir) {
            resolved.push({ dir: d.domain });
          }
        }
        if (resolved.length > 0) setStreams(resolved);
      } catch {
        // No manifest yet — the empty state is fine.
      }
    },
    [dir]
  );

  if (!dir) return null;
  return (
    <div className="int-recent">
      <button className="sync-link" onClick={() => setShow(!show)}>
        {show ? "hide recent data" : "recent data"}
      </button>
      {show &&
        (streams === null ? (
          <GenericStreamView dir={dir} onFirstPage={onFirstPage} />
        ) : (
          streams.map((s) => (
            <GenericStreamView key={s.dir} dir={s.dir} title={s.title} />
          ))
        ))}
    </div>
  );
}

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  api,
  ScreenTimeDevice,
  ScreenTimeSession,
  ScreenTimeSummary,
  SeriesPoint,
} from "../api";
import Chart from "./Chart";

type Range = "today" | "7d" | "30d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "today", label: "Today", days: 1 },
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
];

// Biome syncs land on the runner's slow tick (~15 min); polling faster than
// once a minute buys nothing.
const REFRESH_MS = 60_000;

const KIND_NAMES: Record<string, string> = {
  iphone: "iPhone",
  ipad: "iPad",
  watch: "Apple Watch",
  mac: "Mac",
  "this-mac": "This Mac",
  ios: "iOS Device",
  unknown: "Device",
};

const KIND_BADGES: Record<string, string> = {
  iphone: "iPhone",
  ipad: "iPad",
  watch: "Watch",
  mac: "Mac",
  "this-mac": "This Mac",
  ios: "iOS",
  unknown: "?",
};

/** YYYY-MM-DD in local time (sessions are stored in local time). */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function fmtDuration(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const m = Math.round(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

function fmtClock(rfc3339: string): string {
  return new Date(rfc3339).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** "iphone-58a03ead" → "iPhone", disambiguated as "iPhone · 58a0" when the
 *  catalog holds more than one device of that kind. Labels are stable file
 *  paths; this is display-only. */
function deviceName(
  uuid: string,
  devices: Record<string, ScreenTimeDevice>
): string {
  const info = devices[uuid];
  if (!info) return uuid === "this-mac" ? "This Mac" : uuid.slice(0, 8);
  const kindName = KIND_NAMES[info.kind] ?? info.kind;
  const twins = Object.values(devices).filter((d) => d.kind === info.kind);
  if (twins.length <= 1) return kindName;
  const tail = info.label.split("-").pop() ?? "";
  return `${kindName} · ${tail.slice(0, 4)}`;
}

export default function ScreenTimeView() {
  const [range, setRange] = useState<Range>("today");
  const [device, setDevice] = useState<string | null>(null);
  const [showIdle, setShowIdle] = useState(false);
  const [summary, setSummary] = useState<ScreenTimeSummary | null>(null);
  const [daily, setDaily] = useState<SeriesPoint[]>([]);
  const [timeline, setTimeline] = useState<ScreenTimeSession[]>([]);
  const [devices, setDevices] = useState<Record<string, ScreenTimeDevice>>({});
  const [hasPermission, setHasPermission] = useState(true);

  const refresh = useCallback(
    async (r: Range, dev: string | null, idle: boolean) => {
      const today = new Date();
      const to = localDate(today);
      const days = RANGES.find((x) => x.id === r)!.days;
      const fromDate = new Date(today);
      fromDate.setDate(fromDate.getDate() - (days - 1));
      const from = localDate(fromDate);

      const [s, d] = await Promise.all([
        api.screenTimeSummary(from, to, dev, idle),
        api.screenTimeDevices(),
      ]);
      setSummary(s);
      setDevices(d);
      if (r === "today") {
        setTimeline(await api.screenTimeTimeline(to, dev, idle));
      } else {
        setDaily(await api.screenTimeDaily(from, to, dev, idle));
      }
    },
    []
  );

  // Initial + filter-change load, then poll so new syncs show up.
  useEffect(() => {
    let active = true;
    const tick = () => {
      if (active) refresh(range, device, showIdle).catch(() => {});
    };
    tick();
    const id = setInterval(tick, REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [range, device, showIdle, refresh]);

  useEffect(() => {
    api.screenTimePermission().then(setHasPermission).catch(() => {});
  }, []);

  // Chips ordered most-recently-seen first; the local-Mac backup arm last.
  const deviceChips = useMemo(() => {
    return Object.entries(devices).sort(([ua, a], [ub, b]) => {
      if (ua === "this-mac") return 1;
      if (ub === "this-mac") return -1;
      return b.last_seen.localeCompare(a.last_seen);
    });
  }, [devices]);

  const noDevicesYet = deviceChips.length === 0;
  const maxApp =
    summary && summary.apps.length > 0 ? summary.apps[0].seconds : 1;
  const maxDevice =
    summary && summary.devices.length > 0 ? summary.devices[0].seconds : 1;

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Screen Time</h2>
          <div className="activity-sub">
            App usage from every Apple device on your iCloud account
          </div>
        </div>
        <div className="segmented">
          {RANGES.map((r) => (
            <button
              key={r.id}
              className={range === r.id ? "active" : ""}
              onClick={() => setRange(r.id)}
            >
              {r.label}
            </button>
          ))}
        </div>
      </div>

      {!noDevicesYet && (
        <div className="st-filter-row">
          <div className="st-chips">
            <button
              className={`st-chip ${device === null ? "active" : ""}`}
              onClick={() => setDevice(null)}
            >
              All devices
            </button>
            {deviceChips.map(([uuid]) => (
              <button
                key={uuid}
                className={`st-chip ${device === uuid ? "active" : ""}`}
                onClick={() => setDevice(device === uuid ? null : uuid)}
              >
                {deviceName(uuid, devices)}
              </button>
            ))}
          </div>
          <label className="st-toggle" title="Lock screens, StandBy, and watch faces are recorded but hidden by default.">
            <input
              type="checkbox"
              checked={showIdle}
              onChange={(e) => setShowIdle(e.target.checked)}
            />
            Show lock screens
          </label>
        </div>
      )}

      {summary && !noDevicesYet && (
        <div className="activity-stats">
          <Stat label="Screen time" value={fmtDuration(summary.total_seconds)} />
          <Stat label="Apps" value={String(summary.apps.length)} muted />
          <Stat label="Devices" value={String(summary.devices.length)} muted />
        </div>
      )}

      {noDevicesYet && (
        <div className="activity-empty">
          {hasPermission ? (
            <>
              No screen time synced yet. Sessions from your iPhone, iPad,
              Watch, and other Macs appear within about 15 minutes of the
              collector's first pass — and bank a few weeks of history on the
              first one.
            </>
          ) : (
            <>
              Reading cross-device usage needs <strong>Full Disk Access</strong>.
              Open the <strong>Integrations</strong> tab and follow the setup
              steps on the Screen Time card.
            </>
          )}
        </div>
      )}

      {summary && !noDevicesYet && summary.total_seconds === 0 && (
        <div className="activity-empty">
          No sessions in this range
          {device ? " for this device" : ""}. Devices sync their usage
          through iCloud with a delay of minutes to hours.
        </div>
      )}

      {summary && device === null && summary.devices.length > 1 && (
        <div className="activity-trend">
          <div className="activity-section-title">By device</div>
          <div className="app-bars">
            {summary.devices.map((d) => (
              <div
                key={d.device}
                className="app-bar st-clickable"
                onClick={() => setDevice(d.device)}
                title={`${d.label} — click to filter`}
              >
                <div className="app-bar-name">
                  {deviceName(d.device, devices)}
                </div>
                <div className="app-bar-track">
                  <div
                    className="app-bar-fill"
                    style={{ width: `${(d.seconds / maxDevice) * 100}%` }}
                  />
                </div>
                <div className="app-bar-time">{fmtDuration(d.seconds)}</div>
              </div>
            ))}
          </div>
        </div>
      )}

      {summary && summary.apps.length > 0 && (
        <div className="activity-trend">
          <div className="activity-section-title">Top apps</div>
          <div className="app-bars">
            {summary.apps.slice(0, 14).map((a) => (
              <div key={a.bundle_id} className="app-bar">
                <div className="app-bar-name" title={a.bundle_id}>
                  {a.app || "Unknown"}
                </div>
                <div className="app-bar-track">
                  <div
                    className="app-bar-fill"
                    style={{ width: `${(a.seconds / maxApp) * 100}%` }}
                  />
                </div>
                <div className="app-bar-time">{fmtDuration(a.seconds)}</div>
              </div>
            ))}
          </div>
        </div>
      )}

      {range !== "today" && daily.length > 0 && (
        <div className="activity-trend">
          <div className="activity-section-title">Screen time per day</div>
          <Chart points={daily} name="Screen time" unit="hr" kind="sum" />
        </div>
      )}

      {range === "today" && timeline.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">Today, most recent first</div>
          {[...timeline]
            .reverse()
            .slice(0, 60)
            .map((s, i) => (
              <div key={i} className="tl-row">
                <div className="tl-time">
                  {fmtClock(s.start)} – {fmtClock(s.end)}
                </div>
                <div className="tl-body">
                  <span className="tl-app">
                    {s.app || "Unknown"}
                    {device === null && (
                      <span className="tl-badge">
                        {KIND_BADGES[s.device_kind] ?? s.device_kind}
                      </span>
                    )}
                  </span>
                </div>
                <div className="tl-dur">{fmtDuration(s.seconds)}</div>
              </div>
            ))}
        </div>
      )}

      <div className="activity-footnote">
        Read from the usage streams iCloud already syncs between your Apple
        devices — nothing leaves your machine. Apple keeps only a few weeks
        on disk; Trove banks them for good. Raw sessions:{" "}
        <code>~/Documents/Trove/screen-time/</code> — one folder per device, one JSONL
        file per day.
      </div>
    </div>
  );
}

function Stat({
  label,
  value,
  muted,
}: {
  label: string;
  value: string;
  muted?: boolean;
}) {
  return (
    <div className={`stat ${muted ? "muted" : ""}`}>
      <div className="stat-value">{value}</div>
      <div className="stat-label">{label}</div>
    </div>
  );
}

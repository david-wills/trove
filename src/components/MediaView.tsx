import { useCallback, useEffect, useState } from "react";
import { api, LiveSpan, MediaItem, MediaSummary, SeriesPoint } from "../api";
import Chart from "./Chart";

type Range = "today" | "7d" | "30d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "today", label: "Today", days: 1 },
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
];

type Category = "all" | "music" | "podcast" | "audiobook" | "video";

const CATEGORIES: { id: Category; label: string }[] = [
  { id: "all", label: "All" },
  { id: "music", label: "Music" },
  { id: "podcast", label: "Podcasts" },
  { id: "audiobook", label: "Audiobooks" },
  { id: "video", label: "Video" },
];

/** Short badge text per content category, shown in the merged stream. */
const CATEGORY_BADGE: Record<string, string> = {
  music: "music",
  podcast: "podcast",
  audiobook: "audiobook",
  video: "video",
  other: "other",
};


// Closed plays land when a track/span ends and podcast events when the
// change-gated snapshot fires (~15 min) — a tight poll just burns cycles;
// 15s matches the other views.
const REFRESH_MS = 15000;

// Live web playback updates on the extension's ~5s snapshot cadence.
const LIVE_REFRESH_MS = 5000;

/** YYYY-MM-DD in local time (plays are stored in local time). */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function fmtClock(rfc3339: string): string {
  return new Date(rfc3339).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function fmtDuration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins}m`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m`;
}

/** The secondary line under a timeline row's title. */
function itemDetail(it: MediaItem): string {
  if (it.source === "web") return it.detail || it.subtitle;
  if (it.source === "music" && it.detail) return `${it.subtitle} — ${it.detail}`;
  return it.subtitle;
}

export default function MediaView() {
  const [range, setRange] = useState<Range>("today");
  const [category, setCategory] = useState<Category>("all");
  const [device, setDevice] = useState<string>("all");
  const [summary, setSummary] = useState<MediaSummary | null>(null);
  const [daily, setDaily] = useState<SeriesPoint[]>([]);
  const [timeline, setTimeline] = useState<MediaItem[]>([]);
  const [live, setLive] = useState<LiveSpan[]>([]);

  const refresh = useCallback(async (r: Range) => {
    const today = new Date();
    const to = localDate(today);
    const days = RANGES.find((x) => x.id === r)!.days;
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (days - 1));
    const from = localDate(fromDate);

    setSummary(await api.mediaSummary(from, to));
    if (r === "today") {
      setTimeline(await api.mediaTimeline(to));
    } else {
      setDaily(await api.mediaDaily(from, to));
    }
  }, []);

  useEffect(() => {
    let active = true;
    const tick = () => {
      if (active) refresh(range).catch(() => {});
    };
    tick();
    const id = setInterval(tick, REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [range, refresh]);

  // Live web playback (open audible spans from the extension), polled on its
  // own cadence so the elapsed time ticks while a video plays. Music has no
  // live arm yet — scrobbles land when the track closes.
  useEffect(() => {
    let active = true;
    const tick = () => {
      api
        .browserLive()
        .then((spans) => {
          if (active) setLive(spans.filter((s) => s.audible));
        })
        .catch(() => {});
    };
    tick();
    const id = setInterval(tick, LIVE_REFRESH_MS);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, []);

  const inCategory = (c: string) => category === "all" || c === category;
  const onDevice = (d: string | undefined) =>
    device === "all" || (d ?? "") === device;
  const top = summary
    ? summary.top.filter((u) => inCategory(u.category) && onDevice(u.device))
    : [];
  const maxPlays = top.length > 0 ? Math.max(top[0].plays, 1) : 1;
  const rows = timeline.filter(
    (it) => inCategory(it.category) && onDevice(it.device),
  );
  const total = summary ? summary.plays + summary.partials : 0;
  // Live web spans are Mac playback in the video category.
  const showLive =
    live.length > 0 &&
    (category === "all" || category === "video") &&
    (device === "all" || device === "mac");
  // Device filter chips: the devices actually present, most-used first.
  const deviceChips = summary
    ? Object.entries(summary.devices).sort((a, b) => b[1] - a[1])
    : [];

  return (
    <div className="view view--scroll">
      <div className="view-header">
        <div>
          <h2>Media</h2>
          <div className="view-sub">
            Music · Podcasts · Audiobooks · Video — every play, one stream
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

      <div className="segmented media-sources">
        {CATEGORIES.map((c) => (
          <button
            key={c.id}
            className={category === c.id ? "active" : ""}
            onClick={() => setCategory(c.id)}
          >
            {c.label}
          </button>
        ))}
      </div>

      {deviceChips.length > 1 && (
        <div className="segmented media-sources">
          <button
            className={device === "all" ? "active" : ""}
            onClick={() => setDevice("all")}
          >
            All devices
          </button>
          {deviceChips.map(([name, n]) => (
            <button
              key={name}
              className={device === name ? "active" : ""}
              onClick={() => setDevice(name)}
            >
              {name} · {n}
            </button>
          ))}
        </div>
      )}

      {showLive && (
        <div className="live-now">
          <div className="view-section-title">
            <span className="live-dot" /> Playing now
          </div>
          {live.map((s) => (
            <div key={s.url} className="tl-row live-row">
              <div className="tl-body">
                <span className="tl-app">
                  {s.favicon && (
                    <img
                      className="tl-favicon"
                      src={s.favicon}
                      alt=""
                      loading="lazy"
                      onError={(e) => {
                        e.currentTarget.style.visibility = "hidden";
                      }}
                    />
                  )}
                  {s.title || s.url}
                  {!s.foreground && <span className="tl-badge">background</span>}
                </span>
                <span className="tl-title">{s.url}</span>
              </div>
              <div className="tl-dur">{fmtDuration(s.duration_secs)}</div>
            </div>
          ))}
        </div>
      )}

      {summary && (
        <div className="view-stats">
          <Stat label="Plays" value={String(summary.plays)} />
          <Stat label="Listening" value={fmtDuration(summary.seconds)} />
          <Stat label="Partials" value={String(summary.partials)} muted />
        </div>
      )}

      {summary && total === 0 && (
        <div className="view-empty">
          No plays recorded {range === "today" ? "today" : "in this range"}.
          Music scrobbles land live as tracks finish; web playback is captured
          by the browser extension while it's connected; podcast listens
          appear within ~15 minutes (the next library snapshot after the
          playhead moves). Only listening from now on is recorded — none of
          these sources keep their own history.
        </div>
      )}

      {top.length > 0 && (
        <div className="app-bars">
          {top.slice(0, 12).map((u) => (
            <div key={`${u.category}:${u.name}`} className="app-bar">
              <div className="app-bar-name" title={u.name}>
                {u.name}
              </div>
              <div className="app-bar-track">
                <div
                  className="app-bar-fill"
                  style={{ width: `${(u.plays / maxPlays) * 100}%` }}
                />
              </div>
              <div className="app-bar-time">
                {u.plays}
                {u.seconds > 0 && ` · ${fmtDuration(u.seconds)}`}
                {category === "all" && (
                  <span className="tl-badge">{CATEGORY_BADGE[u.category]}</span>
                )}
              </div>
            </div>
          ))}
        </div>
      )}

      {range !== "today" && daily.length > 0 && (
        <div className="view-trend">
          <div className="view-section-title">Plays per day</div>
          <Chart points={daily} name="Plays" unit="" kind="sum" />
        </div>
      )}

      {range === "today" && rows.length > 0 && (
        <div className="view-timeline">
          <div className="view-section-title">Today, most recent first</div>
          {[...rows]
            .sort((a, b) => (a.ts < b.ts ? 1 : -1))
            .slice(0, 60)
            .map((it, i) => (
              <div key={i} className="tl-row">
                <div className="tl-time">{fmtClock(it.ts)}</div>
                <div className="tl-body">
                  <span className="tl-app">
                    {it.favicon && (
                      <img
                        className="tl-favicon"
                        src={it.favicon}
                        alt=""
                        loading="lazy"
                        onError={(e) => {
                          e.currentTarget.style.visibility = "hidden";
                        }}
                      />
                    )}
                    {it.title}
                    {category === "all" && (
                      <span className="tl-badge">
                        {CATEGORY_BADGE[it.category]}
                      </span>
                    )}
                    {device === "all" && it.device && (
                      <span className="tl-badge">{it.device}</span>
                    )}
                    {it.kind === "partial" && (
                      <span className="tl-badge">
                        {it.source === "music" ? "skip" : "partial"}
                      </span>
                    )}
                  </span>
                  <span className="tl-title">{itemDetail(it)}</span>
                </div>
                <div className="tl-dur">
                  {it.seconds > 0 ? (
                    fmtDuration(it.seconds)
                  ) : (
                    <span className="tl-dur-none">—</span>
                  )}
                </div>
              </div>
            ))}
        </div>
      )}

      <div className="view-footnote">
        One read-time stream over per-source files: music scrobbles in{" "}
        <code>~/Documents/Trove/music/plays/</code>, podcast listens in{" "}
        <code>~/Documents/Trove/podcasts/events/</code>, audible web spans in{" "}
        <code>~/Documents/Trove/browser/</code>, and iPhone playback in{" "}
        <code>~/Documents/Trove/media/nowplaying/</code>. The tabs are content type:
        iPhone sessions are sorted into music, podcasts, audiobooks, or video
        from the playing app. Spotify joins here when connected.
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

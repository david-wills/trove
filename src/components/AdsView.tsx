import { useCallback, useEffect, useState } from "react";
import { AdRecord, AdsSummary, AdUsage, api, SeriesPoint } from "../api";
import Chart from "./Chart";

type Range = "today" | "7d" | "30d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "today", label: "Today", days: 1 },
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
];

// The extension batches closed ad records within ~30s; polling on the Web
// tab's cadence keeps the view fresh without buying anything faster.
const REFRESH_MS = 15000;

/** YYYY-MM-DD in local time (records are keyed by local close day). */
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

/** Compact viewed time: "8s", "12m", "1h 4m". */
function fmtDuration(secs: number): string {
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  const h = Math.floor(secs / 3600);
  const m = Math.round((secs % 3600) / 60);
  return m > 0 ? `${h}h ${m}m` : `${h}h`;
}

/** Bare host of a page URL, e.g. "https://www.example.com/x" → "example.com". */
function hostOf(url: string): string {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return url;
  }
}

export default function AdsView({
  onOpenIntegrations,
}: {
  onOpenIntegrations?: () => void;
}) {
  const [range, setRange] = useState<Range>("today");
  const [summary, setSummary] = useState<AdsSummary | null>(null);
  const [seen, setSeen] = useState<SeriesPoint[]>([]);
  const [viewedDaily, setViewedDaily] = useState<SeriesPoint[]>([]);
  const [timeline, setTimeline] = useState<AdRecord[]>([]);
  // null = status unknown (still loading) — don't flash the setup banner.
  const [observerOn, setObserverOn] = useState<boolean | null>(null);
  const [everCollected, setEverCollected] = useState(true);
  // The opt-in advertiser-identity resolver (`browser-ads-identify`).
  const [identifyOn, setIdentifyOn] = useState(false);

  const refresh = useCallback(async (r: Range) => {
    const today = new Date();
    const to = localDate(today);
    const days = RANGES.find((x) => x.id === r)!.days;
    const fromDate = new Date(today);
    fromDate.setDate(fromDate.getDate() - (days - 1));
    const from = localDate(fromDate);

    const [s, integrations] = await Promise.all([
      api.adsSummary(from, to),
      api.integrationsStatus(),
    ]);
    setSummary(s);
    const ads = integrations.find((i) => i.id === "browser-ads");
    setObserverOn(ads ? ads.enabled : false);
    setEverCollected(!!ads?.last_data);
    setIdentifyOn(
      !!integrations.find((i) => i.id === "browser-ads-identify")?.enabled,
    );
    if (r === "today") {
      setTimeline(await api.adsTimeline(to));
    } else {
      const daily = await api.adsDaily(from, to);
      setSeen(daily.seen);
      setViewedDaily(
        daily.viewed_secs.map((p) => ({
          date: p.date,
          value: Math.round((p.value / 60) * 10) / 10,
        })),
      );
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

  const viewablePct =
    summary && summary.ads > 0
      ? `${Math.round((summary.viewable / summary.ads) * 100)}%`
      : "—";
  const needsSetup = observerOn === false || !everCollected;

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Ads</h2>
          <div className="activity-sub">
            The display ads the web showed you — who served them, who paid,
            and how long they were actually on screen.
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

      {needsSetup && (
        <div className="perm-banner">
          <div className="perm-text">
            {observerOn === false ? (
              <>
                <strong>Ad observation is off.</strong> It's the opt-in second
                arm of the browser extension: enable "Page observation" then
                "Ad observation" in the extension's options (Chrome will ask
                for site access), and switch on the{" "}
                <strong>Ad observation</strong> integration here.
              </>
            ) : (
              <>
                <strong>No ads recorded yet.</strong> The vault-side toggle is
                on, but records only land once the extension's own switches
                are too: enable "Page observation" then "Ad observation" in
                the extension's options (Chrome will ask for site access).
              </>
            )}{" "}
            Pure observation — never blocks ads, never reads page text.
          </div>
          {onOpenIntegrations && (
            <div className="perm-actions">
              <button className="btn-primary" onClick={onOpenIntegrations}>
                Open Integrations
              </button>
            </div>
          )}
        </div>
      )}

      {summary && (
        <div className="activity-stats">
          <Stat label="Ads seen" value={String(summary.ads)} />
          <Stat label="Viewable" value={viewablePct} muted />
          <Stat
            label="Ad time"
            value={summary.viewed_secs > 0 ? fmtDuration(summary.viewed_secs) : "—"}
            muted
          />
        </div>
      )}

      {summary && summary.ads === 0 && !needsSetup && (
        <div className="activity-empty">
          No ads recorded {range === "today" ? "today" : "in this range"} yet.
          Records land within about half a minute of an ad leaving the screen.
        </div>
      )}

      {summary && summary.networks.length > 0 && (
        <UsageBars
          title="Ad networks · who served them"
          rows={summary.networks}
        />
      )}

      {summary && summary.advertisers.length > 0 && (
        <UsageBars
          title="Advertisers · who paid for your attention"
          rows={summary.advertisers}
        />
      )}

      {range !== "today" && seen.length > 0 && (
        <div className="activity-trend">
          <div className="activity-section-title">Ads seen per day</div>
          <Chart points={seen} name="Ads" unit="" kind="sum" />
        </div>
      )}

      {range !== "today" && viewedDaily.some((p) => p.value > 0) && (
        <div className="activity-trend">
          <div className="activity-section-title">Ad-viewing time per day</div>
          <Chart points={viewedDaily} name="Ad time" unit="min" kind="sum" />
        </div>
      )}

      {range === "today" && timeline.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">Today, most recent first</div>
          {[...timeline]
            .sort((a, b) => (a.end < b.end ? 1 : -1))
            .slice(0, 60)
            .map((r, i) => (
              <div key={i} className="tl-row">
                <div className="tl-time">{fmtClock(r.end)}</div>
                <div className="tl-body">
                  <span className="tl-app">
                    {r.advertiser && r.advertiser_id ? (
                      <a
                        href={`https://adstransparency.google.com/advertiser/${r.advertiser_id}`}
                        target="_blank"
                        rel="noreferrer"
                        title="See this advertiser on Google's ad-transparency center"
                      >
                        {r.advertiser}
                      </a>
                    ) : (
                      r.advertiser || r.network
                    )}
                    {r.advertiser && (
                      <span className="tl-badge">via {r.network}</span>
                    )}
                    {(r.w ?? 0) > 0 && (
                      <span className="tl-badge">
                        {r.w}×{r.h}
                      </span>
                    )}
                  </span>
                  <span className="tl-title">on {hostOf(r.page_url)}</span>
                </div>
                <div className="tl-dur">
                  {(r.viewed_secs ?? 0) > 0 ? (
                    fmtDuration(r.viewed_secs!)
                  ) : (
                    <span className="tl-dur-none">—</span>
                  )}
                  {r.viewable && <span className="tl-dur-sub">viewable</span>}
                </div>
              </div>
            ))}
        </div>
      )}

      {summary && summary.ads > 0 && !identifyOn && (
        <div className="activity-footnote">
          Advertisers show as the domain or network behind each ad — the
          paying company's name isn't in what the page reveals. To resolve who
          paid (e.g. the agency or brand of record), enable{" "}
          <strong>Advertiser identity lookup</strong> in Integrations; it
          fetches Google's public ad-transparency page per ad and is the only
          part of Ads that touches the network.
          {onOpenIntegrations && (
            <>
              {" "}
              <button className="btn-link" onClick={onOpenIntegrations}>
                Open Integrations
              </button>
            </>
          )}
        </div>
      )}

      <div className="activity-footnote">
        Observed by the browser extension's opt-in page observer — viewability
        is the MRC display bar (≥50% visible for ≥1s). Nothing is blocked and
        no page content is captured. Raw records:{" "}
        <code>~/Documents/Trove/browser/ads/</code> — one JSONL file per day.
      </div>
    </div>
  );
}

/** Ranked aggregate bars. Fill metric adapts: viewed time when any accrued
 *  (the API ranks by it), else plain counts. */
function UsageBars({ title, rows }: { title: string; rows: AdUsage[] }) {
  const byTime = rows.some((r) => r.viewed_secs > 0);
  const max = Math.max(...rows.map((r) => (byTime ? r.viewed_secs : r.count)), 1);
  return (
    <div className="app-bars">
      <div className="activity-section-title">{title}</div>
      {rows.slice(0, 12).map((r) => (
        <div
          key={r.name}
          className="app-bar"
          title={`${r.name} — ${r.count} ads, ${r.viewable} viewable${
            r.viewed_secs > 0 ? `, ${fmtDuration(r.viewed_secs)} on screen` : ""
          }`}
        >
          <div className="app-bar-name">{r.name}</div>
          <div className="app-bar-track">
            <div
              className="app-bar-fill"
              style={{
                width: `${((byTime ? r.viewed_secs : r.count) / max) * 100}%`,
              }}
            />
          </div>
          <div className="app-bar-time">
            {byTime && r.viewed_secs > 0
              ? fmtDuration(r.viewed_secs)
              : String(r.count)}
          </div>
        </div>
      ))}
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

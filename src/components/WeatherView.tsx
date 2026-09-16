import { ReactNode, useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  WeatherDay,
  WeatherLocation,
  WeatherObservation,
  WeatherSyncState,
} from "../api";
import WeatherChart from "./WeatherChart";

type Range = "7d" | "30d" | "90d";

const RANGES: { id: Range; label: string; days: number }[] = [
  { id: "7d", label: "7 Days", days: 7 },
  { id: "30d", label: "30 Days", days: 30 },
  { id: "90d", label: "90 Days", days: 90 },
];

// Observations land hourly; poll well under that so a fresh one shows soon.
const REFRESH_MS = 60000;

// Temperature series colors: warm accent for highs, a cool counterpart for
// lows. The blue is weather-semantic, used nowhere else in the app.
const WARM = "#d4a847";
const COOL = "#6f9bd1";

/** YYYY-MM-DD in local time (observations are stored in local time). */
function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function fmtAgo(rfc3339: string): string {
  const mins = Math.round((Date.now() - new Date(rfc3339).getTime()) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const h = Math.floor(mins / 60);
  return h < 48 ? `${h}h ago` : `${Math.floor(h / 24)}d ago`;
}

function fmtHour(rfc3339: string): string {
  return new Date(rfc3339).toLocaleTimeString([], { hour: "numeric" });
}

/** WMO weather interpretation codes → glyph + label. */
function describeCode(code: number, isDay: boolean): [string, string] {
  if (code === 0) return [isDay ? "☀" : "☾", "Clear"];
  if (code <= 2) return [isDay ? "⛅" : "☁", "Partly cloudy"];
  if (code === 3) return ["☁", "Overcast"];
  if (code <= 48) return ["≋", "Fog"];
  if (code <= 57) return ["☂", "Drizzle"];
  if (code <= 67) return ["☔", "Rain"];
  if (code <= 77) return ["❄", "Snow"];
  if (code <= 82) return ["☔", "Showers"];
  if (code <= 86) return ["❄", "Snow showers"];
  return ["⚡", "Thunderstorm"];
}

/** Atmosphere wash for the hero card, keyed to conditions. */
function heroGlow(code: number, isDay: boolean): string {
  if (code <= 1) {
    return isDay ? "rgba(212, 168, 71, 0.18)" : "rgba(122, 150, 224, 0.14)";
  }
  if (code <= 3) {
    return isDay ? "rgba(180, 170, 140, 0.12)" : "rgba(122, 150, 224, 0.10)";
  }
  if (code <= 48) return "rgba(154, 157, 166, 0.14)";
  if (code <= 67 || (code >= 80 && code <= 82)) return "rgba(111, 155, 209, 0.16)";
  if (code <= 86) return "rgba(190, 210, 230, 0.14)";
  return "rgba(176, 127, 212, 0.16)";
}

const COMPASS = [
  "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
  "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW",
];

/** Direction the wind blows FROM, as a compass point. */
function compassDir(deg: number): string {
  return COMPASS[Math.round(((deg % 360) + 360) % 360 / 22.5) % 16];
}

function uvCategory(uv: number): { label: string; color: string } {
  if (uv < 3) return { label: "Low", color: "#7fc97f" };
  if (uv < 6) return { label: "Moderate", color: "#d4a847" };
  if (uv < 8) return { label: "High", color: "#d4824a" };
  if (uv < 11) return { label: "Very high", color: "#e0635c" };
  return { label: "Extreme", color: "#b07fd4" };
}

/** Pressure tendency from today's observations: latest vs ~3 h earlier. */
function pressureTrend(today: WeatherObservation[]): string | null {
  if (today.length < 2) return null;
  const latest = today[today.length - 1];
  const latestMs = new Date(latest.ts).getTime();
  const earlier = [...today]
    .reverse()
    .find((o) => latestMs - new Date(o.ts).getTime() >= 3 * 3600_000);
  if (!earlier) return null;
  const delta = latest.pressure_hpa - earlier.pressure_hpa;
  if (delta > 1) return "rising ↗";
  if (delta < -1) return "falling ↘";
  return "steady →";
}

function fmtPlace(lat: number, lon: number, place: string): string {
  return place || `${lat.toFixed(2)}, ${lon.toFixed(2)}`;
}

function precipSplit(o: WeatherObservation): string {
  const parts: string[] = [];
  if (o.rain_mm > 0) parts.push(`rain ${o.rain_mm.toFixed(1)} mm`);
  if (o.showers_mm > 0) parts.push(`showers ${o.showers_mm.toFixed(1)} mm`);
  if (o.snowfall_cm > 0) parts.push(`snow ${o.snowfall_cm.toFixed(1)} cm`);
  return parts.join(" · ") || "past hour";
}

export default function WeatherView() {
  const [range, setRange] = useState<Range>("7d");
  const [latest, setLatest] = useState<WeatherObservation | null>(null);
  const [today, setToday] = useState<WeatherObservation[]>([]);
  const [daily, setDaily] = useState<WeatherDay[]>([]);
  const [sync, setSync] = useState<WeatherSyncState | null>(null);
  const [manual, setManual] = useState<WeatherLocation | null>(null);
  const [permission, setPermission] = useState<string>("granted");
  const [showForm, setShowForm] = useState(false);
  const [formPlace, setFormPlace] = useState("");
  const [formLat, setFormLat] = useState("");
  const [formLon, setFormLon] = useState("");
  const [formError, setFormError] = useState<string | null>(null);
  const stripRef = useRef<HTMLDivElement>(null);

  const refresh = useCallback(async (r: Range) => {
    const now = new Date();
    const to = localDate(now);
    const days = RANGES.find((x) => x.id === r)!.days;
    const fromDate = new Date(now);
    fromDate.setDate(fromDate.getDate() - (days - 1));

    const [l, t, d, s, loc, perm] = await Promise.all([
      api.weatherLatest(),
      api.weatherTimeline(to),
      api.weatherDaily(localDate(fromDate), to),
      api.weatherSyncInfo(),
      api.weatherLocation(),
      api.weatherPermission(),
    ]);
    setLatest(l);
    setToday(t);
    setDaily(d);
    setSync(s);
    setManual(loc);
    setPermission(perm);
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

  // The strip reads morning → now; land scrolled to the newest hour.
  useEffect(() => {
    const strip = stripRef.current;
    if (strip) strip.scrollLeft = strip.scrollWidth;
  }, [today]);

  const requestLocation = async () => {
    try {
      await api.requestWeatherPermission();
    } finally {
      refresh(range).catch(() => {});
    }
  };

  const saveManual = async () => {
    const lat = parseFloat(formLat);
    const lon = parseFloat(formLon);
    if (!isFinite(lat) || lat < -90 || lat > 90 || !isFinite(lon) || lon < -180 || lon > 180) {
      setFormError("Latitude must be -90…90 and longitude -180…180.");
      return;
    }
    setFormError(null);
    await api.setWeatherLocation({ lat, lon, place: formPlace.trim() });
    setShowForm(false);
    refresh(range).catch(() => {});
  };

  const clearManual = async () => {
    await api.setWeatherLocation(null);
    refresh(range).catch(() => {});
  };

  // The collector is inert until either source exists — that's the moment
  // to show setup, not an error state.
  const needsLocation = permission !== "granted" && !manual;

  const dates = daily.map((d) => d.date);
  const hasPrecip = daily.some((d) => d.precip_mm > 0);
  const todayStats = daily.find((d) => d.date === localDate(new Date()));

  // Day-list range bars share one temperature scale across the visible days.
  const scaleMin = Math.min(...daily.map((d) => d.temp_min));
  const scaleMax = Math.max(...daily.map((d) => d.temp_max));
  const scaleSpan = Math.max(scaleMax - scaleMin, 1);

  const [glyph, condition] = latest
    ? describeCode(latest.weather_code, latest.is_day)
    : ["", ""];
  const trend = pressureTrend(today);

  return (
    <div className="activity-view">
      <div className="activity-header">
        <div>
          <h2>Weather</h2>
          <div className="activity-sub">
            {latest ? (
              <>
                {fmtPlace(latest.lat, latest.lon, latest.place ?? "")} · observed{" "}
                {fmtAgo(latest.ts)}
              </>
            ) : (
              <>Waiting for the first observation…</>
            )}
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

      {needsLocation && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Where are you?</strong> Weather needs a location — allow
            Location Services, or set one by hand. Either choice is the
            opt-in: until then nothing is sent anywhere. Only coordinates
            rounded to about 1 km ever leave this Mac.
          </div>
          <div className="perm-actions">
            <button className="btn-primary" onClick={requestLocation}>
              Use My Location
            </button>
            <button className="btn-ghost" onClick={() => setShowForm((v) => !v)}>
              Set Manually
            </button>
          </div>
        </div>
      )}

      {/* A "no location" error is stale once a manual location exists — the
          next pass is guaranteed to pick it up, so don't alarm meanwhile. */}
      {!needsLocation && sync?.error && !(manual && sync.error.startsWith("no location")) && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Last weather pass failed.</strong> {sync.error}
          </div>
        </div>
      )}

      {showForm && (
        <div className="perm-banner">
          <div className="perm-text">
            <strong>Manual location.</strong> Find your coordinates on any map
            app — two decimals is plenty.
            <div className="weather-form">
              <input
                placeholder="Place label (optional)"
                value={formPlace}
                onChange={(e) => setFormPlace(e.target.value)}
              />
              <input
                placeholder="Latitude (e.g. 34.05)"
                value={formLat}
                onChange={(e) => setFormLat(e.target.value)}
              />
              <input
                placeholder="Longitude (e.g. -118.24)"
                value={formLon}
                onChange={(e) => setFormLon(e.target.value)}
              />
            </div>
            {formError && <div className="weather-form-error">{formError}</div>}
          </div>
          <div className="perm-actions">
            <button className="btn-primary" onClick={saveManual}>
              Save
            </button>
            <button className="btn-ghost" onClick={() => setShowForm(false)}>
              Cancel
            </button>
          </div>
        </div>
      )}

      {latest && (
        <div
          className="weather-hero"
          style={{
            backgroundImage: `radial-gradient(110% 180% at 82% -30%, ${heroGlow(
              latest.weather_code,
              latest.is_day,
            )}, transparent 60%)`,
          }}
        >
          <div className="weather-hero-main">
            <div className="weather-hero-temp">
              {latest.temp_c.toFixed(1)}°
            </div>
            <div className="weather-hero-cond">
              {condition} · feels like {latest.apparent_c.toFixed(1)}°
            </div>
            {todayStats && (
              <div className="weather-hero-range">
                <span style={{ color: WARM }}>
                  H {todayStats.temp_max.toFixed(0)}°
                </span>
                <span style={{ color: COOL }}>
                  L {todayStats.temp_min.toFixed(0)}°
                </span>
                {todayStats.precip_mm > 0 && (
                  <span>{todayStats.precip_mm.toFixed(1)} mm today</span>
                )}
              </div>
            )}
          </div>
          <div className="weather-hero-glyph" aria-hidden>
            {glyph}
          </div>
        </div>
      )}

      {latest && (
        <div className="weather-tiles">
          <Tile
            label="Humidity"
            value={`${Math.round(latest.humidity_pct)}%`}
            sub={`dew point ${latest.dew_point_c.toFixed(1)}°`}
          />
          <Tile
            label="Wind"
            value={`${Math.round(latest.wind_kmh)} km/h`}
            sub={
              <>
                <span
                  className="weather-wind-arrow"
                  style={{
                    transform: `rotate(${(latest.wind_dir_deg + 180) % 360}deg)`,
                  }}
                  aria-hidden
                >
                  ↑
                </span>{" "}
                {compassDir(latest.wind_dir_deg)} · gusts{" "}
                {Math.round(latest.wind_gusts_kmh)}
              </>
            }
          />
          <Tile
            label="Pressure"
            value={`${Math.round(latest.pressure_hpa)} hPa`}
            sub={trend ?? "sea level"}
          />
          <Tile
            label="Cloud cover"
            value={`${Math.round(latest.cloud_pct)}%`}
            sub={latest.is_day ? "daytime" : "nighttime"}
          />
          {latest.uv_index != null && (
            <Tile
              label="UV index"
              value={latest.uv_index.toFixed(1)}
              sub={
                <>
                  <span
                    className="weather-uv-dot"
                    style={{ background: uvCategory(latest.uv_index).color }}
                  />{" "}
                  {uvCategory(latest.uv_index).label}
                </>
              }
            />
          )}
          {latest.precip_mm > 0 && (
            <Tile
              label="Precipitation"
              value={`${latest.precip_mm.toFixed(1)} mm`}
              sub={precipSplit(latest)}
            />
          )}
        </div>
      )}

      {!latest && !needsLocation && (
        <div className="activity-empty">
          No observations yet. Trove records conditions once an hour while
          it is open — the first one should land within 15 minutes.
        </div>
      )}

      {today.length > 0 && (
        <div className="activity-timeline">
          <div className="activity-section-title">Today, hour by hour</div>
          <div className="weather-strip" ref={stripRef}>
            {today.map((o, i) => {
              const [g] = describeCode(o.weather_code, o.is_day);
              const now = i === today.length - 1;
              return (
                <div
                  key={o.ts}
                  className={`weather-hour ${now ? "now" : ""}`}
                  title={`${describeCode(o.weather_code, o.is_day)[1]} · ${Math.round(o.humidity_pct)}% humidity · ${Math.round(o.wind_kmh)} km/h wind`}
                >
                  <div className="weather-hour-time">
                    {now ? "Now" : fmtHour(o.ts)}
                  </div>
                  <div className="weather-hour-glyph">{g}</div>
                  <div className="weather-hour-temp">
                    {Math.round(o.temp_c)}°
                  </div>
                  <div className="weather-hour-precip">
                    {o.precip_mm > 0 ? `${o.precip_mm.toFixed(1)}` : " "}
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      )}

      {daily.length > 1 && (
        <div className="activity-trend">
          <div className="activity-section-title">Temperature °C</div>
          <WeatherChart
            dates={dates}
            unit="°C"
            band
            series={[
              { label: "High", color: WARM, values: daily.map((d) => d.temp_max) },
              { label: "Low", color: COOL, values: daily.map((d) => d.temp_min) },
            ]}
          />
          {hasPrecip && (
            <>
              <div className="activity-section-title weather-chart-gap">
                Precipitation mm
              </div>
              <WeatherChart
                dates={dates}
                unit="mm"
                height={200}
                series={[
                  {
                    label: "Precipitation",
                    color: COOL,
                    values: daily.map((d) => d.precip_mm),
                    bars: true,
                  },
                ]}
              />
            </>
          )}
        </div>
      )}

      {daily.length > 1 && daily.length <= 31 && (
        <div className="activity-timeline">
          <div className="activity-section-title">Day by day</div>
          <div className="weather-days">
            {[...daily].reverse().map((d) => (
              <DayRow
                key={d.date}
                day={d}
                scaleMin={scaleMin}
                scaleSpan={scaleSpan}
              />
            ))}
          </div>
        </div>
      )}

      <div className="activity-footnote">
        Conditions come from the Open-Meteo public API — no account, no key,
        no tracking; only your rounded coordinates are sent. Raw records:{" "}
        <code>~/Documents/Trove/weather/</code> — one JSONL file per month.
        {manual && (
          <>
            {" "}
            Manual location: {fmtPlace(manual.lat, manual.lon, manual.place)}{" "}
            <button className="btn-link" onClick={clearManual}>
              clear
            </button>
            .
          </>
        )}
        {!manual && !needsLocation && (
          <>
            {" "}
            <button className="btn-link" onClick={() => setShowForm(true)}>
              Set a manual location
            </button>{" "}
            to override Location Services.
          </>
        )}
      </div>
    </div>
  );
}

function Tile({
  label,
  value,
  sub,
}: {
  label: string;
  value: string;
  sub: ReactNode;
}) {
  return (
    <div className="weather-tile">
      <div className="weather-tile-label">{label}</div>
      <div className="weather-tile-value">{value}</div>
      <div className="weather-tile-sub">{sub}</div>
    </div>
  );
}

function DayRow({
  day,
  scaleMin,
  scaleSpan,
}: {
  day: WeatherDay;
  scaleMin: number;
  scaleSpan: number;
}) {
  const isToday = day.date === localDate(new Date());
  const dateLabel = isToday
    ? "Today"
    : new Date(`${day.date}T00:00:00`).toLocaleDateString([], {
        weekday: "short",
        month: "numeric",
        day: "numeric",
      });
  const left = ((day.temp_min - scaleMin) / scaleSpan) * 100;
  const width = Math.max(((day.temp_max - day.temp_min) / scaleSpan) * 100, 3);
  return (
    <div className="weather-day-row">
      <div className="weather-day-date">{dateLabel}</div>
      <div className="weather-day-low">{day.temp_min.toFixed(0)}°</div>
      <div className="weather-day-track">
        <div
          className="weather-day-fill"
          style={{ left: `${left}%`, width: `${width}%` }}
        />
      </div>
      <div className="weather-day-high">{day.temp_max.toFixed(0)}°</div>
      <div className="weather-day-extra">
        {day.precip_mm > 0 && (
          <span className="weather-day-precip">
            {day.precip_mm.toFixed(1)} mm
          </span>
        )}
        {day.observations < 20 && (
          <span className="weather-day-cov">{day.observations}h</span>
        )}
      </div>
    </div>
  );
}

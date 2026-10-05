// Shared vocabulary of the Health views (both read-side shapes): source
// labels and colours, date helpers, range math, and the palette generic
// series draw from.

import type { Bucket, HealthSource, SleepSession } from "../api";

export const SOURCE_META: Record<HealthSource, { label: string; color: string }> = {
  "apple-health": { label: "Apple Health", color: "#d4a847" },
  oura: { label: "Oura", color: "#6f9fd8" },
};

/** Any `source` string → label/colour, falling back to the string itself. */
export function sourceMeta(source: string): { label: string; color: string } {
  return (
    SOURCE_META[source as HealthSource] ?? {
      label: titleCase(source),
      color: PALETTE[hash(source) % PALETTE.length],
    }
  );
}

/** Categorical series colours (generic charts, boards). */
export const PALETTE = ["#6f9fd8", "#d4a847", "#7fc97f", "#b07fd4", "#e0635c", "#5fbfbf"];
export const GAP_COLOR = "#e0635c";

function hash(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
  return h;
}

export function fmtHours(h: number): string {
  const mins = Math.round(h * 60);
  return `${Math.floor(mins / 60)}h ${String(mins % 60).padStart(2, "0")}m`;
}

export function fmtDay(day: string): string {
  return new Date(`${day}T00:00:00`).toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
}

export function fmtTime(ts: string): string {
  return new Date(ts).toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
}

/** "running" / "late_nap" → "Running" / "Late nap". */
export function titleCase(s: string): string {
  const clean = s.replace(/[_-]/g, " ");
  return clean.charAt(0).toUpperCase() + clean.slice(1);
}

export function todayKey(): string {
  return isoDay(new Date());
}

export function isoDay(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

export function addDays(day: string, n: number): string {
  const d = new Date(`${day}T00:00:00`);
  d.setDate(d.getDate() + n);
  return isoDay(d);
}

/** `days` back from `to` (default today), inclusive on both ends. */
export function rangeFor(days: number, to?: string | null): { from: string; to: string } {
  const end = to && to.length === 10 ? to : todayKey();
  return { from: addDays(end, -(Math.max(days, 1) - 1)), to: end };
}

/** Start of the bucket a day falls in — Monday for weeks, the 1st for months. */
export function bucketStart(day: string, bucket: Bucket): string {
  if (bucket === "day") return day;
  const d = new Date(`${day}T00:00:00`);
  if (bucket === "week") {
    d.setDate(d.getDate() - ((d.getDay() + 6) % 7));
  } else {
    d.setDate(1);
  }
  return isoDay(d);
}

/** Every bucket start from `from` to `to`, so a chart's x axis has a slot
 *  for days without data (which then read as gaps, not as compression). */
export function bucketKeys(from: string, to: string, bucket: Bucket): string[] {
  const out: string[] = [];
  let cur = bucketStart(from, bucket);
  const last = bucketStart(to, bucket);
  let guard = 0;
  while (cur <= last && guard++ < 20000) {
    out.push(cur);
    const d = new Date(`${cur}T00:00:00`);
    if (bucket === "day") d.setDate(d.getDate() + 1);
    else if (bucket === "week") d.setDate(d.getDate() + 7);
    else d.setMonth(d.getMonth() + 1);
    cur = isoDay(d);
  }
  return out;
}

/** Hours asleep of a session, or null when the source didn't report it. */
export function asleepHours(s: SleepSession): number | null {
  return s.asleep_seconds != null ? s.asleep_seconds / 3600 : null;
}

/** Per-source colour keyed on a relay's origin when it has one. */
export function sessionColor(s: SleepSession): string {
  return s.origin ? sourceMeta(s.origin.toLowerCase().replace(/\s+/g, "-")).color : sourceMeta(s.source).color;
}

export function sessionLabel(s: SleepSession): string {
  const base = sourceMeta(s.source).label;
  return s.origin ? `${s.origin} via ${base}` : base;
}

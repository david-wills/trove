//! The `health-sleep` domain contract: sleep as **sessions** — one record per
//! night, nap, or rest, as one source observed it — under
//! `health/sleep/<source>/YYYY-MM.jsonl` (month of [`Session::day`]).
//!
//! This is the normalized convergence of every sleep tracker, not a superset
//! of them: when a session started and ended, how long was spent asleep, and
//! the stage totals where the source has them. Everything else a source knows
//! about a night rides under [`Session::extra`] (scalars) or stays in the
//! source's raw file (per-sample arrays), joinable by `guid`.
//!
//! Two writers live in this crate, and both are **projections of raw they
//! already hold**, so each regenerates its own month files whole rather than
//! appending: Oura projects `health/oura/sleep.jsonl` ([`oura_session`]) on
//! every sync that touches it; the Apple Health importer stitches
//! `SleepAnalysis` intervals into sessions per origin ([`apple_sessions`]) and
//! rewrites its folder on every import. Apple Health is a *relay* — its rows
//! come from an Apple Watch, the Oura app, AutoSleep, Pillow, … — so the same
//! night can legitimately arrive twice (Oura directly, Oura again through
//! Apple), each marked by [`Session::origin`]. That is by design: writers
//! record what they observed; [`dedupe_relays`] is the read-time opinion
//! (the device beats the relay) a view applies so a night shows once.
//!
//! See `docs/vault-spec/domains/health-sleep.md` for the field-level spec.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::store::Partition;
use crate::vault::Vault;

/// The contract root; sources are its subdirectories. The Apple Health
/// importer's per-stage CSVs also sit under it as *files*, which readers of
/// this contract ignore (they scan directories only).
pub const SLEEP_ROOT: &str = "health/sleep";

/// Apple `SleepAnalysis` intervals from one origin closer together than this
/// form one session. Part of the contract (it fixes guids), not a tunable.
const APPLE_SESSION_GAP_SECS: i64 = 3600;

/// One sleep session — one line of `health/sleep/<source>/YYYY-MM.jsonl`.
/// Matches `health-sleep.session.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Session {
    /// `YYYY-MM-DD` the session belongs to, as the source attributes it
    /// (Oura's `day`, whose sleep day turns over at 18:00; otherwise the
    /// local date of `end`); the partition key's day.
    pub day: String,
    /// RFC3339 local time the session began.
    pub start: String,
    /// RFC3339 local time the session ended.
    pub end: String,
    /// Collector id, identical to the source folder name.
    pub source: String,
    /// Source-unique id: the dedupe key and the join key into the raw file.
    pub guid: String,
    /// The app or device that recorded the session when the writer is a
    /// relay (Apple Health's `sourceName`); empty when the writer is the device.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
    /// `"sleep"` | `"nap"` | `"rest"` — the source's own classification, where
    /// it has one; empty when the source does not classify.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Total time asleep, every stage summed, awake time excluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asleep_seconds: Option<u64>,
    /// Time in bed, `start` to `end`, awake time included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_bed_seconds: Option<u64>,
    /// Deep (slow-wave) sleep, where the source reports stages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_seconds: Option<u64>,
    /// REM sleep, where the source reports stages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rem_seconds: Option<u64>,
    /// Light sleep (Apple's "Core"), where the source reports stages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_seconds: Option<u64>,
    /// Time awake inside the session, where the source reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awake_seconds: Option<u64>,
    /// The source's scalar fields the shape has no column for.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Oura: project one raw `sleep` record

/// Raw `sleep` fields that map onto typed columns (everything else scalar
/// goes to `extra`; arrays and objects stay in the raw file).
const OURA_MAPPED: &[&str] = &[
    "id",
    "day",
    "bedtime_start",
    "bedtime_end",
    "type",
    "total_sleep_duration",
    "time_in_bed",
    "deep_sleep_duration",
    "rem_sleep_duration",
    "light_sleep_duration",
    "awake_time",
];

/// Project one raw Oura API v2 `sleep` record onto the contract. `None` when
/// the record lacks any of the identifying fields.
pub fn oura_session(raw: &Value) -> Option<Session> {
    let obj = raw.as_object()?;
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    let secs = |k: &str| obj.get(k).and_then(Value::as_f64).map(|v| v.max(0.0).round() as u64);
    let kind = match obj.get("type").and_then(Value::as_str) {
        Some("long_sleep") => "sleep",
        Some("sleep") | Some("late_nap") => "nap",
        Some("rest") => "rest",
        _ => "",
    };
    let extra: Map<String, Value> = obj
        .iter()
        .filter(|(k, v)| {
            !OURA_MAPPED.contains(&k.as_str()) && !v.is_null() && !v.is_array() && !v.is_object()
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Some(Session {
        day: text("day")?,
        start: text("bedtime_start")?,
        end: text("bedtime_end")?,
        source: "oura".into(),
        guid: text("id")?,
        origin: String::new(),
        kind: kind.into(),
        asleep_seconds: secs("total_sleep_duration"),
        in_bed_seconds: secs("time_in_bed"),
        deep_seconds: secs("deep_sleep_duration"),
        rem_seconds: secs("rem_sleep_duration"),
        light_seconds: secs("light_sleep_duration"),
        awake_seconds: secs("awake_time"),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Apple Health: stitch `SleepAnalysis` intervals into sessions

/// One `HKCategoryTypeIdentifierSleepAnalysis` record from an export.
#[derive(Debug, Clone)]
pub struct AppleInterval {
    /// Apple's `sourceName` — the app or device that wrote the sample.
    pub origin: String,
    pub start: DateTime<FixedOffset>,
    pub end: DateTime<FixedOffset>,
    /// The stage with its `HKCategoryValueSleepAnalysis` prefix stripped:
    /// `InBed`, `Awake`, `AsleepCore`, `AsleepDeep`, `AsleepREM`,
    /// `AsleepUnspecified` (older exports: `Asleep`).
    pub stage: String,
}

struct Build {
    origin: String,
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    deep: i64,
    rem: i64,
    light: i64,
    awake: i64,
    unspecified: i64,
}

impl Build {
    fn new(iv: AppleInterval) -> Self {
        let mut b = Build {
            origin: iv.origin.clone(),
            start: iv.start,
            end: iv.start,
            deep: 0,
            rem: 0,
            light: 0,
            awake: 0,
            unspecified: 0,
        };
        b.add(iv);
        b
    }

    fn add(&mut self, iv: AppleInterval) {
        self.end = self.end.max(iv.end);
        let secs = (iv.end - iv.start).num_seconds().max(0);
        match iv.stage.as_str() {
            "AsleepDeep" => self.deep += secs,
            "AsleepREM" => self.rem += secs,
            "AsleepCore" => self.light += secs,
            "Awake" => self.awake += secs,
            s if s.starts_with("Asleep") => self.unspecified += secs,
            // InBed (and anything unknown) only extends the span.
            _ => {}
        }
    }

    fn finish(self) -> Session {
        let in_bed = (self.end - self.start).num_seconds().max(0);
        let asleep = (self.deep + self.rem + self.light + self.unspecified).min(in_bed);
        let staged = self.deep + self.rem + self.light > 0;
        let some = |v: i64| (v > 0).then_some(v as u64);
        Session {
            day: self.end.format("%Y-%m-%d").to_string(),
            start: self.start.to_rfc3339(),
            end: self.end.to_rfc3339(),
            source: "apple-health".into(),
            guid: format!("{}:{}", origin_slug(&self.origin), self.start.to_rfc3339()),
            origin: self.origin,
            kind: String::new(),
            asleep_seconds: Some(asleep as u64),
            in_bed_seconds: Some(in_bed as u64),
            deep_seconds: if staged { Some(self.deep as u64) } else { None },
            rem_seconds: if staged { Some(self.rem as u64) } else { None },
            light_seconds: if staged { Some(self.light as u64) } else { None },
            awake_seconds: some(self.awake),
            extra: Map::new(),
        }
    }
}

/// Stitch Apple intervals into sessions: per origin, intervals less than an
/// hour apart form one session; stage durations are summed inside it.
/// Sorted by `start`. Deterministic for a given export, so guids are stable
/// across re-imports.
pub fn apple_sessions(mut intervals: Vec<AppleInterval>) -> Vec<Session> {
    intervals.sort_by(|a, b| a.origin.cmp(&b.origin).then(a.start.cmp(&b.start)));
    let gap = Duration::seconds(APPLE_SESSION_GAP_SECS);
    let mut out = Vec::new();
    let mut cur: Option<Build> = None;
    for iv in intervals {
        match cur.as_mut() {
            Some(b) if b.origin == iv.origin && iv.start <= b.end + gap => b.add(iv),
            _ => {
                if let Some(b) = cur.take() {
                    out.push(b.finish());
                }
                cur = Some(Build::new(iv));
            }
        }
    }
    if let Some(b) = cur {
        out.push(b.finish());
    }
    out.sort_by(|a, b| a.start.cmp(&b.start).then(a.guid.cmp(&b.guid)));
    out
}

/// `"David’s Apple Watch"` → `david-s-apple-watch`: lowercase ASCII
/// alphanumerics, every other run collapsed to one dash.
pub fn origin_slug(origin: &str) -> String {
    let mut out = String::with_capacity(origin.len());
    let mut dash = false;
    for c in origin.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Read-time opinion

/// The device beats the relay: drop relay rows (non-empty `origin`) whose
/// origin slug names a source that writes its own folder in `sessions`
/// (Apple Health's `"Oura"` rows when `oura/` is present). Everything else —
/// including two different origins on one night — is kept.
pub fn dedupe_relays(sessions: Vec<Session>) -> Vec<Session> {
    let direct: BTreeSet<String> = sessions
        .iter()
        .filter(|s| s.origin.is_empty())
        .map(|s| s.source.clone())
        .collect();
    sessions
        .into_iter()
        .filter(|s| s.origin.is_empty() || !direct.contains(&origin_slug(&s.origin)))
        .collect()
}

// ---------------------------------------------------------------------------
// Vault I/O

impl Vault {
    /// Rewrite one source's month files from `sessions` (atomic per file).
    /// With `replace_all`, month files the new set does not cover are removed
    /// too — a full re-import; otherwise only the months present are touched.
    /// Sessions are deduped by `guid` (last wins) and sorted by `start`.
    pub fn write_sleep_sessions(&self, source: &str, sessions: &[Session], replace_all: bool) -> Result<usize> {
        let dir = format!("{SLEEP_ROOT}/{source}");
        let mut by_month: BTreeMap<String, BTreeMap<String, &Session>> = BTreeMap::new();
        for s in sessions {
            let key = Partition::Month
                .key(&s.day)
                .with_context(|| format!("sleep session {}: day {:?} is not YYYY-MM-DD", s.guid, s.day))?;
            by_month.entry(key.to_string()).or_default().insert(s.guid.clone(), s);
        }
        if replace_all {
            for stale in self.stream(&dir, Partition::Month).partitions()? {
                if !by_month.contains_key(&stale) {
                    fs::remove_file(self.resolve(&format!("{dir}/{stale}.jsonl"))?)?;
                }
            }
        }
        let mut written = 0;
        for (month, recs) in by_month {
            let mut recs: Vec<&Session> = recs.into_values().collect();
            recs.sort_by(|a, b| a.start.cmp(&b.start).then(a.guid.cmp(&b.guid)));
            written += recs.len();
            self.write_snapshot(&format!("{dir}/{month}.jsonl"), &recs)?;
        }
        Ok(written)
    }

    /// Project every raw Oura `sleep` record onto the contract and rewrite
    /// `health/sleep/oura/` whole. Cheap (hundreds of records a year) and
    /// idempotent; the sync calls it whenever the raw file changes.
    pub fn rebuild_oura_sleep_sessions(&self) -> Result<usize> {
        let raw: Vec<Value> = self.read_snapshot("health/oura/sleep.jsonl")?;
        let sessions: Vec<Session> = raw.iter().filter_map(oura_session).collect();
        self.write_sleep_sessions("oura", &sessions, true)
    }

    /// Every source's sessions with `day` in `from..=to` (`YYYY-MM-DD`),
    /// sorted by `start`. Reads only the month partitions in range.
    pub fn sleep_sessions(&self, from: &str, to: &str) -> Result<Vec<Session>> {
        let root = self.resolve(SLEEP_ROOT)?;
        let Ok(entries) = fs::read_dir(&root) else {
            return Ok(Vec::new());
        };
        let (from_m, to_m) = (from.get(..7).unwrap_or(from), to.get(..7).unwrap_or(to));
        let mut out = Vec::new();
        let mut sources: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        sources.sort();
        for source in sources {
            let stream = self.stream(&format!("{SLEEP_ROOT}/{source}"), Partition::Month);
            for key in stream.partitions()? {
                if key.as_str() < from_m || key.as_str() > to_m {
                    continue;
                }
                out.extend(
                    stream
                        .read::<Session>(&key)?
                        .into_iter()
                        .filter(|s| s.day.as_str() >= from && s.day.as_str() <= to),
                );
            }
        }
        out.sort_by(|a, b| a.start.cmp(&b.start).then(a.guid.cmp(&b.guid)));
        Ok(out)
    }
}

impl Vault {
    /// [`Vault::sleep_sessions`] for a view: relay rows hidden when the
    /// device writes its own folder (`dedupe`), and `extra` trimmed to
    /// scalars a row can show (numbers, booleans, short strings) — Oura's
    /// per-30-second phase strings stay in the file, joinable by `guid`.
    pub fn sleep_sessions_view(&self, from: &str, to: &str, dedupe: bool) -> Result<Vec<Session>> {
        let mut sessions = self.sleep_sessions(from, to)?;
        if dedupe {
            sessions = dedupe_relays(sessions);
        }
        for s in &mut sessions {
            s.extra.retain(|_, v| match v {
                Value::Number(_) | Value::Bool(_) => true,
                Value::String(t) => t.len() <= 40,
                _ => false,
            });
        }
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    fn iv(origin: &str, start: &str, end: &str, stage: &str) -> AppleInterval {
        AppleInterval { origin: origin.into(), start: t(start), end: t(end), stage: stage.into() }
    }

    #[test]
    fn oura_record_projects_scalars_and_maps_kind() {
        let raw = json!({
            "id": "abc", "day": "2026-08-19", "type": "long_sleep",
            "bedtime_start": "2026-08-18T22:56:30.000-07:00", "bedtime_end": "2026-08-19T06:52:07.000-07:00",
            "total_sleep_duration": 26640, "time_in_bed": 28537, "deep_sleep_duration": 4680,
            "rem_sleep_duration": 6870, "light_sleep_duration": 15090, "awake_time": 1897,
            "efficiency": 93, "average_hrv": 57, "ring_id": null, "low_battery_alert": false,
            "heart_rate": {"items": [1, 2]}, "sleep_phase_5_min": "4433"
        });
        let s = oura_session(&raw).unwrap();
        assert_eq!(s.kind, "sleep");
        assert_eq!(s.guid, "abc");
        assert_eq!(s.asleep_seconds, Some(26640));
        assert_eq!(s.light_seconds, Some(15090));
        assert_eq!(s.extra["efficiency"], 93);
        assert_eq!(s.extra["low_battery_alert"], false);
        assert!(s.extra.get("ring_id").is_none(), "nulls dropped");
        assert!(s.extra.get("heart_rate").is_none(), "objects stay in raw");
        assert_eq!(s.extra["sleep_phase_5_min"], "4433", "a scalar string is a scalar");
        assert_eq!(oura_session(&json!({"id": "x", "type": "sleep"})), None, "no day/bedtimes");
        assert_eq!(oura_session(&json!({"id":"n","day":"2026-01-01","type":"late_nap","bedtime_start":"a","bedtime_end":"b"})).unwrap().kind, "nap");
    }

    #[test]
    fn apple_intervals_group_per_origin_within_an_hour() {
        let sessions = apple_sessions(vec![
            iv("Apple Watch", "2024-01-15T23:00:00-08:00", "2024-01-16T07:00:00-08:00", "InBed"),
            iv("Apple Watch", "2024-01-15T23:00:00-08:00", "2024-01-16T01:00:00-08:00", "AsleepCore"),
            iv("Apple Watch", "2024-01-16T01:00:00-08:00", "2024-01-16T01:10:00-08:00", "Awake"),
            iv("Apple Watch", "2024-01-16T01:10:00-08:00", "2024-01-16T02:40:00-08:00", "AsleepREM"),
            iv("Apple Watch", "2024-01-16T02:40:00-08:00", "2024-01-16T03:40:00-08:00", "AsleepDeep"),
            // Same origin, next night: a new session.
            iv("Apple Watch", "2024-01-16T23:30:00-08:00", "2024-01-17T06:30:00-08:00", "AsleepUnspecified"),
            // A different origin on the first night: its own session.
            iv("Oura", "2024-01-15T23:05:00-08:00", "2024-01-16T06:58:00-08:00", "InBed"),
            iv("Oura", "2024-01-15T23:05:00-08:00", "2024-01-16T06:58:00-08:00", "AsleepUnspecified"),
        ]);
        assert_eq!(sessions.len(), 3);
        let watch = &sessions[0];
        assert_eq!(watch.origin, "Apple Watch");
        assert_eq!(watch.guid, "apple-watch:2024-01-15T23:00:00-08:00");
        assert_eq!(watch.day, "2024-01-16");
        assert_eq!(watch.in_bed_seconds, Some(8 * 3600));
        assert_eq!(watch.light_seconds, Some(2 * 3600));
        assert_eq!(watch.rem_seconds, Some(90 * 60));
        assert_eq!(watch.deep_seconds, Some(3600));
        assert_eq!(watch.awake_seconds, Some(600));
        assert_eq!(watch.asleep_seconds, Some(2 * 3600 + 90 * 60 + 3600));
        let oura = &sessions[1];
        assert_eq!(oura.origin, "Oura");
        assert_eq!(oura.deep_seconds, None, "no stages reported → no stage columns");
        assert_eq!(oura.asleep_seconds, oura.in_bed_seconds, "unspecified asleep spans the session");
        let next = &sessions[2];
        assert_eq!(next.day, "2024-01-17");
        assert_eq!(next.kind, "", "Apple does not classify; never inferred");
    }

    #[test]
    fn origin_slugs_and_relay_dedupe() {
        assert_eq!(origin_slug("David’s Apple Watch"), "david-s-apple-watch");
        assert_eq!(origin_slug("Oura"), "oura");
        assert_eq!(origin_slug("  Pillow!"), "pillow");
        let mk = |source: &str, origin: &str, guid: &str| Session {
            day: "2026-06-10".into(),
            start: "2026-06-09T23:00:00-07:00".into(),
            end: "2026-06-10T07:00:00-07:00".into(),
            source: source.into(),
            guid: guid.into(),
            origin: origin.into(),
            ..Default::default()
        };
        let kept = dedupe_relays(vec![
            mk("oura", "", "direct"),
            mk("apple-health", "Oura", "relay-oura"),
            mk("apple-health", "Apple Watch", "relay-watch"),
        ]);
        let guids: Vec<&str> = kept.iter().map(|s| s.guid.as_str()).collect();
        assert_eq!(guids, ["direct", "relay-watch"]);
        // Without a direct Oura folder, the relay row is the only Oura observation and stays.
        let kept = dedupe_relays(vec![mk("apple-health", "Oura", "relay-oura")]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn writes_months_atomically_and_reads_in_range() {
        let dir = std::env::temp_dir().join(format!("trove-sleep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir.clone()).unwrap();
        let mk = |day: &str, guid: &str| Session {
            day: day.into(),
            start: format!("{day}T00:30:00-07:00"),
            end: format!("{day}T07:00:00-07:00"),
            source: "oura".into(),
            guid: guid.into(),
            ..Default::default()
        };
        v.write_sleep_sessions("oura", &[mk("2026-05-30", "a"), mk("2026-06-01", "b"), mk("2026-06-01", "b")], true)
            .unwrap();
        assert!(dir.join("health/sleep/oura/2026-05.jsonl").exists());
        assert_eq!(
            fs::read_to_string(dir.join("health/sleep/oura/2026-06.jsonl")).unwrap().lines().count(),
            1,
            "duplicate guid collapses"
        );
        // A partial rewrite leaves other months alone; a full one removes them.
        v.write_sleep_sessions("oura", &[mk("2026-06-02", "c")], false).unwrap();
        assert!(dir.join("health/sleep/oura/2026-05.jsonl").exists());
        v.write_sleep_sessions("oura", &[mk("2026-06-02", "c")], true).unwrap();
        assert!(!dir.join("health/sleep/oura/2026-05.jsonl").exists());
        // Apple's CSVs at the root are not a source.
        fs::write(dir.join("health/sleep/2026-06.csv"), "start,end,value,unit,source\n").unwrap();
        v.write_sleep_sessions("apple-health", &[mk("2026-06-02", "w")], true).unwrap();
        let all = v.sleep_sessions("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(all.len(), 2);
        assert!(v.sleep_sessions("2026-07-01", "2026-07-31").unwrap().is_empty());
        let bad = Session { day: "nope".into(), guid: "z".into(), ..Default::default() };
        assert!(v.write_sleep_sessions("oura", &[bad], false).is_err());
    }

    #[test]
    fn rebuilds_oura_projection_from_raw() {
        let dir = std::env::temp_dir().join(format!("trove-sleep-raw-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir.clone()).unwrap();
        assert_eq!(v.rebuild_oura_sleep_sessions().unwrap(), 0, "no raw file → nothing");
        let raw = vec![
            json!({"id":"1","day":"2026-03-02","type":"long_sleep","bedtime_start":"2026-03-01T23:00:00-08:00","bedtime_end":"2026-03-02T07:00:00-08:00","total_sleep_duration":27000}),
            json!({"id":"2","day":"2026-04-02","type":"rest","bedtime_start":"2026-04-02T14:00:00-07:00","bedtime_end":"2026-04-02T14:20:00-07:00"}),
            json!({"not":"a sleep"}),
        ];
        v.write_snapshot("health/oura/sleep.jsonl", &raw).unwrap();
        assert_eq!(v.rebuild_oura_sleep_sessions().unwrap(), 2);
        let s = v.sleep_sessions("2026-03-01", "2026-04-30").unwrap();
        assert_eq!(s.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>(), ["sleep", "rest"]);
    }
}

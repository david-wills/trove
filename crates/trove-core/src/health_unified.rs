//! Read-side views over all health sources — the unified Health tab.
//!
//! Apple Health lives in `health/<metric>/` (CSV, written by [`crate::health`])
//! and Oura lives in `health/oura/` (JSONL, written by [`crate::oura`]). This
//! module joins them at read time: a canonical metric catalog where the same
//! measurement from both sources lands under one slug, served as *separate
//! per-source series* — no merging, no truth-picking.
//!
//! Apple's catalog and series read straight off `.trove/health-summary.json`
//! and `daily.csv`, written at import time by [`crate::health`]. Oura has no
//! import step — it's continuously synced — so it gets its own rebuildable
//! index, `.trove/oura-summary.json`, maintained here: per-day `(count,
//! sum)` rows per collection, kept fresh by comparing each source file's
//! size+mtime on every read and reparsing only what changed (see
//! [`Vault::ensure_oura_summary`]). The catalog and every Oura series read
//! from that index, never the raw JSONL directly, so opening the Health tab
//! costs the size of the index, not the size of the vault.
//! `oura_overview`, `oura_sleep_nights`, `oura_heartrate_range`, and
//! `health_workouts` still read their raw files directly — they need fields
//! the rollup doesn't carry, and touch data that's already small or
//! month-windowed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::health::{Bucket, MetricKind, SeriesPoint};
use crate::store::FileStamp;
use crate::vault::Vault;

pub const SOURCE_APPLE: &str = "apple-health";
pub const SOURCE_OURA: &str = "oura";

/// One source's contribution to a unified metric.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct UnifiedSourceInfo {
    pub source: String,
    pub records: u64,
    pub first_date: String,
    pub last_date: String,
}

/// A canonical metric with every source that reports it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct UnifiedMetric {
    pub slug: String,
    pub name: String,
    pub unit: String,
    pub kind: MetricKind,
    /// Methodology caveat shown under the chart (e.g. Apple HRV is SDNN,
    /// Oura's is rMSSD — comparable trend, different absolute numbers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub sources: Vec<UnifiedSourceInfo>,
}

/// One source's series for a metric — charted side by side, never merged.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SourceSeries {
    pub source: String,
    pub points: Vec<SeriesPoint>,
}

/// Latest value of one Oura daily score, for the Overview cards.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct OuraDayScore {
    pub slug: String,
    pub name: String,
    pub day: String,
    /// Numeric score/value; None for purely categorical scores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Categorical reading (resilience level, stress day summary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One night from Oura's `sleep` sessions. Durations in hours.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SleepNight {
    pub day: String,
    pub bedtime_start: String,
    pub bedtime_end: String,
    /// "long_sleep" | "late_nap" | "sleep" | "rest".
    pub kind: String,
    pub total_hours: f64,
    pub deep_hours: f64,
    pub rem_hours: f64,
    pub light_hours: f64,
    pub awake_hours: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efficiency: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_hrv: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lowest_heart_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_heart_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respiratory_rate: Option<f64>,
}

/// One intraday heart-rate sample (Oura records ~every 5 minutes).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct HeartratePoint {
    /// RFC3339, as recorded.
    pub ts: String,
    pub bpm: f64,
}

/// One workout or session, from either source.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WorkoutItem {
    pub source: String,
    pub day: String,
    pub start: String,
    pub end: String,
    /// Activity type ("Running", "walking", "meditation", …).
    pub activity: String,
    /// "workout" | "session" (Oura sessions: meditation, naps, breathing).
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calories: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_km: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intensity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// How a metric's value is read out of an Oura record.
enum OuraValue {
    /// Top-level numeric field.
    Field(&'static str),
    /// One level of nesting (`spo2_percentage.average`).
    Nested(&'static str, &'static str),
    SecondsAsHours(&'static str),
    SecondsAsMinutes(&'static str),
}

impl OuraValue {
    fn extract(&self, r: &Value) -> Option<f64> {
        match self {
            OuraValue::Field(f) => r.get(f)?.as_f64(),
            OuraValue::Nested(a, b) => r.get(a)?.get(b)?.as_f64(),
            OuraValue::SecondsAsHours(f) => Some(r.get(f)?.as_f64()? / 3600.0),
            OuraValue::SecondsAsMinutes(f) => Some(r.get(f)?.as_f64()? / 60.0),
        }
    }
}

struct OuraMetricDef {
    /// Canonical slug — equals the Apple slug when both sources report it.
    slug: &'static str,
    name: &'static str,
    unit: &'static str,
    kind: MetricKind,
    /// File stem under `health/oura/` ("heartrate" is month-partitioned).
    collection: &'static str,
    value: OuraValue,
    note: Option<&'static str>,
}

/// Every Oura-derived chartable metric. Overlapping slugs (`sleep`,
/// `heart-rate`, `steps`, …) match the Apple import's slugs in
/// [`crate::health`]; the rest are Oura-only.
static OURA_METRICS: &[OuraMetricDef] = &[
    OuraMetricDef {
        slug: "readiness-score",
        name: "Readiness Score",
        unit: "",
        kind: MetricKind::Avg,
        collection: "daily_readiness",
        value: OuraValue::Field("score"),
        note: None,
    },
    OuraMetricDef {
        slug: "sleep-score",
        name: "Sleep Score",
        unit: "",
        kind: MetricKind::Avg,
        collection: "daily_sleep",
        value: OuraValue::Field("score"),
        note: None,
    },
    OuraMetricDef {
        slug: "activity-score",
        name: "Activity Score",
        unit: "",
        kind: MetricKind::Avg,
        collection: "daily_activity",
        value: OuraValue::Field("score"),
        note: None,
    },
    OuraMetricDef {
        slug: "sleep",
        name: "Sleep",
        unit: "hr",
        kind: MetricKind::SumThenAvg,
        collection: "sleep",
        value: OuraValue::SecondsAsHours("total_sleep_duration"),
        note: None,
    },
    OuraMetricDef {
        slug: "heart-rate",
        name: "Heart Rate",
        unit: "count/min",
        kind: MetricKind::Avg,
        collection: "heartrate",
        value: OuraValue::Field("bpm"),
        note: None,
    },
    OuraMetricDef {
        slug: "resting-heart-rate",
        name: "Resting Heart Rate",
        unit: "count/min",
        kind: MetricKind::Avg,
        collection: "sleep",
        value: OuraValue::Field("lowest_heart_rate"),
        note: Some("Oura's value is the lowest heart rate during sleep; Apple's is its daily resting estimate."),
    },
    OuraMetricDef {
        slug: "hrv",
        name: "Heart Rate Variability",
        unit: "ms",
        kind: MetricKind::Avg,
        collection: "sleep",
        value: OuraValue::Field("average_hrv"),
        note: Some("Apple measures HRV as SDNN, Oura as nightly rMSSD — trends compare, absolute numbers don't."),
    },
    OuraMetricDef {
        slug: "respiratory-rate",
        name: "Respiratory Rate",
        unit: "count/min",
        kind: MetricKind::Avg,
        collection: "sleep",
        value: OuraValue::Field("average_breath"),
        note: None,
    },
    OuraMetricDef {
        slug: "blood-oxygen",
        name: "Blood Oxygen",
        unit: "%",
        kind: MetricKind::Avg,
        collection: "daily_spo2",
        value: OuraValue::Nested("spo2_percentage", "average"),
        note: None,
    },
    OuraMetricDef {
        slug: "steps",
        name: "Steps",
        unit: "count",
        kind: MetricKind::Sum,
        collection: "daily_activity",
        value: OuraValue::Field("steps"),
        note: None,
    },
    OuraMetricDef {
        slug: "active-energy",
        name: "Active Energy",
        unit: "kcal",
        kind: MetricKind::Sum,
        collection: "daily_activity",
        value: OuraValue::Field("active_calories"),
        note: None,
    },
    OuraMetricDef {
        slug: "vo2-max",
        name: "VO2 Max",
        unit: "mL/kg/min",
        kind: MetricKind::Avg,
        collection: "vo2_max",
        value: OuraValue::Field("vo2_max"),
        note: None,
    },
    OuraMetricDef {
        slug: "stress-high",
        name: "Stress (high)",
        unit: "min/day",
        kind: MetricKind::SumThenAvg,
        collection: "daily_stress",
        value: OuraValue::SecondsAsMinutes("stress_high"),
        note: None,
    },
    OuraMetricDef {
        slug: "recovery-high",
        name: "Recovery (high)",
        unit: "min/day",
        kind: MetricKind::SumThenAvg,
        collection: "daily_stress",
        value: OuraValue::SecondsAsMinutes("recovery_high"),
        note: None,
    },
    OuraMetricDef {
        slug: "temperature-deviation",
        name: "Temperature Deviation",
        unit: "°C",
        kind: MetricKind::Avg,
        collection: "daily_readiness",
        value: OuraValue::Field("temperature_deviation"),
        note: None,
    },
    OuraMetricDef {
        slug: "cardiovascular-age",
        name: "Cardiovascular Age",
        unit: "yr",
        kind: MetricKind::Avg,
        collection: "daily_cardiovascular_age",
        value: OuraValue::Field("vascular_age"),
        note: None,
    },
];

/// The rebuildable per-day Oura index at `.trove/oura-summary.json`. Lives
/// next to `OURA_METRICS`, which defines its contents. See
/// [`Vault::ensure_oura_summary`].
const OURA_SUMMARY_REL: &str = ".trove/oura-summary.json";
/// Bump when `OURA_METRICS` changes shape in a way that would make a cached
/// row wrong (new slug, changed extraction) — forces a full rebuild.
const OURA_SUMMARY_VERSION: u32 = 1;

/// One day's contribution to a metric — count of records that had a value,
/// and their sum. Exactly what `fold_buckets` consumes, and what the
/// catalog's records/first/last derive from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct DailyRow {
    day: String,
    count: u64,
    sum: f64,
}

/// The whole rebuildable Oura index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OuraSummary {
    version: u32,
    /// Stamp of every source file this summary was built from, keyed by its
    /// path relative to `health/oura/` ("sleep.jsonl",
    /// "heartrate/2026-06.jsonl").
    files: BTreeMap<String, FileStamp>,
    /// slug -> daily rows, for every non-heartrate `OURA_METRICS` def.
    metrics: BTreeMap<String, Vec<DailyRow>>,
    /// "YYYY-MM" -> that month file's daily rows, for the heart-rate def —
    /// keyed per file so a sync touching one month re-parses ~1 MB, not the
    /// whole multi-year set.
    heartrate_months: BTreeMap<String, Vec<DailyRow>>,
}

impl OuraSummary {
    fn empty() -> Self {
        OuraSummary {
            version: OURA_SUMMARY_VERSION,
            files: BTreeMap::new(),
            metrics: BTreeMap::new(),
            heartrate_months: BTreeMap::new(),
        }
    }
}

/// Every distinct non-heartrate collection `OURA_METRICS` reads — a fixed,
/// fully enumerable set, so missing files are handled inline against it
/// (unlike heartrate's month files, which need a diff against prior state).
fn oura_collections() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = OURA_METRICS
        .iter()
        .map(|d| d.collection)
        .filter(|c| *c != "heartrate")
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Per-day `(count, sum)` for one def over one already-parsed file's
/// records, in file order — the same accumulation `oura_metric_series` and
/// `oura_metric_info` used to do directly, now run once per source file
/// instead of once per read.
fn daily_rows_for(def: &OuraMetricDef, records: &[Value]) -> Vec<DailyRow> {
    let mut daily: BTreeMap<NaiveDate, (f64, u64)> = BTreeMap::new();
    for r in records {
        let Some(day) = record_day(r) else { continue };
        let Some(v) = def.value.extract(r) else { continue };
        let e = daily.entry(day).or_insert((0.0, 0));
        e.0 += v;
        e.1 += 1;
    }
    daily
        .into_iter()
        .map(|(day, (sum, count))| DailyRow {
            day: day.format("%Y-%m-%d").to_string(),
            count,
            sum,
        })
        .collect()
}

/// One def's daily rows out of the summary — the heart-rate def
/// concatenates every month, everything else reads its own slug.
fn oura_daily_rows<'a>(def: &OuraMetricDef, summary: &'a OuraSummary) -> Vec<&'a DailyRow> {
    if def.collection == "heartrate" {
        summary.heartrate_months.values().flatten().collect()
    } else {
        summary.metrics.get(def.slug).map(|v| v.iter().collect()).unwrap_or_default()
    }
}

/// Record count and date span for one Oura metric, from the summary; None
/// when it has no extractable data yet.
fn oura_source_info(def: &OuraMetricDef, summary: &OuraSummary) -> Option<UnifiedSourceInfo> {
    let rows = oura_daily_rows(def, summary);
    if rows.is_empty() {
        return None;
    }
    let count: u64 = rows.iter().map(|r| r.count).sum();
    let first = rows.iter().map(|r| r.day.as_str()).min()?.to_string();
    let last = rows.iter().map(|r| r.day.as_str()).max()?.to_string();
    Some(UnifiedSourceInfo {
        source: SOURCE_OURA.into(),
        records: count,
        first_date: first,
        last_date: last,
    })
}

fn set_metric_rows(summary: &mut OuraSummary, slug: &str, rows: Vec<DailyRow>) {
    if rows.is_empty() {
        summary.metrics.remove(slug);
    } else {
        summary.metrics.insert(slug.to_string(), rows);
    }
}

fn set_heartrate_rows(summary: &mut OuraSummary, month: &str, rows: Vec<DailyRow>) {
    if rows.is_empty() {
        summary.heartrate_months.remove(month);
    } else {
        summary.heartrate_months.insert(month.to_string(), rows);
    }
}

fn day_str(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// The day a record belongs to: the `day` field for collections, the
/// timestamp's own date for heartrate samples.
fn record_day(r: &Value) -> Option<NaiveDate> {
    if let Some(d) = r.get("day").and_then(Value::as_str) {
        return day_str(d);
    }
    let ts = r.get("timestamp").and_then(Value::as_str)?;
    day_str(ts.get(..10)?)
}

/// Same bucket fold as `Vault::health_series` so both sources aggregate
/// identically: per-day (sum, count) → bucketed value by kind.
fn fold_buckets(
    daily: BTreeMap<NaiveDate, (f64, f64)>,
    bucket: Bucket,
    kind: MetricKind,
) -> Vec<SeriesPoint> {
    let mut buckets: BTreeMap<NaiveDate, (f64, f64, u64)> = BTreeMap::new();
    for (date, (sum, count)) in daily {
        let key = match bucket {
            Bucket::Day => date,
            Bucket::Week => {
                date - chrono::Duration::days(date.weekday().num_days_from_monday() as i64)
            }
            Bucket::Month => date.with_day(1).unwrap_or(date),
        };
        let e = buckets.entry(key).or_insert((0.0, 0.0, 0));
        e.0 += sum;
        e.1 += count;
        e.2 += 1;
    }
    buckets
        .into_iter()
        .filter_map(|(day, (sum, count, days))| {
            let value = match kind {
                MetricKind::Sum => sum,
                MetricKind::Avg => {
                    if count == 0.0 {
                        return None;
                    }
                    sum / count
                }
                MetricKind::SumThenAvg => sum / days as f64,
            };
            Some(SeriesPoint {
                date: day.format("%Y-%m-%d").to_string(),
                value,
            })
        })
        .collect()
}

fn opt_f64(r: &Value, field: &str) -> Option<f64> {
    r.get(field).and_then(Value::as_f64)
}

fn str_field(r: &Value, field: &str) -> String {
    r.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn hours(r: &Value, field: &str) -> f64 {
    opt_f64(r, field).unwrap_or(0.0) / 3600.0
}

fn parse_ts(s: &str) -> Option<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(s).ok()
}

/// Apple Health raw timestamps look like "2024-01-15 08:30:21 -0800".
fn parse_apple_ts(s: &str) -> Option<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S %z").ok()
}

fn parse_opt(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        s.parse().ok()
    }
}

impl Vault {
    /// Every chartable metric across all health sources: the Apple import's
    /// catalog with Oura sources attached on matching slugs, plus Oura-only
    /// metrics for collections that have data.
    pub fn health_metrics_unified(&self) -> Result<Vec<UnifiedMetric>> {
        let mut out: Vec<UnifiedMetric> = self
            .list_health_metrics()?
            .into_iter()
            .map(|m| UnifiedMetric {
                slug: m.slug,
                name: m.name,
                unit: m.unit,
                kind: m.kind,
                note: None,
                sources: vec![UnifiedSourceInfo {
                    source: SOURCE_APPLE.into(),
                    records: m.records,
                    first_date: m.first_date,
                    last_date: m.last_date,
                }],
            })
            .collect();
        let summary = self.ensure_oura_summary()?;
        for def in OURA_METRICS {
            let Some(info) = oura_source_info(def, &summary) else {
                continue;
            };
            match out.iter_mut().find(|m| m.slug == def.slug) {
                Some(m) => {
                    m.sources.push(info);
                    if m.note.is_none() {
                        m.note = def.note.map(str::to_string);
                    }
                }
                None => out.push(UnifiedMetric {
                    slug: def.slug.into(),
                    name: def.name.into(),
                    unit: def.unit.into(),
                    kind: def.kind,
                    note: def.note.map(str::to_string),
                    sources: vec![info],
                }),
            }
        }
        Ok(out)
    }

    /// Per-source time series for one canonical metric — one entry per
    /// source that has data, in catalog order (Apple first).
    pub fn health_series_unified(&self, slug: &str, bucket: Bucket) -> Result<Vec<SourceSeries>> {
        let mut out = Vec::new();
        if self.list_health_metrics()?.iter().any(|m| m.slug == slug) {
            out.push(SourceSeries {
                source: SOURCE_APPLE.into(),
                points: self.health_series(slug, bucket)?,
            });
        }
        if let Some(def) = OURA_METRICS.iter().find(|d| d.slug == slug) {
            let points = self.oura_metric_series(def, bucket)?;
            if !points.is_empty() {
                out.push(SourceSeries {
                    source: SOURCE_OURA.into(),
                    points,
                });
            }
        }
        if out.is_empty() {
            bail!("unknown health metric: {slug}");
        }
        Ok(out)
    }

    /// Latest Oura daily scores for the Overview cards. Empty when Oura has
    /// never synced.
    pub fn oura_overview(&self) -> Result<Vec<OuraDayScore>> {
        let mut out = Vec::new();
        let mut push = |slug: &str,
                        name: &str,
                        collection: &str,
                        value: &dyn Fn(&Value) -> Option<f64>,
                        label: &dyn Fn(&Value) -> Option<String>|
         -> Result<()> {
            let records = self.load_oura_collection(collection)?;
            let latest = records
                .iter()
                .filter(|r| r.get("day").and_then(Value::as_str).is_some())
                .max_by_key(|r| str_field(r, "day"));
            if let Some(r) = latest {
                out.push(OuraDayScore {
                    slug: slug.into(),
                    name: name.into(),
                    day: str_field(r, "day"),
                    value: value(r),
                    label: label(r),
                });
            }
            Ok(())
        };
        push("readiness-score", "Readiness", "daily_readiness", &|r| opt_f64(r, "score"), &|_| None)?;
        push("sleep-score", "Sleep", "daily_sleep", &|r| opt_f64(r, "score"), &|_| None)?;
        push("activity-score", "Activity", "daily_activity", &|r| opt_f64(r, "score"), &|_| None)?;
        push(
            "stress-high",
            "Stress",
            "daily_stress",
            &|r| opt_f64(r, "stress_high").map(|s| s / 60.0),
            &|r| r.get("day_summary").and_then(Value::as_str).map(str::to_string),
        )?;
        push(
            "resilience",
            "Resilience",
            "daily_resilience",
            &|_| None,
            &|r| r.get("level").and_then(Value::as_str).map(str::to_string),
        )?;
        push(
            "cardiovascular-age",
            "Cardio Age",
            "daily_cardiovascular_age",
            &|r| opt_f64(r, "vascular_age"),
            &|_| None,
        )?;
        Ok(out)
    }

    /// Recent sleep sessions, newest first (naps and rests included, marked
    /// by `kind`).
    pub fn oura_sleep_nights(&self, limit: usize) -> Result<Vec<SleepNight>> {
        let records = self.load_oura_collection("sleep")?;
        let mut nights: Vec<SleepNight> = records
            .iter()
            .filter_map(|r| {
                let day = r.get("day").and_then(Value::as_str)?;
                Some(SleepNight {
                    day: day.into(),
                    bedtime_start: str_field(r, "bedtime_start"),
                    bedtime_end: str_field(r, "bedtime_end"),
                    kind: str_field(r, "type"),
                    total_hours: hours(r, "total_sleep_duration"),
                    deep_hours: hours(r, "deep_sleep_duration"),
                    rem_hours: hours(r, "rem_sleep_duration"),
                    light_hours: hours(r, "light_sleep_duration"),
                    awake_hours: hours(r, "awake_time"),
                    efficiency: opt_f64(r, "efficiency"),
                    latency_min: opt_f64(r, "latency").map(|s| s / 60.0),
                    average_hrv: opt_f64(r, "average_hrv"),
                    lowest_heart_rate: opt_f64(r, "lowest_heart_rate"),
                    average_heart_rate: opt_f64(r, "average_heart_rate"),
                    respiratory_rate: opt_f64(r, "average_breath"),
                })
            })
            .collect();
        nights.sort_by(|a, b| b.bedtime_start.cmp(&a.bedtime_start).then(b.day.cmp(&a.day)));
        nights.truncate(limit);
        Ok(nights)
    }

    /// Heart-rate samples in an RFC3339 time window, downsampled by simple
    /// chunk-averaging when they exceed `max_points`. Reads only the month
    /// files the window touches.
    pub fn oura_heartrate_range(
        &self,
        start: &str,
        end: &str,
        max_points: usize,
    ) -> Result<Vec<HeartratePoint>> {
        let start_dt = parse_ts(start).with_context(|| format!("bad start time: {start}"))?;
        let end_dt = parse_ts(end).with_context(|| format!("bad end time: {end}"))?;
        // Offsets can shift a sample's stored month relative to the query
        // bounds, so widen the month walk by a day on each side.
        let mut points: Vec<(DateTime<chrono::FixedOffset>, f64)> = Vec::new();
        let mut month = (start_dt - chrono::Duration::days(1)).date_naive().with_day(1).unwrap();
        let last_month = (end_dt + chrono::Duration::days(1)).date_naive().with_day(1).unwrap();
        while month <= last_month {
            let rel = format!("health/oura/heartrate/{}.jsonl", month.format("%Y-%m"));
            for r in self.load_oura_records(&rel)? {
                let Some(ts) = r.get("timestamp").and_then(Value::as_str).and_then(parse_ts)
                else {
                    continue;
                };
                if ts < start_dt || ts > end_dt {
                    continue;
                }
                let Some(bpm) = opt_f64(&r, "bpm") else { continue };
                points.push((ts, bpm));
            }
            month = if month.month() == 12 {
                NaiveDate::from_ymd_opt(month.year() + 1, 1, 1).unwrap()
            } else {
                NaiveDate::from_ymd_opt(month.year(), month.month() + 1, 1).unwrap()
            };
        }
        points.sort_by_key(|(ts, _)| *ts);
        if max_points > 0 && points.len() > max_points {
            let chunk = points.len().div_ceil(max_points);
            points = points
                .chunks(chunk)
                .map(|c| {
                    let bpm = c.iter().map(|(_, b)| b).sum::<f64>() / c.len() as f64;
                    (c[0].0, bpm)
                })
                .collect();
        }
        Ok(points
            .into_iter()
            .map(|(ts, bpm)| HeartratePoint {
                ts: ts.to_rfc3339(),
                bpm,
            })
            .collect())
    }

    /// The newest `limit` workouts and sessions across both sources, sorted
    /// by start time descending. Apple rows come from the monthly CSVs
    /// (newest months first, read only until `limit` is covered).
    pub fn health_workouts(&self, limit: usize) -> Result<Vec<WorkoutItem>> {
        let mut items: Vec<(DateTime<chrono::FixedOffset>, WorkoutItem)> = Vec::new();

        for r in self.load_oura_collection("workout")? {
            if let Some(item) = oura_workout_item(&r, "workout") {
                items.push(item);
            }
        }
        for r in self.load_oura_collection("session")? {
            if let Some(item) = oura_workout_item(&r, "session") {
                items.push(item);
            }
        }

        // Apple monthly CSVs, newest first; stop once this source alone
        // could fill the limit (the merge below truncates globally).
        let dir = self.resolve("health/workouts")?;
        if dir.is_dir() {
            let mut months: Vec<String> = fs::read_dir(&dir)?
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".csv") && n != "daily.csv")
                .collect();
            months.sort();
            months.reverse();
            let mut apple_rows = 0usize;
            for name in months {
                if apple_rows >= limit {
                    break;
                }
                let mut reader =
                    csv::Reader::from_path(dir.join(&name)).with_context(|| format!("reading {name}"))?;
                for row in reader.records() {
                    let row = row?;
                    // start,end,type,duration_min,energy_kcal,distance_km,source
                    let Some(start) = parse_apple_ts(&row[0]) else { continue };
                    let end = parse_apple_ts(&row[1]).unwrap_or(start);
                    items.push((
                        start,
                        WorkoutItem {
                            source: SOURCE_APPLE.into(),
                            day: end.date_naive().format("%Y-%m-%d").to_string(),
                            start: start.to_rfc3339(),
                            end: end.to_rfc3339(),
                            activity: row[2].to_string(),
                            kind: "workout".into(),
                            duration_min: parse_opt(&row[3]),
                            calories: parse_opt(&row[4]),
                            distance_km: parse_opt(&row[5]),
                            intensity: None,
                            label: None,
                        },
                    ));
                    apple_rows += 1;
                }
            }
        }

        items.sort_by_key(|(start, _)| std::cmp::Reverse(*start));
        items.truncate(limit);
        Ok(items.into_iter().map(|(_, item)| item).collect())
    }

    /// Series for one Oura metric def: per-day (sum, count) → bucket fold,
    /// fed from the rebuildable summary rather than raw JSONL.
    fn oura_metric_series(&self, def: &OuraMetricDef, bucket: Bucket) -> Result<Vec<SeriesPoint>> {
        let summary = self.ensure_oura_summary()?;
        let mut daily: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
        for row in oura_daily_rows(def, &summary) {
            let Some(day) = day_str(&row.day) else { continue };
            let e = daily.entry(day).or_insert((0.0, 0.0));
            e.0 += row.sum;
            e.1 += row.count as f64;
        }
        Ok(fold_buckets(daily, bucket, def.kind))
    }

    fn load_oura_collection(&self, name: &str) -> Result<Vec<Value>> {
        self.load_oura_records(&format!("health/oura/{name}.jsonl"))
    }

    /// Bring `.trove/oura-summary.json` up to date and return it: read the
    /// existing summary (missing/unparseable/wrong-version → start over via
    /// [`Vault::ensure_index`]), stat every source file, and for each
    /// new-or-changed one reparse it once (via [`Vault::load_oura_records`])
    /// to recompute the daily rows of every def that reads it. Deleted
    /// files drop their rows. Written back only when something actually
    /// changed.
    ///
    /// Called on every unified-health read; the fresh path costs a handful
    /// of `stat`s and a small JSON read/write, since nothing changed since
    /// the last sync (`Vault::collect_oura` also calls this right after
    /// writing, so reads normally find everything already current).
    pub(crate) fn ensure_oura_summary(&self) -> Result<OuraSummary> {
        self.ensure_index(OURA_SUMMARY_REL, OURA_SUMMARY_VERSION, OuraSummary::empty, |mut summary| {
            let mut dirty = false;

            // Non-heartrate collections: a fixed, fully enumerable set — a
            // missing file is handled inline, no diff against prior state
            // needed.
            for name in oura_collections() {
                let key = format!("{name}.jsonl");
                let rel = format!("health/oura/{name}.jsonl");
                match self.stat_file(&rel)? {
                    Some(stamp) => {
                        if summary.files.get(&key) != Some(&stamp) {
                            let records = self.load_oura_records(&rel)?;
                            for def in OURA_METRICS.iter().filter(|d| d.collection == name) {
                                set_metric_rows(&mut summary, def.slug, daily_rows_for(def, &records));
                            }
                            summary.files.insert(key, stamp);
                            dirty = true;
                        }
                    }
                    None => {
                        if summary.files.remove(&key).is_some() {
                            dirty = true;
                        }
                        for def in OURA_METRICS.iter().filter(|d| d.collection == name) {
                            if summary.metrics.remove(def.slug).is_some() {
                                dirty = true;
                            }
                        }
                    }
                }
            }

            // Heartrate month files come and go as the ring syncs, so
            // (unlike the fixed collection set above) dropped months are
            // found by diffing against what the summary already knows.
            let months = self.oura_heartrate_months()?;
            let heartrate_def = OURA_METRICS
                .iter()
                .find(|d| d.collection == "heartrate")
                .expect("heart-rate def is always registered");
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for month in &months {
                let key = format!("heartrate/{month}.jsonl");
                let rel = format!("health/oura/heartrate/{month}.jsonl");
                seen.insert(key.clone());
                let Some(stamp) = self.stat_file(&rel)? else { continue };
                if summary.files.get(&key) != Some(&stamp) {
                    let records = self.load_oura_records(&rel)?;
                    set_heartrate_rows(&mut summary, month, daily_rows_for(heartrate_def, &records));
                    summary.files.insert(key, stamp);
                    dirty = true;
                }
            }
            let stale_months: Vec<String> = summary
                .files
                .keys()
                .filter(|k| k.starts_with("heartrate/") && !seen.contains(*k))
                .cloned()
                .collect();
            for key in stale_months {
                summary.files.remove(&key);
                if let Some(month) = key.strip_prefix("heartrate/").and_then(|s| s.strip_suffix(".jsonl")) {
                    summary.heartrate_months.remove(month);
                }
                dirty = true;
            }

            Ok((summary, dirty))
        })
    }

    /// Every heartrate month file present on disk ("YYYY-MM"), sorted.
    fn oura_heartrate_months(&self) -> Result<Vec<String>> {
        let dir = self.resolve("health/oura/heartrate")?;
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut months: Vec<String> = fs::read_dir(&dir)?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| n.strip_suffix(".jsonl").map(str::to_string))
            .collect();
        months.sort();
        Ok(months)
    }
}

fn oura_workout_item(
    r: &Value,
    kind: &str,
) -> Option<(DateTime<chrono::FixedOffset>, WorkoutItem)> {
    let start_str = r.get("start_datetime").and_then(Value::as_str)?;
    let start = parse_ts(start_str)?;
    let end = r
        .get("end_datetime")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .unwrap_or(start);
    let activity = if kind == "session" {
        str_field(r, "type")
    } else {
        str_field(r, "activity")
    };
    Some((
        start,
        WorkoutItem {
            source: SOURCE_OURA.into(),
            day: str_field(r, "day"),
            start: start.to_rfc3339(),
            end: end.to_rfc3339(),
            activity,
            kind: kind.into(),
            duration_min: Some((end - start).num_seconds() as f64 / 60.0),
            calories: opt_f64(r, "calories"),
            distance_km: opt_f64(r, "distance").map(|m| m / 1000.0),
            intensity: r.get("intensity").and_then(Value::as_str).map(str::to_string),
            label: r.get("label").and_then(Value::as_str).map(str::to_string),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-health-unified-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn write_jsonl(v: &Vault, rel: &str, records: &[Value]) {
        let path = v.root().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn oura_only_metrics_list_and_series() {
        let v = temp_vault("metrics");
        write_jsonl(
            &v,
            "health/oura/daily_activity.jsonl",
            &[
                json!({"id": "a", "day": "2026-06-01", "score": 80, "steps": 10000, "active_calories": 500}),
                json!({"id": "b", "day": "2026-06-02", "score": 90, "steps": 6000, "active_calories": 300}),
                json!({"id": "c", "day": "2026-07-01", "score": 70, "steps": 4000, "active_calories": 200}),
            ],
        );
        let metrics = v.health_metrics_unified().unwrap();
        let steps = metrics.iter().find(|m| m.slug == "steps").unwrap();
        assert_eq!(steps.sources.len(), 1);
        assert_eq!(steps.sources[0].source, SOURCE_OURA);
        assert_eq!(steps.sources[0].records, 3);
        assert_eq!(steps.sources[0].first_date, "2026-06-01");
        assert_eq!(steps.sources[0].last_date, "2026-07-01");
        // Collections with no data never appear.
        assert!(!metrics.iter().any(|m| m.slug == "readiness-score"));

        let series = v.health_series_unified("steps", Bucket::Month).unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].source, SOURCE_OURA);
        assert_eq!(series[0].points[0].value, 16000.0);
        assert_eq!(series[0].points[1].value, 4000.0);

        // Scores average across coarser buckets.
        let scores = v.health_series_unified("activity-score", Bucket::Month).unwrap();
        assert_eq!(scores[0].points[0].value, 85.0);

        assert!(v.health_series_unified("nope", Bucket::Day).is_err());
    }

    #[test]
    fn sleep_nights_parse_and_chart_as_hours() {
        let v = temp_vault("sleep");
        write_jsonl(
            &v,
            "health/oura/sleep.jsonl",
            &[
                json!({
                    "id": "s1", "day": "2026-06-02", "type": "long_sleep",
                    "bedtime_start": "2026-06-01T23:10:00-07:00",
                    "bedtime_end": "2026-06-02T07:00:00-07:00",
                    "total_sleep_duration": 25200, "deep_sleep_duration": 5400,
                    "rem_sleep_duration": 6000, "light_sleep_duration": 13800,
                    "awake_time": 3000, "efficiency": 92, "latency": 600,
                    "average_hrv": 52, "lowest_heart_rate": 48,
                    "average_heart_rate": 58, "average_breath": 14.5
                }),
                json!({
                    "id": "s2", "day": "2026-06-02", "type": "late_nap",
                    "bedtime_start": "2026-06-02T14:00:00-07:00",
                    "bedtime_end": "2026-06-02T14:45:00-07:00",
                    "total_sleep_duration": 2700
                }),
            ],
        );
        let nights = v.oura_sleep_nights(10).unwrap();
        assert_eq!(nights.len(), 2);
        // Newest first: the nap started later.
        assert_eq!(nights[0].kind, "late_nap");
        let night = &nights[1];
        assert!((night.total_hours - 7.0).abs() < 1e-9);
        assert!((night.deep_hours - 1.5).abs() < 1e-9);
        assert_eq!(night.efficiency, Some(92.0));
        assert_eq!(night.latency_min, Some(10.0));
        assert_eq!(night.lowest_heart_rate, Some(48.0));

        // Daily sleep series sums both sessions of the day.
        let series = v.health_series_unified("sleep", Bucket::Day).unwrap();
        assert_eq!(series[0].source, SOURCE_OURA);
        assert!((series[0].points[0].value - 7.75).abs() < 1e-9);
    }

    #[test]
    fn heartrate_range_filters_and_downsamples() {
        let v = temp_vault("hr");
        let records: Vec<Value> = (0..120)
            .map(|i| {
                json!({
                    "bpm": 60 + (i % 10),
                    "source": "ppg",
                    "timestamp": format!("2026-06-01T{:02}:{:02}:00+00:00", i / 12, (i % 12) * 5)
                })
            })
            .collect();
        write_jsonl(&v, "health/oura/heartrate/2026-06.jsonl", &records);

        let pts = v
            .oura_heartrate_range("2026-06-01T00:00:00+00:00", "2026-06-01T01:00:00+00:00", 500)
            .unwrap();
        assert_eq!(pts.len(), 13); // inclusive bounds, every 5 minutes
        assert_eq!(pts[0].bpm, 60.0);

        let down = v
            .oura_heartrate_range("2026-06-01T00:00:00+00:00", "2026-06-01T10:00:00+00:00", 30)
            .unwrap();
        assert!(down.len() <= 30, "got {}", down.len());

        // Day-level series averages all samples.
        let series = v.health_series_unified("heart-rate", Bucket::Day).unwrap();
        assert_eq!(series[0].points.len(), 1);
        assert_eq!(series[0].points[0].date, "2026-06-01");
    }

    #[test]
    fn workouts_merge_both_sources_newest_first() {
        let v = temp_vault("workouts");
        write_jsonl(
            &v,
            "health/oura/workout.jsonl",
            &[json!({
                "id": "w1", "day": "2026-06-02", "activity": "running",
                "calories": 320, "distance": 5000.0, "intensity": "moderate",
                "start_datetime": "2026-06-02T08:00:00-07:00",
                "end_datetime": "2026-06-02T08:30:00-07:00"
            })],
        );
        write_jsonl(
            &v,
            "health/oura/session.jsonl",
            &[json!({
                "id": "m1", "day": "2026-06-03", "type": "meditation",
                "start_datetime": "2026-06-03T07:00:00-07:00",
                "end_datetime": "2026-06-03T07:15:00-07:00"
            })],
        );
        let dir = v.root().join("health/workouts");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("2026-06.csv"),
            "start,end,type,duration_min,energy_kcal,distance_km,source\n\
             2026-06-01 17:00:00 -0700,2026-06-01 17:45:00 -0700,Running,45,410,7.2,Apple Watch\n",
        )
        .unwrap();

        let items = v.health_workouts(10).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].activity, "meditation");
        assert_eq!(items[0].kind, "session");
        assert_eq!(items[1].source, SOURCE_OURA);
        assert_eq!(items[1].distance_km, Some(5.0));
        assert!((items[1].duration_min.unwrap() - 30.0).abs() < 1e-9);
        assert_eq!(items[2].source, SOURCE_APPLE);
        assert_eq!(items[2].calories, Some(410.0));
        assert_eq!(items[2].day, "2026-06-01");

        let one = v.health_workouts(1).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn overview_reports_latest_scores_and_levels() {
        let v = temp_vault("overview");
        write_jsonl(
            &v,
            "health/oura/daily_readiness.jsonl",
            &[
                json!({"id": "r1", "day": "2026-06-10", "score": 70, "temperature_deviation": -0.2}),
                json!({"id": "r2", "day": "2026-06-11", "score": 84, "temperature_deviation": 0.1}),
            ],
        );
        write_jsonl(
            &v,
            "health/oura/daily_resilience.jsonl",
            &[json!({"id": "z1", "day": "2026-06-11", "level": "solid"})],
        );
        write_jsonl(
            &v,
            "health/oura/daily_stress.jsonl",
            &[json!({"id": "t1", "day": "2026-06-11", "stress_high": 3600, "recovery_high": 7200, "day_summary": "normal"})],
        );
        let scores = v.oura_overview().unwrap();
        let readiness = scores.iter().find(|s| s.slug == "readiness-score").unwrap();
        assert_eq!(readiness.value, Some(84.0));
        assert_eq!(readiness.day, "2026-06-11");
        let resilience = scores.iter().find(|s| s.slug == "resilience").unwrap();
        assert_eq!(resilience.label.as_deref(), Some("solid"));
        let stress = scores.iter().find(|s| s.slug == "stress-high").unwrap();
        assert_eq!(stress.value, Some(60.0));
        assert_eq!(stress.label.as_deref(), Some("normal"));
        // No sleep/activity files — those cards are simply absent.
        assert!(!scores.iter().any(|s| s.slug == "sleep-score"));
    }

    #[test]
    fn apple_and_oura_merge_under_one_slug() {
        let v = temp_vault("merge");
        // Minimal Apple import via the real importer.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<HealthData>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="iPhone" unit="count" startDate="2026-06-01 10:00:00 -0700" endDate="2026-06-01 10:10:00 -0700" value="1000"/>
</HealthData>"#;
        let xml_path = std::env::temp_dir().join(format!(
            "trove-health-unified-merge-{}.xml",
            std::process::id()
        ));
        fs::write(&xml_path, xml).unwrap();
        v.import_health_export(&xml_path, |_| {}).unwrap();
        write_jsonl(
            &v,
            "health/oura/daily_activity.jsonl",
            &[json!({"id": "a", "day": "2026-06-01", "steps": 9000})],
        );

        let metrics = v.health_metrics_unified().unwrap();
        let steps = metrics.iter().find(|m| m.slug == "steps").unwrap();
        let sources: Vec<&str> = steps.sources.iter().map(|s| s.source.as_str()).collect();
        assert_eq!(sources, [SOURCE_APPLE, SOURCE_OURA]);

        let series = v.health_series_unified("steps", Bucket::Day).unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].points[0].value, 1000.0);
        assert_eq!(series[1].points[0].value, 9000.0);
    }

    // -- .trove/oura-summary.json: rebuildable index (health-refactor Step 2) --

    #[test]
    fn summary_written_once_and_reused_when_unchanged() {
        let v = temp_vault("summary-reuse");
        write_jsonl(
            &v,
            "health/oura/daily_activity.jsonl",
            &[json!({"id": "a", "day": "2026-06-01", "score": 80, "steps": 10000, "active_calories": 500})],
        );
        v.health_metrics_unified().unwrap();
        let summary_path = v.root().join(".trove/oura-summary.json");
        assert!(summary_path.exists(), "summary written on first read");
        let mtime1 = fs::metadata(&summary_path).unwrap().modified().unwrap();
        let body1 = fs::read_to_string(&summary_path).unwrap();

        // Nothing changed on disk — the second read must not rewrite the
        // file at all (not even byte-identically).
        v.health_metrics_unified().unwrap();
        let mtime2 = fs::metadata(&summary_path).unwrap().modified().unwrap();
        let body2 = fs::read_to_string(&summary_path).unwrap();
        assert_eq!(mtime1, mtime2, "unchanged files must not trigger a rewrite");
        assert_eq!(body1, body2);
    }

    #[test]
    fn hand_edited_collection_file_is_detected_and_reflected() {
        let v = temp_vault("summary-edit");
        write_jsonl(
            &v,
            "health/oura/daily_activity.jsonl",
            &[json!({"id": "a", "day": "2026-06-01", "score": 80, "steps": 1000, "active_calories": 100})],
        );
        let metrics = v.health_metrics_unified().unwrap();
        let steps = metrics.iter().find(|m| m.slug == "steps").unwrap();
        assert_eq!(steps.sources[0].records, 1);

        // A hand edit that changes the file's size (a record added).
        write_jsonl(
            &v,
            "health/oura/daily_activity.jsonl",
            &[
                json!({"id": "a", "day": "2026-06-01", "score": 80, "steps": 1000, "active_calories": 100}),
                json!({"id": "b", "day": "2026-06-02", "score": 85, "steps": 2000, "active_calories": 150}),
            ],
        );
        let metrics = v.health_metrics_unified().unwrap();
        let steps = metrics.iter().find(|m| m.slug == "steps").unwrap();
        assert_eq!(steps.sources[0].records, 2);
        assert_eq!(steps.sources[0].last_date, "2026-06-02");

        let series = v.health_series_unified("steps", Bucket::Day).unwrap();
        assert_eq!(series[0].points.len(), 2);
        assert_eq!(series[0].points[1].value, 2000.0);
    }

    #[test]
    fn deleted_collection_file_drops_its_metrics() {
        let v = temp_vault("summary-delete");
        write_jsonl(
            &v,
            "health/oura/daily_activity.jsonl",
            &[json!({"id": "a", "day": "2026-06-01", "score": 80, "steps": 1000, "active_calories": 100})],
        );
        let metrics = v.health_metrics_unified().unwrap();
        assert!(metrics.iter().any(|m| m.slug == "steps"));
        assert!(metrics.iter().any(|m| m.slug == "activity-score"));

        fs::remove_file(v.root().join("health/oura/daily_activity.jsonl")).unwrap();
        let metrics = v.health_metrics_unified().unwrap();
        assert!(!metrics.iter().any(|m| m.slug == "steps"));
        assert!(!metrics.iter().any(|m| m.slug == "active-energy"));
        assert!(!metrics.iter().any(|m| m.slug == "activity-score"));
        assert!(v.health_series_unified("steps", Bucket::Day).is_err());
    }

    #[test]
    fn corrupt_and_wrong_version_summary_self_repair() {
        let v = temp_vault("summary-corrupt");
        write_jsonl(
            &v,
            "health/oura/daily_activity.jsonl",
            &[json!({"id": "a", "day": "2026-06-01", "score": 80, "steps": 1000, "active_calories": 100})],
        );
        let summary_path = v.root().join(".trove/oura-summary.json");
        fs::create_dir_all(summary_path.parent().unwrap()).unwrap();

        fs::write(&summary_path, "{ not json at all").unwrap();
        let metrics = v.health_metrics_unified().unwrap();
        let steps = metrics.iter().find(|m| m.slug == "steps").unwrap();
        assert_eq!(steps.sources[0].records, 1, "corrupt summary self-repairs");
        let body = fs::read_to_string(&summary_path).unwrap();
        assert!(serde_json::from_str::<Value>(&body).unwrap()["version"] == 1);

        fs::write(
            &summary_path,
            r#"{"version": 999, "files": {}, "metrics": {}, "heartrate_months": {}}"#,
        )
        .unwrap();
        let metrics = v.health_metrics_unified().unwrap();
        let steps = metrics.iter().find(|m| m.slug == "steps").unwrap();
        assert_eq!(steps.sources[0].records, 1, "wrong-version summary self-repairs");
        let body = fs::read_to_string(&summary_path).unwrap();
        assert!(serde_json::from_str::<Value>(&body).unwrap()["version"] == 1);
    }

    #[test]
    fn heartrate_is_keyed_per_month_and_edits_are_isolated() {
        let v = temp_vault("summary-hr-months");
        write_jsonl(
            &v,
            "health/oura/heartrate/2026-05.jsonl",
            &[json!({"bpm": 50, "source": "ppg", "timestamp": "2026-05-31T23:00:00+00:00"})],
        );
        write_jsonl(
            &v,
            "health/oura/heartrate/2026-06.jsonl",
            &[
                json!({"bpm": 60, "source": "ppg", "timestamp": "2026-06-01T00:00:00+00:00"}),
                json!({"bpm": 70, "source": "ppg", "timestamp": "2026-06-01T00:05:00+00:00"}),
            ],
        );
        let series = v.health_series_unified("heart-rate", Bucket::Day).unwrap();
        assert_eq!(series[0].points.len(), 2);
        assert_eq!(series[0].points[0].date, "2026-05-31");
        assert_eq!(series[0].points[0].value, 50.0);
        assert_eq!(series[0].points[1].date, "2026-06-01");
        assert!((series[0].points[1].value - 65.0).abs() < 1e-9);

        let metrics = v.health_metrics_unified().unwrap();
        let hr = metrics.iter().find(|m| m.slug == "heart-rate").unwrap();
        assert_eq!(hr.sources[0].records, 3);
        assert_eq!(hr.sources[0].first_date, "2026-05-31");
        assert_eq!(hr.sources[0].last_date, "2026-06-01");

        // Modify only May (different size) — June's rows must be unaffected
        // and June's file must not need reparsing to stay correct.
        write_jsonl(
            &v,
            "health/oura/heartrate/2026-05.jsonl",
            &[
                json!({"bpm": 50, "source": "ppg", "timestamp": "2026-05-31T23:00:00+00:00"}),
                json!({"bpm": 54, "source": "ppg", "timestamp": "2026-05-31T23:30:00+00:00"}),
            ],
        );
        let series = v.health_series_unified("heart-rate", Bucket::Day).unwrap();
        assert_eq!(series[0].points.len(), 2);
        assert!((series[0].points[0].value - 52.0).abs() < 1e-9, "may updated");
        assert!((series[0].points[1].value - 65.0).abs() < 1e-9, "june untouched");
    }

    #[test]
    fn deleted_heartrate_month_drops_from_the_summary() {
        let v = temp_vault("summary-hr-delete");
        write_jsonl(
            &v,
            "health/oura/heartrate/2026-05.jsonl",
            &[json!({"bpm": 50, "source": "ppg", "timestamp": "2026-05-31T23:00:00+00:00"})],
        );
        write_jsonl(
            &v,
            "health/oura/heartrate/2026-06.jsonl",
            &[json!({"bpm": 60, "source": "ppg", "timestamp": "2026-06-01T00:00:00+00:00"})],
        );
        v.health_metrics_unified().unwrap();

        fs::remove_file(v.root().join("health/oura/heartrate/2026-05.jsonl")).unwrap();
        let series = v.health_series_unified("heart-rate", Bucket::Day).unwrap();
        assert_eq!(series[0].points.len(), 1);
        assert_eq!(series[0].points[0].date, "2026-06-01");

        let summary_body =
            fs::read_to_string(v.root().join(".trove/oura-summary.json")).unwrap();
        let summary: Value = serde_json::from_str(&summary_body).unwrap();
        assert!(summary["heartrate_months"].get("2026-05").is_none());
        assert!(summary["files"].get("heartrate/2026-05.jsonl").is_none());
        assert!(summary["heartrate_months"].get("2026-06").is_some());
    }
}

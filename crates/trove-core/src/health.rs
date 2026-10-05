//! Apple Health export import + time-series reads.
//!
//! Importing streams `export.xml` — read directly out of `export.zip` when
//! given the zip — with quick-xml. The file can be hundreds of MB, so it is
//! never loaded whole. Raw records land in `health/<metric>/<YYYY-MM>.csv`
//! (full fidelity, the source of truth); daily aggregates land in
//! `health/<metric>/daily.csv`, which is the level charts read so the UI
//! never has to chew through millions of rows. A human-readable
//! `health/index.md` and a rebuildable `.trove/health-summary.json` round
//! out the import.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter};
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use crate::health_sleep::{apple_sessions, AppleInterval};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};
use crate::vault::Vault;

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::file_mtime(&vault.root().join(SUMMARY_REL))
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<crate::registry::ImportOutcome> {
    let summary = vault.import_health_export(path, |p| progress(p))?;
    let records: u64 = summary.metrics.iter().map(|m| m.records).sum();
    Ok(crate::registry::ImportOutcome {
        headline: format!(
            "{records} records imported across {} metrics",
            summary.metrics.len()
        ),
        counts: [("records", records), ("metrics", summary.metrics.len() as u64)].into(),
    })
}

static IMPORT: crate::registry::ImportSpec = crate::registry::ImportSpec {
    signatures: &[],
    accepts: &["zip", "xml"],
    params: &[],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "health",
        name: "Apple Health",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import the iPhone Health app's export.zip — every metric, charted. Re-imports replace cleanly.",
        domain: "health",
        vault_path: "health/",
        toggleable: false,
        setup: &[
            "On the iPhone: Health app → your profile picture → Export All Health Data.",
            "AirDrop the export.zip to this Mac, then import it here or drop it on the Health tab.",
        ],
        caveats: "HealthKit has no Mac API at all, so imports stay manual — re-export occasionally to stay current. An auto-export watch folder (fed by an iOS shortcut/app) is the planned upgrade.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

const RECORD_HEADER: &[&str] = &["start", "end", "value", "unit", "source"];
const WORKOUT_HEADER: &[&str] = &[
    "start",
    "end",
    "type",
    "duration_min",
    "energy_kcal",
    "distance_km",
    "source",
];
const SUMMARY_REL: &str = ".trove/health-summary.json";
/// Rows buffered across all monthly CSVs before flushing to disk.
const FLUSH_THRESHOLD: usize = 64_000;

/// How daily values combine into coarser (week/month) buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum MetricKind {
    /// Daily totals; coarser buckets sum the days (steps, energy).
    Sum,
    /// Point measurements; coarser buckets take the weighted average (heart rate).
    Avg,
    /// Daily totals; coarser buckets average the days (sleep hours per night).
    SumThenAvg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct MetricSummary {
    /// Folder name under health/, e.g. "heart-rate".
    pub slug: String,
    pub name: String,
    pub unit: String,
    pub kind: MetricKind,
    pub records: u64,
    /// First/last day with data, YYYY-MM-DD.
    pub first_date: String,
    pub last_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct HealthSummary {
    pub imported_at: String,
    pub source: String,
    pub metrics: Vec<MetricSummary>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ImportProgress {
    pub records: u64,
    /// 0–100, based on bytes of XML consumed.
    pub percent: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "lowercase")]
pub enum Bucket {
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SeriesPoint {
    /// Bucket start day, YYYY-MM-DD.
    pub date: String,
    pub value: f64,
}

impl Vault {
    /// Import an Apple Health export (`export.zip` or `export.xml`) into
    /// `health/`. Replaces data for every metric present in the export.
    pub fn import_health_export<F>(&self, source: &Path, mut progress: F) -> Result<HealthSummary>
    where
        F: FnMut(ImportProgress),
    {
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let mut state = ImportState::new(self);
        if ext == "zip" {
            let file = File::open(source).with_context(|| format!("opening {}", source.display()))?;
            let mut archive = zip::ZipArchive::new(BufReader::new(file)).context("reading zip")?;
            let entry_name = archive
                .file_names()
                .find(|n| *n == "export.xml" || n.ends_with("/export.xml"))
                .map(str::to_owned)
                .context("no export.xml inside the zip — is this an Apple Health export?")?;
            let entry = archive.by_name(&entry_name)?;
            let total = entry.size();
            state.parse(BufReader::with_capacity(1 << 20, entry), total, &mut progress)?;
        } else {
            let total = fs::metadata(source)?.len();
            let file = File::open(source).with_context(|| format!("opening {}", source.display()))?;
            state.parse(BufReader::with_capacity(1 << 20, file), total, &mut progress)?;
        }
        let file_name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| source.display().to_string());
        state.finish(&file_name)
    }

    /// Metrics available for charting, from the last import.
    pub fn list_health_metrics(&self) -> Result<Vec<MetricSummary>> {
        let path = self.resolve(SUMMARY_REL)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let summary: HealthSummary =
            serde_json::from_str(&fs::read_to_string(path)?).context("parsing health summary")?;
        Ok(summary.metrics)
    }

    /// Time series for one metric, aggregated in Rust so the frontend never
    /// receives more than a few thousand points.
    pub fn health_series(&self, slug: &str, bucket: Bucket) -> Result<Vec<SeriesPoint>> {
        let kind = self
            .list_health_metrics()?
            .into_iter()
            .find(|m| m.slug == slug)
            .map(|m| m.kind)
            .with_context(|| format!("unknown health metric: {slug}"))?;
        let path = self.resolve(&format!("health/{slug}/daily.csv"))?;
        let mut reader = csv::Reader::from_path(&path)
            .with_context(|| format!("reading health/{slug}/daily.csv"))?;

        // bucket start -> (sum of daily sums, sum of daily counts, days with data)
        let mut buckets: BTreeMap<NaiveDate, (f64, f64, u64)> = BTreeMap::new();
        for row in reader.records() {
            let row = row?;
            let date = NaiveDate::parse_from_str(&row[0], "%Y-%m-%d")?;
            let count: f64 = row[1].parse().unwrap_or(0.0);
            let sum: f64 = row[2].parse().unwrap_or(0.0);
            let key = match bucket {
                Bucket::Day => date,
                Bucket::Week => date - chrono::Duration::days(date.weekday().num_days_from_monday() as i64),
                Bucket::Month => date.with_day(1).unwrap_or(date),
            };
            let e = buckets.entry(key).or_insert((0.0, 0.0, 0));
            e.0 += sum;
            e.1 += count;
            e.2 += 1;
        }

        Ok(buckets
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
            .collect())
    }
}

/// Running daily aggregate for one metric.
#[derive(Debug, Clone, Copy)]
struct Agg {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Agg {
    fn add(&mut self, v: f64) {
        self.count += 1;
        self.sum += v;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }
    fn new(v: f64) -> Self {
        Agg { count: 1, sum: v, min: v, max: v }
    }
}

struct MetricMeta {
    name: String,
    unit: String,
    kind: MetricKind,
    records: u64,
    daily: BTreeMap<NaiveDate, Agg>,
}

struct ImportState<'a> {
    vault: &'a Vault,
    /// rel path -> (header, buffered rows)
    rows: HashMap<String, (&'static [&'static str], Vec<Vec<String>>)>,
    /// Files already created (truncated) during this import.
    opened: HashSet<String>,
    pending: usize,
    metrics: HashMap<String, MetricMeta>,
    records: u64,
    /// Every `SleepAnalysis` interval, stitched into `health-sleep` sessions
    /// at the end of the import (the contract's Apple writer).
    sleep: Vec<AppleInterval>,
}

impl<'a> ImportState<'a> {
    fn new(vault: &'a Vault) -> Self {
        ImportState {
            vault,
            rows: HashMap::new(),
            opened: HashSet::new(),
            pending: 0,
            metrics: HashMap::new(),
            records: 0,
            sleep: Vec::new(),
        }
    }

    fn parse<R: BufRead>(
        &mut self,
        reader: R,
        total_bytes: u64,
        progress: &mut dyn FnMut(ImportProgress),
    ) -> Result<()> {
        let mut xml = Reader::from_reader(reader);
        let mut buf = Vec::with_capacity(4096);
        let mut events: u64 = 0;
        loop {
            match xml.read_event_into(&mut buf) {
                Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => match e.name().as_ref() {
                    b"Record" => self.record(e)?,
                    b"Workout" => self.workout(e)?,
                    _ => {}
                },
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(err) => bail!("XML parse error at byte {}: {err}", xml.buffer_position()),
            }
            buf.clear();
            events += 1;
            if events % 16_384 == 0 {
                let percent = if total_bytes > 0 {
                    (xml.buffer_position() as f64 / total_bytes as f64 * 100.0).min(100.0) as f32
                } else {
                    0.0
                };
                progress(ImportProgress { records: self.records, percent });
            }
        }
        progress(ImportProgress { records: self.records, percent: 100.0 });
        Ok(())
    }

    fn record(&mut self, e: &BytesStart) -> Result<()> {
        let mut ty = None;
        let mut unit = String::new();
        let mut value = None;
        let mut start = None;
        let mut end = None;
        let mut source = String::new();
        for a in e.attributes().flatten() {
            let Ok(v) = a.normalized_value(quick_xml::XmlVersion::Implicit1_0) else { continue };
            match a.key.as_ref() {
                b"type" => ty = Some(v.into_owned()),
                b"unit" => unit = v.into_owned(),
                b"value" => value = Some(v.into_owned()),
                b"startDate" => start = Some(v.into_owned()),
                b"endDate" => end = Some(v.into_owned()),
                b"sourceName" => source = v.into_owned(),
                _ => {}
            }
        }
        let Some(ty) = ty else { return Ok(()) };
        let start_str = start.unwrap_or_default();
        let end_str = end.unwrap_or_else(|| start_str.clone());
        let (Some(start_dt), Some(end_dt)) = (parse_date(&start_str), parse_date(&end_str)) else {
            return Ok(());
        };
        // A record belongs to the day it ended (so sleep crossing midnight
        // counts toward the morning you woke up).
        let day = end_dt.date_naive();

        if let Some(stripped) = ty.strip_prefix("HKCategoryTypeIdentifier") {
            let value = value.unwrap_or_default();
            match stripped {
                "SleepAnalysis" => {
                    let stage = value
                        .strip_prefix("HKCategoryValueSleepAnalysis")
                        .unwrap_or(&value)
                        .to_string();
                    let hours = (end_dt - start_dt).num_seconds() as f64 / 3600.0;
                    if stage.starts_with("Asleep") {
                        self.add("sleep", "Sleep", "hr", MetricKind::SumThenAvg, day, hours);
                    } else {
                        self.touch("sleep", "Sleep", "hr", MetricKind::SumThenAvg);
                    }
                    self.sleep.push(AppleInterval {
                        origin: source.clone(),
                        start: start_dt,
                        end: end_dt,
                        stage: stage.clone(),
                    });
                    self.push_row("sleep", &end_dt, vec![start_str, end_str, stage, String::new(), source])?;
                }
                "MindfulSession" => {
                    let minutes = (end_dt - start_dt).num_seconds() as f64 / 60.0;
                    self.add("mindful-minutes", "Mindful Minutes", "min", MetricKind::SumThenAvg, day, minutes);
                    self.push_row(
                        "mindful-minutes",
                        &end_dt,
                        vec![start_str, end_str, format_num(minutes), "min".into(), source],
                    )?;
                }
                _ => {
                    // Unknown category type: keep the raw value, chart as a daily count.
                    let slug = kebab(stripped);
                    let name = camel_to_words(stripped);
                    self.add(&slug, &name, "count", MetricKind::Sum, day, 1.0);
                    let short = value
                        .strip_prefix("HKCategoryValue")
                        .map(str::to_string)
                        .unwrap_or(value);
                    self.push_row(&slug, &end_dt, vec![start_str, end_str, short, unit, source])?;
                }
            }
            return Ok(());
        }

        // Quantity records (heart rate, steps, ...): numeric values only.
        let stripped = ty
            .strip_prefix("HKQuantityTypeIdentifier")
            .or_else(|| ty.strip_prefix("HKDataType"))
            .unwrap_or(&ty);
        let Some(v) = value.as_deref().and_then(|s| s.parse::<f64>().ok()) else {
            return Ok(());
        };
        let (slug, name, kind) = quantity_metric(stripped, &unit);
        self.add(&slug, &name, &unit, kind, day, v);
        self.push_row(
            &slug,
            &end_dt,
            vec![start_str, end_str, value.unwrap_or_default(), unit, source],
        )?;
        Ok(())
    }

    fn workout(&mut self, e: &BytesStart) -> Result<()> {
        let mut ty = String::new();
        let mut duration = None;
        let mut energy = String::new();
        let mut distance = String::new();
        let mut start = None;
        let mut end = None;
        let mut source = String::new();
        for a in e.attributes().flatten() {
            let Ok(v) = a.normalized_value(quick_xml::XmlVersion::Implicit1_0) else { continue };
            match a.key.as_ref() {
                b"workoutActivityType" => {
                    ty = v
                        .strip_prefix("HKWorkoutActivityType")
                        .map(str::to_string)
                        .unwrap_or_else(|| v.into_owned())
                }
                b"duration" => duration = v.parse::<f64>().ok(),
                b"totalEnergyBurned" => energy = v.into_owned(),
                b"totalDistance" => distance = v.into_owned(),
                b"startDate" => start = Some(v.into_owned()),
                b"endDate" => end = Some(v.into_owned()),
                b"sourceName" => source = v.into_owned(),
                _ => {}
            }
        }
        let start_str = start.unwrap_or_default();
        let end_str = end.unwrap_or_else(|| start_str.clone());
        let Some(end_dt) = parse_date(&end_str) else { return Ok(()) };
        let minutes = duration.unwrap_or(0.0);
        self.add("workouts", "Workouts", "min", MetricKind::Sum, end_dt.date_naive(), minutes);
        self.push_workout_row(
            &end_dt,
            vec![start_str, end_str, ty, format_num(minutes), energy, distance, source],
        )?;
        Ok(())
    }

    /// Register the metric (if new) and fold a value into its daily aggregate.
    fn add(&mut self, slug: &str, name: &str, unit: &str, kind: MetricKind, day: NaiveDate, v: f64) {
        let meta = self.touch(slug, name, unit, kind);
        meta.records += 1;
        meta.daily
            .entry(day)
            .and_modify(|a| a.add(v))
            .or_insert_with(|| Agg::new(v));
    }

    fn touch(&mut self, slug: &str, name: &str, unit: &str, kind: MetricKind) -> &mut MetricMeta {
        let meta = self.metrics.entry(slug.to_string()).or_insert_with(|| MetricMeta {
            name: name.to_string(),
            unit: unit.to_string(),
            kind,
            records: 0,
            daily: BTreeMap::new(),
        });
        if meta.unit.is_empty() && !unit.is_empty() {
            meta.unit = unit.to_string();
        }
        meta
    }

    fn push_row(&mut self, slug: &str, end: &DateTime<FixedOffset>, row: Vec<String>) -> Result<()> {
        let rel = format!("health/{slug}/{}.csv", end.format("%Y-%m"));
        self.buffer(rel, RECORD_HEADER, row)
    }

    fn push_workout_row(&mut self, end: &DateTime<FixedOffset>, row: Vec<String>) -> Result<()> {
        let rel = format!("health/workouts/{}.csv", end.format("%Y-%m"));
        self.buffer(rel, WORKOUT_HEADER, row)
    }

    fn buffer(&mut self, rel: String, header: &'static [&'static str], row: Vec<String>) -> Result<()> {
        self.records += 1;
        self.rows.entry(rel).or_insert_with(|| (header, Vec::new())).1.push(row);
        self.pending += 1;
        if self.pending >= FLUSH_THRESHOLD {
            self.flush()?;
        }
        Ok(())
    }

    // hand-rolled: stays off store.rs — these are CSV files with headers,
    // truncated on first open of an import run and appended thereafter (the
    // `opened` set is import-run state); JsonlStream is JSONL-only and has no
    // notion of headers or per-run truncation.
    fn flush(&mut self) -> Result<()> {
        for (rel, (header, rows)) in self.rows.drain() {
            let path = self.vault.resolve(&rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let fresh = self.opened.insert(rel.clone());
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(fresh)
                .append(!fresh)
                .open(&path)
                .with_context(|| format!("writing {rel}"))?;
            let mut w = csv::Writer::from_writer(BufWriter::new(file));
            if fresh {
                w.write_record(header)?;
            }
            for row in rows {
                w.write_record(&row)?;
            }
            w.flush()?;
        }
        self.pending = 0;
        Ok(())
    }

    /// Flush everything, write daily aggregates, the summary index and index.md.
    fn finish(mut self, source_name: &str) -> Result<HealthSummary> {
        self.flush()?;
        // The sleep contract: a full export replaces the whole source folder.
        let sessions = apple_sessions(std::mem::take(&mut self.sleep));
        self.vault.write_sleep_sessions("apple-health", &sessions, true)?;

        let mut summaries = Vec::new();
        for (slug, meta) in &self.metrics {
            if meta.daily.is_empty() {
                continue;
            }
            let path = self.vault.resolve(&format!("health/{slug}/daily.csv"))?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut w = csv::Writer::from_writer(BufWriter::new(File::create(&path)?));
            w.write_record(["date", "count", "sum", "min", "max", "avg"])?;
            for (day, agg) in &meta.daily {
                w.write_record([
                    day.format("%Y-%m-%d").to_string(),
                    agg.count.to_string(),
                    format_num(agg.sum),
                    format_num(agg.min),
                    format_num(agg.max),
                    format_num(agg.sum / agg.count as f64),
                ])?;
            }
            w.flush()?;
            summaries.push(MetricSummary {
                slug: slug.clone(),
                name: meta.name.clone(),
                unit: meta.unit.clone(),
                kind: meta.kind,
                records: meta.records,
                first_date: meta.daily.keys().next().unwrap().format("%Y-%m-%d").to_string(),
                last_date: meta.daily.keys().last().unwrap().format("%Y-%m-%d").to_string(),
            });
        }
        summaries.sort_by(|a, b| b.records.cmp(&a.records));

        let summary = HealthSummary {
            imported_at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            source: source_name.to_string(),
            metrics: summaries,
        };

        let json_path = self.vault.resolve(SUMMARY_REL)?;
        fs::write(&json_path, serde_json::to_string_pretty(&summary)?)?;

        let mut md = String::new();
        md.push_str("# Health Data\n\n");
        md.push_str(&format!(
            "Imported {} from `{}`.\n\nRaw records: one CSV per metric per month. \
             `daily.csv` in each folder holds per-day aggregates.\n\n",
            summary.imported_at, summary.source
        ));
        md.push_str("| Metric | Folder | Unit | Records | From | To |\n|---|---|---|---|---|---|\n");
        for m in &summary.metrics {
            md.push_str(&format!(
                "| {} | `{}` | {} | {} | {} | {} |\n",
                m.name, m.slug, m.unit, m.records, m.first_date, m.last_date
            ));
        }
        let md_path = self.vault.resolve("health/index.md")?;
        fs::write(&md_path, md)?;

        Ok(summary)
    }
}

/// slug, display name and aggregation kind for a quantity record type.
fn quantity_metric(stripped: &str, unit: &str) -> (String, String, MetricKind) {
    let known: Option<(&str, &str, MetricKind)> = match stripped {
        "HeartRate" => Some(("heart-rate", "Heart Rate", MetricKind::Avg)),
        "RestingHeartRate" => Some(("resting-heart-rate", "Resting Heart Rate", MetricKind::Avg)),
        "WalkingHeartRateAverage" => Some(("walking-heart-rate", "Walking Heart Rate", MetricKind::Avg)),
        "HeartRateVariabilitySDNN" => Some(("hrv", "Heart Rate Variability", MetricKind::Avg)),
        "StepCount" => Some(("steps", "Steps", MetricKind::Sum)),
        "DistanceWalkingRunning" => Some(("walking-distance", "Walking + Running Distance", MetricKind::Sum)),
        "DistanceCycling" => Some(("cycling-distance", "Cycling Distance", MetricKind::Sum)),
        "FlightsClimbed" => Some(("flights-climbed", "Flights Climbed", MetricKind::Sum)),
        "ActiveEnergyBurned" => Some(("active-energy", "Active Energy", MetricKind::Sum)),
        "BasalEnergyBurned" => Some(("basal-energy", "Resting Energy", MetricKind::Sum)),
        "AppleExerciseTime" => Some(("exercise-minutes", "Exercise Minutes", MetricKind::Sum)),
        "AppleStandTime" => Some(("stand-minutes", "Stand Minutes", MetricKind::Sum)),
        "BodyMass" => Some(("body-mass", "Weight", MetricKind::Avg)),
        "BodyMassIndex" => Some(("bmi", "Body Mass Index", MetricKind::Avg)),
        "VO2Max" => Some(("vo2-max", "VO2 Max", MetricKind::Avg)),
        "RespiratoryRate" => Some(("respiratory-rate", "Respiratory Rate", MetricKind::Avg)),
        "OxygenSaturation" => Some(("blood-oxygen", "Blood Oxygen", MetricKind::Avg)),
        "BloodPressureSystolic" => Some(("blood-pressure-systolic", "Blood Pressure (Systolic)", MetricKind::Avg)),
        "BloodPressureDiastolic" => Some(("blood-pressure-diastolic", "Blood Pressure (Diastolic)", MetricKind::Avg)),
        "WalkingSpeed" => Some(("walking-speed", "Walking Speed", MetricKind::Avg)),
        "EnvironmentalAudioExposure" => Some(("environmental-audio", "Environmental Audio Exposure", MetricKind::Avg)),
        "HeadphoneAudioExposure" => Some(("headphone-audio", "Headphone Audio Exposure", MetricKind::Avg)),
        _ => None,
    };
    match known {
        Some((slug, name, kind)) => (slug.to_string(), name.to_string(), kind),
        None => {
            // Plain counts accumulate; everything else averages.
            let kind = if unit == "count" { MetricKind::Sum } else { MetricKind::Avg };
            (kebab(stripped), camel_to_words(stripped), kind)
        }
    }
}

/// Apple Health dates look like "2024-01-15 08:30:21 -0800".
fn parse_date(s: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S %z").ok()
}

fn kebab(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn camel_to_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() && i > 0 && chars[i - 1].is_lowercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Shortest clean representation: no trailing ".0", no float noise.
fn format_num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{:.4}", v)
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    const FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE HealthData [
<!ELEMENT HealthData (Record|Workout)*>
]>
<HealthData locale="en_US">
 <Record type="HKQuantityTypeIdentifierHeartRate" sourceName="Apple Watch" unit="count/min" startDate="2024-01-15 08:30:00 -0800" endDate="2024-01-15 08:30:00 -0800" value="60"/>
 <Record type="HKQuantityTypeIdentifierHeartRate" sourceName="Apple Watch" unit="count/min" startDate="2024-01-15 09:30:00 -0800" endDate="2024-01-15 09:30:00 -0800" value="80"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="iPhone" unit="count" startDate="2024-01-15 10:00:00 -0800" endDate="2024-01-15 10:10:00 -0800" value="1000"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="iPhone" unit="count" startDate="2024-01-16 10:00:00 -0800" endDate="2024-01-16 10:10:00 -0800" value="2000"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="iPhone" unit="count" startDate="2024-02-01 10:00:00 -0800" endDate="2024-02-01 10:10:00 -0800" value="500"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Apple Watch" startDate="2024-01-15 23:00:00 -0800" endDate="2024-01-16 07:00:00 -0800" value="HKCategoryValueSleepAnalysisInBed"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Apple Watch" startDate="2024-01-15 23:00:00 -0800" endDate="2024-01-16 01:00:00 -0800" value="HKCategoryValueSleepAnalysisAsleepCore"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Apple Watch" startDate="2024-01-16 01:00:00 -0800" endDate="2024-01-16 02:30:00 -0800" value="HKCategoryValueSleepAnalysisAsleepREM"/>
 <Workout workoutActivityType="HKWorkoutActivityTypeRunning" duration="32.5" durationUnit="min" sourceName="Apple Watch" startDate="2024-01-15 17:00:00 -0800" endDate="2024-01-15 17:32:30 -0800"/>
</HealthData>
"#;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-health-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn import_fixture(v: &Vault, name: &str) -> HealthSummary {
        let xml_path = std::env::temp_dir().join(format!("trove-health-fixture-{}-{name}.xml", std::process::id()));
        fs::write(&xml_path, FIXTURE).unwrap();
        v.import_health_export(&xml_path, |_| {}).unwrap()
    }

    #[test]
    fn imports_xml_and_aggregates() {
        let v = temp_vault("xml");
        let summary = import_fixture(&v, "xml");

        let slugs: Vec<&str> = summary.metrics.iter().map(|m| m.slug.as_str()).collect();
        assert!(slugs.contains(&"heart-rate"));
        assert!(slugs.contains(&"steps"));
        assert!(slugs.contains(&"sleep"));
        assert!(slugs.contains(&"workouts"));

        // Raw monthly files exist.
        assert!(v.root().join("health/heart-rate/2024-01.csv").exists());
        assert!(v.root().join("health/steps/2024-02.csv").exists());
        assert!(v.root().join("health/index.md").exists());

        // Heart rate daily average: (60 + 80) / 2.
        let hr = v.health_series("heart-rate", Bucket::Day).unwrap();
        assert_eq!(hr.len(), 1);
        assert_eq!(hr[0].date, "2024-01-15");
        assert!((hr[0].value - 70.0).abs() < 1e-9);

        // Steps: daily sums, then monthly totals.
        let daily = v.health_series("steps", Bucket::Day).unwrap();
        assert_eq!(daily.len(), 3);
        assert_eq!(daily[0].value, 1000.0);
        let monthly = v.health_series("steps", Bucket::Month).unwrap();
        assert_eq!(monthly.len(), 2);
        assert_eq!(monthly[0].date, "2024-01-01");
        assert_eq!(monthly[0].value, 3000.0);
        assert_eq!(monthly[1].value, 500.0);

        // Sleep: 2h core + 1.5h REM on the wake day; InBed ignored.
        let sleep = v.health_series("sleep", Bucket::Day).unwrap();
        assert_eq!(sleep.len(), 1);
        assert_eq!(sleep[0].date, "2024-01-16");
        assert!((sleep[0].value - 3.5).abs() < 1e-9);

        // Workouts: total minutes.
        let w = v.health_series("workouts", Bucket::Day).unwrap();
        assert!((w[0].value - 32.5).abs() < 1e-9);

        assert!(v.list_health_metrics().unwrap().len() >= 4);
    }

    /// Exercises the buffered-flush path (>64k rows) and gives a rough feel
    /// for throughput. Run with: cargo test -p trove-core -- --ignored
    #[test]
    #[ignore]
    fn large_import_smoke() {
        let v = temp_vault("large");
        let xml_path =
            std::env::temp_dir().join(format!("trove-health-large-{}.xml", std::process::id()));
        {
            let mut f = std::io::BufWriter::new(File::create(&xml_path).unwrap());
            writeln!(f, r#"<?xml version="1.0" encoding="UTF-8"?>"#).unwrap();
            writeln!(f, "<HealthData>").unwrap();
            // ~200k heart-rate records, one every 5 minutes from 2024-01-01.
            for i in 0..200_000u64 {
                let mins = i * 5;
                let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()
                    + chrono::Duration::days((mins / 1440) as i64);
                let rem = mins % 1440;
                writeln!(
                    f,
                    r#" <Record type="HKQuantityTypeIdentifierHeartRate" sourceName="Watch" unit="count/min" startDate="{d} {h:02}:{m:02}:00 -0800" endDate="{d} {h:02}:{m:02}:00 -0800" value="{val}"/>"#,
                    d = date.format("%Y-%m-%d"),
                    h = rem / 60,
                    m = rem % 60,
                    val = 55 + (i % 60),
                )
                .unwrap();
            }
            writeln!(f, "</HealthData>").unwrap();
        }
        let start = std::time::Instant::now();
        let summary = v.import_health_export(&xml_path, |_| {}).unwrap();
        let elapsed = start.elapsed();
        let hr = summary.metrics.iter().find(|m| m.slug == "heart-rate").unwrap();
        assert_eq!(hr.records, 200_000);
        let daily = v.health_series("heart-rate", Bucket::Day).unwrap();
        assert_eq!(daily.len(), (200_000 * 5 / 1440 + 1) as usize);
        println!("imported 200k records in {elapsed:?}");
    }

    #[test]
    fn imports_from_zip() {
        let v = temp_vault("zip");
        let zip_path = std::env::temp_dir().join(format!("trove-health-{}.zip", std::process::id()));
        let file = File::create(&zip_path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        zw.start_file("apple_health_export/export.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(FIXTURE.as_bytes()).unwrap();
        zw.finish().unwrap();

        let summary = v.import_health_export(&zip_path, |_| {}).unwrap();
        assert!(summary.metrics.iter().any(|m| m.slug == "steps"));
        assert!(v.root().join("health/steps/daily.csv").exists());
    }

    #[test]
    fn reimport_replaces_rather_than_duplicates() {
        let v = temp_vault("reimport");
        import_fixture(&v, "reimport-1");
        import_fixture(&v, "reimport-2");
        // Sums would double if raw rows duplicated on re-import.
        let monthly = v.health_series("steps", Bucket::Month).unwrap();
        assert_eq!(monthly[0].value, 3000.0);
        let raw = fs::read_to_string(v.root().join("health/steps/2024-01.csv")).unwrap();
        assert_eq!(raw.lines().count(), 3, "header + two rows expected");
    }

    #[test]
    fn series_rejects_bad_metric() {
        let v = temp_vault("badmetric");
        import_fixture(&v, "badmetric");
        assert!(v.health_series("nope", Bucket::Day).is_err());
        assert!(v.health_series("../escape", Bucket::Day).is_err());
    }
}

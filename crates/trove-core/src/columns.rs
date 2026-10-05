//! The generic chart's read path: any numeric column in any JSONL table,
//! aggregated per day into a rebuildable index, served as a time series with
//! no mapping (docs/roadmap.md, "Read-side shape" constraints).
//!
//! A **table** is the unit a chart reads: a date-partitioned JSONL directory
//! (`calendar/events`, `health/sleep/oura`) or one undated JSONL file inside
//! a directory of them (`health/oura/daily_sleep`). Tables are discovered,
//! not registered — the same rule as streams — so anything a collector
//! writes is chartable the moment the file lands.
//!
//! The index (`.trove/columns/<table>.json`) holds, per partition file, per
//! day, the record count plus `(count, sum, min, max)` for every numeric
//! field — top level and one level down (`contributors.deep_sleep`,
//! `extra.efficiency`). It is built on first use and kept fresh per file by
//! size+mtime ([`Vault::ensure_index`]), so a chart over a decade of a
//! stream costs the index, not the stream, and a sync that appends to one
//! month re-parses one file. A record's day is [`record_date`]'s: `ts`,
//! `occurrence`, `start`, `day`, … in that order — the same attribution the
//! generic stream read uses, so a table pages and charts by one clock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::health::{Bucket, SeriesPoint};
use crate::query::record_date;
use crate::store::FileStamp;
use crate::vault::Vault;

/// The synthetic column every table has: records per day. `sum` totals
/// them over a bucket, `avg` is records per day, `count` is days with any.
pub const RECORDS_COLUMN: &str = "@records";

const INDEX_DIR: &str = ".trove/columns";
/// Bump when the per-day row shape or the flattening rule changes.
const INDEX_VERSION: u32 = 1;
/// Distinct numeric fields indexed per file; beyond this the rest are
/// ignored (a table with hundreds of numeric keys is a matrix, not a log).
const MAX_COLUMNS: usize = 200;

/// One chartable table, as discovered by [`Vault::list_tables`].
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct TableInfo {
    /// Vault-relative id: the directory for a dated stream
    /// (`calendar/events`), or `dir/stem` for one undated file
    /// (`health/oura/daily_sleep`).
    pub id: String,
    /// Directory holding the files.
    pub dir: String,
    /// File stem when the table is a single undated file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Date-partitioned: many files, one per day or month.
    pub dated: bool,
    /// Named vault-spec contract the directory sits under, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Oldest / newest partition key for a dated table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
}

/// One numeric column of a table, from its index.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ColumnInfo {
    /// Field name; nested one level as `outer.inner`; [`RECORDS_COLUMN`]
    /// for the per-day record count.
    pub name: String,
    /// Records carrying a numeric value for this field.
    pub records: u64,
    /// Distinct days with at least one value.
    pub days: u64,
    pub min: f64,
    pub max: f64,
    /// First / last day with a value, `YYYY-MM-DD`.
    pub first: String,
    pub last: String,
}

/// A table's chartable surface.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct TableColumns {
    pub id: String,
    /// Dated records in the table (undated records are not chartable and
    /// not counted).
    pub records: u64,
    pub days: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// [`RECORDS_COLUMN`] first, then every numeric field by name.
    pub columns: Vec<ColumnInfo>,
}

/// How a column's per-day values fold into a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "lowercase")]
pub enum Agg {
    Sum,
    /// Weighted by record count, so a week's average is the average of its
    /// records, not of its days. The default fold.
    #[default]
    Avg,
    Min,
    Max,
    /// Records with a value (for [`RECORDS_COLUMN`]: days with records).
    Count,
}

impl Agg {
    pub fn parse(s: &str) -> Option<Agg> {
        match s {
            "sum" => Some(Agg::Sum),
            "avg" => Some(Agg::Avg),
            "min" => Some(Agg::Min),
            "max" => Some(Agg::Max),
            "count" => Some(Agg::Count),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The index

/// `(count, sum, min, max)` — a tuple on disk to keep the index small.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Stat(u64, f64, f64, f64);

impl Stat {
    fn one(v: f64) -> Stat {
        Stat(1, v, v, v)
    }

    fn merge(&mut self, o: &Stat) {
        if self.0 == 0 {
            *self = *o;
            return;
        }
        self.0 += o.0;
        self.1 += o.1;
        self.2 = self.2.min(o.2);
        self.3 = self.3.max(o.3);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DayRow {
    /// Dated records that day, numeric fields or not.
    n: u64,
    /// field -> stat, for fields that had a numeric value.
    c: BTreeMap<String, Stat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartIndex {
    stamp: FileStamp,
    days: BTreeMap<String, DayRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ColumnIndex {
    version: u32,
    /// file stem -> its rows; a file re-indexes alone when its stamp moves.
    parts: BTreeMap<String, PartIndex>,
}

impl ColumnIndex {
    fn empty() -> Self {
        ColumnIndex { version: INDEX_VERSION, parts: BTreeMap::new() }
    }
}

/// A resolved table: which files it is made of.
struct Table {
    id: String,
    dir: String,
    /// `(stem, absolute path)`.
    files: Vec<(String, PathBuf)>,
}

/// Numeric fields of one record, flattened one level. Booleans are not
/// numbers; NaN/inf never occur in JSON but are filtered regardless.
fn numeric_fields(v: &Value, out: &mut Vec<(String, f64)>) {
    let Some(obj) = v.as_object() else { return };
    for (k, val) in obj {
        match val {
            Value::Number(n) => {
                if let Some(f) = n.as_f64().filter(|f| f.is_finite()) {
                    out.push((k.clone(), f));
                }
            }
            Value::Object(inner) => {
                for (k2, v2) in inner {
                    if let Value::Number(n) = v2 {
                        if let Some(f) = n.as_f64().filter(|f| f.is_finite()) {
                            out.push((format!("{k}.{k2}"), f));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Per-day rows for one JSONL file. Unparseable lines and undated records
/// are skipped (an undated record has no place on a time axis).
fn index_file(path: &Path) -> Result<BTreeMap<String, DayRow>> {
    let body = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut days: BTreeMap<String, DayRow> = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut fields = Vec::new();
    for line in body.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some(day) = record_date(&v) else { continue };
        let row = days.entry(day.to_string()).or_default();
        row.n += 1;
        fields.clear();
        numeric_fields(&v, &mut fields);
        for (name, f) in fields.drain(..) {
            if !seen.contains(&name) {
                if seen.len() >= MAX_COLUMNS {
                    continue;
                }
                seen.insert(name.clone());
            }
            row.c.entry(name).or_insert(Stat(0, 0.0, 0.0, 0.0)).merge(&Stat::one(f));
        }
    }
    Ok(days)
}

fn is_dated(stem: &str) -> bool {
    NaiveDate::parse_from_str(stem, "%Y-%m-%d").is_ok()
        || NaiveDate::parse_from_str(&format!("{stem}-01"), "%Y-%m-%d").is_ok()
}

/// `.jsonl` stems directly in `dir`, sorted.
fn jsonl_stems(dir: &Path) -> Result<Vec<String>> {
    let mut stems: Vec<String> = fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
                p.file_stem().map(|s| s.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .filter(|s| !s.starts_with('.'))
        .collect();
    stems.sort();
    Ok(stems)
}

fn bucket_start(day: NaiveDate, bucket: Bucket) -> NaiveDate {
    match bucket {
        Bucket::Day => day,
        Bucket::Week => day - chrono::Duration::days(day.weekday().num_days_from_monday() as i64),
        Bucket::Month => day.with_day(1).unwrap_or(day),
    }
}

impl Vault {
    /// Every chartable table in the vault: each dated stream as one table,
    /// each file of an undated directory as its own. Directory listings
    /// only; no file is opened and no index is built.
    pub fn list_tables(&self) -> Result<Vec<TableInfo>> {
        let mut out = Vec::new();
        for s in self.list_streams()? {
            if s.dated {
                out.push(TableInfo {
                    id: s.dir.clone(),
                    dir: s.dir.clone(),
                    file: None,
                    dated: true,
                    domain: s.domain.clone(),
                    first: s.first.clone(),
                    last: s.last.clone(),
                });
                continue;
            }
            let abs = self.resolve_user(&s.dir)?;
            for stem in jsonl_stems(&abs)? {
                out.push(TableInfo {
                    id: format!("{}/{stem}", s.dir),
                    dir: s.dir.clone(),
                    file: Some(stem),
                    dated: false,
                    domain: s.domain.clone(),
                    first: None,
                    last: None,
                });
            }
        }
        Ok(out)
    }

    /// A table id → its files. A directory whose stems are all dates is one
    /// table; a directory of named files is many, addressed as `dir/stem`.
    fn resolve_table(&self, id: &str) -> Result<Table> {
        let id = id.trim().trim_matches('/');
        if id.is_empty() {
            bail!("not a table: empty id");
        }
        let abs = self.resolve_user(id)?;
        if abs.is_dir() {
            let stems = jsonl_stems(&abs)?;
            if stems.is_empty() {
                bail!("not a table: {id} holds no .jsonl files");
            }
            if !stems.iter().all(|s| is_dated(s)) {
                bail!(
                    "{id} is a directory of tables — pick one: {}",
                    stems.iter().map(|s| format!("{id}/{s}")).collect::<Vec<_>>().join(", ")
                );
            }
            let files = stems.into_iter().map(|s| (s.clone(), abs.join(format!("{s}.jsonl")))).collect();
            return Ok(Table { id: id.to_string(), dir: id.to_string(), files });
        }
        let file = abs.with_extension("jsonl");
        if file.is_file() {
            let (dir, stem) = id.rsplit_once('/').unwrap_or(("", id));
            return Ok(Table {
                id: id.to_string(),
                dir: dir.to_string(),
                files: vec![(stem.to_string(), file)],
            });
        }
        bail!("not a table: {id}");
    }

    /// Bring the table's column index up to date and return it: stat every
    /// file, re-index the new or changed ones, drop rows of deleted ones.
    fn ensure_columns(&self, table: &Table) -> Result<ColumnIndex> {
        let rel = format!("{INDEX_DIR}/{}.json", table.id.replace('/', "__"));
        self.ensure_index(&rel, INDEX_VERSION, ColumnIndex::empty, |mut idx| {
            let mut dirty = false;
            let live: BTreeSet<&str> = table.files.iter().map(|(s, _)| s.as_str()).collect();
            let stale: Vec<String> = idx.parts.keys().filter(|k| !live.contains(k.as_str())).cloned().collect();
            for k in stale {
                idx.parts.remove(&k);
                dirty = true;
            }
            for (stem, path) in &table.files {
                let rel_file = if table.dir.is_empty() {
                    format!("{stem}.jsonl")
                } else {
                    format!("{}/{stem}.jsonl", table.dir)
                };
                let Some(stamp) = self.stat_file(&rel_file)? else { continue };
                if idx.parts.get(stem).is_some_and(|p| p.stamp == stamp) {
                    continue;
                }
                let days = index_file(path)?;
                idx.parts.insert(stem.clone(), PartIndex { stamp, days });
                dirty = true;
            }
            Ok((idx, dirty))
        })
    }

    /// The numeric columns of a table with their coverage — the picker
    /// behind "chart anything". Builds or refreshes the index.
    pub fn table_columns(&self, id: &str) -> Result<TableColumns> {
        let table = self.resolve_table(id)?;
        let idx = self.ensure_columns(&table)?;
        struct Acc {
            stat: Stat,
            days: u64,
            first: String,
            last: String,
        }
        let mut cols: BTreeMap<&str, Acc> = BTreeMap::new();
        let mut records = 0u64;
        let mut days = 0u64;
        let mut first: Option<&str> = None;
        let mut last: Option<&str> = None;
        for part in idx.parts.values() {
            for (day, row) in &part.days {
                records += row.n;
                days += 1;
                if first.is_none_or(|f| day.as_str() < f) {
                    first = Some(day);
                }
                if last.is_none_or(|l| day.as_str() > l) {
                    last = Some(day);
                }
                for (name, stat) in &row.c {
                    let e = cols.entry(name).or_insert(Acc {
                        stat: Stat(0, 0.0, 0.0, 0.0),
                        days: 0,
                        first: day.clone(),
                        last: day.clone(),
                    });
                    e.stat.merge(stat);
                    e.days += 1;
                    if day < &e.first {
                        e.first = day.clone();
                    }
                    if day > &e.last {
                        e.last = day.clone();
                    }
                }
            }
        }
        let mut columns = Vec::with_capacity(cols.len() + 1);
        if days > 0 {
            let per_day: Vec<u64> = idx.parts.values().flat_map(|p| p.days.values().map(|r| r.n)).collect();
            columns.push(ColumnInfo {
                name: RECORDS_COLUMN.into(),
                records,
                days,
                min: per_day.iter().copied().min().unwrap_or(0) as f64,
                max: per_day.iter().copied().max().unwrap_or(0) as f64,
                first: first.unwrap_or_default().to_string(),
                last: last.unwrap_or_default().to_string(),
            });
        }
        columns.extend(cols.into_iter().map(|(name, a)| ColumnInfo {
            name: name.to_string(),
            records: a.stat.0,
            days: a.days,
            min: a.stat.2,
            max: a.stat.3,
            first: a.first,
            last: a.last,
        }));
        Ok(TableColumns {
            id: table.id,
            records,
            days,
            first: first.map(str::to_string),
            last: last.map(str::to_string),
            columns,
        })
    }

    /// One column of one table as a bucketed series over `from..=to`
    /// (`YYYY-MM-DD`, inclusive; either bound may be empty for open). Reads
    /// the index only. Days without a value are absent, not zero — a chart
    /// can show the gap.
    pub fn table_series(
        &self,
        id: &str,
        column: &str,
        agg: Agg,
        bucket: Bucket,
        from: &str,
        to: &str,
    ) -> Result<Vec<SeriesPoint>> {
        let table = self.resolve_table(id)?;
        let idx = self.ensure_columns(&table)?;
        let mut buckets: BTreeMap<NaiveDate, Stat> = BTreeMap::new();
        for part in idx.parts.values() {
            for (day, row) in &part.days {
                if (!from.is_empty() && day.as_str() < from) || (!to.is_empty() && day.as_str() > to) {
                    continue;
                }
                let stat = if column == RECORDS_COLUMN {
                    Stat::one(row.n as f64)
                } else {
                    match row.c.get(column) {
                        Some(s) => *s,
                        None => continue,
                    }
                };
                let Ok(date) = NaiveDate::parse_from_str(day, "%Y-%m-%d") else { continue };
                buckets
                    .entry(bucket_start(date, bucket))
                    .or_insert(Stat(0, 0.0, 0.0, 0.0))
                    .merge(&stat);
            }
        }
        Ok(buckets
            .into_iter()
            .filter_map(|(date, s)| {
                let value = match agg {
                    Agg::Sum => s.1,
                    Agg::Avg => {
                        if s.0 == 0 {
                            return None;
                        }
                        s.1 / s.0 as f64
                    }
                    Agg::Min => s.2,
                    Agg::Max => s.3,
                    Agg::Count => s.0 as f64,
                };
                Some(SeriesPoint { date: date.format("%Y-%m-%d").to_string(), value })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-test-columns-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn write(v: &Vault, rel: &str, body: &str) {
        let p = v.root().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn tables_split_undated_dirs_per_file() {
        let v = temp_vault("tables");
        write(&v, "calendar/events/2026-09.jsonl", "{}\n");
        write(&v, "health/oura/daily_sleep.jsonl", "{}\n");
        write(&v, "health/oura/sleep.jsonl", "{}\n");
        write(&v, "health/oura/heartrate/2026-09.jsonl", "{}\n");
        let ids: Vec<String> = v.list_tables().unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            vec!["calendar/events", "health/oura/daily_sleep", "health/oura/sleep", "health/oura/heartrate"]
        );
        let t = v.list_tables().unwrap();
        assert_eq!(t[1].file.as_deref(), Some("daily_sleep"));
        assert!(!t[1].dated);
        assert!(t[0].dated);
        assert_eq!(t[0].domain.as_deref(), Some("calendar"));
    }

    #[test]
    fn columns_flatten_one_level_and_count_records() {
        let v = temp_vault("columns");
        write(
            &v,
            "health/oura/daily_sleep.jsonl",
            "{\"day\":\"2026-09-01\",\"score\":80,\"contributors\":{\"deep_sleep\":60},\"ok\":true,\"id\":\"a\"}\n\
             {\"day\":\"2026-09-01\",\"score\":90,\"contributors\":{\"deep_sleep\":70}}\n\
             {\"day\":\"2026-09-03\",\"score\":70}\n\
             {\"score\":999}\n\
             junk\n",
        );
        let cols = v.table_columns("health/oura/daily_sleep").unwrap();
        assert_eq!((cols.records, cols.days), (3, 2));
        assert_eq!(cols.first.as_deref(), Some("2026-09-01"));
        assert_eq!(cols.last.as_deref(), Some("2026-09-03"));
        let names: Vec<&str> = cols.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec![RECORDS_COLUMN, "contributors.deep_sleep", "score"]);
        let score = cols.columns.iter().find(|c| c.name == "score").unwrap();
        assert_eq!((score.records, score.days), (3, 2));
        assert_eq!((score.min, score.max), (70.0, 90.0));
        let recs = &cols.columns[0];
        assert_eq!((recs.min, recs.max), (1.0, 2.0));
    }

    #[test]
    fn series_aggregate_per_bucket_and_keep_gaps() {
        let v = temp_vault("series");
        write(
            &v,
            "health/oura/daily_sleep.jsonl",
            "{\"day\":\"2026-09-01\",\"score\":80}\n\
             {\"day\":\"2026-09-01\",\"score\":90}\n\
             {\"day\":\"2026-09-03\",\"score\":70}\n\
             {\"day\":\"2026-09-08\",\"score\":50}\n",
        );
        let id = "health/oura/daily_sleep";
        let avg = v.table_series(id, "score", Agg::Avg, Bucket::Day, "", "").unwrap();
        let pts: Vec<(&str, f64)> = avg.iter().map(|p| (p.date.as_str(), p.value)).collect();
        assert_eq!(pts, vec![("2026-09-01", 85.0), ("2026-09-03", 70.0), ("2026-09-08", 50.0)]);
        let sum = v.table_series(id, "score", Agg::Sum, Bucket::Day, "2026-09-02", "2026-09-05").unwrap();
        assert_eq!(sum.len(), 1);
        assert_eq!(sum[0].value, 70.0);
        // Week of Mon 2026-08-31 holds the 1st and 3rd; week of Mon 09-07 holds the 8th.
        let week = v.table_series(id, "score", Agg::Avg, Bucket::Week, "", "").unwrap();
        let pts: Vec<(&str, f64)> = week.iter().map(|p| (p.date.as_str(), p.value)).collect();
        assert_eq!(pts, vec![("2026-08-31", 80.0), ("2026-09-07", 50.0)]);
        let n = v.table_series(id, RECORDS_COLUMN, Agg::Sum, Bucket::Month, "", "").unwrap();
        assert_eq!((n[0].date.as_str(), n[0].value), ("2026-09-01", 4.0));
        let days = v.table_series(id, RECORDS_COLUMN, Agg::Count, Bucket::Month, "", "").unwrap();
        assert_eq!(days[0].value, 3.0);
        let mx = v.table_series(id, "score", Agg::Max, Bucket::Month, "", "").unwrap();
        assert_eq!(mx[0].value, 90.0);
    }

    #[test]
    fn index_refreshes_only_changed_files() {
        let v = temp_vault("refresh");
        write(&v, "calendar/events/2026-08.jsonl", "{\"start\":\"2026-08-30T09:00:00-07:00\"}\n");
        write(&v, "calendar/events/2026-09.jsonl", "{\"start\":\"2026-09-01T09:00:00-07:00\"}\n");
        let first = v.table_series("calendar/events", RECORDS_COLUMN, Agg::Sum, Bucket::Month, "", "").unwrap();
        assert_eq!(first.len(), 2);
        let index_path = v.root().join(".trove/columns/calendar__events.json");
        assert!(index_path.exists());
        let before: ColumnIndex = serde_json::from_str(&fs::read_to_string(&index_path).unwrap()).unwrap();
        // Append to September; August must keep its stamp.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(
            &v,
            "calendar/events/2026-09.jsonl",
            "{\"start\":\"2026-09-01T09:00:00-07:00\"}\n{\"start\":\"2026-09-02T09:00:00-07:00\"}\n",
        );
        let after_series =
            v.table_series("calendar/events", RECORDS_COLUMN, Agg::Sum, Bucket::Month, "", "").unwrap();
        assert_eq!(after_series[1].value, 2.0);
        let after: ColumnIndex = serde_json::from_str(&fs::read_to_string(&index_path).unwrap()).unwrap();
        assert_eq!(before.parts["2026-08"].stamp, after.parts["2026-08"].stamp);
        assert_ne!(before.parts["2026-09"].stamp, after.parts["2026-09"].stamp);
        // Deleting a file drops its rows.
        fs::remove_file(v.root().join("calendar/events/2026-08.jsonl")).unwrap();
        let gone = v.table_series("calendar/events", RECORDS_COLUMN, Agg::Sum, Bucket::Month, "", "").unwrap();
        assert_eq!(gone.len(), 1);
    }

    #[test]
    fn table_ids_are_jailed_and_directories_of_tables_are_refused() {
        let v = temp_vault("jail");
        write(&v, "health/oura/daily_sleep.jsonl", "{\"day\":\"2026-09-01\",\"score\":1}\n");
        write(&v, ".trove/columns/x.jsonl", "{\"day\":\"2026-09-01\",\"score\":1}\n");
        assert!(v.table_columns(".trove/columns/x").is_err());
        assert!(v.table_columns("../etc").is_err());
        assert!(v.table_columns("").is_err());
        assert!(v.table_columns("nope").is_err());
        let err = v.table_columns("health/oura").unwrap_err().to_string();
        assert!(err.contains("health/oura/daily_sleep"), "{err}");
    }
}

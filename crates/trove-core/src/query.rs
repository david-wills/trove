//! Generic, registry-driven read paths shared by the app's Tauri commands and
//! the `trove-mcp` server: "what sources exist", "what streams hold data",
//! and "give me a page of raw records" — the reads that need no per-domain
//! knowledge. Everything here is O(displayed) (directory listings, or the
//! partition files a page actually touches) and goes through
//! [`Vault::resolve_user`], so a caller-supplied path can neither escape the
//! vault nor reach `.trove/`.
//!
//! Typed, aggregated reads (health series, activity summaries, …) stay in
//! their domain modules; this module is the raw layer beneath them.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{bail, Result};
use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::Value;

use crate::contracts::DOMAINS;
use crate::integrations::{IntegrationKind, INTEGRATIONS};
use crate::store::Partition;
use crate::vault::Vault;

/// Hard cap on one page of [`Vault::read_stream_page`]. The GUI reads far
/// less; an agent asking for more should paginate.
pub const STREAM_PAGE_MAX: usize = 1000;

/// One page of a generic stream read, newest records first.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct StreamPage {
    pub records: Vec<Value>,
    /// Partition keys present (within the requested bounds), newest first.
    pub partitions: Vec<String>,
    /// Offset of the next page, or `None` when this page ended the stream.
    pub next_offset: Option<u32>,
}

/// One directory of JSONL files under the vault, as found by
/// [`Vault::list_streams`].
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct StreamInfo {
    /// Vault-relative directory, e.g. `correspondence/imessage`.
    pub dir: String,
    /// `.jsonl` files directly in the directory.
    pub files: u64,
    /// Total size of those files.
    pub bytes: u64,
    /// Oldest / newest partition key (`YYYY-MM` or `YYYY-MM-DD`) when the
    /// files are date-named; otherwise the lexically first / last stem.
    pub first: Option<String>,
    pub last: Option<String>,
    /// Every file stem is a date key — the stream is time-partitioned and
    /// [`Vault::read_stream_page`]'s `from`/`to` bounds apply. `false` for
    /// raw per-collection files such as `health/oura/sleep.jsonl`.
    pub dated: bool,
    /// The named vault-spec contract this directory sits under, if any.
    pub domain: Option<String>,
    /// Newest modification time of any file in the directory, RFC3339 local.
    pub modified: Option<String>,
}

/// One registry entry plus whether its vault folder holds anything —
/// [`list_sources`]'s row.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SourceInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: IntegrationKind,
    pub domain: &'static str,
    /// Where its data lands, vault-relative (a directory prefix).
    pub vault_path: &'static str,
    pub description: &'static str,
    /// The login it needs, by connection id; `None` for local sources.
    pub connection: Option<&'static str>,
    /// Anything at all under `vault_path` (or the def's own probe says so).
    pub has_data: bool,
    /// The def's "when did data last land" probe, when it has one and
    /// data exists — an RFC3339 stamp or a partition key, per source.
    pub last_data: Option<String>,
}

/// Every registry def with its data presence. Cheap: one directory probe
/// per def plus its own `last_data` hook.
pub fn list_sources(vault: &Vault) -> Vec<SourceInfo> {
    INTEGRATIONS
        .iter()
        .map(|def| {
            let last_data = def.last_data.and_then(|probe| probe(vault));
            let dir = vault.root().join(def.vault_path.trim_end_matches('/'));
            SourceInfo {
                id: def.id,
                name: def.name,
                kind: def.kind,
                domain: def.domain,
                vault_path: def.vault_path,
                description: def.description,
                connection: def.connection,
                has_data: last_data.is_some() || dir_has_files(&dir, 0),
                last_data,
            }
        })
        .collect()
}

/// Whether `dir` holds any regular file, looking at most `MAX_DEPTH` levels
/// down. Bounded so a probe over a large tree stays a handful of listings.
fn dir_has_files(dir: &Path, depth: usize) -> bool {
    const MAX_DEPTH: usize = 3;
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut subdirs = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.is_file() {
            return true;
        }
        if path.is_dir() && depth < MAX_DEPTH {
            subdirs.push(path);
        }
    }
    subdirs.iter().any(|d| dir_has_files(d, depth + 1))
}

/// A partition key is date-shaped (`YYYY-MM` or `YYYY-MM-DD`).
fn is_dated(key: &str) -> bool {
    Partition::Day.key(key).is_some_and(|k| k.len() == key.len())
        || Partition::Month.key(key).is_some_and(|k| k.len() == key.len())
}

/// Inclusive bound check on a partition key, comparing on the shorter of
/// the two lengths so a month key (`2026-09`) and a day bound
/// (`2026-09-10`) compare as "the month that contains the day". Keys that
/// aren't date-shaped ignore bounds (there is nothing to compare).
fn in_bounds(key: &str, from: Option<&str>, to: Option<&str>) -> bool {
    if !is_dated(key) {
        return true;
    }
    let cmp = |bound: &str| -> std::cmp::Ordering {
        let n = key.len().min(bound.len());
        match (key.get(..n), bound.get(..n)) {
            (Some(k), Some(b)) => k.cmp(b),
            _ => key.cmp(bound),
        }
    };
    from.map_or(true, |f| cmp(f) != std::cmp::Ordering::Less)
        && to.map_or(true, |t| cmp(t) != std::cmp::Ordering::Greater)
}

/// The date (`YYYY-MM-DD`) a record belongs to, read off its first
/// recognizable timestamp field. `ts` is the vault-wide convention
/// (`docs/vault-spec/conventions.md`); the others cover the snapshot-shaped
/// contracts (calendar `occurrence`/`start`, Oura `day`, imports with a
/// bare `date`). RFC3339 strings start with the date, so the first ten
/// characters are the day in the record's own local time. `None` when no
/// such field is present — such records are never filtered out.
pub fn record_date(r: &Value) -> Option<&str> {
    const KEYS: &[&str] = &["ts", "occurrence", "start", "day", "date", "timestamp", "time"];
    let obj = r.as_object()?;
    KEYS.iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .and_then(|s| s.get(..10))
        .filter(|d| Partition::Day.key(d).is_some())
}

impl Vault {
    /// Newest-first raw records from any JSONL directory under the vault —
    /// the generic "Recent data" read. `from`/`to` are inclusive bounds
    /// (`YYYY-MM-DD` or `YYYY-MM`): they select the partition files to read
    /// (a month partition is kept when it contains a day bound), and a
    /// day-precision bound additionally drops records inside those files
    /// whose own date (see [`record_date`]) falls outside it, so "one day"
    /// of a month-partitioned stream is one day, not the whole month.
    /// `limit` is clamped to [`STREAM_PAGE_MAX`]; `max_bytes`, when given,
    /// ends the page early once the serialized records exceed it (at least
    /// one record is always returned), so a stream of large records (email
    /// bodies) can't produce a page a client refuses. `next_offset` counts
    /// records after filtering, so paging with it is consistent. Only the
    /// partition files the page touches are read. Refuses paths that escape
    /// the vault or enter `.trove/`.
    pub fn read_stream_page(
        &self,
        dir: &str,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        offset: usize,
        max_bytes: Option<usize>,
    ) -> Result<StreamPage> {
        let dir = dir.trim().trim_matches('/');
        if dir.is_empty() {
            bail!("not a readable stream: empty path");
        }
        let abs = self.resolve_user(dir)?;
        if !abs.is_dir() {
            bail!("not a readable stream: {dir} is not a directory in the vault");
        }
        let limit = limit.clamp(1, STREAM_PAGE_MAX);
        // Partition granularity only matters for appends; `partitions()`
        // lists every `.jsonl` stem regardless.
        let stream = self.stream(dir, Partition::Day);
        let mut partitions = stream.partitions()?;
        partitions.retain(|k| in_bounds(k, from, to));
        partitions.reverse();
        // Day-precision bounds also apply per record (a month file holds
        // the whole month); month-precision bounds are fully expressed by
        // the partition choice.
        let day_from = from.filter(|f| f.len() == 10);
        let day_to = to.filter(|t| t.len() == 10);
        let keep = |r: &Value| match record_date(r) {
            Some(d) => {
                day_from.map_or(true, |f| d >= f) && day_to.map_or(true, |t| d <= t)
            }
            None => true,
        };
        let mut records = Vec::with_capacity(limit);
        let mut skip = offset;
        let mut more = false;
        let mut bytes = 0usize;
        'outer: for key in &partitions {
            let mut rows: Vec<Value> = stream.read(key)?;
            rows.reverse();
            for r in rows.into_iter().filter(keep) {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if records.len() >= limit {
                    more = true;
                    break 'outer;
                }
                if let Some(budget) = max_bytes {
                    bytes += r.to_string().len();
                    if bytes > budget && !records.is_empty() {
                        more = true;
                        break 'outer;
                    }
                }
                records.push(r);
            }
        }
        let next_offset = more.then(|| (offset + records.len()) as u32);
        Ok(StreamPage { records, partitions, next_offset })
    }

    /// Every directory under the vault holding `.jsonl` files directly,
    /// with its partition range — a map of what can be read with
    /// [`Vault::read_stream_page`]. Directory listings only; no file is
    /// opened. `.trove/`, `artifacts/`, and `inbox/` are not streams.
    pub fn list_streams(&self) -> Result<Vec<StreamInfo>> {
        const MAX_DEPTH: usize = 5;
        fn walk(vault: &Vault, dir: &Path, rel: &str, depth: usize, out: &mut Vec<StreamInfo>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            let mut stems = Vec::new();
            let mut bytes = 0u64;
            let mut newest: Option<SystemTime> = None;
            let mut subdirs = Vec::new();
            for e in entries.flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    if depth == 0 && (name == "artifacts" || name == "inbox") {
                        continue;
                    }
                    subdirs.push((path, name));
                } else if path.extension().is_some_and(|x| x == "jsonl") {
                    if let Some(stem) = path.file_stem() {
                        stems.push(stem.to_string_lossy().into_owned());
                    }
                    if let Ok(m) = e.metadata() {
                        bytes += m.len();
                        if let Ok(t) = m.modified() {
                            if newest.is_none_or(|b| t > b) {
                                newest = Some(t);
                            }
                        }
                    }
                }
            }
            if !stems.is_empty() {
                stems.sort();
                let dated = stems.iter().all(|s| is_dated(s));
                out.push(StreamInfo {
                    dir: rel.to_string(),
                    files: stems.len() as u64,
                    bytes,
                    first: stems.first().cloned(),
                    last: stems.last().cloned(),
                    dated,
                    domain: DOMAINS
                        .iter()
                        .find(|c| rel == c.root || rel.starts_with(&format!("{}/", c.root)))
                        .map(|c| c.id.to_string()),
                    modified: newest.map(|t| DateTime::<Local>::from(t).to_rfc3339()),
                });
            }
            if depth < MAX_DEPTH {
                subdirs.sort_by(|a, b| a.1.cmp(&b.1));
                for (path, name) in subdirs {
                    let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                    walk(vault, &path, &child, depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self, self.root(), "", 0, &mut out);
        out.sort_by(|a, b| a.dir.cmp(&b.dir));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-test-query-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn write(v: &Vault, rel: &str, body: &str) {
        let p = v.root().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn bounds_compare_on_the_shorter_key() {
        assert!(in_bounds("2026-09", Some("2026-09-10"), None));
        assert!(!in_bounds("2026-08", Some("2026-09-10"), None));
        assert!(in_bounds("2026-09-05", Some("2026-09-05"), Some("2026-09-05")));
        assert!(!in_bounds("2026-09-05", Some("2026-09-06"), None));
        assert!(!in_bounds("2026-09-05", None, Some("2026-09-04")));
        assert!(in_bounds("2026-09-05", None, Some("2026-09")));
        // Non-date stems ignore bounds.
        assert!(in_bounds("sleep", Some("2026-09-01"), Some("2026-09-02")));
    }

    #[test]
    fn page_is_newest_first_bounded_and_paginated() {
        let v = temp_vault("page");
        write(&v, "activity/2026-09-13.jsonl", "{\"n\":1}\n{\"n\":2}\n");
        write(&v, "activity/2026-09-14.jsonl", "{\"n\":3}\nnot json\n{\"n\":4}\n");
        write(&v, "activity/2026-09-15.jsonl", "{\"n\":5}\n");

        let all = v.read_stream_page("activity", None, None, 10, 0, None).unwrap();
        let ns: Vec<u64> = all.records.iter().map(|r| r["n"].as_u64().unwrap()).collect();
        assert_eq!(ns, vec![5, 4, 3, 2, 1]);
        assert_eq!(all.partitions, vec!["2026-09-15", "2026-09-14", "2026-09-13"]);
        assert_eq!(all.next_offset, None);

        let first = v.read_stream_page("activity/", None, None, 2, 0, None).unwrap();
        assert_eq!(first.records.len(), 2);
        assert_eq!(first.next_offset, Some(2));
        let second = v.read_stream_page("activity", None, None, 2, 2, None).unwrap();
        let ns: Vec<u64> = second.records.iter().map(|r| r["n"].as_u64().unwrap()).collect();
        assert_eq!(ns, vec![3, 2]);
        let last = v.read_stream_page("activity", None, None, 2, 4, None).unwrap();
        assert_eq!(last.records.len(), 1);
        assert_eq!(last.next_offset, None);

        let bounded = v
            .read_stream_page("activity", Some("2026-09-14"), Some("2026-09-14"), 10, 0, None)
            .unwrap();
        assert_eq!(bounded.partitions, vec!["2026-09-14"]);
        assert_eq!(bounded.records.len(), 2);
    }

    #[test]
    fn day_bounds_filter_records_inside_a_month_partition() {
        let v = temp_vault("dayfilter");
        write(
            &v,
            "calendar/events/2026-09.jsonl",
            "{\"occurrence\":\"2026-09-07T09:00:00-07:00\",\"title\":\"a\"}\n\
             {\"occurrence\":\"2026-09-08T10:00:00-07:00\",\"title\":\"b\"}\n\
             {\"occurrence\":\"2026-09-08T14:00:00-07:00\",\"title\":\"c\"}\n\
             {\"title\":\"undated stays\"}\n\
             {\"occurrence\":\"2026-09-09T10:00:00-07:00\",\"title\":\"d\"}\n",
        );
        let day = v
            .read_stream_page("calendar/events", Some("2026-09-08"), Some("2026-09-08"), 10, 0, None)
            .unwrap();
        let titles: Vec<&str> = day.records.iter().map(|r| r["title"].as_str().unwrap()).collect();
        assert_eq!(titles, vec!["undated stays", "c", "b"]);
        assert_eq!(day.partitions, vec!["2026-09"]);
        // A month bound leaves the file's records alone.
        let month = v.read_stream_page("calendar/events", Some("2026-09"), Some("2026-09"), 10, 0, None).unwrap();
        assert_eq!(month.records.len(), 5);
        // Paging counts filtered records.
        let p1 = v.read_stream_page("calendar/events", Some("2026-09-08"), None, 2, 0, None).unwrap();
        assert_eq!(p1.records.len(), 2);
        assert_eq!(p1.next_offset, Some(2));
        let p2 = v.read_stream_page("calendar/events", Some("2026-09-08"), None, 2, 2, None).unwrap();
        let titles: Vec<&str> = p2.records.iter().map(|r| r["title"].as_str().unwrap()).collect();
        assert_eq!(titles, vec!["c", "b"]);
        assert_eq!(p2.next_offset, None);
    }

    #[test]
    fn day_bounds_filter_records_in_undated_files_too() {
        let v = temp_vault("undated");
        write(
            &v,
            "health/oura/daily_sleep.jsonl",
            "{\"day\":\"2026-08-30\",\"score\":80}\n{\"day\":\"2026-09-08\",\"score\":81}\n{\"day\":\"2026-09-09\",\"score\":82}\n",
        );
        let p = v
            .read_stream_page("health/oura", Some("2026-09-01"), Some("2026-09-08"), 10, 0, None)
            .unwrap();
        assert_eq!(p.partitions, vec!["daily_sleep"]);
        assert_eq!(p.records.len(), 1);
        assert_eq!(p.records[0]["score"], 81);
    }

    #[test]
    fn byte_budget_ends_a_page_early_but_returns_at_least_one() {
        let v = temp_vault("bytes");
        let big = "x".repeat(500);
        let body: String = (0..5).map(|i| format!("{{\"n\":{i},\"body\":\"{big}\"}}\n")).collect();
        write(&v, "correspondence/email/2026-09.jsonl", &body);
        let p = v.read_stream_page("correspondence/email", None, None, 100, 0, Some(1200)).unwrap();
        assert_eq!(p.records.len(), 2);
        assert_eq!(p.next_offset, Some(2));
        let tiny = v.read_stream_page("correspondence/email", None, None, 100, 2, Some(1)).unwrap();
        assert_eq!(tiny.records.len(), 1);
        assert_eq!(tiny.next_offset, Some(3));
        let rest = v.read_stream_page("correspondence/email", None, None, 100, 3, None).unwrap();
        assert_eq!(rest.records.len(), 2);
        assert_eq!(rest.next_offset, None);
    }

    #[test]
    fn record_date_reads_the_conventional_fields() {
        use serde_json::json;
        assert_eq!(record_date(&json!({"ts":"2026-09-08T10:00:00-07:00"})), Some("2026-09-08"));
        assert_eq!(record_date(&json!({"title":"x","occurrence":"2026-09-08T10:00:00-07:00"})), Some("2026-09-08"));
        assert_eq!(record_date(&json!({"day":"2026-09-08"})), Some("2026-09-08"));
        assert_eq!(record_date(&json!({"ts":"yesterday"})), None);
        assert_eq!(record_date(&json!({"title":"x"})), None);
        assert_eq!(record_date(&json!([1])), None);
    }

    #[test]
    fn page_refuses_trove_and_escapes() {
        let v = temp_vault("jail");
        write(&v, ".trove/secrets/2026-09-01.jsonl", "{\"token\":\"x\"}\n");
        assert!(v.read_stream_page(".trove/secrets", None, None, 10, 0, None).is_err());
        assert!(v.read_stream_page(".Trove/secrets", None, None, 10, 0, None).is_err());
        assert!(v.read_stream_page("activity/../.trove/secrets", None, None, 10, 0, None).is_err());
        assert!(v.read_stream_page("../", None, None, 10, 0, None).is_err());
        assert!(v.read_stream_page("/etc", None, None, 10, 0, None).is_err());
        assert!(v.read_stream_page("", None, None, 10, 0, None).is_err());
        assert!(v.read_stream_page("nope", None, None, 10, 0, None).is_err());
    }

    #[test]
    fn streams_are_discovered_with_ranges_and_contracts() {
        let v = temp_vault("streams");
        write(&v, "correspondence/imessage/2026-08.jsonl", "{}\n");
        write(&v, "correspondence/imessage/2026-09.jsonl", "{}\n");
        write(&v, "health/oura/sleep.jsonl", "{}\n");
        write(&v, "health/oura/daily_sleep.jsonl", "{}\n");
        write(&v, "health/steps/daily.csv", "date,count\n");
        write(&v, ".trove/live/browser-1.jsonl", "{}\n");
        write(&v, "artifacts/note.jsonl", "{}\n");

        let streams = v.list_streams().unwrap();
        let dirs: Vec<&str> = streams.iter().map(|s| s.dir.as_str()).collect();
        assert_eq!(dirs, vec!["correspondence/imessage", "health/oura"]);
        let im = &streams[0];
        assert_eq!((im.files, im.dated), (2, true));
        assert_eq!(im.first.as_deref(), Some("2026-08"));
        assert_eq!(im.last.as_deref(), Some("2026-09"));
        assert_eq!(im.domain.as_deref(), Some("correspondence"));
        let oura = &streams[1];
        assert!(!oura.dated);
        assert_eq!(oura.domain, None);
    }

    #[test]
    fn sources_report_data_presence() {
        let v = temp_vault("sources");
        let before = list_sources(&v);
        assert!(before.iter().any(|s| s.id == "imessage"));
        assert!(before.iter().all(|s| !s.has_data));
        write(&v, "correspondence/imessage/2026-09.jsonl", "{}\n");
        let after = list_sources(&v);
        assert!(after.iter().find(|s| s.id == "imessage").unwrap().has_data);
    }
}

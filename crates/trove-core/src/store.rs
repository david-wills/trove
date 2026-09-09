//! The shared write substrate: every vault store is built from a small set
//! of file mechanics, consolidated here so collectors get the conventions
//! for free instead of hand-rolling them.
//!
//! - [`JsonlStream`] — a directory of date-partitioned append-only JSONL
//!   files (`<dir>/YYYY-MM-DD.jsonl` or `<dir>/YYYY-MM.jsonl`), the shape of
//!   every event stream in the vault.
//! - [`Vault::write_snapshot`] / [`Vault::read_snapshot`] — atomically
//!   rewritten JSONL state files (the snapshot half of the snapshot+events
//!   pattern).
//! - [`write_atomic`] / [`write_json_atomic`] / [`write_atomic_secret`] —
//!   the one true atomic write (sibling tmp + rename; 0600 first for
//!   secrets).
//! - [`FileStamp`] / [`Vault::stat_file`] / [`Vault::ensure_index`] — the
//!   rebuildable-`.trove/`-index machinery: a size+mtime staleness stamp for
//!   one source file, and a generic read-check-rebuild-write frame for a
//!   versioned JSON index (the domain-specific file walk stays with the
//!   caller — see [`Vault::ensure_index`]'s doc for why).
//!
//! These helpers produce **byte-identical output** to the hand-rolled
//! writers they replace — this module consolidates code, never changes file
//! formats. Domain semantics (dedupe strategy, diff engines, what counts as
//! an event) stay in the domain modules; only file mechanics live here.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::vault::Vault;

/// Partition granularity for a [`JsonlStream`]: which prefix of an RFC3339
/// timestamp names the file (RFC3339 starts `YYYY-MM-DD`, so both prefixes
/// sort lexically).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partition {
    /// One file per local day: `YYYY-MM-DD.jsonl`.
    Day,
    /// One file per local month: `YYYY-MM.jsonl`.
    Month,
}

impl Partition {
    /// The partition key of a timestamp (its day or month prefix), or `None`
    /// if the timestamp can't yield one — too short, a non-UTF-8-boundary
    /// prefix, or a prefix that isn't date-shaped (`YYYY-MM[-DD]`). Never
    /// panics, and never guesses: a malformed timestamp must surface as an
    /// error at the write site ([`JsonlStream::append`] propagates it), not
    /// file records into a garbage-named partition.
    pub fn key<'a>(&self, ts: &'a str) -> Option<&'a str> {
        let n = match self {
            Partition::Day => 10,
            Partition::Month => 7,
        };
        let key = ts.get(..n)?;
        key.bytes()
            .enumerate()
            .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })
            .then_some(key)
    }
}

/// A directory of date-partitioned, append-only JSONL files under the vault.
/// Cheap to construct per use: `vault.stream("activity", Partition::Day)`.
pub struct JsonlStream<'v> {
    vault: &'v Vault,
    dir: String,
    partition: Partition,
    flock: bool,
}

impl Vault {
    /// An append-only JSONL stream at the vault-relative directory `dir`.
    pub fn stream(&self, dir: &str, partition: Partition) -> JsonlStream<'_> {
        JsonlStream { vault: self, dir: dir.to_string(), partition, flock: false }
    }
}

impl JsonlStream<'_> {
    /// Take an exclusive flock per partition file while appending — for the
    /// rare stream with multiple legitimate writer processes (`browser/`:
    /// the watcher's history sync plus one extension host per Chrome
    /// profile). Writers hold it for microseconds, so blocking is fine.
    pub fn with_flock(mut self) -> Self {
        self.flock = true;
        self
    }

    /// Append records to their partition files, grouped by `ts(record)`.
    /// One JSON object per line, newline-terminated; parent directories are
    /// created on demand. Callers are responsible for not re-appending
    /// records the vault already holds (cursors for live collectors, guid
    /// dedupe for imports — dedupe is a domain decision, not file mechanics).
    ///
    /// A record whose timestamp can't yield a partition key (not an RFC3339
    /// `YYYY-MM[-DD]…` prefix — see [`Partition::key`]) is an error: the
    /// whole call fails *before any file is touched*, so nothing is silently
    /// dropped or misfiled. A malformed ts is a programmer error in the
    /// calling collector; it should fail loudly, not panic and not land in a
    /// garbage-named file.
    pub fn append<T: Serialize>(&self, records: &[T], ts: impl Fn(&T) -> &str) -> Result<()> {
        let mut by_key: BTreeMap<&str, Vec<&T>> = BTreeMap::new();
        for r in records {
            let t = ts(r);
            let key = self.partition.key(t).with_context(|| {
                format!(
                    "cannot partition record into {}/: timestamp {t:?} has no {:?} date prefix",
                    self.dir, self.partition
                )
            })?;
            by_key.entry(key).or_default().push(r);
        }
        for (key, rs) in by_key {
            let rel = format!("{}/{key}.jsonl", self.dir);
            let path = self.vault.resolve(&rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {rel}"))?;
            #[cfg(unix)]
            if self.flock {
                use std::os::unix::io::AsRawFd;
                let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
                if rc != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("locking {rel}"));
                }
            }
            for r in rs {
                writeln!(f, "{}", serde_json::to_string(r)?)?;
            }
            // Dropping `f` releases the flock.
        }
        Ok(())
    }

    /// Every record of one partition, in file order. Missing file = empty;
    /// blank and unparseable lines are skipped (lenient by convention — one
    /// collector writing something extra must never break a reader).
    pub fn read<T: DeserializeOwned>(&self, key: &str) -> Result<Vec<T>> {
        let rel = format!("{}/{key}.jsonl", self.dir);
        let path = self.vault.resolve(&rel)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<T>(l).ok())
            .collect())
    }

    /// The partition keys present on disk, sorted ascending (date-named
    /// files sort lexically). Missing directory = empty.
    pub fn partitions(&self) -> Result<Vec<String>> {
        let path = self.vault.resolve(&self.dir)?;
        let Ok(entries) = fs::read_dir(&path) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        out.sort();
        Ok(out)
    }
}

impl Vault {
    /// Atomically rewrite a JSONL snapshot file (sibling tmp + rename), one
    /// record per line — the snapshot half of the snapshot+events pattern.
    pub fn write_snapshot<T: Serialize>(&self, rel: &str, records: &[T]) -> Result<()> {
        let path = self.resolve(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        for r in records {
            body.push_str(&serde_json::to_string(r)?);
            body.push('\n');
        }
        let tmp = path.with_extension("jsonl.tmp");
        fs::write(&tmp, body).with_context(|| format!("writing {rel}"))?;
        fs::rename(&tmp, &path).with_context(|| format!("publishing {rel}"))?;
        Ok(())
    }

    /// Read a JSONL snapshot leniently: missing file = empty, blank and
    /// unparseable lines skipped.
    pub fn read_snapshot<T: DeserializeOwned>(&self, rel: &str) -> Result<Vec<T>> {
        let path = self.resolve(rel)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<T>(l).ok())
            .collect())
    }
}

/// A source file's identity for staleness detection. Equality compare,
/// never "newer than": a restored backup, clock skew, or hand-rollback all
/// read as "changed" and trigger a reparse rather than being silently
/// missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileStamp {
    pub(crate) size: u64,
    pub(crate) mtime_ms: i64,
}

impl Vault {
    /// `(size, mtime_ms)` of a vault-relative file, or `None` if it doesn't
    /// exist. Callers stat before parsing so a file that changes again
    /// mid-read is caught as stale on the *next* check rather than silently
    /// paired with the wrong stamp.
    pub(crate) fn stat_file(&self, rel: &str) -> Result<Option<FileStamp>> {
        let path = self.resolve(rel)?;
        match fs::metadata(&path) {
            Ok(m) => Ok(Some(FileStamp {
                size: m.len(),
                mtime_ms: m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("stat {rel}")),
        }
    }

    /// Read-check-rebuild-write frame for a rebuildable JSON index cached at
    /// a `.trove/`-relative path (the pattern behind every `.trove/
    /// <domain>-summary.json`).
    ///
    /// The on-disk shape must carry a top-level `"version"` field (every
    /// summary struct does, by convention). Missing, unparseable, or a
    /// `version` other than the one passed in all start from `default()`
    /// and force a rewrite; otherwise the existing value loads as-is.
    /// Either way the (possibly-default) value is handed to `rebuild`, which
    /// walks whatever source files it owns — stat-before-parse via
    /// [`FileStamp`] / [`Vault::stat_file`], reparsing only what changed —
    /// and returns the updated value plus whether anything changed.
    /// [`write_json_atomic`] publishes the result only when something is
    /// actually dirty: the load needed repairing, or `rebuild` says so.
    ///
    /// Deliberately stops here: enumerating *which* source files to walk is
    /// domain-specific (a fixed collection set plus a dynamic month-file set
    /// for Oura; something else entirely for the next domain summary), so
    /// that part stays in the caller's `rebuild` closure rather than being
    /// forced into one generic shape.
    pub(crate) fn ensure_index<T, D, R>(&self, rel: &str, version: u32, default: D, rebuild: R) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        D: FnOnce() -> T,
        R: FnOnce(T) -> Result<(T, bool)>,
    {
        let (loaded, load_dirty) = self.load_versioned_index(rel, version, default);
        let (value, rebuild_dirty) = rebuild(loaded)?;
        if load_dirty || rebuild_dirty {
            write_json_atomic(&self.resolve(rel)?, &value)?;
        }
        Ok(value)
    }

    /// The persisted index at `rel`, or `default()` plus `true` when the
    /// file is missing/unparseable/a different version — a "needs rewrite"
    /// flag so a corrupt or stale-shape file gets replaced by a valid
    /// default even if [`Vault::ensure_index`]'s `rebuild` closure finds no
    /// source files changed on its own.
    fn load_versioned_index<T: DeserializeOwned>(
        &self,
        rel: &str,
        version: u32,
        default: impl FnOnce() -> T,
    ) -> (T, bool) {
        let Ok(path) = self.resolve(rel) else {
            return (default(), false);
        };
        if !path.exists() {
            return (default(), false);
        }
        let parsed = fs::read_to_string(&path).ok().and_then(|body| {
            let value: serde_json::Value = serde_json::from_str(&body).ok()?;
            if value.get("version")?.as_u64()? != version as u64 {
                return None;
            }
            serde_json::from_value::<T>(value).ok()
        });
        match parsed {
            Some(v) => (v, false),
            None => (default(), true),
        }
    }
}

/// Atomic write: sibling tmp + rename, parent dirs created on demand. A
/// crash or concurrent reader can never see a torn file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_sibling(path);
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("publishing {}", path.display()))?;
    Ok(())
}

/// [`write_atomic`] of pretty-printed JSON.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, serde_json::to_string_pretty(value)?.as_bytes())
}

/// Atomic write for secrets: the tmp file is chmod 0600 *before* the rename,
/// so the published file can never be briefly world-readable. This matters
/// most for rotated refresh tokens (single-use): a torn token file would
/// strand the account until the user reconnects.
pub fn write_atomic_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_sibling(path);
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("publishing {}", path.display()))?;
    Ok(())
}

/// `<name>.tmp` next to the target (same filesystem, so the rename is atomic).
fn tmp_sibling(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-store-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Rec {
        ts: String,
        n: u32,
    }

    fn rec(ts: &str, n: u32) -> Rec {
        Rec { ts: ts.into(), n }
    }

    #[test]
    fn stream_partitions_by_day_and_month_byte_exact() {
        let v = temp_vault("partition");
        let records = [
            rec("2026-06-10T09:00:00-07:00", 1),
            rec("2026-06-11T09:00:00-07:00", 2),
            rec("2026-06-10T10:00:00-07:00", 3),
        ];
        v.stream("things", Partition::Day).append(&records, |r| &r.ts).unwrap();
        // Grouped into day files, one compact JSON line each, \n-terminated —
        // the golden byte shape every hand-rolled writer produced.
        let day10 = fs::read_to_string(v.root().join("things/2026-06-10.jsonl")).unwrap();
        assert_eq!(
            day10,
            "{\"ts\":\"2026-06-10T09:00:00-07:00\",\"n\":1}\n{\"ts\":\"2026-06-10T10:00:00-07:00\",\"n\":3}\n"
        );
        let day11 = fs::read_to_string(v.root().join("things/2026-06-11.jsonl")).unwrap();
        assert_eq!(day11, "{\"ts\":\"2026-06-11T09:00:00-07:00\",\"n\":2}\n");

        // Month partitioning lands both June days in one file; appends extend.
        v.stream("monthly", Partition::Month).append(&records, |r| &r.ts).unwrap();
        v.stream("monthly", Partition::Month)
            .append(&[rec("2026-07-01T00:00:00-07:00", 4)], |r| &r.ts)
            .unwrap();
        assert_eq!(
            v.stream("monthly", Partition::Month).partitions().unwrap(),
            vec!["2026-06", "2026-07"]
        );
        let june: Vec<Rec> = v.stream("monthly", Partition::Month).read("2026-06").unwrap();
        assert_eq!(june.len(), 3);
    }

    #[test]
    fn malformed_timestamps_error_cleanly_and_write_nothing() {
        // Partition::key never panics and never yields a non-date key.
        assert_eq!(Partition::Day.key("2026-06-10T09:00:00-07:00"), Some("2026-06-10"));
        assert_eq!(Partition::Month.key("2026-06-10T09:00:00-07:00"), Some("2026-06"));
        assert_eq!(Partition::Day.key(""), None, "empty");
        assert_eq!(Partition::Day.key("2026-06"), None, "too short for Day");
        assert_eq!(Partition::Month.key("2026"), None, "too short for Month");
        assert_eq!(Partition::Day.key("not-a-date!!"), None, "not date-shaped");
        assert_eq!(Partition::Day.key("June 10 2026"), None, "not date-shaped");
        assert_eq!(Partition::Day.key("2026—06—10T00:00:00Z"), None, "em-dash, non-char-boundary");

        // append: one bad ts fails the whole call before any file is touched —
        // nothing dropped, nothing misfiled.
        let v = temp_vault("malformed");
        let s = v.stream("things", Partition::Day);
        let err = s
            .append(&[rec("2026-06-10T09:00:00-07:00", 1), rec("oops", 2)], |r| &r.ts)
            .unwrap_err();
        assert!(err.to_string().contains("\"oops\""), "names the offender: {err:#}");
        assert!(!v.root().join("things").exists(), "no partial write");

        // The good records alone still land.
        s.append(&[rec("2026-06-10T09:00:00-07:00", 1)], |r| &r.ts).unwrap();
        assert_eq!(s.partitions().unwrap(), vec!["2026-06-10"]);
    }

    #[test]
    fn stream_reads_are_lenient_and_missing_is_empty() {
        let v = temp_vault("lenient");
        let s = v.stream("things", Partition::Day);
        assert!(s.read::<Rec>("2026-06-11").unwrap().is_empty());
        assert!(s.partitions().unwrap().is_empty());

        fs::create_dir_all(v.root().join("things")).unwrap();
        fs::write(
            v.root().join("things/2026-06-11.jsonl"),
            "{\"ts\":\"2026-06-11T09:00:00-07:00\",\"n\":1}\nnot json\n\n{\"ts\":\"2026-06-11T10:00:00-07:00\",\"n\":2,\"extra\":\"tolerated\"}\n",
        )
        .unwrap();
        let rows: Vec<Rec> = s.read("2026-06-11").unwrap();
        assert_eq!(rows.len(), 2, "bad and blank lines skipped, unknown fields tolerated");
    }

    #[test]
    fn snapshot_round_trip_is_atomic_rewrite() {
        let v = temp_vault("snapshot");
        v.write_snapshot("box/state.jsonl", &[rec("2026-06-11T09:00:00-07:00", 1)]).unwrap();
        v.write_snapshot("box/state.jsonl", &[rec("2026-06-11T10:00:00-07:00", 2)]).unwrap();
        let rows: Vec<Rec> = v.read_snapshot("box/state.jsonl").unwrap();
        assert_eq!(rows, vec![rec("2026-06-11T10:00:00-07:00", 2)], "rewrite replaces");
        assert!(!v.root().join("box/state.jsonl.tmp").exists(), "tmp cleaned up by rename");
    }

    #[test]
    fn atomic_writers_publish_whole_files() {
        let v = temp_vault("atomic");
        let path = v.root().join("nested/dir/value.json");
        write_json_atomic(&path, &rec("2026-06-11T09:00:00-07:00", 7)).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"n\": 7"), "pretty json: {body}");

        let secret = v.root().join(".trove/sync/test-secret.json");
        write_atomic_secret(&secret, b"{\"token\":\"abc\"}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&secret).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secret must be 0600");
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Counter {
        version: u32,
        n: u32,
    }

    #[test]
    fn ensure_index_rebuilds_on_version_mismatch_and_skips_when_unchanged() {
        let v = temp_vault("ensure-index");
        let rel = ".trove/counter.json";
        let path = v.root().join(rel);

        // No file yet: `default()` seeds the value, `rebuild` marks it
        // dirty, so it gets written.
        let result: Counter = v
            .ensure_index(rel, 1, || Counter { version: 1, n: 0 }, |mut c| {
                c.n = 1;
                Ok((c, true))
            })
            .unwrap();
        assert_eq!(result, Counter { version: 1, n: 1 });
        assert!(path.exists(), "dirty rebuild is written");
        let mtime1 = fs::metadata(&path).unwrap().modified().unwrap();
        let body1 = fs::read_to_string(&path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));

        // Same version, `rebuild` reports nothing changed: the existing
        // value loads as-is and the file must not be rewritten at all.
        let result: Counter = v
            .ensure_index(rel, 1, || Counter { version: 1, n: 99 }, |c| {
                assert_eq!(c, Counter { version: 1, n: 1 }, "loaded the existing value, not default()");
                Ok((c, false))
            })
            .unwrap();
        assert_eq!(result, Counter { version: 1, n: 1 });
        let mtime2 = fs::metadata(&path).unwrap().modified().unwrap();
        let body2 = fs::read_to_string(&path).unwrap();
        assert_eq!(mtime1, mtime2, "unchanged must not trigger a rewrite");
        assert_eq!(body1, body2);

        std::thread::sleep(std::time::Duration::from_millis(10));

        // Bumping the required version makes the stored file stale-shaped:
        // `rebuild` receives `default()` (not the on-disk value) and, even
        // though it reports no further change, the version mismatch alone
        // forces a rewrite.
        let result: Counter = v
            .ensure_index(rel, 2, || Counter { version: 2, n: 7 }, |c| {
                assert_eq!(c, Counter { version: 2, n: 7 }, "version mismatch => default(), not stale value");
                Ok((c, false))
            })
            .unwrap();
        assert_eq!(result, Counter { version: 2, n: 7 });
        let mtime3 = fs::metadata(&path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime3, "version mismatch forces a rewrite even when rebuild reports unchanged");
    }
}

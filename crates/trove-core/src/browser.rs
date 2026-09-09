//! Browser history collector — the first M3 ("copy-then-read another app's
//! database") source. Chrome (per-profile `History` DBs, no permissions) and
//! Safari (`~/Library/Safari/History.db`, needs Full Disk Access — granted
//! per-binary in System Settings, no programmatic prompt exists).
//!
//! Source of truth is one JSONL file per local day: `browser/YYYY-MM-DD.jsonl`
//! (keyed by visit day), one visit per line:
//!
//! ```json
//! {"time":"2026-06-10T14:03:01-07:00","url":"https://news.ycombinator.com/",
//!  "title":"Hacker News","browser":"chrome","profile":"Default",
//!  "duration_secs":42,"source":"history"}
//! ```
//!
//! **This import is the backup, not the primary.** The planned browser
//! watcher extension (tab URL + title + `audible` via native messaging into
//! troved — see docs/data-sources.md §2) is the richer, real-time capture and
//! writes the same stream with `source:"extension"`. Both always record (the
//! history import keeps running even once the extension exists — it backfills
//! anything the extension missed, e.g. while troved was down, and preserves
//! history past Chrome's ~90-day retention); precedence is resolved at *read*
//! time, where extension rows win over history rows covering the same
//! browsing. The raw files keep both, per the files-first principle.
//!
//! Chrome keeps `History` (SQLite) per profile; it stays locked while Chrome
//! runs, so each sync copies the DB (and its WAL) to a temp file and reads the
//! copy — the original is never opened, let alone written. Imports are
//! incremental: `.trove/browser-sync.json` records the highest `visit_time`
//! imported per browser/profile (in the source DB's native epoch). If that
//! file is ever lost the cursors are rebuilt by scanning the JSONL logs, so a
//! resync never duplicates rows. The very first sync has cursor 0 and pulls
//! the profile's entire history — the backfill is free.
//!
//! Sync runs inside the watcher owner loop (see [`crate::runner`]), so the
//! single-writer lock that already guards activity events also guarantees
//! only one process imports browser history at a time.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::activity::days;
use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;

/// Seconds between browser-history syncs in the watcher loop.
pub const BROWSER_SYNC_SECS: u64 = 900;

// Both browsers land in the same stream and share one sync pass, so "last
// sync ran" is the honest per-browser answer.
fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_browser_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

// One pass covers both browsers; the per-browser toggles are consulted
// inside `collect_browser_history` (and Safari additionally preflights FDA).
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> anyhow::Result<crate::registry::CollectOutcome> {
    let s = vault.collect_browser_history()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_visits > 0, || {
        format!("imported {} browser visits", s.new_visits)
    }))
}

fn safari_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(safari_permission_ok()),
        required: true,
    }
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. Chrome owns the
/// shared history pass (one sync covers both browsers); Safari is co-gated.
pub static CHROME_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "chrome-history",
        name: "Chrome history",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Imports history from every Chrome profile every 15 minutes — the backup behind the live extension. No permission needed.",
        domain: "browser",
        vault_path: "browser/",
        toggleable: true,
        setup: &["Works immediately, all profiles."],
        caveats: "Chrome itself retains only ~90 days of history — anything older exists in the vault only if it was imported in time; every day this runs preserves visits Chrome will eventually delete.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(BROWSER_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static SAFARI_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "safari-history",
        name: "Safari history",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Imports Safari history every 15 minutes. Needs Full Disk Access.",
        domain: "browser",
        vault_path: "browser/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
        ],
        caveats: "Safari retains roughly a year of history — anything older exists in the vault only if it was imported in time.",
    },
    behavior: Behavior::CoveredBy("chrome-history"),
    permission: Some(safari_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

const SYNC_FILE: &str = ".trove/browser-sync.json";

/// Microseconds between the Chrome/Windows epoch (1601-01-01) and the Unix
/// epoch (1970-01-01). Chrome's `visit_time` is µs since 1601, UTC.
const CHROME_EPOCH_OFFSET_US: i64 = 11_644_473_600_000_000;

/// Seconds between the Unix epoch and the Safari/Core Data epoch
/// (2001-01-01). Safari's `visit_time` is REAL seconds since 2001, UTC; we
/// carry it as integer µs since 2001 so cursors stay exact.
const SAFARI_EPOCH_OFFSET_S: i64 = 978_307_200;

/// One page visit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct BrowserVisit {
    /// RFC3339 local time, e.g. "2026-06-10T14:03:01-07:00".
    pub time: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// "chrome" (later: "safari", …).
    pub browser: String,
    #[serde(default)]
    pub profile: String,
    /// Time on the page per Chrome's `visit_duration`; 0 when unknown.
    #[serde(default)]
    pub duration_secs: u64,
    /// Provenance: "history" (imported from the browser's own DB, this
    /// module) or "extension" (live capture — richer and authoritative where
    /// both exist). Readers resolve overlap in favor of "extension".
    #[serde(default = "default_source")]
    pub source: String,
    /// Extension rows only: the tab played audio at some point in the span.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub audible: bool,
    /// Extension rows only: seconds spent as the active tab of the focused
    /// window (≤ `duration_secs`; the rest is background audible playback).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub foreground_secs: u64,
    /// Extension rows only: the tab's favicon URL when the browser reported
    /// one. Cosmetic (timeline UI); empty for history rows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub favicon: String,
    /// Extension rows only: the page this visit was navigated from (the prior
    /// URL for link/form-submit navigations), from `webNavigation`. Empty when
    /// the navigation had no in-browser referrer (typed, reload, bookmark) or
    /// it couldn't be reconstructed (service worker had restarted).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub referrer: String,
    /// Extension rows only: how the navigation happened per `webNavigation`
    /// (`link`, `typed`, `reload`, `form_submit`, …). Empty for history rows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transition: String,
    /// Extension rows only: total open tabs (all windows) when this span
    /// opened — a rough "tab clutter" signal. 0/absent when unknown.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub tab_count: u32,
}

fn default_source() -> String {
    "history".into()
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// Visits per domain over a date range.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DomainUsage {
    pub domain: String,
    pub visits: u64,
}

/// Aggregate of a date range for the Web view.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct BrowserSummary {
    pub visits: u64,
    /// Domains by visit count, descending.
    pub domains: Vec<DomainUsage>,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Serialize)]
pub struct BrowserSyncStats {
    /// Profile databases found (0 = Chrome not installed).
    pub sources: u32,
    pub new_visits: u64,
}

/// Incremental-sync cursors, persisted in `.trove/browser-sync.json`.
/// Rebuildable from the JSONL logs (see [`Vault::rebuild_browser_sync`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct BrowserSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest imported `visit_time` (source-native epoch), keyed
    /// "browser/profile", e.g. "chrome/Default".
    pub cursors: BTreeMap<String, i64>,
}

/// A discovered Chrome profile with a History database.
struct ChromeProfile {
    name: String,
    history_db: PathBuf,
}

/// Chrome profile dirs that hold a History DB ("Default", "Profile N").
fn chrome_profiles() -> Vec<ChromeProfile> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let base = home.join("Library/Application Support/Google/Chrome");
    let Ok(entries) = fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out: Vec<ChromeProfile> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name != "Default" && !name.starts_with("Profile ") {
                return None;
            }
            let history_db = e.path().join("History");
            history_db
                .exists()
                .then_some(ChromeProfile { name, history_db })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Safari's history database (one per user; Safari has no profile split in
/// History.db — profile-aware visits still land in the same file).
fn safari_history_db() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/Safari/History.db"))
}

/// Whether this process can read Safari's history. False means either Full
/// Disk Access hasn't been granted to this binary or Safari has never run.
/// There is no API to prompt for FDA — the UI deep-links to System Settings.
pub fn safari_permission_ok() -> bool {
    safari_history_db().is_some_and(|p| fs::File::open(p).is_ok())
}

/// Chrome `visit_time` (µs since 1601, UTC) → local time. None for zero or
/// out-of-range values (Chrome stores 0 for some imported/synthetic rows).
fn chrome_time_to_local(visit_time_us: i64) -> Option<DateTime<Local>> {
    let unix_us = visit_time_us.checked_sub(CHROME_EPOCH_OFFSET_US)?;
    if unix_us <= 0 {
        return None;
    }
    DateTime::from_timestamp_micros(unix_us).map(|t| t.with_timezone(&Local))
}

/// Inverse of [`chrome_time_to_local`], for rebuilding cursors from JSONL.
fn rfc3339_to_chrome_us(time: &str) -> Option<i64> {
    let t = DateTime::parse_from_rfc3339(time).ok()?;
    t.timestamp_micros().checked_add(CHROME_EPOCH_OFFSET_US)
}

/// Safari `visit_time` as integer µs since 2001 → local time.
fn safari_us_to_local(visit_us: i64) -> Option<DateTime<Local>> {
    let unix_us = visit_us.checked_add(SAFARI_EPOCH_OFFSET_S.checked_mul(1_000_000)?)?;
    if unix_us <= 0 {
        return None;
    }
    DateTime::from_timestamp_micros(unix_us).map(|t| t.with_timezone(&Local))
}

/// Inverse of [`safari_us_to_local`], for rebuilding cursors from JSONL.
fn rfc3339_to_safari_us(time: &str) -> Option<i64> {
    let t = DateTime::parse_from_rfc3339(time).ok()?;
    t.timestamp_micros().checked_sub(SAFARI_EPOCH_OFFSET_S * 1_000_000)
}

/// Cursor-map key for a source: "chrome/Default", "safari" (no profiles).
fn cursor_key(browser: &str, profile: &str) -> String {
    if profile.is_empty() {
        browser.to_string()
    } else {
        format!("{browser}/{profile}")
    }
}

/// The site a URL points at, for aggregation: host without "www.", or the
/// scheme ("file", "chrome") when there is no host.
pub(crate) fn domain_of(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("", url),
    };
    // Non-web schemes (file://, chrome://, …) group under the scheme — their
    // "host" is a page or path, not a site.
    if !scheme.is_empty() && scheme != "http" && scheme != "https" {
        return scheme.to_lowercase();
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return if scheme.is_empty() { "other".into() } else { scheme.into() };
    }
    host.strip_prefix("www.").unwrap_or(host).to_lowercase()
}

/// Read visits newer than `cursor` out of a Chrome History DB (or a copy of
/// one) and append them to the vault. Returns (rows imported, new cursor).
/// Split out from the copy step so tests can run it on a synthetic DB.
fn import_chrome_db(
    vault: &Vault,
    db: &Path,
    profile: &str,
    cursor: i64,
) -> Result<(u64, i64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening chrome history copy {}", db.display()))?;
    let mut stmt = conn.prepare(
        "SELECT v.visit_time, u.url, COALESCE(u.title, ''), v.visit_duration
         FROM visits v JOIN urls u ON v.url = u.id
         WHERE v.visit_time > ?1
         ORDER BY v.visit_time",
    )?;
    let mut rows = stmt.query([cursor])?;
    let mut visits = Vec::new();
    let mut max = cursor;
    while let Some(row) = rows.next()? {
        let visit_time: i64 = row.get(0)?;
        max = max.max(visit_time);
        let Some(local) = chrome_time_to_local(visit_time) else {
            continue;
        };
        visits.push(BrowserVisit {
            time: local.to_rfc3339(),
            url: row.get(1)?,
            title: row.get(2)?,
            browser: "chrome".into(),
            profile: profile.into(),
            duration_secs: (row.get::<_, i64>(3)?.max(0) as u64) / 1_000_000,
            source: default_source(),
            audible: false,
            foreground_secs: 0,
            favicon: String::new(),
            referrer: String::new(),
            transition: String::new(),
            tab_count: 0,
        });
    }
    vault.append_browser_visits(&visits)?;
    Ok((visits.len() as u64, max))
}

/// Read visits newer than `cursor` (µs since 2001) out of a Safari History
/// DB (or a copy) and append them to the vault. Returns (rows imported, new
/// cursor). The CAST keeps the cursor comparison in exact integer µs — the
/// stored REAL never round-trips through float math on our side.
fn import_safari_db(vault: &Vault, db: &Path, cursor: i64) -> Result<(u64, i64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening safari history copy {}", db.display()))?;
    let mut stmt = conn.prepare(
        "SELECT CAST(v.visit_time * 1000000 AS INTEGER), i.url, COALESCE(v.title, '')
         FROM history_visits v JOIN history_items i ON v.history_item = i.id
         WHERE CAST(v.visit_time * 1000000 AS INTEGER) > ?1
         ORDER BY v.visit_time",
    )?;
    let mut rows = stmt.query([cursor])?;
    let mut visits = Vec::new();
    let mut max = cursor;
    while let Some(row) = rows.next()? {
        let visit_us: i64 = row.get(0)?;
        max = max.max(visit_us);
        let Some(local) = safari_us_to_local(visit_us) else {
            continue;
        };
        visits.push(BrowserVisit {
            time: local.to_rfc3339(),
            url: row.get(1)?,
            title: row.get(2)?,
            browser: "safari".into(),
            profile: String::new(),
            duration_secs: 0,
            source: default_source(),
            audible: false,
            foreground_secs: 0,
            favicon: String::new(),
            referrer: String::new(),
            transition: String::new(),
            tab_count: 0,
        });
    }
    vault.append_browser_visits(&visits)?;
    Ok((visits.len() as u64, max))
}

/// Copy a source DB (+ its `-wal`, which holds the newest rows until the
/// browser checkpoints) to temp files, import via `f`, clean up. The original
/// is never opened — browsers hold locks on it while running; a torn copy
/// mid-write just fails the query and the next sync retries. Shared with
/// every other copy-then-read collector (iMessage, screen time's sync.db).
pub(crate) fn import_via_copy<R>(
    db: &Path,
    stem: &str,
    f: impl FnOnce(&Path) -> Result<R>,
) -> Result<R> {
    // Callers key `stem` by pid, which is not unique when concurrent callers
    // in one process (parallel tests) import different DBs — a per-call
    // sequence keeps the copies disjoint.
    static COPY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COPY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("{stem}-{seq}.db"));
    let tmp_wal = std::env::temp_dir().join(format!("{stem}-{seq}.db-wal"));
    let _ = fs::remove_file(&tmp);
    let _ = fs::remove_file(&tmp_wal);
    fs::copy(db, &tmp).with_context(|| format!("copying {}", db.display()))?;
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    if wal.exists() {
        let _ = fs::copy(&wal, &tmp_wal);
    }
    let result = f(&tmp);
    let _ = fs::remove_file(&tmp);
    let _ = fs::remove_file(&tmp_wal);
    result
}

fn import_chrome_profile(vault: &Vault, profile: &ChromeProfile, cursor: i64) -> Result<(u64, i64)> {
    let stem = format!(
        "trove-chrome-{}-{}",
        std::process::id(),
        profile.name.replace(' ', "-").to_lowercase()
    );
    import_via_copy(&profile.history_db, &stem, |tmp| {
        import_chrome_db(vault, tmp, &profile.name, cursor)
    })
}

impl Vault {
    /// One sync pass over every installed browser profile. Incremental via
    /// the persisted cursors; per-profile failures are logged and skipped so
    /// one bad profile never blocks the rest.
    pub fn collect_browser_history(&self) -> Result<BrowserSyncStats> {
        let mut state = match self.read_browser_sync() {
            Some(s) => s,
            None => self.rebuild_browser_sync(),
        };
        let mut stats = BrowserSyncStats {
            sources: 0,
            new_visits: 0,
        };
        // Each browser is its own hub integration — consult the toggle here
        // (not just in the runner) so one can run while the other is off.
        if self.integration_enabled("chrome-history") {
            for profile in chrome_profiles() {
                stats.sources += 1;
                let key = cursor_key("chrome", &profile.name);
                let cursor = state.cursors.get(&key).copied().unwrap_or(0);
                match import_chrome_profile(self, &profile, cursor) {
                    Ok((n, max)) => {
                        stats.new_visits += n;
                        state.cursors.insert(key, max);
                    }
                    Err(e) => {
                        eprintln!("trove browser: chrome profile {:?} sync failed: {e:#}", profile.name)
                    }
                }
            }
        }
        // Safari: silently skipped while unreadable (no FDA, or Safari never
        // used) — the UI surfaces the permission state; logging it every sync
        // would just be noise.
        if self.integration_enabled("safari-history") && safari_permission_ok() {
            let db = safari_history_db().expect("permission_ok implies path");
            stats.sources += 1;
            let key = cursor_key("safari", "");
            let cursor = state.cursors.get(&key).copied().unwrap_or(0);
            let stem = format!("trove-safari-{}", std::process::id());
            match import_via_copy(&db, &stem, |tmp| import_safari_db(self, tmp, cursor)) {
                Ok((n, max)) => {
                    stats.new_visits += n;
                    state.cursors.insert(key, max);
                }
                Err(e) => eprintln!("trove browser: safari sync failed: {e:#}"),
            }
        }
        state.updated = Local::now().to_rfc3339();
        self.write_browser_sync(&state)?;
        Ok(stats)
    }

    /// Append visits to their day's JSONL log (keyed by local visit day).
    ///
    /// Unlike the activity stream, this file has multiple legitimate writers:
    /// the watcher owner's history sync plus one extension host process per
    /// running Chrome profile (spawned by Chrome, outside the watcher lock).
    /// An exclusive flock per day file keeps their lines whole; writers hold
    /// it for microseconds, so blocking is fine.
    pub fn append_browser_visits(&self, visits: &[BrowserVisit]) -> Result<()> {
        self.stream("browser", crate::store::Partition::Day)
            .with_flock()
            .append(visits, |v| &v.time)
    }

    /// All visits for one local day (YYYY-MM-DD), in file order (which is
    /// chronological per profile). Where an extension span and a history
    /// visit cover the same browsing, the extension row wins — see
    /// [`crate::browser_ext::resolve_extension_overlap`]; the raw file keeps
    /// both.
    pub fn browser_timeline(&self, date: &str) -> Result<Vec<BrowserVisit>> {
        let path = self.resolve(&format!("browser/{date}.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body =
            fs::read_to_string(&path).with_context(|| format!("reading browser/{date}.jsonl"))?;
        let rows = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<BrowserVisit>(l).ok())
            .collect();
        Ok(crate::browser_ext::resolve_extension_overlap(rows))
    }

    /// Visit count plus per-domain counts over an inclusive date range.
    pub fn browser_summary(&self, from: &str, to: &str) -> Result<BrowserSummary> {
        let mut domains: HashMap<String, u64> = HashMap::new();
        let mut visits = 0u64;
        for date in days(from, to)? {
            for v in self.browser_timeline(&date)? {
                visits += 1;
                *domains.entry(domain_of(&v.url)).or_default() += 1;
            }
        }
        let mut domains: Vec<DomainUsage> = domains
            .into_iter()
            .map(|(domain, visits)| DomainUsage { domain, visits })
            .collect();
        domains.sort_by(|a, b| b.visits.cmp(&a.visits).then_with(|| a.domain.cmp(&b.domain)));
        Ok(BrowserSummary { visits, domains })
    }

    /// Visits per day over an inclusive range — a trend series for the chart.
    /// Days with no data are omitted.
    pub fn browser_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut out = Vec::new();
        for date in days(from, to)? {
            let n = self.browser_timeline(&date)?.len();
            if n > 0 {
                out.push(SeriesPoint {
                    date,
                    value: n as f64,
                });
            }
        }
        Ok(out)
    }

    /// The persisted sync state, if a sync has ever run.
    pub fn read_browser_sync(&self) -> Option<BrowserSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_browser_sync(&self, state: &BrowserSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Reconstruct cursors from the JSONL logs — used when the sync file is
    /// missing so a resync appends only genuinely new visits instead of
    /// re-importing history it already holds.
    fn rebuild_browser_sync(&self) -> BrowserSyncState {
        let mut cursors: BTreeMap<String, i64> = BTreeMap::new();
        let dir = self.root().join("browser");
        let Ok(entries) = fs::read_dir(&dir) else {
            return BrowserSyncState::default();
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            for line in body.lines() {
                let Ok(v) = serde_json::from_str::<BrowserVisit>(line) else {
                    continue;
                };
                // Cursors track what the *history import* has seen, so only
                // its own rows count — an extension row at time T must not
                // mask history visits at or before T that were never
                // imported. Cursors are source-native, so each browser
                // converts with its own epoch.
                if v.source != "history" {
                    continue;
                }
                let us = match v.browser.as_str() {
                    "chrome" => rfc3339_to_chrome_us(&v.time),
                    "safari" => rfc3339_to_safari_us(&v.time),
                    _ => None,
                };
                let Some(us) = us else {
                    continue;
                };
                let key = cursor_key(&v.browser, &v.profile);
                let cur = cursors.entry(key).or_insert(0);
                *cur = (*cur).max(us);
            }
        }
        BrowserSyncState {
            updated: String::new(),
            cursors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-browser-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Chrome visit_time for a fixed local datetime, so JSONL day grouping is
    /// deterministic in any timezone.
    fn chrome_us(d: u32, h: u32) -> i64 {
        let t = Local.with_ymd_and_hms(2026, 6, d, h, 0, 0).unwrap();
        t.timestamp_micros() + CHROME_EPOCH_OFFSET_US
    }

    fn fake_history_db(name: &str, rows: &[(i64, &str, &str, i64)]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-fakechrome-{}-{name}.db",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT);
             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER,
                                  visit_time INTEGER, visit_duration INTEGER);",
        )
        .unwrap();
        for (i, (time, url, title, duration)) in rows.iter().enumerate() {
            let id = i as i64 + 1;
            conn.execute("INSERT INTO urls VALUES (?1, ?2, ?3)", (id, url, title))
                .unwrap();
            conn.execute(
                "INSERT INTO visits VALUES (?1, ?1, ?2, ?3)",
                (id, time, duration),
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn chrome_epoch_conversion() {
        // 2026-06-10T00:00:00Z in Chrome time.
        let us = 1_781_049_600i64 * 1_000_000 + CHROME_EPOCH_OFFSET_US;
        let t = chrome_time_to_local(us).unwrap();
        assert_eq!(t.timestamp(), 1_781_049_600);
        // Zero and pre-epoch values are dropped, not mangled.
        assert!(chrome_time_to_local(0).is_none());
        assert!(chrome_time_to_local(CHROME_EPOCH_OFFSET_US).is_none());
        // Round trip through the JSONL representation.
        assert_eq!(rfc3339_to_chrome_us(&t.to_rfc3339()), Some(us));
    }

    #[test]
    fn domain_extraction() {
        assert_eq!(domain_of("https://www.youtube.com/watch?v=x"), "youtube.com");
        assert_eq!(domain_of("https://news.ycombinator.com/item?id=1"), "news.ycombinator.com");
        assert_eq!(domain_of("http://user:pw@example.com:8080/p"), "example.com");
        assert_eq!(domain_of("file:///Users/me/doc.pdf"), "file");
        assert_eq!(domain_of("chrome://settings/"), "chrome");
    }

    #[test]
    fn import_is_incremental_and_aggregates() {
        let v = temp_vault("import");
        let db = fake_history_db(
            "import",
            &[
                (chrome_us(9, 10), "https://www.rust-lang.org/", "Rust", 5_000_000),
                (chrome_us(10, 9), "https://news.ycombinator.com/", "HN", 0),
                (chrome_us(10, 11), "https://news.ycombinator.com/item?id=1", "A story", 60_000_000),
                (0, "https://broken.example/", "zero timestamp is skipped", 0),
            ],
        );

        let (n, cursor) = import_chrome_db(&v, &db, "Default", 0).unwrap();
        assert_eq!(n, 3);
        assert_eq!(cursor, chrome_us(10, 11));

        let day = v.browser_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].title, "HN");
        assert_eq!(day[1].duration_secs, 60);

        let s = v.browser_summary("2026-06-09", "2026-06-10").unwrap();
        assert_eq!(s.visits, 3);
        assert_eq!(s.domains[0].domain, "news.ycombinator.com");
        assert_eq!(s.domains[0].visits, 2);

        let daily = v.browser_daily("2026-06-08", "2026-06-11").unwrap();
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[1].value, 2.0);

        // Re-import from the stored cursor: nothing new, nothing duplicated.
        let (n2, cursor2) = import_chrome_db(&v, &db, "Default", cursor).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(cursor2, cursor);
        assert_eq!(v.browser_summary("2026-06-09", "2026-06-10").unwrap().visits, 3);

        let _ = fs::remove_file(db);
    }

    /// Safari visit_time (REAL seconds since 2001) for a fixed local datetime.
    fn safari_s(d: u32, h: u32) -> f64 {
        let t = Local.with_ymd_and_hms(2026, 6, d, h, 0, 0).unwrap();
        (t.timestamp_micros() - SAFARI_EPOCH_OFFSET_S * 1_000_000) as f64 / 1e6
    }

    fn fake_safari_db(name: &str, rows: &[(f64, &str, &str)]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-fakesafari-{}-{name}.db",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE history_items (id INTEGER PRIMARY KEY, url TEXT NOT NULL UNIQUE);
             CREATE TABLE history_visits (id INTEGER PRIMARY KEY, history_item INTEGER,
                                          visit_time REAL NOT NULL, title TEXT NULL);",
        )
        .unwrap();
        for (i, (time, url, title)) in rows.iter().enumerate() {
            let id = i as i64 + 1;
            conn.execute("INSERT INTO history_items VALUES (?1, ?2)", (id, url))
                .unwrap();
            conn.execute(
                "INSERT INTO history_visits VALUES (?1, ?1, ?2, ?3)",
                (id, time, title),
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn safari_epoch_conversion() {
        // 2026-06-10T00:00:00Z: unix 1_781_049_600 → µs since 2001.
        let us = (1_781_049_600 - SAFARI_EPOCH_OFFSET_S) * 1_000_000;
        let t = safari_us_to_local(us).unwrap();
        assert_eq!(t.timestamp(), 1_781_049_600);
        assert_eq!(rfc3339_to_safari_us(&t.to_rfc3339()), Some(us));
    }

    #[test]
    fn safari_import_is_incremental_and_cursor_rebuilds() {
        let v = temp_vault("safari");
        let db = fake_safari_db(
            "import",
            &[
                (safari_s(9, 10) + 0.802754, "https://www.wsj.com/", "WSJ"),
                (safari_s(10, 11) + 0.532795, "https://www.nytimes.com/", "NYT"),
            ],
        );

        let (n, cursor) = import_safari_db(&v, &db, 0).unwrap();
        assert_eq!(n, 2);

        let day = v.browser_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].browser, "safari");
        assert_eq!(day[0].title, "NYT");

        // Incremental: nothing new from the stored cursor.
        let (n2, cursor2) = import_safari_db(&v, &db, cursor).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(cursor2, cursor);

        // Cursor survives losing the sync file (fractional seconds included).
        let rebuilt = v.rebuild_browser_sync();
        assert_eq!(rebuilt.cursors.get("safari"), Some(&cursor));

        let _ = fs::remove_file(db);
    }

    #[test]
    fn cursor_rebuilds_from_jsonl_logs() {
        let v = temp_vault("rebuild");
        let db = fake_history_db(
            "rebuild",
            &[
                (chrome_us(9, 8), "https://example.com/a", "A", 0),
                (chrome_us(10, 12), "https://example.com/b", "B", 0),
            ],
        );
        import_chrome_db(&v, &db, "Default", 0).unwrap();

        // Lose the sync file: the rebuilt cursor must match the newest visit,
        // so the next sync wouldn't re-import anything.
        let rebuilt = v.rebuild_browser_sync();
        assert_eq!(rebuilt.cursors.get("chrome/Default"), Some(&chrome_us(10, 12)));

        let _ = fs::remove_file(db);
    }
}

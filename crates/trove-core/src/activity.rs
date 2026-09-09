//! Activity watcher: turns a stream of "what app is frontmost / is the user
//! idle" samples into merged events, and stores them files-first.
//!
//! Source of truth is one JSONL file per local day: `activity/YYYY-MM-DD.jsonl`,
//! one merged event per line:
//!
//! ```json
//! {"start":"2026-06-10T14:03:01-07:00","end":"2026-06-10T14:09:22-07:00",
//!  "seconds":381,"app":"Code","bundle_id":"","title":"activity.rs","afk":false}
//! ```
//!
//! AFK ("away from keyboard") spans are recorded as events with `afk: true`
//! and an empty app, so per-app totals never count idle time. Reads aggregate
//! these on the fly — activity volume is tiny next to health, so no derived
//! index is needed yet.
//!
//! The [`Watcher`] merge logic is deliberately free of any OS or threading so
//! it can be unit-tested with synthetic samples and timestamps; the platform
//! sampling lives in [`crate::sampler`].

use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind, PermissionInfo};
use crate::registry::{Behavior, IntegrationDef, LiveCollector};
use crate::sampler::Sample;
use crate::vault::Vault;

/// Seconds between samples. Also drives the gap heuristic below.
pub const POLL_SECS: u64 = 5;

/// The live activity collector: samples the OS every poll and appends the
/// spans the [`Watcher`] state machine closes. Built once per lock takeover.
struct ActivityLive {
    watcher: Watcher,
}

fn make_live() -> Box<dyn LiveCollector> {
    Box::new(ActivityLive { watcher: Watcher::new(WatchConfig::default()) })
}

impl LiveCollector for ActivityLive {
    fn tick(&mut self, vault: &Vault, now: DateTime<Local>, enabled: bool) {
        if enabled {
            let closed = self.watcher.tick(now, &crate::sampler::sample());
            if !closed.is_empty() {
                if let Err(e) = vault.append_activity_events(&closed) {
                    eprintln!("trove watcher: failed to append events: {e:#}");
                }
            }
        } else if let Some(e) = self.watcher.flush(now) {
            // Toggled off mid-span: close out what was collected while
            // enabled rather than dropping it.
            if let Err(err) = vault.append_activity_events(&[e]) {
                eprintln!("trove watcher: failed to append events: {err:#}");
            }
        }
    }

    fn shutdown(&mut self, vault: &Vault, now: DateTime<Local>) {
        if let Some(e) = self.watcher.flush(now) {
            if let Err(err) = vault.append_activity_events(&[e]) {
                eprintln!("trove watcher: failed to flush final event: {err:#}");
            }
        }
    }

    fn current(&self, now: DateTime<Local>) -> Option<ActivityEvent> {
        self.watcher.current(now)
    }
}

fn def_permission() -> PermissionInfo {
    PermissionInfo {
        kind: "screen-recording",
        granted: Some(crate::sampler::screen_recording_ok()),
        required: false,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join("activity"))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "activity",
        name: "App activity",
        kind: IntegrationKind::Live,
        default_on: true,
        description: "Tracks the frontmost app and window title with idle detection, 24/7 via the troved daemon. Window titles of other apps need Screen Recording.",
        domain: "activity",
        vault_path: "activity/",
        toggleable: true,
        setup: &[
            "App-level tracking works immediately, no permission needed.",
            "For window titles: System Settings → Privacy & Security → Screen Recording → add Trove and the troved binary.",
            "Restart the daemon after granting: launchctl kickstart -k gui/501/com.davidwills.troved",
        ],
        caveats: "Screen Recording grants are per-binary and reset whenever the binary is rebuilt — expect to re-grant after updates until the daemon is signed and bundled.",
    },
    behavior: Behavior::Live(make_live),
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// One contiguous span of using a single app/window — or being away.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ActivityEvent {
    /// RFC3339 local time, e.g. "2026-06-10T14:03:01-07:00".
    pub start: String,
    pub end: String,
    pub seconds: u64,
    /// Owning app; empty for AFK spans.
    pub app: String,
    #[serde(default)]
    pub bundle_id: String,
    #[serde(default)]
    pub title: String,
    pub afk: bool,
}

/// Per-app time over a date range (active time only).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AppUsage {
    pub app: String,
    pub seconds: u64,
}

/// Aggregate of a date range for the Activity view.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ActivitySummary {
    pub active_seconds: u64,
    pub afk_seconds: u64,
    /// Apps by active time, descending.
    pub apps: Vec<AppUsage>,
}

/// Tunables for [`Watcher`].
#[derive(Debug, Clone, Copy)]
pub struct WatchConfig {
    /// Idle seconds before the user counts as away.
    pub afk_threshold_secs: f64,
    /// If this many seconds pass with no sample (sleep, app paused), the open
    /// event is closed at its last-known end rather than stretched across the
    /// gap.
    pub max_gap_secs: f64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            afk_threshold_secs: 120.0,
            max_gap_secs: (POLL_SECS * 6) as f64,
        }
    }
}

/// The currently-open (still-growing) event.
#[derive(Debug, Clone)]
struct Open {
    start: DateTime<Local>,
    end: DateTime<Local>,
    app: String,
    bundle_id: String,
    title: String,
    afk: bool,
}

impl Open {
    fn to_event(&self, end: DateTime<Local>) -> ActivityEvent {
        let end = end.max(self.start);
        ActivityEvent {
            start: self.start.to_rfc3339(),
            end: end.to_rfc3339(),
            seconds: (end - self.start).num_seconds().max(0) as u64,
            app: self.app.clone(),
            bundle_id: self.bundle_id.clone(),
            title: self.title.clone(),
            afk: self.afk,
        }
    }
}

/// Merges samples into events. Holds no OS handles — feed it `tick`s.
pub struct Watcher {
    cfg: WatchConfig,
    open: Option<Open>,
}

impl Watcher {
    pub fn new(cfg: WatchConfig) -> Self {
        Watcher { cfg, open: None }
    }

    /// Feed one sample taken at `now`. Returns any events that just *closed*
    /// (the in-progress event is held until its state changes — see
    /// [`Watcher::current`] / [`Watcher::flush`]).
    pub fn tick(&mut self, now: DateTime<Local>, sample: &Sample) -> Vec<ActivityEvent> {
        let mut closed = Vec::new();
        let afk = sample.idle_seconds >= self.cfg.afk_threshold_secs;

        // A long gap since the last sample means the machine slept or the
        // process was paused: close the open event at its last-known end and
        // don't count the gap as activity.
        let gap_end = match &self.open {
            Some(o) if (now - o.end).num_seconds() as f64 > self.cfg.max_gap_secs => Some(o.end),
            _ => None,
        };
        if let Some(end) = gap_end {
            if let Some(e) = self.close_at(end) {
                closed.push(e);
            }
        }

        let matches = self
            .open
            .as_ref()
            .is_some_and(|o| Self::same_state(o, afk, sample));

        if matches {
            // Same activity continues — just extend its end.
            self.open.as_mut().unwrap().end = now;
        } else {
            // State changed: close the old span at the boundary, open a new one.
            // Entering AFK back-dates the boundary to when input actually
            // stopped (now - idle), so idle time isn't billed to the last app.
            let boundary = if afk {
                let idle_start = now - millis(sample.idle_seconds);
                match &self.open {
                    Some(o) => idle_start.clamp(o.start, now),
                    None => idle_start.min(now),
                }
            } else {
                now
            };
            if let Some(e) = self.close_at(boundary) {
                closed.push(e);
            }
            self.open = Some(Open {
                start: boundary,
                end: now,
                app: if afk { String::new() } else { sample.app.clone() },
                bundle_id: if afk { String::new() } else { sample.bundle_id.clone() },
                title: if afk { String::new() } else { sample.title.clone() },
                afk,
            });
        }
        closed
    }

    /// The in-progress event as of `now`, for a live "current activity" view.
    pub fn current(&self, now: DateTime<Local>) -> Option<ActivityEvent> {
        self.open.as_ref().map(|o| o.to_event(now))
    }

    /// Close out the open event (e.g. on shutdown), if any.
    pub fn flush(&mut self, now: DateTime<Local>) -> Option<ActivityEvent> {
        self.close_at(now)
    }

    /// Take the open event, ending it at `end`. Drops zero-length spans.
    fn close_at(&mut self, end: DateTime<Local>) -> Option<ActivityEvent> {
        let o = self.open.take()?;
        let e = o.to_event(end);
        (e.seconds > 0).then_some(e)
    }

    fn same_state(o: &Open, afk: bool, s: &Sample) -> bool {
        if afk {
            o.afk
        } else {
            !o.afk && o.app == s.app && o.title == s.title
        }
    }
}

fn millis(secs: f64) -> Duration {
    Duration::milliseconds((secs * 1000.0) as i64)
}

impl Vault {
    /// Append merged events to their day's JSONL log (keyed by start day).
    pub fn append_activity_events(&self, events: &[ActivityEvent]) -> Result<()> {
        self.stream("activity", crate::store::Partition::Day)
            .append(events, |e| &e.start)
    }

    /// All events for one local day (YYYY-MM-DD), in file order.
    pub fn activity_timeline(&self, date: &str) -> Result<Vec<ActivityEvent>> {
        let path = self.resolve(&format!("activity/{date}.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path).with_context(|| format!("reading activity/{date}.jsonl"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<ActivityEvent>(l).ok())
            .collect())
    }

    /// Per-app active time plus active/AFK totals over an inclusive date range.
    pub fn activity_summary(&self, from: &str, to: &str) -> Result<ActivitySummary> {
        let mut apps: HashMap<String, u64> = HashMap::new();
        let mut active = 0u64;
        let mut afk = 0u64;
        for date in days(from, to)? {
            for e in self.activity_timeline(&date)? {
                if e.afk {
                    afk += e.seconds;
                } else {
                    active += e.seconds;
                    *apps.entry(e.app).or_default() += e.seconds;
                }
            }
        }
        let mut apps: Vec<AppUsage> = apps
            .into_iter()
            .map(|(app, seconds)| AppUsage { app, seconds })
            .collect();
        apps.sort_by(|a, b| b.seconds.cmp(&a.seconds).then_with(|| a.app.cmp(&b.app)));
        Ok(ActivitySummary {
            active_seconds: active,
            afk_seconds: afk,
            apps,
        })
    }

    /// Active hours per day over an inclusive range — a trend series for the
    /// chart. Days with no data are omitted.
    pub fn activity_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut out = Vec::new();
        for date in days(from, to)? {
            let secs: u64 = self
                .activity_timeline(&date)?
                .iter()
                .filter(|e| !e.afk)
                .map(|e| e.seconds)
                .sum();
            if secs > 0 {
                out.push(SeriesPoint {
                    date,
                    value: secs as f64 / 3600.0,
                });
            }
        }
        Ok(out)
    }
}

/// Inclusive list of YYYY-MM-DD strings from `from` to `to`.
pub(crate) fn days(from: &str, to: &str) -> Result<Vec<String>> {
    let start = NaiveDate::parse_from_str(from, "%Y-%m-%d").context("bad from date")?;
    let end = NaiveDate::parse_from_str(to, "%Y-%m-%d").context("bad to date")?;
    let mut out = Vec::new();
    let mut d = start;
    while d <= end {
        out.push(d.format("%Y-%m-%d").to_string());
        match d.succ_opt() {
            Some(n) => d = n,
            None => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 10, h, m, s).unwrap()
    }

    fn active(app: &str, title: &str) -> Sample {
        Sample {
            app: app.into(),
            bundle_id: String::new(),
            title: title.into(),
            idle_seconds: 0.0,
        }
    }

    fn cfg() -> WatchConfig {
        WatchConfig {
            afk_threshold_secs: 120.0,
            max_gap_secs: 30.0,
        }
    }

    #[test]
    fn merges_same_app_and_splits_on_change() {
        let mut w = Watcher::new(cfg());
        assert!(w.tick(at(9, 0, 0), &active("Code", "a.rs")).is_empty());
        assert!(w.tick(at(9, 0, 5), &active("Code", "a.rs")).is_empty());
        // Switching app closes the first event.
        let closed = w.tick(at(9, 0, 10), &active("Safari", "Hacker News"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].app, "Code");
        assert_eq!(closed[0].seconds, 10);
        assert!(!closed[0].afk);
        // The Safari event is still open.
        let cur = w.current(at(9, 0, 15)).unwrap();
        assert_eq!(cur.app, "Safari");
        assert_eq!(cur.seconds, 5);
    }

    #[test]
    fn title_change_splits_within_same_app() {
        let mut w = Watcher::new(cfg());
        w.tick(at(9, 0, 0), &active("Code", "a.rs"));
        let closed = w.tick(at(9, 0, 5), &active("Code", "b.rs"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].title, "a.rs");
    }

    #[test]
    fn afk_backdates_to_last_input() {
        let mut w = Watcher::new(cfg());
        // Active in Code; the last real input lands at 9:01:00. We keep polling
        // at a realistic cadence (<= max_gap apart): once input stops, idle
        // climbs but stays below threshold, so Code keeps extending.
        w.tick(at(9, 0, 0), &active("Code", "a.rs"));
        for ((h, m, s), idle) in [
            ((9, 0, 30), 30.0),
            ((9, 1, 0), 0.0),
            ((9, 1, 30), 30.0),
            ((9, 2, 0), 60.0),
            ((9, 2, 30), 90.0),
        ] {
            let mut sample = active("Code", "a.rs");
            sample.idle_seconds = idle;
            assert!(w.tick(at(h, m, s), &sample).is_empty());
        }
        // 9:03:00, idle 120s (>= threshold): user went away at 9:01:00, so the
        // Code span should end there and AFK begin there.
        let mut idle = active("Code", "a.rs");
        idle.idle_seconds = 120.0;
        let closed = w.tick(at(9, 3, 0), &idle);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].app, "Code");
        assert_eq!(closed[0].seconds, 60, "active span ends when input stopped");
        let afk = w.current(at(9, 3, 0)).unwrap();
        assert!(afk.afk);
        assert_eq!(afk.app, "");
        assert_eq!(afk.seconds, 120);
    }

    #[test]
    fn long_gap_closes_without_billing_the_gap() {
        let mut w = Watcher::new(cfg());
        w.tick(at(9, 0, 0), &active("Code", "a.rs"));
        w.tick(at(9, 0, 5), &active("Code", "a.rs"));
        // 10-minute gap (machine asleep): the Code event ends at 9:00:05.
        let closed = w.tick(at(9, 10, 5), &active("Code", "a.rs"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].seconds, 5);
        // A fresh Code event opens at the wake time.
        assert_eq!(w.current(at(9, 10, 10)).unwrap().seconds, 5);
    }

    #[test]
    fn vault_round_trip_and_summary() {
        let dir = std::env::temp_dir().join(format!("trove-activity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();

        let mut w = Watcher::new(cfg());
        w.tick(at(9, 0, 0), &active("Code", "a.rs"));
        w.tick(at(9, 0, 30), &active("Code", "a.rs"));
        let mut events = w.tick(at(9, 1, 0), &active("Safari", "news"));
        events.extend(w.flush(at(9, 1, 30)));
        v.append_activity_events(&events).unwrap();

        let day = v.activity_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);

        let s = v.activity_summary("2026-06-10", "2026-06-10").unwrap();
        assert_eq!(s.active_seconds, 90); // 60 Code + 30 Safari
        assert_eq!(s.apps[0].app, "Code");
        assert_eq!(s.apps[0].seconds, 60);

        let daily = v.activity_daily("2026-06-09", "2026-06-11").unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].date, "2026-06-10");
        assert!((daily[0].value - 90.0 / 3600.0).abs() < 1e-9);
    }
}

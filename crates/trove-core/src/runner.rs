//! The periodic sync loop the app runs while it is open, and the read side
//! of the external collector's heartbeat.
//!
//! Trove has no always-on process of its own (docs/roadmap.md, decision 3).
//! Every [`Behavior::Periodic`] def runs from [`run_sync`] on a background
//! thread of the app; the first pass fires one poll after start, so opening
//! the app *is* the sync. Exactly one app instance syncs a given vault at a
//! time, coordinated by an OS advisory file lock (`.trove/sync.lock`); a
//! second instance idles and retries each poll, so when the owner exits (the
//! kernel releases flock on process death, crash included) the next one
//! takes over within ~one poll.
//!
//! The live streams (`activity/`, `browser/` extension rows, `browser/ads/`,
//! `music/plays/`) are written by the separate `trove-collector` binary,
//! which holds its own lock (`.trove/watcher.lock`) and heartbeats
//! `.trove/watcher-state.json`. This module only *reads* that heartbeat, so
//! the app can say whether the collector is running, what it is using, and
//! what the in-progress activity event is.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::activity::ActivityEvent;
use crate::integrations::INTEGRATIONS;
use crate::music_library::should_snapshot;
use crate::registry::{Advance, Behavior, Cadence, Gate};
use crate::vault::Vault;

/// Seconds between sync-loop polls; also the collector's heartbeat cadence
/// (the freshness window below is a multiple of it).
pub const POLL_SECS: u64 = 5;

const SYNC_LOCK_FILE: &str = ".trove/sync.lock";

/// The external collector's heartbeat (written by `trove-collector`, never
/// by this crate).
const COLLECTOR_STATE_FILE: &str = ".trove/watcher-state.json";

/// launchd label the external collector installs itself under. Shared with
/// the hub's "is it installed" check so they can never drift.
pub const COLLECTOR_LABEL: &str = "com.davidwills.trove-collector";

/// Where `trove-collector install` writes its launch agent plist.
pub fn collector_plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{COLLECTOR_LABEL}.plist")))
}

/// Heartbeat written by the external collector every poll. Removed on its
/// graceful shutdown; after a crash it goes stale instead (see
/// [`CollectorState::is_fresh`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectorState {
    pub pid: u32,
    /// The writing program's name, e.g. "trove-collector".
    pub role: String,
    /// RFC3339 local time of the last tick.
    pub updated: String,
    /// The in-progress (not yet written) activity event, if any.
    pub current: Option<ActivityEvent>,
    /// Resident memory of the collector process, in MB, when it reports it.
    #[serde(default)]
    pub rss_mb: Option<u64>,
}

impl CollectorState {
    /// Whether the heartbeat is recent enough to indicate a live collector.
    pub fn is_fresh(&self) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.updated)
            .map(|t| {
                Local::now().signed_duration_since(t).num_seconds() <= (POLL_SECS * 3) as i64
            })
            .unwrap_or(false)
    }
}

/// What the hub shows about the external collector. Cheap: one plist stat
/// and one tiny file read.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CollectorStatus {
    /// A fresh heartbeat exists.
    pub running: bool,
    /// The launch agent plist is present (it may still not be loaded).
    pub installed: bool,
    pub pid: Option<u32>,
    pub role: Option<String>,
    /// RFC3339 local time of the last heartbeat, fresh or not.
    pub updated: Option<String>,
    pub rss_mb: Option<u64>,
}

/// Thread-safe handle for embedding [`run_sync`]: request shutdown from
/// another thread. Cheap to clone.
#[derive(Clone, Default)]
pub struct SyncControl {
    stop: Arc<AtomicBool>,
}

impl SyncControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the loop to stop after the current pass.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// Blocking: contend for the vault's sync lock and, while holding it, run
/// every periodic collector on its cadence. While another app instance
/// holds the lock, idle and retry. Returns only when [`SyncControl::stop`]
/// is called. Collection errors inside the loop are logged to stderr and
/// skipped, never fatal.
pub fn run_sync(root: PathBuf, control: SyncControl) -> Result<()> {
    let vault = Vault::open_or_create(root)?;
    while !control.stopped() {
        match try_lock(&vault)? {
            Some(lock) => {
                owner_loop(&vault, &control);
                drop(lock);
            }
            None => sleep_unless_stopped(&control, StdDuration::from_secs(POLL_SECS)),
        }
    }
    Ok(())
}

/// The actual sync loop, run only while holding the lock. Entirely
/// registry-driven: every [`Behavior::Periodic`] def runs through its
/// [`Cadence`] gate. Adding a collector never touches this loop.
fn owner_loop(vault: &Vault, control: &SyncControl) {
    // Reverse the `CoveredBy` edges once: owner id → the rider ids whose
    // toggles also keep the owner's periodic pass running.
    let mut riders: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
    for d in INTEGRATIONS {
        if let Behavior::CoveredBy(owner) = d.behavior {
            riders.entry(owner).or_default().push(d.id);
        }
    }
    let mut jobs: HashMap<&'static str, JobState> = HashMap::new();
    while !control.stopped() {
        sleep_unless_stopped(control, StdDuration::from_secs(POLL_SECS));
        if control.stopped() {
            break;
        }
        // Hub toggles (`.trove/integrations.json`) are re-read every tick —
        // one tiny file — so disabling an integration takes effect within a
        // poll, no restart.
        let settings = vault.integration_settings();
        let enabled = |id: &str| crate::integrations::enabled_in(&settings, id);
        let now = Local::now();
        // One `Instant` and one `now` serve the whole tick, so jobs sharing a
        // period stay in lockstep forever (a slow pass can't fragment the
        // block across ticks) and a mid-tick midnight can't split a daily
        // gate from its snapshot stamp. The first pass runs one poll after
        // lock takeover. Collect errors are logged and skipped, never fatal.
        let tick_started = Instant::now();
        for def in INTEGRATIONS {
            let Behavior::Periodic { cadence, collect } = def.behavior else {
                continue;
            };
            let js = jobs.entry(def.id).or_default();
            let on = enabled(def.id)
                || riders.get(def.id).is_some_and(|rs| rs.iter().any(|g| enabled(g)));
            let Some(probed) = js.admit(&cadence, on, tick_started, now) else {
                continue;
            };
            match collect(vault, now) {
                Ok(out) => {
                    js.commit(&cadence.gate, probed, now);
                    if let Some(s) = out.summary {
                        eprintln!("trove sync: {s}");
                    }
                }
                Err(e) => eprintln!("trove sync: {} sync failed: {e:#}", def.id),
            }
        }
    }
}

/// Per-collector gating state for the registry-driven job loop, keyed by
/// integration id. Fresh per lock takeover — the first pass runs one poll
/// after takeover, and timers reset on handoff.
#[derive(Default)]
struct JobState {
    /// Timer for `Cadence::every_secs`. Always stamped from the tick's
    /// single `Instant`, never per-job `Instant::now()`, so jobs sharing a
    /// period can never drift apart however long one of them blocks.
    last_attempt: Option<Instant>,
    /// Local time of the last successful pass (`Gate::LocalDay`).
    last_success: Option<DateTime<Local>>,
    /// Source mtime at the last successful pass (`Gate::SourceMtime`).
    last_mtime: Option<SystemTime>,
}

impl JobState {
    /// Decide whether a job runs this tick, advancing the attempt timer per
    /// its `Advance` mode. Returns `None` to skip, or `Some(probed_mtime)`
    /// to run — the probed value goes back into [`JobState::commit`] on
    /// success, so mtime gates advance to exactly what was seen *before* the
    /// collect (a source write racing the copy re-triggers next tick).
    fn admit(
        &mut self,
        cadence: &Cadence,
        enabled: bool,
        tick_started: Instant,
        now: DateTime<Local>,
    ) -> Option<Option<SystemTime>> {
        let due = self
            .last_attempt
            .is_none_or(|t| tick_started.duration_since(t).as_secs() >= cadence.every_secs);
        if !due {
            return None;
        }
        // `Due` consumes the window even while the toggle is off (re-enabling
        // waits out the rest of the window); `Run` stamps only on a real run,
        // so re-enabling fires within one poll.
        if cadence.advance == Advance::Due {
            self.last_attempt = Some(tick_started);
        }
        if !enabled {
            return None;
        }
        if cadence.advance == Advance::Run {
            self.last_attempt = Some(tick_started);
        }
        match cadence.gate {
            Gate::Always => Some(None),
            Gate::LocalDay => should_snapshot(self.last_success, now).then_some(None),
            Gate::SourceMtime(probe) => {
                let m = probe();
                (m.is_some() && m != self.last_mtime).then_some(m)
            }
        }
    }

    /// Called only after a successful pass — failures leave the gate state
    /// untouched, so the next due tick retries (TCC denials self-recover
    /// without burning the daily/mtime gate).
    fn commit(&mut self, gate: &Gate, probed: Option<SystemTime>, now: DateTime<Local>) {
        match gate {
            Gate::Always => {}
            Gate::LocalDay => self.last_success = Some(now),
            Gate::SourceMtime(_) => self.last_mtime = probed,
        }
    }
}

/// Sleep `total` in short slices, returning early once stop is requested, so
/// shutdown never waits out a full poll interval.
fn sleep_unless_stopped(control: &SyncControl, total: StdDuration) {
    let slice = StdDuration::from_millis(250);
    let mut elapsed = StdDuration::ZERO;
    while elapsed < total && !control.stopped() {
        std::thread::sleep(slice);
        elapsed += slice;
    }
}

/// Try to take the sync lock. The returned `File` *is* the lock — keep it
/// alive while syncing; the kernel releases it when the fd closes or the
/// process dies.
#[cfg(unix)]
fn try_lock(vault: &Vault) -> Result<Option<File>> {
    use std::os::unix::io::AsRawFd;
    let path = vault.resolve(SYNC_LOCK_FILE)?;
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&path)
        .context("opening sync.lock")?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok((rc == 0).then_some(f))
}

/// Non-unix: no enforcement (the app is macOS-only anyway).
#[cfg(not(unix))]
fn try_lock(vault: &Vault) -> Result<Option<File>> {
    let path = vault.resolve(SYNC_LOCK_FILE)?;
    Ok(Some(
        OpenOptions::new().create(true).write(true).open(&path)?,
    ))
}

impl Vault {
    /// The external collector's heartbeat, if a state file exists (check
    /// [`CollectorState::is_fresh`] before trusting it).
    pub fn read_collector_state(&self) -> Option<CollectorState> {
        let path = self.resolve(COLLECTOR_STATE_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Everything the hub shows about the external collector.
    pub fn collector_status(&self) -> CollectorStatus {
        let state = self.read_collector_state();
        let fresh = state.as_ref().is_some_and(|s| s.is_fresh());
        CollectorStatus {
            running: fresh,
            installed: collector_plist_path().is_some_and(|p| p.exists()),
            pid: state.as_ref().filter(|_| fresh).map(|s| s.pid),
            role: state.as_ref().filter(|_| fresh).map(|s| s.role.clone()),
            updated: state.as_ref().map(|s| s.updated.clone()),
            rss_mb: state.as_ref().filter(|_| fresh).and_then(|s| s.rss_mb),
        }
    }

    /// The in-progress activity event from a fresh collector heartbeat.
    pub fn collector_current(&self) -> Option<ActivityEvent> {
        self.read_collector_state().filter(|s| s.is_fresh()).and_then(|s| s.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-runner-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn lock_is_exclusive_until_released() {
        let v = temp_vault("lock");
        let first = try_lock(&v).unwrap();
        assert!(first.is_some());
        // flock treats separately-opened descriptors independently, so a
        // second open in the same process models a second process.
        assert!(try_lock(&v).unwrap().is_none(), "second lock must fail");
        drop(first);
        assert!(try_lock(&v).unwrap().is_some(), "released lock is reacquirable");
    }

    fn local(s: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Local)
    }

    #[test]
    fn job_due_and_advance_semantics() {
        let every = Cadence::every(900);
        let on_run = Cadence::every_on_run(900);
        let t0 = Instant::now();
        let now = Local::now();

        // First tick after takeover is always due.
        let mut js = JobState::default();
        assert!(js.admit(&every, true, t0, now).is_some());
        // Stamped: not due again until the window passes…
        assert!(js.admit(&every, true, t0 + StdDuration::from_secs(895), now).is_none());
        // …then due exactly at the window, stamped from the tick Instant.
        assert!(js.admit(&every, true, t0 + StdDuration::from_secs(900), now).is_some());

        // Advance::Due consumes the window even while disabled: re-enabling
        // mid-window stays quiet until the next boundary.
        let mut js = JobState::default();
        assert!(js.admit(&every, false, t0, now).is_none());
        assert!(js.admit(&every, true, t0 + StdDuration::from_secs(5), now).is_none());
        assert!(js.admit(&every, true, t0 + StdDuration::from_secs(900), now).is_some());

        // Advance::Run leaves the timer untouched while disabled: re-enable
        // fires on the next tick.
        let mut js = JobState::default();
        assert!(js.admit(&on_run, false, t0, now).is_none());
        assert!(js.admit(&on_run, true, t0 + StdDuration::from_secs(5), now).is_some());
        assert!(js.admit(&on_run, true, t0 + StdDuration::from_secs(10), now).is_none());
    }

    #[test]
    fn job_local_day_gate_commits_on_success_only() {
        let daily = Cadence::daily(900);
        let t0 = Instant::now();
        let morning = local("2026-06-11T09:00:00-07:00");
        let noon = local("2026-06-11T12:00:00-07:00");
        let tomorrow = local("2026-06-12T09:00:00-07:00");

        let mut js = JobState::default();
        assert!(js.admit(&daily, true, t0, morning).is_some());
        // No commit (the pass failed) → the next due tick retries same-day.
        assert!(js
            .admit(&daily, true, t0 + StdDuration::from_secs(900), noon)
            .is_some());
        js.commit(&daily.gate, None, noon);
        // Committed → closed for the rest of the day, open again tomorrow.
        assert!(js
            .admit(&daily, true, t0 + StdDuration::from_secs(1800), noon)
            .is_none());
        assert!(js
            .admit(&daily, true, t0 + StdDuration::from_secs(2700), tomorrow)
            .is_some());
    }

    // Controllable probe for the mtime-gate test (hooks are fn pointers, so
    // the test signal rides a static).
    static PROBE_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn test_probe() -> Option<SystemTime> {
        let s = PROBE_SECS.load(Ordering::SeqCst);
        (s != 0).then(|| SystemTime::UNIX_EPOCH + StdDuration::from_secs(s))
    }

    #[test]
    fn job_mtime_gate_advances_on_success_only() {
        let gated = Cadence::on_change(900, test_probe);
        let t0 = Instant::now();
        let now = Local::now();
        let tick = |n: u64| t0 + StdDuration::from_secs(900 * n);

        let mut js = JobState::default();
        // Missing source (probe None) keeps the gate closed.
        PROBE_SECS.store(0, Ordering::SeqCst);
        assert!(js.admit(&gated, true, tick(0), now).is_none());

        // Source appears → open; failure (no commit) → still open next tick.
        PROBE_SECS.store(1_000, Ordering::SeqCst);
        let probed = js.admit(&gated, true, tick(1), now).expect("gate open");
        assert!(probed.is_some());
        assert!(js.admit(&gated, true, tick(2), now).is_some());

        // Success commits the *probed* value → unchanged source stays closed.
        js.commit(&gated.gate, probed, now);
        assert!(js.admit(&gated, true, tick(3), now).is_none());

        // A write racing the copy: source changed again → re-triggers.
        PROBE_SECS.store(2_000, Ordering::SeqCst);
        assert!(js.admit(&gated, true, tick(4), now).is_some());
    }

    #[test]
    fn collector_heartbeat_is_read_with_freshness() {
        let v = temp_vault("heartbeat");
        let status = v.collector_status();
        assert!(!status.running);
        assert!(status.pid.is_none() && status.updated.is_none());

        // A fresh heartbeat, exactly as trove-collector writes it (extra
        // fields tolerated, `rss_mb` optional).
        let fresh = CollectorState {
            pid: 42,
            role: "trove-collector".into(),
            updated: Local::now().to_rfc3339(),
            current: None,
            rss_mb: Some(31),
        };
        let path = v.resolve(COLLECTOR_STATE_FILE).unwrap();
        fs::write(&path, serde_json::to_vec(&fresh).unwrap()).unwrap();
        let status = v.collector_status();
        assert!(status.running);
        assert_eq!(status.pid, Some(42));
        assert_eq!(status.role.as_deref(), Some("trove-collector"));
        assert_eq!(status.rss_mb, Some(31));

        // Stale: still reports when it was last seen, but not as running.
        let stale = CollectorState {
            updated: (Local::now() - chrono::Duration::seconds(60)).to_rfc3339(),
            ..fresh
        };
        fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let status = v.collector_status();
        assert!(!status.running);
        assert!(status.pid.is_none());
        assert_eq!(status.updated.as_deref(), Some(stale.updated.as_str()));
        assert!(v.collector_current().is_none());
    }
}

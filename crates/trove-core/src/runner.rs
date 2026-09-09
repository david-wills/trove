//! The always-on watcher loop shared by the GUI app and the `troved` daemon.
//!
//! Exactly one process may write activity events at a time. Coordination is an
//! OS advisory file lock (`.trove/watcher.lock`): whoever holds it runs the
//! sample→tick→append loop and heartbeats `.trove/watcher-state.json`; every
//! other process idles, mirrors that heartbeat for its live "current activity"
//! view, and retries the lock each poll — so when the owner exits (the kernel
//! releases flock on process death, crash included) the next contender takes
//! over within ~one poll. No pidfiles, no IPC: the handoff between app and
//! daemon is automatic in both directions.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::activity::{ActivityEvent, POLL_SECS};
use crate::integrations::INTEGRATIONS;
use crate::music_library::should_snapshot;
use crate::registry::{Advance, Behavior, Cadence, Gate};
use crate::vault::Vault;

const STATE_FILE: &str = ".trove/watcher-state.json";
const LOCK_FILE: &str = ".trove/watcher.lock";

/// launchd label for the troved daemon. Shared with `troved install` and the
/// app's "is the daemon installed" check so they can never drift.
pub const DAEMON_LABEL: &str = "com.davidwills.troved";

/// Where `troved install` writes the launch agent plist.
pub fn daemon_plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{DAEMON_LABEL}.plist")))
}

/// Which kind of process is hosting the watcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherRole {
    App,
    Daemon,
}

impl WatcherRole {
    pub fn as_str(self) -> &'static str {
        match self {
            WatcherRole::App => "app",
            WatcherRole::Daemon => "daemon",
        }
    }
}

/// Heartbeat written by the lock owner every poll. Non-owners read it to show
/// who is collecting and what the in-progress event is. Removed on graceful
/// shutdown; after a crash it goes stale instead (see [`WatcherState::is_fresh`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatcherState {
    pub pid: u32,
    /// "app" or "daemon".
    pub role: String,
    /// RFC3339 local time of the last tick.
    pub updated: String,
    /// The in-progress (not yet written) event, if any.
    pub current: Option<ActivityEvent>,
}

impl WatcherState {
    /// Whether the heartbeat is recent enough to indicate a live owner.
    pub fn is_fresh(&self) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.updated)
            .map(|t| {
                Local::now().signed_duration_since(t).num_seconds() <= (POLL_SECS * 3) as i64
            })
            .unwrap_or(false)
    }
}

/// Thread-safe handle for embedding [`run_watcher`]: request shutdown and read
/// live state from other threads. Cheap to clone.
#[derive(Clone, Default)]
pub struct WatchControl {
    inner: Arc<ControlInner>,
}

#[derive(Default)]
struct ControlInner {
    stop: AtomicBool,
    owns: AtomicBool,
    current: Mutex<Option<ActivityEvent>>,
}

impl WatchControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the loop to stop. It flushes the open event before returning.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.inner.stop.load(Ordering::SeqCst)
    }

    /// Does this process currently hold the single-writer lock?
    pub fn owns(&self) -> bool {
        self.inner.owns.load(Ordering::SeqCst)
    }

    /// The in-progress event — ours if we own the lock, otherwise mirrored
    /// from the owner's heartbeat.
    pub fn current(&self) -> Option<ActivityEvent> {
        self.inner.current.lock().unwrap().clone()
    }

    fn set_owns(&self, v: bool) {
        self.inner.owns.store(v, Ordering::SeqCst);
    }

    fn set_current(&self, v: Option<ActivityEvent>) {
        *self.inner.current.lock().unwrap() = v;
    }
}

/// Blocking: contend for the vault's single-writer lock and, while holding it,
/// run the watcher loop. While another process holds the lock, idle and mirror
/// its heartbeat. Returns only when [`WatchControl::stop`] is called (the open
/// event is flushed first). Collection errors inside the loop are logged to
/// stderr and skipped, never fatal — a daemon must outlive transient failures.
pub fn run_watcher(root: PathBuf, role: WatcherRole, control: WatchControl) -> Result<()> {
    let vault = Vault::open_or_create(root)?;
    while !control.stopped() {
        match try_lock(&vault)? {
            Some(lock) => {
                control.set_owns(true);
                owner_loop(&vault, role, &control);
                control.set_owns(false);
                drop(lock);
            }
            None => {
                let state = vault.read_watcher_state().filter(|s| s.is_fresh());
                control.set_current(state.and_then(|s| s.current));
                sleep_unless_stopped(&control, StdDuration::from_secs(POLL_SECS));
            }
        }
    }
    Ok(())
}

/// The actual collection loop, run only while holding the lock — so
/// single-writer applies to the whole vault. Entirely registry-driven: live
/// collectors ([`Behavior::Live`]) tick every poll, periodic collect hooks
/// ([`Behavior::Periodic`]) run through their [`Cadence`] gates. Adding a
/// collector never touches this loop.
fn owner_loop(vault: &Vault, role: WatcherRole, control: &WatchControl) {
    // Live collectors are built once per takeover (the music listener starts
    // its notification channel here, exactly as before).
    let mut live: Vec<(&'static crate::registry::IntegrationDef, Box<dyn crate::registry::LiveCollector>)> =
        INTEGRATIONS
            .iter()
            .filter_map(|d| match d.behavior {
                Behavior::Live(make) => Some((*d, make())),
                _ => None,
            })
            .collect();
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
        // poll, no restart, in app and daemon alike.
        let settings = vault.integration_settings();
        let enabled = |id: &str| crate::integrations::enabled_in(&settings, id);
        let now = Local::now();
        for (def, collector) in &mut live {
            collector.tick(vault, now, enabled(def.id));
        }
        let current = live.iter().find_map(|(_, l)| l.current(now));
        control.set_current(current.clone());
        let state = WatcherState {
            pid: std::process::id(),
            role: role.as_str().into(),
            updated: now.to_rfc3339(),
            current,
        };
        if let Err(e) = vault.write_watcher_state(&state) {
            eprintln!("trove watcher: failed to write heartbeat: {e:#}");
        }
        // Registry-driven periodic collectors. One `Instant` and one `now`
        // serve the whole tick, so jobs sharing a period stay in lockstep
        // forever (the old shared slow-tick timer, generalized — a slow
        // browser copy can't fragment the block across ticks) and a mid-tick
        // midnight can't split a daily gate from its snapshot stamp. The
        // first pass runs one poll after lock takeover, as before. Collect
        // errors are logged and skipped, never fatal.
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
                        eprintln!("trove watcher: {s}");
                    }
                }
                Err(e) => eprintln!("trove watcher: {} sync failed: {e:#}", def.id),
            }
        }
    }
    // Graceful shutdown: every live collector closes out its open spans, then
    // the heartbeat is cleared so status flips immediately instead of waiting
    // out the freshness window.
    let now = Local::now();
    for (_, collector) in &mut live {
        collector.shutdown(vault, now);
    }
    control.set_current(None);
    if let Err(e) = vault.clear_watcher_state() {
        eprintln!("trove watcher: failed to clear heartbeat: {e:#}");
    }
}

/// Per-collector gating state for the registry-driven job loop, keyed by
/// integration id. Fresh per lock takeover, like the old inline timers — the
/// first pass runs one poll after takeover, and timers reset on handoff.
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
        // `Due` consumes the window even while the toggle is off (the
        // historical slow-tick semantics: re-enabling waits out the rest of
        // the window); `Run` stamps only on a real run, so re-enabling fires
        // within one poll (the historical oura/gmail semantics).
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
fn sleep_unless_stopped(control: &WatchControl, total: StdDuration) {
    let slice = StdDuration::from_millis(250);
    let mut elapsed = StdDuration::ZERO;
    while elapsed < total && !control.stopped() {
        std::thread::sleep(slice);
        elapsed += slice;
    }
}

/// Try to take the single-writer lock. The returned `File` *is* the lock —
/// keep it alive while collecting; the kernel releases it when the fd closes
/// or the process dies.
#[cfg(unix)]
fn try_lock(vault: &Vault) -> Result<Option<File>> {
    use std::os::unix::io::AsRawFd;
    let path = vault.resolve(LOCK_FILE)?;
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&path)
        .context("opening watcher.lock")?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok((rc == 0).then_some(f))
}

/// Non-unix: no enforcement (the daemon is macOS-only anyway).
#[cfg(not(unix))]
fn try_lock(vault: &Vault) -> Result<Option<File>> {
    let path = vault.resolve(LOCK_FILE)?;
    Ok(Some(
        OpenOptions::new().create(true).write(true).open(&path)?,
    ))
}

impl Vault {
    /// The watcher heartbeat, if a state file exists (check
    /// [`WatcherState::is_fresh`] before trusting it).
    pub fn read_watcher_state(&self) -> Option<WatcherState> {
        let path = self.resolve(STATE_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Atomic write (temp + rename) so readers never see a torn file.
    fn write_watcher_state(&self, state: &WatcherState) -> Result<()> {
        let path = self.resolve(STATE_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn clear_watcher_state(&self) -> Result<()> {
        let path = self.resolve(STATE_FILE)?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
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
    fn state_round_trip_freshness_and_clear() {
        let v = temp_vault("state");
        assert!(v.read_watcher_state().is_none());

        let fresh = WatcherState {
            pid: 42,
            role: "daemon".into(),
            updated: Local::now().to_rfc3339(),
            current: None,
        };
        v.write_watcher_state(&fresh).unwrap();
        let read = v.read_watcher_state().unwrap();
        assert_eq!(read.pid, 42);
        assert_eq!(read.role, "daemon");
        assert!(read.is_fresh());

        let stale = WatcherState {
            updated: (Local::now() - chrono::Duration::seconds(60)).to_rfc3339(),
            ..fresh
        };
        v.write_watcher_state(&stale).unwrap();
        assert!(!v.read_watcher_state().unwrap().is_fresh());

        v.clear_watcher_state().unwrap();
        assert!(v.read_watcher_state().is_none());
    }
}

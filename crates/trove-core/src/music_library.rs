//! Apple Music library snapshot + diff collector.
//!
//! Complements the live scrobbler in [`crate::music`]: the scrobbler captures
//! the unrecoverable play *stream*, this module captures the library *state*
//! (what's in the library, lifetime play counts, ratings) and the slow drift
//! of that state over time. Mirrors the [`crate::tasks`] snapshot+events
//! precedent:
//!
//! - `music/library/tracks.jsonl` — the current snapshot, one track per line,
//!   atomically rewritten (temp file + rename) on every snapshot.
//! - `music/library/events/YYYY-MM.jsonl` — append-only diff events:
//!   `{"ts":…,"kind":"added"|"removed"|"changed","id":…,"title":…,"artist":…}`,
//!   where `changed` events also carry a `changes` list of
//!   `{"field":…,"old":…,"new":…}` for the fields that drift (play_count,
//!   rating, last_played).
//!
//! **The first-ever snapshot emits no events** — it just writes the baseline.
//! A 17k-track library on day one is state, not 17k "added" events; the event
//! stream records change *from the baseline on*.
//!
//! The diff and the persistence orchestrator are pure/OS-free (tracks and
//! timestamps injected — the [`crate::activity::Watcher`] pattern); the thin
//! [`read_library`] reader binds iTunesLibrary.framework per the spike notes
//! in `docs/notes/itlibrary-spike.md` and is only exercised at runtime by the
//! collector host, never in tests. Snapshots are cheap but the data drifts
//! slowly, so the owner loop takes the first snapshot of each local day —
//! gate on [`should_snapshot`].

use std::collections::{BTreeMap, HashSet};
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;

const TRACKS_FILE: &str = "music/library/tracks.jsonl";

// A Media Library TCC denial surfaces as the read failing — it logs and
// retries next gate-open tick, never fatal (the gate commits only on
// success, so the daily snapshot isn't burned by a denial).
fn def_collect(vault: &Vault, now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let stats = read_library()
        .and_then(|tracks| vault.music_library_snapshot(&tracks, &now.to_rfc3339()))?;
    Ok(crate::registry::CollectOutcome::note(format!(
        "music library snapshot: {stats:?}"
    )))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "media-library",
        granted: None, // only fails at read time; no cheap preflight
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::file_mtime(&vault.root().join(TRACKS_FILE))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "music-library",
        name: "Apple Music library",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Daily library snapshot — play counts, ratings, added and removed tracks. The diff is the only retroactive music signal: it catches plays made while no collector ran.",
        domain: "media",
        vault_path: "music/library/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Media & Apple Music → add the troved binary. No prompt will ever appear — this grant is manual.",
            "Restart the daemon after granting.",
        ],
        caveats: "Until the grant is made the daily snapshot fails quietly (it logs and retries) — the status here can't preflight this permission, so check for a tracks.jsonl after the first day.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::daily(crate::browser::BROWSER_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// One track of the library snapshot — a line in `music/library/tracks.jsonl`.
/// Everything except `id` defaults, so lines written by older versions keep
/// deserializing as fields are added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryTrack {
    /// Music's persistent ID as 16 uppercase hex digits (same form the
    /// scrobbler records), the diff key.
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub genre: String,
    /// Music's lifetime play count.
    #[serde(default)]
    pub play_count: u64,
    /// Star rating ×20 (0–100); 0 = unrated.
    #[serde(default)]
    pub rating: i64,
    /// RFC3339 local time of the last play Music knows about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<String>,
    /// RFC3339 local time the track entered the library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_added: Option<String>,
    /// Track length in seconds; 0 when Music doesn't know.
    #[serde(default)]
    pub duration_secs: f64,
}

/// One drifted field on a `changed` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct FieldChange {
    /// "play_count", "rating", or "last_played".
    pub field: String,
    pub old: Value,
    pub new: Value,
}

/// One line of `music/library/events/YYYY-MM.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryEvent {
    /// RFC3339 local time of the snapshot that observed the change.
    pub ts: String,
    /// "added", "removed", or "changed".
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    /// Only on `changed` events: exactly the fields that drifted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<FieldChange>,
}

/// Result of one snapshot pass, for logging/status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LibrarySnapshotStats {
    /// Tracks in the snapshot just written.
    pub tracks: u64,
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
    /// True when this was the first-ever snapshot (baseline, no events).
    pub baseline: bool,
}

/// Daily cadence gate for the owner loop: take a snapshot when none has run
/// yet, or when the last one ran on an earlier local calendar day.
pub fn should_snapshot(last_run: Option<DateTime<Local>>, now: DateTime<Local>) -> bool {
    match last_run {
        None => true,
        Some(t) => t.date_naive() < now.date_naive(),
    }
}

/// The pure diff: compare two snapshots, emit events stamped `ts`. Order is
/// deterministic — added/changed in new-snapshot order, then removed in
/// old-snapshot order.
pub fn diff_snapshots(old: &[LibraryTrack], new: &[LibraryTrack], ts: &str) -> Vec<LibraryEvent> {
    let old_by_id: BTreeMap<&str, &LibraryTrack> =
        old.iter().map(|t| (t.id.as_str(), t)).collect();
    let new_ids: HashSet<&str> = new.iter().map(|t| t.id.as_str()).collect();
    let mut events = Vec::new();
    for track in new {
        match old_by_id.get(track.id.as_str()) {
            None => events.push(event(ts, "added", track, Vec::new())),
            Some(before) => {
                let changes = field_changes(before, track);
                if !changes.is_empty() {
                    events.push(event(ts, "changed", track, changes));
                }
            }
        }
    }
    for track in old {
        if !new_ids.contains(track.id.as_str()) {
            events.push(event(ts, "removed", track, Vec::new()));
        }
    }
    events
}

fn event(ts: &str, kind: &str, t: &LibraryTrack, changes: Vec<FieldChange>) -> LibraryEvent {
    LibraryEvent {
        ts: ts.to_string(),
        kind: kind.to_string(),
        id: t.id.clone(),
        title: t.title.clone(),
        artist: t.artist.clone(),
        changes,
    }
}

/// The fields that drift and matter: play_count, rating, last_played.
/// (Metadata edits — retagged genre, fixed title — just update the snapshot
/// silently; they're corrections, not events in your listening life.)
fn field_changes(old: &LibraryTrack, new: &LibraryTrack) -> Vec<FieldChange> {
    let mut out = Vec::new();
    let mut push = |field: &str, old: Value, new: Value| {
        out.push(FieldChange {
            field: field.to_string(),
            old,
            new,
        });
    };
    if old.play_count != new.play_count {
        push("play_count", old.play_count.into(), new.play_count.into());
    }
    if old.rating != new.rating {
        push("rating", old.rating.into(), new.rating.into());
    }
    if old.last_played != new.last_played {
        let v = |o: &Option<String>| o.clone().map(Value::from).unwrap_or(Value::Null);
        push("last_played", v(&old.last_played), v(&new.last_played));
    }
    out
}

impl Vault {
    /// The stored library snapshot; empty when no snapshot has ever run.
    /// Lenient line-by-line parse, like every other vault reader.
    pub fn load_music_library(&self) -> Result<Vec<LibraryTrack>> {
        let path = self.resolve(TRACKS_FILE)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body =
            fs::read_to_string(&path).with_context(|| format!("reading {TRACKS_FILE}"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<LibraryTrack>(l).ok())
            .collect())
    }

    /// One snapshot pass: diff `fresh` against the stored snapshot, append
    /// the diff events to their month's log, atomically rewrite the snapshot.
    /// The first-ever snapshot writes the baseline and emits no events. `ts`
    /// is the injected RFC3339 local snapshot time (callers pass
    /// `Local::now().to_rfc3339()`).
    pub fn music_library_snapshot(
        &self,
        fresh: &[LibraryTrack],
        ts: &str,
    ) -> Result<LibrarySnapshotStats> {
        let baseline = !self.resolve(TRACKS_FILE)?.exists();
        let events = if baseline {
            Vec::new()
        } else {
            diff_snapshots(&self.load_music_library()?, fresh, ts)
        };
        self.append_music_library_events(&events)?;
        self.write_music_library(fresh)?;
        let count = |kind: &str| events.iter().filter(|e| e.kind == kind).count() as u64;
        Ok(LibrarySnapshotStats {
            tracks: fresh.len() as u64,
            added: count("added"),
            removed: count("removed"),
            changed: count("changed"),
            baseline,
        })
    }

    /// Library diff events over an inclusive date range, chronological.
    pub fn music_library_events(&self, from: &str, to: &str) -> Result<Vec<LibraryEvent>> {
        let (from_month, to_month) = (&from[..7.min(from.len())], &to[..7.min(to.len())]);
        let dir = self.root().join("music/library/events");
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(out);
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(month) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if month < from_month || month > to_month {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            out.extend(
                body.lines()
                    .filter_map(|l| serde_json::from_str::<LibraryEvent>(l).ok())
                    .filter(|e| {
                        let day = &e.ts[..10.min(e.ts.len())];
                        day >= from && day <= to
                    }),
            );
        }
        out.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(out)
    }

    /// Atomic snapshot rewrite: temp file in place, then rename, so readers
    /// never see a torn file.
    fn write_music_library(&self, tracks: &[LibraryTrack]) -> Result<()> {
        self.write_snapshot(TRACKS_FILE, tracks)
    }

    /// Append events to their month's JSONL log (keyed by local event month).
    fn append_music_library_events(&self, events: &[LibraryEvent]) -> Result<()> {
        self.stream("music/library/events", crate::store::Partition::Month)
            .append(events, |e| &e.ts)
    }
}

/// Read the live Apple Music library via iTunesLibrary.framework. Runtime
/// only (the collector host calls it); tests inject [`LibraryTrack`]s
/// directly. Requires the Media Library TCC grant — see
/// `docs/notes/itlibrary-spike.md` for the signing/TCC realities (the
/// toolchain's ad-hoc linker signature is sufficient; no extra build config).
pub fn read_library() -> Result<Vec<LibraryTrack>> {
    imp::read_library()
}

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{anyhow, Result};
    use chrono::{DateTime, Local};
    use objc2_foundation::{NSDate, NSString};
    use objc2_itunes_library::ITLibrary;

    use super::LibraryTrack;

    pub fn read_library() -> Result<Vec<LibraryTrack>> {
        let api = NSString::from_str("1.0");
        let lib = unsafe { ITLibrary::libraryWithAPIVersion_error(&api) }.map_err(|e| {
            anyhow!(
                "ITLibrary unavailable ({} {}): {}",
                e.domain(),
                e.code(),
                e.localizedDescription()
            )
        })?;
        let items = unsafe { lib.allMediaItems() };
        let mut out = Vec::with_capacity(items.len());
        for item in items.iter() {
            unsafe {
                out.push(LibraryTrack {
                    // Canonical Music form: 16 uppercase hex digits (matches
                    // the scrobbler's persistent_id).
                    id: format!("{:016X}", item.persistentID().longLongValue() as u64),
                    title: item.title().to_string(),
                    artist: item
                        .artist()
                        .and_then(|a| a.name())
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                    album: item
                        .album()
                        .title()
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                    genre: item.genre().to_string(),
                    play_count: item.playCount() as u64,
                    rating: item.rating() as i64,
                    last_played: item.lastPlayedDate().map(|d| rfc3339(&d)),
                    date_added: item.addedDate().map(|d| rfc3339(&d)),
                    duration_secs: item.totalTime() as f64 / 1000.0,
                });
            }
        }
        Ok(out)
    }

    fn rfc3339(date: &NSDate) -> String {
        DateTime::from_timestamp_millis((date.timeIntervalSince1970() * 1000.0) as i64)
            .map(|t| t.with_timezone(&Local).to_rfc3339())
            .unwrap_or_default()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use anyhow::{bail, Result};

    use super::LibraryTrack;

    pub fn read_library() -> Result<Vec<LibraryTrack>> {
        bail!("the Apple Music library is only readable on macOS");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-music-lib-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn track(id: &str, title: &str) -> LibraryTrack {
        LibraryTrack {
            id: id.into(),
            title: title.into(),
            artist: "Artist".into(),
            album: "Album".into(),
            genre: "Pop".into(),
            play_count: 3,
            rating: 80,
            last_played: Some("2026-06-01T10:00:00-07:00".into()),
            date_added: Some("2025-01-01T09:00:00-08:00".into()),
            duration_secs: 228.7,
        }
    }

    const TS: &str = "2026-06-10T12:00:00-07:00";

    /// Byte-parity contract for both write paths (snapshot rewrite +
    /// month-keyed event append): exact file bytes, pinned before the port
    /// onto `store` and unchanged by it.
    #[test]
    fn snapshot_and_events_write_byte_identical_files() {
        let v = temp_vault("parity");
        let a = track("A", "Kept");
        let c = track("C", "Added");
        v.music_library_snapshot(&[a.clone()], "2026-06-09T12:00:00-07:00").unwrap();
        v.music_library_snapshot(&[a.clone(), c.clone()], TS).unwrap();
        v.music_library_snapshot(&[a], "2026-06-11T12:00:00-07:00").unwrap();

        assert_eq!(
            fs::read_to_string(v.root().join("music/library/tracks.jsonl")).unwrap(),
            "{\"id\":\"A\",\"title\":\"Kept\",\"artist\":\"Artist\",\"album\":\"Album\",\"genre\":\"Pop\",\"play_count\":3,\"rating\":80,\"last_played\":\"2026-06-01T10:00:00-07:00\",\"date_added\":\"2025-01-01T09:00:00-08:00\",\"duration_secs\":228.7}\n",
            "snapshot rewrite"
        );
        assert_eq!(
            fs::read_to_string(v.root().join("music/library/events/2026-06.jsonl")).unwrap(),
            format!(
                "{{\"ts\":\"{TS}\",\"kind\":\"added\",\"id\":\"C\",\"title\":\"Added\",\"artist\":\"Artist\"}}\n\
                 {{\"ts\":\"2026-06-11T12:00:00-07:00\",\"kind\":\"removed\",\"id\":\"C\",\"title\":\"Added\",\"artist\":\"Artist\"}}\n"
            ),
            "month-keyed event append extends across calls"
        );
    }

    #[test]
    fn diff_detects_added_removed_changed() {
        let kept = track("A", "Kept");
        let mut bumped = kept.clone();
        bumped.play_count = 4;
        let old = vec![kept.clone(), track("B", "Removed")];
        let new = vec![bumped, track("C", "Added")];

        let events = diff_snapshots(&old, &new, TS);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, "changed");
        assert_eq!(events[0].id, "A");
        assert_eq!(events[1].kind, "added");
        assert_eq!(events[1].id, "C");
        assert_eq!(events[1].title, "Added");
        assert_eq!(events[2].kind, "removed");
        assert_eq!(events[2].id, "B");
        assert!(events.iter().all(|e| e.ts == TS));
        assert!(events[1].changes.is_empty(), "added carries no change list");
    }

    #[test]
    fn changed_carries_only_the_drifted_fields() {
        let before = track("A", "Song");
        let mut after = before.clone();
        after.play_count = 4;
        after.last_played = Some(TS.into());
        // rating untouched; metadata edits are not events either.
        after.genre = "Synthpop".into();

        let events = diff_snapshots(&[before], &[after], TS);
        assert_eq!(events.len(), 1);
        let changes = &events[0].changes;
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].field, "play_count");
        assert_eq!(changes[0].old, Value::from(3));
        assert_eq!(changes[0].new, Value::from(4));
        assert_eq!(changes[1].field, "last_played");
        assert_eq!(changes[1].old, Value::from("2026-06-01T10:00:00-07:00"));
        assert_eq!(changes[1].new, Value::from(TS));
    }

    #[test]
    fn rating_change_and_null_last_played_old_value() {
        let mut before = track("A", "Song");
        before.last_played = None;
        let mut after = before.clone();
        after.rating = 100;
        after.last_played = Some(TS.into());

        let events = diff_snapshots(&[before], &[after], TS);
        assert_eq!(events[0].changes.len(), 2);
        assert_eq!(events[0].changes[0].field, "rating");
        assert_eq!(events[0].changes[0].old, Value::from(80));
        assert_eq!(events[0].changes[0].new, Value::from(100));
        assert_eq!(events[0].changes[1].field, "last_played");
        assert_eq!(events[0].changes[1].old, Value::Null);
    }

    #[test]
    fn identical_snapshots_produce_no_events() {
        let tracks = vec![track("A", "One"), track("B", "Two")];
        assert!(diff_snapshots(&tracks, &tracks.clone(), TS).is_empty());
    }

    #[test]
    fn first_snapshot_is_a_silent_baseline() {
        let v = temp_vault("baseline");
        let tracks = vec![track("A", "One"), track("B", "Two")];
        let stats = v.music_library_snapshot(&tracks, TS).unwrap();
        assert!(stats.baseline);
        assert_eq!(stats.tracks, 2);
        assert_eq!(stats.added + stats.removed + stats.changed, 0);
        assert_eq!(v.load_music_library().unwrap(), tracks);
        assert!(
            !v.root().join("music/library/events").exists(),
            "no event files on day one"
        );
        assert!(v.music_library_events("2026-06-01", "2026-06-30").unwrap().is_empty());
    }

    #[test]
    fn second_snapshot_diffs_and_appends_events() {
        let v = temp_vault("diff");
        let a = track("A", "Kept");
        v.music_library_snapshot(&[a.clone(), track("B", "Doomed")], "2026-06-09T08:00:00-07:00")
            .unwrap();

        let mut a2 = a.clone();
        a2.play_count = 4;
        let stats = v.music_library_snapshot(&[a2.clone(), track("C", "New")], TS).unwrap();
        assert!(!stats.baseline);
        assert_eq!((stats.added, stats.removed, stats.changed), (1, 1, 1));
        assert_eq!(stats.tracks, 2);

        // Snapshot fully rewritten: B gone, no temp file lingering.
        let now = v.load_music_library().unwrap();
        assert_eq!(now, vec![a2, track("C", "New")]);
        assert!(!v.root().join("music/library/tracks.jsonl.tmp").exists());

        // Events landed in their month's log, in diff order.
        assert!(v.root().join("music/library/events/2026-06.jsonl").exists());
        let events = v.music_library_events("2026-06-10", "2026-06-10").unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, "changed");
        assert_eq!(events[1].kind, "added");
        assert_eq!(events[2].kind, "removed");
        assert_eq!(events[2].title, "Doomed");

        // Third snapshot with no drift appends nothing.
        let stats = v.music_library_snapshot(&now, "2026-06-11T08:00:00-07:00").unwrap();
        assert_eq!(stats.added + stats.removed + stats.changed, 0);
        assert_eq!(v.music_library_events("2026-06-01", "2026-06-30").unwrap().len(), 3);
    }

    #[test]
    fn events_filter_by_month_and_day() {
        let v = temp_vault("ranges");
        v.music_library_snapshot(&[track("A", "One")], "2026-05-31T08:00:00-07:00")
            .unwrap();
        v.music_library_snapshot(&[track("A", "One"), track("B", "Two")], "2026-05-31T20:00:00-07:00")
            .unwrap();
        v.music_library_snapshot(&[track("B", "Two")], TS).unwrap();
        assert!(v.root().join("music/library/events/2026-05.jsonl").exists());
        assert!(v.root().join("music/library/events/2026-06.jsonl").exists());

        let all = v.music_library_events("2026-05-01", "2026-06-30").unwrap();
        assert_eq!(all.len(), 2, "baseline emitted nothing");
        assert_eq!(all[0].kind, "added");
        assert_eq!(all[1].kind, "removed");
        let june = v.music_library_events("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(june.len(), 1);
        assert_eq!(june[0].id, "A");
    }

    #[test]
    fn minimal_old_style_line_still_parses() {
        // A line from a hypothetical older schema: only id and title. All
        // later fields must default rather than poison the snapshot load.
        let t: LibraryTrack =
            serde_json::from_str(r#"{"id":"DAE7C16E62517F1A","title":"Take You There"}"#).unwrap();
        assert_eq!(t.id, "DAE7C16E62517F1A");
        assert_eq!(t.title, "Take You There");
        assert_eq!(t.artist, "");
        assert_eq!(t.play_count, 0);
        assert_eq!(t.rating, 0);
        assert!(t.last_played.is_none());
        assert!(t.date_added.is_none());
        assert_eq!(t.duration_secs, 0.0);

        // And None options are skipped on write, keeping lines minimal.
        let body = serde_json::to_string(&t).unwrap();
        assert!(!body.contains("last_played"));
        assert!(!body.contains("date_added"));
    }

    #[test]
    fn should_snapshot_runs_once_per_local_day() {
        let at = |d: u32, h: u32| Local.with_ymd_and_hms(2026, 6, d, h, 0, 0).unwrap();
        assert!(should_snapshot(None, at(10, 9)), "never ran: run now");
        assert!(
            !should_snapshot(Some(at(10, 0)), at(10, 23)),
            "already ran today"
        );
        assert!(
            should_snapshot(Some(at(9, 23)), at(10, 0)),
            "new day, even minutes apart"
        );
        assert!(
            !should_snapshot(Some(at(11, 0)), at(10, 23)),
            "clock went backwards: don't double-run"
        );
    }
}

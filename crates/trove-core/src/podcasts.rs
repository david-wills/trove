//! Apple Podcasts snapshot + diff collector.
//!
//! Podcasts keeps no listen history — `MTLibrary.sqlite` holds only the
//! *current* play state per episode (verified by spike, see
//! `docs/notes/podcasts-schema.md`; the Core Data change log retains ~2 weeks
//! and no values). So this is a snapshot+diff source in the
//! [`crate::music_library`] / [`crate::tasks`] mold — but unlike the daily
//! library snapshots, podcasts diff on **every watcher slow tick on which the
//! DB changed** ([`podcasts_db_mtime`] gates the copy), because the playhead
//! is persisted as it plays: ~15-minute session resolution and playhead-delta
//! listening estimates instead of one opaque event per day. iPhone listens
//! arrive on the same gate when iCloud sync touches the DB:
//!
//! - `podcasts/episodes.jsonl` — every episode with play signal, one per
//!   line, atomically rewritten (temp file + rename) on every snapshot.
//! - `podcasts/events/YYYY-MM.jsonl` — append-only diff events:
//!   `played` (completion), `progress` (playhead advanced on an in-progress
//!   episode), `added` (new episode with play signal), `removed`.
//!
//! **The first-ever snapshot emits no events** — baseline only, same rule as
//! the music library.
//!
//! Schema gotchas the diff encodes (all measured, see the spike note):
//! completing an episode can *reset* `ZPLAYHEAD` to 0 and flip `ZPLAYSTATE`
//! back to 0 while incrementing `ZPLAYCOUNT` — so a play-count bump is the
//! reliable completion signal and emits exactly one `played` event no matter
//! what the other fields did. iCloud-synced plays arrive with no
//! `ZLASTDATEPLAYED`, so `played` events may carry `last_played: None`.
//! `ZPLAYSTATELASTMODIFIEDDATE` is mass-touched by sync and is never treated
//! as evidence of a listen — the field diff decides.
//!
//! The reader takes the DB path as a parameter (fixture-testable); the live
//! path goes through copy-then-read like every other third-party SQLite
//! source (the DB is WAL-locked by Podcasts). Requires Full Disk Access at
//! runtime; tests never touch the real container.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;

const EPISODES_FILE: &str = "podcasts/episodes.jsonl";

// Sync mass-touches the DB without material change; only event-bearing
// passes (and the baseline) are worth a log line. An FDA-denied copy fails
// here, so the mtime gate doesn't commit and the pass retries next tick.
fn def_collect(
    vault: &Vault,
    now: chrono::DateTime<chrono::Local>,
) -> Result<crate::registry::CollectOutcome> {
    let stats =
        read_podcasts_via_copy().and_then(|eps| vault.podcasts_snapshot(&eps, &now.to_rfc3339()))?;
    Ok(crate::registry::CollectOutcome::note_if(
        stats.baseline || stats.played + stats.progress + stats.added + stats.removed > 0,
        || format!("podcasts snapshot: {stats:?}"),
    ))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(crate::registry::readable(&default_podcasts_db_path())),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::file_mtime(&vault.root().join(EPISODES_FILE))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "podcasts",
        name: "Apple Podcasts",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Snapshots episode progress and completions from the Podcasts app whenever its library changes.",
        domain: "media",
        vault_path: "podcasts/",
        toggleable: true,
        setup: &["Uses the same Full Disk Access grant as Messages and Safari — nothing extra once that's done."],
        caveats: "Podcasts stores no listen history, only current per-episode state — listens are reconstructed by diffing snapshots, and iCloud-synced plays arrive dateless.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::on_change(crate::browser::BROWSER_SYNC_SECS, podcasts_db_mtime), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Seconds between the Apple epoch (2001-01-01) and the unix epoch.
const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

/// Playhead must advance this much on an in-progress episode to count as a
/// `progress` event — below it, resumes/scrubs are noise.
const PROGRESS_MIN_SECS: f64 = 60.0;

/// One episode of the snapshot — a line in `podcasts/episodes.jsonl`. Only
/// `uuid` is required, so lines written by older versions keep deserializing
/// as fields are added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodcastEpisode {
    /// `ZMTEPISODE.ZUUID` — stable, unique, non-null; the diff key.
    pub uuid: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub show_title: String,
    #[serde(default)]
    pub show_author: String,
    #[serde(default)]
    pub feed_url: String,
    /// Raw `ZPLAYSTATE`: 0 unplayed, 1 in progress, 2 played/marked-played.
    #[serde(default)]
    pub play_state: i64,
    #[serde(default)]
    pub playhead_secs: f64,
    #[serde(default)]
    pub duration_secs: f64,
    #[serde(default)]
    pub play_count: i64,
    /// RFC3339 local time of the last *local* play; None for iCloud-synced
    /// plays (sparse by design — see the spike note).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pub_date: Option<String>,
}

impl PodcastEpisode {
    /// The listened predicate from the spike note: any evidence of play
    /// activity. Untouched back-catalog rows (≈10.8k of 11.7k) fail this.
    pub fn has_play_signal(&self) -> bool {
        self.play_state != 0
            || self.play_count > 0
            || self.last_played.is_some()
            || self.playhead_secs > 0.0
    }
}

/// One line of `podcasts/events/YYYY-MM.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodcastEvent {
    /// RFC3339 local time of the snapshot that observed the change.
    pub ts: String,
    /// "played", "progress", "added", or "removed".
    pub kind: String,
    pub uuid: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub show_title: String,
    /// The row's last-played date when Podcasts knows it (local plays only —
    /// iCloud-synced plays are dateless and land as None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<String>,
    /// Estimated content-seconds listened since the previous snapshot:
    /// the playhead delta for `progress`, the remaining tail for a
    /// count-bump `played`. An estimate — forward scrubs inflate it, and a
    /// state-flip-only `played` (marked played / iCloud completion) carries
    /// no listening evidence, so it stays 0. 0 = unknown.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seconds: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Result of one snapshot pass, for logging/status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PodcastSnapshotStats {
    /// Episodes in the snapshot just written.
    pub episodes: u64,
    pub played: u64,
    pub progress: u64,
    pub added: u64,
    pub removed: u64,
    /// True when this was the first-ever snapshot (baseline, no events).
    pub baseline: bool,
}

/// The pure diff: compare two snapshots, emit events stamped `ts`. At most
/// one event per episode — completion beats progress, and the play-count
/// bump is the completion signal regardless of what playhead/state did
/// (completing an episode resets the playhead and can flip the state back).
pub fn diff_episodes(
    old: &[PodcastEpisode],
    new: &[PodcastEpisode],
    ts: &str,
) -> Vec<PodcastEvent> {
    let old_by_id: BTreeMap<&str, &PodcastEpisode> =
        old.iter().map(|e| (e.uuid.as_str(), e)).collect();
    let new_ids: HashSet<&str> = new.iter().map(|e| e.uuid.as_str()).collect();
    let mut events = Vec::new();
    for ep in new {
        match old_by_id.get(ep.uuid.as_str()) {
            // New rows only matter once they carry play signal — the feed
            // backfills thousands of untouched back-catalog episodes.
            None => {
                if ep.has_play_signal() {
                    events.push(event(ts, "added", ep, 0));
                }
            }
            Some(before) => {
                if let Some((kind, seconds)) = episode_change(before, ep) {
                    events.push(event(ts, kind, ep, seconds));
                }
            }
        }
    }
    for ep in old {
        if !new_ids.contains(ep.uuid.as_str()) {
            events.push(event(ts, "removed", ep, 0));
        }
    }
    events
}

fn event(ts: &str, kind: &str, ep: &PodcastEpisode, seconds: u64) -> PodcastEvent {
    PodcastEvent {
        ts: ts.to_string(),
        kind: kind.to_string(),
        uuid: ep.uuid.clone(),
        title: ep.title.clone(),
        show_title: ep.show_title.clone(),
        last_played: ep.last_played.clone(),
        seconds,
    }
}

/// What happened to an episode between snapshots, if anything material, plus
/// the estimated content-seconds listened (0 = unknown).
fn episode_change(old: &PodcastEpisode, new: &PodcastEpisode) -> Option<(&'static str, u64)> {
    // Completion: play_count bump is authoritative (playhead resets and the
    // state can flip 2→0 in the same write). The remaining tail from the old
    // playhead is the listening estimate; with frequent snapshots a real
    // listen-through arrives with the playhead near the end, so the tail is
    // small and honest (a scrub-to-end completion inflates it — accepted).
    if new.play_count > old.play_count {
        let tail = if old.play_state == 1 && old.duration_secs > old.playhead_secs {
            (old.duration_secs - old.playhead_secs).round() as u64
        } else {
            0
        };
        return Some(("played", tail));
    }
    // A 0/!2→2 flip without a count bump is "marked played" / an iCloud
    // completion — still a play, but no listening evidence to count.
    if new.play_state == 2 && old.play_state != 2 {
        return Some(("played", 0));
    }
    if new.play_state == 1 && new.playhead_secs >= old.playhead_secs + PROGRESS_MIN_SECS {
        return Some(("progress", (new.playhead_secs - old.playhead_secs).round() as u64));
    }
    None
}

impl Vault {
    /// The stored episode snapshot; empty when no snapshot has ever run.
    /// Lenient line-by-line parse, like every other vault reader.
    pub fn load_podcasts(&self) -> Result<Vec<PodcastEpisode>> {
        let path = self.resolve(EPISODES_FILE)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body =
            fs::read_to_string(&path).with_context(|| format!("reading {EPISODES_FILE}"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<PodcastEpisode>(l).ok())
            .collect())
    }

    /// One snapshot pass: diff `fresh` against the stored snapshot, append
    /// the diff events to their month's log, atomically rewrite the snapshot.
    /// The first-ever snapshot writes the baseline and emits no events. `ts`
    /// is the injected RFC3339 local snapshot time.
    pub fn podcasts_snapshot(
        &self,
        fresh: &[PodcastEpisode],
        ts: &str,
    ) -> Result<PodcastSnapshotStats> {
        let baseline = !self.resolve(EPISODES_FILE)?.exists();
        let events = if baseline {
            Vec::new()
        } else {
            diff_episodes(&self.load_podcasts()?, fresh, ts)
        };
        self.append_podcast_events(&events)?;
        self.write_podcasts(fresh)?;
        let count = |kind: &str| events.iter().filter(|e| e.kind == kind).count() as u64;
        Ok(PodcastSnapshotStats {
            episodes: fresh.len() as u64,
            played: count("played"),
            progress: count("progress"),
            added: count("added"),
            removed: count("removed"),
            baseline,
        })
    }

    /// Podcast diff events over an inclusive date range, chronological.
    pub fn podcasts_events(&self, from: &str, to: &str) -> Result<Vec<PodcastEvent>> {
        let (from_month, to_month) = (&from[..7.min(from.len())], &to[..7.min(to.len())]);
        let dir = self.root().join("podcasts/events");
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
                    .filter_map(|l| serde_json::from_str::<PodcastEvent>(l).ok())
                    .filter(|e| {
                        let day = &e.ts[..10.min(e.ts.len())];
                        day >= from && day <= to
                    }),
            );
        }
        out.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(out)
    }

    /// Atomic snapshot rewrite: temp file in place, then rename.
    fn write_podcasts(&self, episodes: &[PodcastEpisode]) -> Result<()> {
        self.write_snapshot(EPISODES_FILE, episodes)
    }

    /// Append events to their month's JSONL log (keyed by local event month).
    fn append_podcast_events(&self, events: &[PodcastEvent]) -> Result<()> {
        self.stream("podcasts/events", crate::store::Partition::Month).append(events, |e| &e.ts)
    }
}

/// The live Podcasts library DB (FDA-gated group container).
pub fn default_podcasts_db_path() -> PathBuf {
    dirs_home()
        .join("Library/Group Containers/243LU875E5.groups.com.apple.podcasts/Documents/MTLibrary.sqlite")
}

/// Newest modification time across the live DB and its WAL/SHM siblings —
/// the snapshot gate: unchanged mtime means nothing played (locally) and
/// nothing synced in, so the copy+diff can be skipped. Playhead writes land
/// in the WAL, which is why the siblings count. `None` when unreadable
/// (no FDA, no Podcasts library) — callers treat that as "don't snapshot".
pub fn podcasts_db_mtime() -> Option<std::time::SystemTime> {
    let db = default_podcasts_db_path();
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            fs::metadata(PathBuf::from(format!("{}{suffix}", db.display())))
                .and_then(|m| m.modified())
                .ok()
        })
        .max()
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Read the live DB via copy-then-read (Podcasts holds a WAL lock on the
/// original; a torn copy just fails the query and the next pass retries).
/// Copies the `-wal`/`-shm` siblings too — the newest rows live in the WAL
/// until Podcasts checkpoints.
pub fn read_podcasts_via_copy() -> Result<Vec<PodcastEpisode>> {
    let db = default_podcasts_db_path();
    let stem = format!("trove-podcasts-{}", std::process::id());
    let tmp = std::env::temp_dir().join(format!("{stem}.db"));
    let cleanup = |tmp: &Path| {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", tmp.display())));
        }
    };
    cleanup(&tmp);
    fs::copy(&db, &tmp).with_context(|| format!("copying {}", db.display()))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            let _ = fs::copy(&side, PathBuf::from(format!("{}{suffix}", tmp.display())));
        }
    }
    let result = read_podcast_library(&tmp);
    cleanup(&tmp);
    result
}

/// Read every episode with play signal (plus show metadata) out of an
/// MTLibrary.sqlite (or a copy, or a test fixture). Timestamps are Apple
/// epoch (seconds since 2001-01-01 UTC) and convert to RFC3339 local.
pub fn read_podcast_library(db_path: &Path) -> Result<Vec<PodcastEpisode>> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("opening podcasts library copy {}", db_path.display()))?;
    // The listened predicate from the spike note: skip the untouched back
    // catalog. LEFT JOIN — an episode row may outlive its show row.
    let mut stmt = conn.prepare(
        "SELECT e.ZUUID, e.ZTITLE, p.ZTITLE, p.ZAUTHOR, p.ZFEEDURL,
                e.ZPLAYSTATE, e.ZPLAYHEAD, e.ZDURATION, e.ZPLAYCOUNT,
                e.ZLASTDATEPLAYED, e.ZPUBDATE
         FROM ZMTEPISODE e LEFT JOIN ZMTPODCAST p ON p.Z_PK = e.ZPODCAST
         WHERE e.ZPLAYSTATE = 2 OR e.ZPLAYCOUNT > 0
            OR e.ZLASTDATEPLAYED IS NOT NULL
            OR (e.ZPLAYSTATE = 1 AND e.ZPLAYHEAD > 0)",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PodcastEpisode {
            uuid: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            show_title: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            show_author: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            feed_url: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            play_state: row.get::<_, Option<i64>>(5)?.unwrap_or_default(),
            playhead_secs: row.get::<_, Option<f64>>(6)?.unwrap_or_default(),
            duration_secs: row.get::<_, Option<f64>>(7)?.unwrap_or_default(),
            play_count: row.get::<_, Option<i64>>(8)?.unwrap_or_default(),
            last_played: row
                .get::<_, Option<f64>>(9)?
                .and_then(apple_epoch_to_rfc3339),
            pub_date: row
                .get::<_, Option<f64>>(10)?
                .and_then(apple_epoch_to_rfc3339),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let ep = row?;
        if !ep.uuid.is_empty() {
            out.push(ep);
        }
    }
    Ok(out)
}

/// Apple epoch (seconds since 2001-01-01 UTC) → RFC3339 local.
fn apple_epoch_to_rfc3339(secs: f64) -> Option<String> {
    if secs <= 0.0 {
        return None;
    }
    chrono::DateTime::from_timestamp(secs as i64 + APPLE_EPOCH_OFFSET, 0)
        .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-podcasts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn episode(uuid: &str, title: &str) -> PodcastEpisode {
        PodcastEpisode {
            uuid: uuid.into(),
            title: title.into(),
            show_title: "The Indicator".into(),
            show_author: "NPR".into(),
            feed_url: "https://feeds.npr.org/510325".into(),
            play_state: 1,
            playhead_secs: 120.0,
            duration_secs: 528.0,
            play_count: 0,
            last_played: Some("2026-06-09T08:00:00-07:00".into()),
            pub_date: Some("2026-06-08T03:00:00-07:00".into()),
        }
    }

    const TS: &str = "2026-06-11T04:00:00-07:00";

    /// Byte-parity contract for both write paths (snapshot rewrite +
    /// month-keyed event append): exact file bytes, pinned before the port
    /// onto `store` and unchanged by it.
    #[test]
    fn snapshot_and_events_write_byte_identical_files() {
        let v = temp_vault("parity");
        let a = episode("A", "One");
        let b = episode("B", "Two");
        v.podcasts_snapshot(&[a.clone()], "2026-06-10T04:00:00-07:00").unwrap();
        v.podcasts_snapshot(&[a.clone(), b.clone()], TS).unwrap();
        v.podcasts_snapshot(&[a], "2026-06-12T04:00:00-07:00").unwrap();

        assert_eq!(
            fs::read_to_string(v.root().join("podcasts/episodes.jsonl")).unwrap(),
            "{\"uuid\":\"A\",\"title\":\"One\",\"show_title\":\"The Indicator\",\"show_author\":\"NPR\",\"feed_url\":\"https://feeds.npr.org/510325\",\"play_state\":1,\"playhead_secs\":120.0,\"duration_secs\":528.0,\"play_count\":0,\"last_played\":\"2026-06-09T08:00:00-07:00\",\"pub_date\":\"2026-06-08T03:00:00-07:00\"}\n",
            "snapshot rewrite"
        );
        let b_fields = "\"uuid\":\"B\",\"title\":\"Two\",\"show_title\":\"The Indicator\",\"last_played\":\"2026-06-09T08:00:00-07:00\"";
        assert_eq!(
            fs::read_to_string(v.root().join("podcasts/events/2026-06.jsonl")).unwrap(),
            format!(
                "{{\"ts\":\"{TS}\",\"kind\":\"added\",{b_fields}}}\n\
                 {{\"ts\":\"2026-06-12T04:00:00-07:00\",\"kind\":\"removed\",{b_fields}}}\n"
            ),
            "month-keyed event append extends across calls"
        );
    }

    #[test]
    fn completion_with_playhead_reset_is_one_played_event() {
        // The measured gotcha: finishing an episode bumps ZPLAYCOUNT while
        // resetting ZPLAYHEAD to 0 and flipping ZPLAYSTATE back to 0.
        let before = episode("A", "fish sticks");
        let mut after = before.clone();
        after.play_count = 1;
        after.play_state = 0;
        after.playhead_secs = 0.0;
        after.last_played = Some(TS.into());

        let events = diff_episodes(&[before], &[after], TS);
        assert_eq!(events.len(), 1, "exactly one event, not played+progress");
        assert_eq!(events[0].kind, "played");
        assert_eq!(events[0].last_played.as_deref(), Some(TS));
        assert_eq!(events[0].seconds, 408, "tail from the old playhead: 528 - 120");
    }

    #[test]
    fn no_material_change_no_events() {
        // Sync mass-touches ZPLAYSTATELASTMODIFIEDDATE without changing
        // anything we store — identical snapshots must be silent.
        let eps = vec![episode("A", "One"), episode("B", "Two")];
        assert!(diff_episodes(&eps, &eps.clone(), TS).is_empty());
    }

    #[test]
    fn icloud_play_arrives_dateless() {
        // A play synced from the iPhone: state flips to 2, no ZLASTDATEPLAYED.
        let mut before = episode("A", "Divided Mind");
        before.play_state = 0;
        before.playhead_secs = 0.0;
        before.last_played = None;
        let mut after = before.clone();
        after.play_state = 2;

        let events = diff_episodes(&[before], &[after], TS);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "played");
        assert!(events[0].last_played.is_none());
        assert_eq!(events[0].seconds, 0, "state flip carries no listening evidence");
    }

    #[test]
    fn progress_needs_sixty_seconds() {
        let before = episode("A", "One");
        let mut crawled = before.clone();
        crawled.playhead_secs += 59.0;
        assert!(diff_episodes(&[before.clone()], &[crawled], TS).is_empty());

        let mut advanced = before.clone();
        advanced.playhead_secs += 754.0;
        let events = diff_episodes(&[before], &[advanced], TS);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "progress");
        assert_eq!(events[0].seconds, 754, "the playhead delta");
    }

    #[test]
    fn added_requires_play_signal_removed_does_not() {
        let mut untouched = episode("B", "Back catalog");
        untouched.play_state = 0;
        untouched.playhead_secs = 0.0;
        untouched.play_count = 0;
        untouched.last_played = None;
        let old = vec![episode("A", "Gone")];
        let new = vec![untouched, episode("C", "Started today")];

        let events = diff_episodes(&old, &new, TS);
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].kind.as_str(), events[0].uuid.as_str()), ("added", "C"));
        assert_eq!((events[1].kind.as_str(), events[1].uuid.as_str()), ("removed", "A"));
    }

    #[test]
    fn first_snapshot_is_a_silent_baseline_then_diffs() {
        let v = temp_vault("baseline");
        let a = episode("A", "One");
        let stats = v.podcasts_snapshot(&[a.clone()], "2026-06-10T04:00:00-07:00").unwrap();
        assert!(stats.baseline);
        assert_eq!(stats.episodes, 1);
        assert_eq!(stats.played + stats.progress + stats.added + stats.removed, 0);
        assert!(!v.root().join("podcasts/events").exists());

        let mut a2 = a.clone();
        a2.play_count = 1;
        let stats = v.podcasts_snapshot(&[a2.clone(), episode("B", "New")], TS).unwrap();
        assert!(!stats.baseline);
        assert_eq!((stats.played, stats.added), (1, 1));
        assert_eq!(v.load_podcasts().unwrap(), vec![a2, episode("B", "New")]);
        assert!(!v.root().join("podcasts/episodes.jsonl.tmp").exists());

        let events = v.podcasts_events("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "played");
        assert_eq!(events[1].kind, "added");
    }

    #[test]
    fn minimal_old_style_line_still_parses() {
        let e: PodcastEpisode = serde_json::from_str(r#"{"uuid":"68812100-X"}"#).unwrap();
        assert_eq!(e.uuid, "68812100-X");
        assert_eq!(e.title, "");
        assert_eq!(e.play_state, 0);
        assert_eq!(e.play_count, 0);
        assert!(e.last_played.is_none());
        assert!(e.pub_date.is_none());

        let body = serde_json::to_string(&e).unwrap();
        assert!(!body.contains("last_played"));
        assert!(!body.contains("pub_date"));

        // Events written before the seconds field still parse, and a
        // zero-seconds event doesn't serialize the field.
        let ev: PodcastEvent =
            serde_json::from_str(r#"{"ts":"2026-06-10T04:00:00-07:00","kind":"played","uuid":"X"}"#)
                .unwrap();
        assert_eq!(ev.seconds, 0);
        assert!(!serde_json::to_string(&ev).unwrap().contains("seconds"));
    }

    #[test]
    fn reader_joins_filters_and_converts_epochs() {
        let dir = std::env::temp_dir()
            .join(format!("trove-podcasts-fixture-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("MTLibrary.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZMTPODCAST (Z_PK INTEGER PRIMARY KEY, ZTITLE TEXT,
                 ZAUTHOR TEXT, ZFEEDURL TEXT);
             CREATE TABLE ZMTEPISODE (Z_PK INTEGER PRIMARY KEY, ZUUID TEXT,
                 ZTITLE TEXT, ZPODCAST INTEGER, ZPLAYSTATE INTEGER,
                 ZPLAYHEAD REAL, ZDURATION REAL, ZPLAYCOUNT INTEGER,
                 ZLASTDATEPLAYED REAL, ZPUBDATE REAL);
             INSERT INTO ZMTPODCAST VALUES
                 (1, 'The Indicator', 'NPR', 'https://feeds.npr.org/510325');
             -- In progress, played 2026-03-03T05:47:18Z = Apple 794209638.
             INSERT INTO ZMTEPISODE VALUES
                 (10, 'UUID-PLAYED', 'fish sticks', 1, 1, 267.0, 528.0, 0,
                  794209638.0, 793497600.0);
             -- Untouched back catalog: must be filtered out by the query.
             INSERT INTO ZMTEPISODE VALUES
                 (11, 'UUID-UNTOUCHED', 'old one', 1, 0, 0.0, 1000.0, 0,
                  NULL, 700000000.0);
             -- Orphaned episode (deleted show): LEFT JOIN must keep it.
             INSERT INTO ZMTEPISODE VALUES
                 (12, 'UUID-ORPHAN', 'orphan', 99, 2, 0.0, 900.0, 1,
                  NULL, NULL);",
        )
        .unwrap();
        drop(conn);

        let mut eps = read_podcast_library(&db).unwrap();
        eps.sort_by(|a, b| a.uuid.cmp(&b.uuid));
        assert_eq!(eps.len(), 2, "untouched back catalog filtered out");

        let orphan = &eps[0];
        assert_eq!(orphan.uuid, "UUID-ORPHAN");
        assert_eq!(orphan.show_title, "");
        assert_eq!(orphan.play_count, 1);
        assert!(orphan.last_played.is_none());
        assert!(orphan.pub_date.is_none());

        let played = &eps[1];
        assert_eq!(played.uuid, "UUID-PLAYED");
        assert_eq!(played.show_title, "The Indicator");
        assert_eq!(played.show_author, "NPR");
        assert_eq!(played.playhead_secs, 267.0);
        // Apple epoch 794209638 = 2026-03-03T05:47:18Z, rendered local.
        let lp = played.last_played.as_deref().unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(lp).unwrap();
        assert_eq!(parsed.timestamp(), 794209638 + APPLE_EPOCH_OFFSET);

        let _ = fs::remove_dir_all(&dir);
    }
}

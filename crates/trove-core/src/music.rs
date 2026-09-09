//! Apple Music play history (scrobbler).
//!
//! Music.app posts a `com.apple.Music.playerInfo` distributed notification on
//! every play/pause/stop/track change, but keeps no play *history* itself
//! (only lifetime counts) — this stream is unrecoverable unless captured
//! live, which is why the scrobbler ships before most other collectors.
//!
//! Source of truth is one JSONL file per local day:
//! `music/plays/YYYY-MM-DD.jsonl`, one completed play per line:
//!
//! ```json
//! {"start":"2026-06-10T20:03:01-07:00","end":"2026-06-10T20:06:50-07:00",
//!  "seconds_played":228,"track":"Take On Me","artist":"a-ha",
//!  "album":"Hunting High & Low","genre":"Pop","duration_secs":228.7,
//!  "persistent_id":"DAE7C16E62517F1A","full_play":true}
//! ```
//!
//! **Full fidelity at write time, opinions at read time:** every play longer
//! than a tiny anti-flicker floor (5s) is recorded — skips included, they're
//! signal too ("songs I always bail on"). The Last.fm-style judgment (half
//! the track or 4 minutes — did you actually *listen* to it?) is stored as
//! the `full_play` flag, the default filter for charts/stats; the underlying
//! `seconds_played`/`duration_secs` are always present, so readers can apply
//! any other rule later.
//!
//! Like [`crate::activity::Watcher`], the [`Scrobbler`] holds no OS handles —
//! feed it timestamped [`PlayerEvent`]s and it emits closed plays. The
//! platform notification listener lives in [`crate::music_listener`].

use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::activity::days;
use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind};
use crate::music_listener::{MusicListener, TimedPlayerEvent};
use crate::registry::{Behavior, IntegrationDef, LiveCollector};
use crate::vault::Vault;

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join("music/plays"))
}

/// The live scrobbler: owns the playerInfo notification channel and the
/// [`Scrobbler`] state machine. Built once per lock takeover — exactly where
/// the listener used to start — so the main-run-loop delivery contract is
/// unchanged (headless hosts still pump it; see [`crate::music_listener`]).
struct MusicLive {
    /// `Some` until shutdown ([`MusicListener::stop`] takes it by value).
    listener: Option<MusicListener>,
    rx: std::sync::mpsc::Receiver<TimedPlayerEvent>,
    scrobbler: Scrobbler,
}

fn make_live() -> Box<dyn LiveCollector> {
    let (listener, rx) = MusicListener::start();
    Box::new(MusicLive {
        listener: Some(listener),
        rx,
        scrobbler: Scrobbler::new(ScrobbleConfig::default()),
    })
}

impl MusicLive {
    fn append(&self, vault: &Vault, plays: &[Play], context: &str) {
        if !plays.is_empty() {
            if let Err(e) = vault.append_music_plays(plays) {
                eprintln!("trove watcher: failed to {context} music plays: {e:#}");
            }
        }
    }
}

impl LiveCollector for MusicLive {
    fn tick(&mut self, vault: &Vault, now: DateTime<Local>, enabled: bool) {
        // Drain notifications delivered since the last tick; each is stamped
        // at delivery time, so this cadence costs no accuracy.
        let mut plays = Vec::new();
        if enabled {
            while let Ok((ts, ev)) = self.rx.try_recv() {
                plays.extend(self.scrobbler.handle(ts, &ev));
            }
        } else {
            // Disabled: discard new events (re-enabling must not replay a
            // backlog of stale player state), close out any open play.
            while self.rx.try_recv().is_ok() {}
            plays.extend(self.scrobbler.flush(now));
        }
        self.append(vault, &plays, "append");
    }

    fn shutdown(&mut self, vault: &Vault, now: DateTime<Local>) {
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
        let mut plays = Vec::new();
        while let Ok((ts, ev)) = self.rx.try_recv() {
            plays.extend(self.scrobbler.handle(ts, &ev));
        }
        plays.extend(self.scrobbler.flush(now));
        self.append(vault, &plays, "flush final");
    }
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static SCROBBLER_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "music-scrobbler",
        name: "Apple Music scrobbler",
        kind: IntegrationKind::Live,
        default_on: true,
        description: "Records every play as it happens, skips included. No permission needed.",
        domain: "media",
        vault_path: "music/plays/",
        toggleable: true,
        setup: &[],
        caveats: "Music.app keeps no play history of its own — plays are captured only while a collector runs; gaps can never be backfilled. The daily library snapshot below catches what this misses, but only as count drift.",
    },
    behavior: Behavior::Live(make_live),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Track metadata carried by a playerInfo notification.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub name: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    /// Track length in seconds ("Total Time", when present).
    pub duration_secs: Option<f64>,
    /// Music's persistent ID as uppercase hex; empty if absent.
    pub persistent_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Playing,
    Paused,
    Stopped,
}

/// One playerInfo notification, decoded. A track change arrives as a bare
/// `Stopped` (no track) followed by `Playing` with the new track.
#[derive(Debug, Clone)]
pub struct PlayerEvent {
    pub state: PlayerState,
    pub track: Option<TrackInfo>,
}

/// One completed play (full listen or skip) — a line in
/// `music/plays/YYYY-MM-DD.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Play {
    /// RFC3339 local time playback of this track began.
    pub start: String,
    /// RFC3339 local time playback last ran (pauses after this are not included).
    pub end: String,
    /// Seconds actually spent playing — pauses excluded.
    pub seconds_played: u64,
    pub track: String,
    pub artist: String,
    pub album: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub persistent_id: String,
    /// Did this count as actually listening (Last.fm rule, see
    /// [`ScrobbleConfig`])? False = a skip. The default read-time filter;
    /// recomputable from `seconds_played`/`duration_secs` if the rule changes.
    #[serde(default)]
    pub full_play: bool,
}

/// Tunables for [`Scrobbler`]. The full-play judgment follows the Last.fm
/// convention.
#[derive(Debug, Clone, Copy)]
pub struct ScrobbleConfig {
    /// Plays shorter than this aren't recorded at all (anti-flicker floor —
    /// rapid track-flipping, not listening).
    pub min_record_secs: f64,
    /// `full_play` when this fraction of the track played (duration known)…
    pub full_fraction: f64,
    /// …or after this many seconds, whichever comes first (long tracks).
    pub full_secs: f64,
}

/// `full_play` threshold when the track duration is unknown (radio streams):
/// no fraction to compute, so fall back to the classic 30s scrobble bar.
const UNKNOWN_DURATION_FULL_SECS: f64 = 30.0;

impl Default for ScrobbleConfig {
    fn default() -> Self {
        ScrobbleConfig {
            min_record_secs: 5.0,
            full_fraction: 0.5,
            full_secs: 240.0,
        }
    }
}

/// The in-progress play.
#[derive(Debug, Clone)]
struct OpenPlay {
    track: TrackInfo,
    start: DateTime<Local>,
    /// Accumulated playing time, pauses excluded.
    played: f64,
    /// When the current playing stretch began; `None` while paused.
    resumed_at: Option<DateTime<Local>>,
    /// When playback last ran — the `end` of the play if it closes while paused.
    last_active: DateTime<Local>,
}

/// Folds playerInfo events into completed plays. Holds no OS handles.
pub struct Scrobbler {
    cfg: ScrobbleConfig,
    open: Option<OpenPlay>,
}

impl Scrobbler {
    pub fn new(cfg: ScrobbleConfig) -> Self {
        Scrobbler { cfg, open: None }
    }

    /// Feed one event received at `now`. Returns a play if one just closed
    /// and cleared the recording floor (skips included — see `full_play`).
    pub fn handle(&mut self, now: DateTime<Local>, event: &PlayerEvent) -> Option<Play> {
        match event.state {
            PlayerState::Playing => {
                let track = event.track.as_ref()?;
                if self.open.as_ref().is_some_and(|o| same_track(&o.track, track)) {
                    // Same track: resume if paused; otherwise it's a seek or a
                    // duplicate notification — the play just continues.
                    let o = self.open.as_mut().unwrap();
                    if o.resumed_at.is_none() {
                        o.resumed_at = Some(now);
                    }
                    o.last_active = now;
                    None
                } else {
                    let done = self.close(now);
                    self.open = Some(OpenPlay {
                        track: track.clone(),
                        start: now,
                        played: 0.0,
                        resumed_at: Some(now),
                        last_active: now,
                    });
                    done
                }
            }
            PlayerState::Paused => {
                if let Some(o) = self.open.as_mut() {
                    if let Some(r) = o.resumed_at.take() {
                        o.played += secs_between(r, now);
                        o.last_active = now;
                    }
                }
                None
            }
            PlayerState::Stopped => self.close(now),
        }
    }

    /// Close out the open play (e.g. on shutdown), if any clears the floor.
    pub fn flush(&mut self, now: DateTime<Local>) -> Option<Play> {
        self.close(now)
    }

    fn close(&mut self, now: DateTime<Local>) -> Option<Play> {
        let mut o = self.open.take()?;
        if let Some(r) = o.resumed_at.take() {
            o.played += secs_between(r, now);
            o.last_active = now;
        }
        if o.track.name.is_empty() || o.played < self.cfg.min_record_secs {
            return None;
        }
        Some(Play {
            start: o.start.to_rfc3339(),
            end: o.last_active.to_rfc3339(),
            seconds_played: o.played.round() as u64,
            full_play: self.is_full(&o),
            track: o.track.name,
            artist: o.track.artist,
            album: o.track.album,
            genre: o.track.genre,
            duration_secs: o.track.duration_secs,
            persistent_id: o.track.persistent_id,
        })
    }

    fn is_full(&self, o: &OpenPlay) -> bool {
        match o.track.duration_secs {
            Some(d) => o.played >= d * self.cfg.full_fraction || o.played >= self.cfg.full_secs,
            None => o.played >= UNKNOWN_DURATION_FULL_SECS,
        }
    }
}

/// Persistent IDs are authoritative when both sides have one; metadata
/// otherwise (streaming radio etc. can lack IDs).
fn same_track(a: &TrackInfo, b: &TrackInfo) -> bool {
    if !a.persistent_id.is_empty() && !b.persistent_id.is_empty() {
        a.persistent_id == b.persistent_id
    } else {
        a.name == b.name && a.artist == b.artist && a.album == b.album
    }
}

fn secs_between(from: DateTime<Local>, to: DateTime<Local>) -> f64 {
    ((to - from).num_milliseconds() as f64 / 1000.0).max(0.0)
}

/// One artist's listening over a range.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ArtistUsage {
    pub artist: String,
    /// Full plays only (the default chart filter — see [`Play::full_play`]).
    pub plays: u64,
    /// Seconds actually played, skips included.
    pub seconds: u64,
}

/// Headline listening numbers over a date range.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct MusicSummary {
    /// Full plays (Last.fm rule).
    pub plays: u64,
    /// Recorded partial plays — bailed before the full-play bar.
    pub skips: u64,
    /// Seconds actually played across all rows, pauses excluded.
    pub seconds_played: u64,
    /// Artists by full plays, descending.
    pub artists: Vec<ArtistUsage>,
}

impl Vault {
    /// Append completed plays to their day's JSONL log (keyed by start day).
    pub fn append_music_plays(&self, plays: &[Play]) -> Result<()> {
        self.stream("music/plays", crate::store::Partition::Day).append(plays, |p| &p.start)
    }

    /// All plays for one local day (YYYY-MM-DD), in file order.
    pub fn music_timeline(&self, date: &str) -> Result<Vec<Play>> {
        let path = self.resolve(&format!("music/plays/{date}.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)
            .with_context(|| format!("reading music/plays/{date}.jsonl"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Play>(l).ok())
            .collect())
    }

    /// Play/skip counts, time played, and per-artist totals over an
    /// inclusive date range.
    pub fn music_summary(&self, from: &str, to: &str) -> Result<MusicSummary> {
        let mut artists: HashMap<String, ArtistUsage> = HashMap::new();
        let (mut plays, mut skips, mut seconds_played) = (0u64, 0u64, 0u64);
        for date in days(from, to)? {
            for p in self.music_timeline(&date)? {
                seconds_played += p.seconds_played;
                let a = artists.entry(p.artist.clone()).or_insert_with(|| ArtistUsage {
                    artist: p.artist,
                    plays: 0,
                    seconds: 0,
                });
                a.seconds += p.seconds_played;
                if p.full_play {
                    plays += 1;
                    a.plays += 1;
                } else {
                    skips += 1;
                }
            }
        }
        let mut artists: Vec<ArtistUsage> = artists.into_values().collect();
        artists.sort_by(|a, b| {
            b.plays
                .cmp(&a.plays)
                .then(b.seconds.cmp(&a.seconds))
                .then_with(|| a.artist.cmp(&b.artist))
        });
        Ok(MusicSummary {
            plays,
            skips,
            seconds_played,
            artists,
        })
    }

    /// Full plays per day over an inclusive range — a trend series for the
    /// chart. Days with no plays are omitted.
    pub fn music_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut out = Vec::new();
        for date in days(from, to)? {
            let n = self
                .music_timeline(&date)?
                .iter()
                .filter(|p| p.full_play)
                .count();
            if n > 0 {
                out.push(SeriesPoint {
                    date,
                    value: n as f64,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 10, h, m, s).unwrap()
    }

    fn track(name: &str, duration: Option<f64>) -> TrackInfo {
        TrackInfo {
            name: name.into(),
            artist: "Artist".into(),
            album: "Album".into(),
            genre: "Pop".into(),
            duration_secs: duration,
            persistent_id: format!("{:016X}", name.len() as u64),
        }
    }

    fn playing(t: &TrackInfo) -> PlayerEvent {
        PlayerEvent {
            state: PlayerState::Playing,
            track: Some(t.clone()),
        }
    }

    fn paused(t: &TrackInfo) -> PlayerEvent {
        PlayerEvent {
            state: PlayerState::Paused,
            track: Some(t.clone()),
        }
    }

    /// Track changes arrive as a bare Stopped with no track info.
    fn stopped() -> PlayerEvent {
        PlayerEvent {
            state: PlayerState::Stopped,
            track: None,
        }
    }

    fn scrobbler() -> Scrobbler {
        Scrobbler::new(ScrobbleConfig::default())
    }

    /// Byte-parity contract for the append path: exact file bytes, pinned
    /// before the port onto `store::JsonlStream` and unchanged by it.
    #[test]
    fn append_writes_byte_identical_jsonl() {
        let dir = std::env::temp_dir().join(format!("trove-music-{}-parity", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();
        let play = |start: &str, end: &str| Play {
            start: start.into(),
            end: end.into(),
            seconds_played: 200,
            track: "Take On Me".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            genre: "Pop".into(),
            duration_secs: Some(200.0),
            persistent_id: "000000000000000B".into(),
            full_play: true,
        };
        let p1 = play("2026-06-10T09:00:00-07:00", "2026-06-10T09:03:20-07:00");
        let p2 = play("2026-06-11T09:00:00-07:00", "2026-06-11T09:03:20-07:00");
        v.append_music_plays(&[p1.clone(), p2]).unwrap();
        v.append_music_plays(&[p1]).unwrap(); // second call extends

        let line = |start: &str, end: &str| {
            format!(
                "{{\"start\":\"{start}\",\"end\":\"{end}\",\"seconds_played\":200,\
                 \"track\":\"Take On Me\",\"artist\":\"Artist\",\"album\":\"Album\",\
                 \"genre\":\"Pop\",\"duration_secs\":200.0,\
                 \"persistent_id\":\"000000000000000B\",\"full_play\":true}}"
            )
        };
        let d10 = line("2026-06-10T09:00:00-07:00", "2026-06-10T09:03:20-07:00");
        assert_eq!(
            fs::read_to_string(v.root().join("music/plays/2026-06-10.jsonl")).unwrap(),
            format!("{d10}\n{d10}\n"),
            "day-keyed append extends across calls"
        );
        assert_eq!(
            fs::read_to_string(v.root().join("music/plays/2026-06-11.jsonl")).unwrap(),
            format!("{}\n", line("2026-06-11T09:00:00-07:00", "2026-06-11T09:03:20-07:00")),
        );
    }

    #[test]
    fn full_play_is_recorded_and_flagged() {
        let mut s = scrobbler();
        let t = track("Take On Me", Some(200.0));
        assert!(s.handle(at(9, 0, 0), &playing(&t)).is_none());
        let play = s.handle(at(9, 3, 20), &stopped()).expect("recorded");
        assert_eq!(play.seconds_played, 200);
        assert!(play.full_play);
        assert_eq!(play.track, "Take On Me");
        assert_eq!(play.start, at(9, 0, 0).to_rfc3339());
        assert_eq!(play.end, at(9, 3, 20).to_rfc3339());
    }

    #[test]
    fn skip_is_recorded_as_not_full() {
        let mut s = scrobbler();
        let a = track("Skipped", Some(200.0));
        let b = track("Kept", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&a));
        // Skip after 20s: Music posts bare Stopped, then Playing with track B.
        let skip = s.handle(at(9, 0, 20), &stopped()).expect("skips are data");
        assert_eq!(skip.track, "Skipped");
        assert_eq!(skip.seconds_played, 20);
        assert!(!skip.full_play, "20s of 200s is a skip");
        assert!(s.handle(at(9, 0, 20), &playing(&b)).is_none());
        let play = s.flush(at(9, 2, 0)).expect("100s of 200s is full");
        assert_eq!(play.track, "Kept");
        assert_eq!(play.seconds_played, 100);
        assert!(play.full_play);
    }

    #[test]
    fn flicker_below_floor_is_dropped() {
        let mut s = scrobbler();
        let t = track("Flicked Past", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&t));
        assert!(
            s.handle(at(9, 0, 3), &stopped()).is_none(),
            "3s < 5s floor: not even a skip"
        );
    }

    #[test]
    fn direct_track_change_closes_previous() {
        // Defensive: a Playing for a new track with no intervening Stopped.
        let mut s = scrobbler();
        let a = track("First", Some(200.0));
        let b = track("Second", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&a));
        let play = s.handle(at(9, 3, 0), &playing(&b)).expect("First closes");
        assert_eq!(play.track, "First");
        assert_eq!(play.seconds_played, 180);
        assert!(play.full_play);
    }

    #[test]
    fn pause_resume_accumulates_play_time_only() {
        let mut s = scrobbler();
        let t = track("Paused Song", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&t));
        assert!(s.handle(at(9, 1, 0), &paused(&t)).is_none()); // 60s played
        // Nine minutes idle, then resume for 40s more.
        s.handle(at(9, 10, 0), &playing(&t));
        let play = s.handle(at(9, 10, 40), &stopped()).expect("recorded");
        assert_eq!(play.seconds_played, 100, "pause time excluded");
        assert!(play.full_play, "100s of 200s");
        assert_eq!(play.start, at(9, 0, 0).to_rfc3339());
        assert_eq!(play.end, at(9, 10, 40).to_rfc3339());
    }

    #[test]
    fn close_while_paused_ends_at_last_active() {
        let mut s = scrobbler();
        let t = track("Abandoned", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&t));
        s.handle(at(9, 2, 0), &paused(&t)); // 120s played
        let play = s.flush(at(11, 0, 0)).expect("recorded");
        assert_eq!(play.seconds_played, 120);
        assert!(play.full_play);
        assert_eq!(play.end, at(9, 2, 0).to_rfc3339(), "pause tail not included");
    }

    #[test]
    fn four_minute_rule_marks_long_tracks_full() {
        let mut s = scrobbler();
        let t = track("Long Mix", Some(1200.0));
        s.handle(at(9, 0, 0), &playing(&t));
        let play = s.handle(at(9, 4, 10), &stopped()).expect("recorded");
        assert_eq!(play.seconds_played, 250);
        assert!(play.full_play, "250s >= 240s four-minute rule");

        s.handle(at(9, 5, 0), &playing(&t));
        let partial = s.handle(at(9, 7, 0), &stopped()).expect("recorded");
        assert!(!partial.full_play, "120s of 1200s, under both bars");
    }

    #[test]
    fn unknown_duration_full_at_thirty_seconds() {
        let mut s = scrobbler();
        let t = track("Radio Stream", None);
        s.handle(at(9, 0, 0), &playing(&t));
        let full = s.handle(at(9, 0, 35), &stopped()).expect("recorded");
        assert!(full.full_play, "35s >= 30s unknown-duration bar");

        s.handle(at(9, 1, 0), &playing(&t));
        let skip = s.handle(at(9, 1, 25), &stopped()).expect("recorded");
        assert!(!skip.full_play, "25s < 30s bar, but still recorded");
    }

    #[test]
    fn duplicate_playing_does_not_reset_the_play() {
        let mut s = scrobbler();
        let t = track("Seeked", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&t));
        // Seek posts another Playing for the same track.
        assert!(s.handle(at(9, 0, 30), &playing(&t)).is_none());
        let play = s.handle(at(9, 2, 0), &stopped()).expect("recorded");
        assert_eq!(play.start, at(9, 0, 0).to_rfc3339());
        assert_eq!(play.seconds_played, 120);
    }

    #[test]
    fn stopped_with_nothing_open_is_a_noop() {
        let mut s = scrobbler();
        assert!(s.handle(at(9, 0, 0), &stopped()).is_none());
        assert!(s.flush(at(9, 0, 1)).is_none());
    }

    #[test]
    fn vault_round_trip() {
        let dir = std::env::temp_dir().join(format!("trove-music-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();

        let mut s = scrobbler();
        let a = track("One", Some(200.0));
        let b = track("Skipped", Some(200.0));
        let c = track("Two", Some(200.0));
        let mut plays = Vec::new();
        s.handle(at(9, 0, 0), &playing(&a));
        plays.extend(s.handle(at(9, 3, 0), &stopped()));
        s.handle(at(9, 3, 0), &playing(&b));
        plays.extend(s.handle(at(9, 3, 20), &playing(&c))); // skipped at 20s
        plays.extend(s.flush(at(9, 6, 20)));
        v.append_music_plays(&plays).unwrap();

        let day = v.music_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3);
        assert_eq!(day[0].track, "One");
        assert!(day[0].full_play);
        assert_eq!(day[1].track, "Skipped");
        assert!(!day[1].full_play, "skip round-trips with its flag");
        assert_eq!(day[2].track, "Two");
        assert_eq!(day[2].seconds_played, 180);
        assert!(v.music_timeline("2026-06-09").unwrap().is_empty());
    }

    #[test]
    fn summary_and_daily_split_full_plays_from_skips() {
        let dir = std::env::temp_dir().join(format!("trove-music-sum-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();

        let play = |day: &str, artist: &str, secs: u64, full: bool| Play {
            start: format!("{day}T09:00:00-07:00"),
            end: format!("{day}T09:10:00-07:00"),
            seconds_played: secs,
            track: format!("{artist} song"),
            artist: artist.into(),
            album: "Album".into(),
            genre: "Pop".into(),
            duration_secs: Some(200.0),
            persistent_id: String::new(),
            full_play: full,
        };
        v.append_music_plays(&[
            play("2026-06-09", "Cannons", 200, true),
            play("2026-06-09", "Cannons", 20, false),
            play("2026-06-10", "Cannons", 180, true),
            play("2026-06-10", "Gorillaz", 300, true),
        ])
        .unwrap();

        let s = v.music_summary("2026-06-09", "2026-06-10").unwrap();
        assert_eq!(s.plays, 3);
        assert_eq!(s.skips, 1);
        assert_eq!(s.seconds_played, 700, "skip time still counts as listening time");
        assert_eq!(s.artists[0].artist, "Cannons");
        assert_eq!(s.artists[0].plays, 2, "skip not counted as a play");
        assert_eq!(s.artists[0].seconds, 400);
        assert_eq!(s.artists[1].artist, "Gorillaz");

        let daily = v.music_daily("2026-06-08", "2026-06-10").unwrap();
        assert_eq!(daily.len(), 2, "empty day omitted");
        assert_eq!(daily[0].date, "2026-06-09");
        assert_eq!(daily[0].value, 1.0, "skips excluded from the trend");
        assert_eq!(daily[1].value, 2.0);
    }
}

//! Apple Music play history: the read side of the `music/plays/` stream.
//!
//! Music.app posts a `com.apple.Music.playerInfo` distributed notification on
//! every play/pause/stop/track change, but keeps no play *history* itself
//! (only lifetime counts) — this stream is unrecoverable unless captured
//! live. The capture (the notification listener and the scrobbling state
//! machine) lives in the external `trove-collector` program; this crate only
//! reads what it writes, plus [`Vault::append_music_plays`] as the
//! byte-parity reference writer used by tests and imports.
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
//! than a tiny anti-flicker floor is recorded — skips included, they're
//! signal too ("songs I always bail on"). The Last.fm-style judgment (half
//! the track or 4 minutes — did you actually *listen* to it?) is stored as
//! the `full_play` flag, the default filter for charts/stats; the underlying
//! `seconds_played`/`duration_secs` are always present, so readers can apply
//! any other rule later.

use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::activity::days;
use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};
use crate::vault::Vault;

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join("music/plays"))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static SCROBBLER_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "music-scrobbler",
        name: "Apple Music scrobbler",
        kind: IntegrationKind::Live,
        default_on: true,
        description: "Records every play as it happens, skips included, via the trove-collector background process. No permission needed.",
        domain: "media",
        vault_path: "music/plays/",
        toggleable: true,
        setup: &[
            "Install trove-collector (github.com/david-wills/trove-collector); it listens for Music.app's playerInfo notifications while it runs.",
        ],
        caveats: "Music.app keeps no play history of its own — plays are captured only while the collector runs; gaps can never be backfilled. The daily library snapshot below catches what this misses, but only as count drift.",
    },
    behavior: Behavior::External { collector: "trove-collector" },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

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
    /// Did this count as actually listening (Last.fm rule: half the track or
    /// four minutes, 30 s when the duration is unknown)? False = a skip. The
    /// default read-time filter; recomputable from `seconds_played` /
    /// `duration_secs` if the rule changes.
    #[serde(default)]
    pub full_play: bool,
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

    fn play(day: &str, artist: &str, secs: u64, full: bool) -> Play {
        Play {
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
        }
    }

    /// Byte-parity contract for the append path: exact file bytes, the line
    /// shape the external collector writes and this reader expects.
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
    fn vault_round_trip() {
        let dir = std::env::temp_dir().join(format!("trove-music-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();

        v.append_music_plays(&[
            play("2026-06-10", "One", 180, true),
            play("2026-06-10", "Skipped", 20, false),
        ])
        .unwrap();

        let day = v.music_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].artist, "One");
        assert!(day[0].full_play);
        assert!(!day[1].full_play, "skip round-trips with its flag");
        assert!(v.music_timeline("2026-06-09").unwrap().is_empty());
    }

    #[test]
    fn summary_and_daily_split_full_plays_from_skips() {
        let dir = std::env::temp_dir().join(format!("trove-music-sum-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();

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

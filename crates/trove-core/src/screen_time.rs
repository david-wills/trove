//! Screen Time / cross-device usage collector — reads Apple's Biome
//! `App.InFocus` event streams, which iCloud syncs from every signed-in
//! device (iPhone, iPad, Watch, other Macs) onto this Mac. This is Trove's
//! *only* window into iPhone/iPad activity (HealthKit-style: nothing else on
//! a Mac can see it). Spike note: `docs/notes/screen-time-spike.md`.
//!
//! Source of truth is one JSONL file per device per local day,
//! `screen-time/<device-label>/YYYY-MM-DD.jsonl` (keyed by session start
//! day), one foreground session per line:
//!
//! ```json
//! {"start":"2026-06-09T14:03:01-07:00","end":"2026-06-09T14:09:22-07:00",
//!  "seconds":381,"app":"Safari","bundle_id":"com.apple.mobilesafari",
//!  "device":"58A03EAD-…","device_kind":"iphone","source":"biome-infocus"}
//! ```
//!
//! `screen-time/devices.json` catalogs every seen device UUID (kind, label,
//! last activity) — a rebuildable index, like every derived file.
//!
//! **Time-sensitive, scrobbler-class.** Biome keeps only ~4 rolling 512 KB
//! segments per device (a few weeks each); old segments are deleted and the
//! history is unrecoverable. Every day this runs preserves usage that
//! otherwise vanishes — which is also why every parse is guarded and
//! anomalies are logged-and-skipped, never allowed to panic the owner loop.
//!
//! **Other devices are captured unconditionally; this Mac's own stream is an
//! opt-in backup** (decided with David 2026-06-11, mirroring the Chrome
//! history ↔ extension contract). The live activity watcher
//! ([`crate::activity`]) is the primary, richer capture of Mac foreground
//! time (AFK, window titles, real-time). The `screen-time-this-mac` toggle
//! (default **off**) additionally reads `App.InFocus/local` for users who
//! don't run the watcher; both may write, and the watcher wins at *read*
//! time — [`Vault::screen_time_timeline`] drops `this-mac` sessions that
//! overlap activity events, so enabling the backup never double-counts. The
//! raw files keep both, per the files-first principle.
//!
//! Mechanics are the M3 copy-then-read shape ([`crate::browser`] is the
//! template): incremental per-device cursors in `.trove/screen-time-sync.json`
//! (highest session end, µs since the Apple epoch), rebuildable by scanning
//! the output JSONL so a lost cursor never duplicates rows. Segments are
//! plain append-only files (not SQLite), so they are read directly — there
//! is no lock or WAL to respect, and the per-record CRC in the SEGB framing
//! ([`crate::segb`]) rejects a torn tail; `sync.db` (device identity) *is*
//! SQLite and goes through the shared copy-then-read helper. Syncs ride the
//! owner loop's slow tick, mtime-gated like Podcasts.
//!
//! **Second arm: `Media.NowPlaying/remote` → `media/nowplaying/` (the
//! `nowplaying` toggle).** The same SEGB mechanics decode per-device
//! playback transitions (title/artist/album/app, playing/paused) into
//! wall-clock listening sessions — real "what was playing on the phone"
//! timestamps, feeding the unified media stream ([`crate::media`]) as the
//! `iphone-nowplaying` source. **iPhone/iPad devices only, by design:** a
//! Watch's stream mirrors whatever the paired phone plays (it's the
//! remote-control display) and a Mac's duplicates that Mac's own scrobbler —
//! either would double-count. iPhone *podcast* listens also surface through
//! the Podcasts snapshot diff; the media merge prefers the NowPlaying
//! session (real times, real seconds) and drops the same-episode-same-day
//! podcast event at read time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::activity::days;
use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::segb::{self, ProtoValue, APPLE_EPOCH_OFFSET_S};
use crate::vault::Vault;

const SYNC_FILE: &str = ".trove/screen-time-sync.json";
const DEVICES_FILE: &str = "screen-time/devices.json";

// One pass serves all three arms; their toggles are consulted inside
// `collect_screen_time`. An FDA denial fails here, so the mtime gate doesn't
// commit and the pass retries next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_screen_time()?;
    Ok(crate::registry::CollectOutcome::note_if(
        s.new_sessions + s.new_plays > 0,
        || {
            format!(
                "screen time synced — {} sessions from {} devices, {} plays{}",
                s.new_sessions,
                s.devices,
                s.new_plays,
                if s.skipped_records > 0 {
                    format!(" ({} records skipped)", s.skipped_records)
                } else {
                    String::new()
                }
            )
        },
    ))
}

// All three arms read the same FDA-gated Biome streams.
fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(screen_time_permission_ok()),
        required: true,
    }
}

// One sync pass covers all devices, so "last sync ran" is the honest answer
// for the shared arm (the other arms have their own per-day files to probe).
fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_screen_time_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

fn this_mac_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join("screen-time/this-mac"))
}

fn nowplaying_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join("media/nowplaying"))
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. One collect pass
/// serves all three toggles below (the other two defs are co-gated onto this
/// one); the per-arm opt-ins are consulted inside the collector.
pub static SCREEN_TIME_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "screen-time",
        name: "Screen Time (other devices)",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Per-app usage sessions from every iCloud-synced device — iPhone, iPad, Apple Watch, other Macs — read from the Biome streams Apple syncs onto this Mac. The only window into iPhone and iPad activity.",
        domain: "screen-time",
        vault_path: "screen-time/",
        toggleable: true,
        setup: &["Uses the same Full Disk Access grant as Messages and Safari — nothing extra once that's done."],
        caveats: "Apple keeps only a few weeks of these streams per device, then deletes them unrecoverably — usage history exists only from when this collector starts running. Devices sync on iCloud's schedule (minutes to hours), never in real time.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::on_change(crate::browser::BROWSER_SYNC_SECS, screen_time_mtime), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Registered in [`crate::integrations::INTEGRATIONS`]. Collected by the
/// shared screen-time pass above.
pub static NOWPLAYING_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "nowplaying",
        name: "Now Playing (iPhone & iPad)",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "What was actually playing on the iPhone or iPad — music, podcasts, video audio — as timestamped listening sessions, from the same synced streams as Screen Time. Feeds the unified Media view.",
        domain: "media",
        vault_path: "media/nowplaying/",
        toggleable: true,
        setup: &["Uses the same Full Disk Access grant as Messages and Safari — nothing extra once that's done."],
        caveats: "iPhone and iPad only by design: the Watch's stream mirrors the phone and a Mac's duplicates the Mac scrobbler, so both are excluded to avoid double-counting. Seconds are wall-clock play time (seeks are invisible). Same few-weeks retention as Screen Time.",
    },
    behavior: Behavior::CoveredBy("screen-time"),
    permission: Some(def_permission),
    last_data: Some(nowplaying_last_data),
    connection: None,
    pull: None,
};

/// Registered in [`crate::integrations::INTEGRATIONS`]. Collected by the
/// shared screen-time pass above; the opt-in is consulted in the collector.
pub static THIS_MAC_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "screen-time-this-mac",
        name: "Screen Time (this Mac, backup)",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads this Mac's own foreground-app stream from Biome — a backup for setups where the live activity watcher isn't running. Off by default: the watcher above is the richer, primary capture.",
        domain: "screen-time",
        vault_path: "screen-time/this-mac/",
        toggleable: true,
        setup: &["Enable only if you want Mac usage without the live activity watcher (or to import the recent weeks Biome still holds)."],
        caveats: "Where the activity watcher also covered a span, the watcher wins at read time, so enabling this never double-counts — but it adds little while the watcher runs. No window titles, no AFK detection.",
    },
    behavior: Behavior::CoveredBy("screen-time"),
    permission: Some(def_permission),
    last_data: Some(this_mac_last_data),
    connection: None,
    pull: None,
};

/// Provenance stamped on every session this module writes.
pub const SCREEN_TIME_SOURCE: &str = "biome-infocus";

/// Provenance stamped on Now Playing sessions (the spike's name for the
/// source — it covers iPad too, but the phone is what it exists for).
pub const NOWPLAYING_SOURCE: &str = "iphone-nowplaying";

/// Now Playing sessions shorter than this are channel-surfing noise, not
/// listening — the same floor the Music scrobbler applies to plays.
const NOWPLAYING_MIN_SECS: i64 = 5;

/// A pause/metadata blip is one listen, not two, when the *same title*
/// resumes within this window. Also the gap past which a same-title stop is
/// treated as a real end (billed to the stop) rather than a momentary pause.
const NOWPLAYING_BRIDGE_SECS: i64 = 90;

/// Seconds of slack when deciding a `this-mac` session overlaps an activity
/// span (same constant class as the browser extension's overlap rule).
const OVERLAP_SLACK_SECS: i64 = 120;

/// One contiguous foreground session of one app on one device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ScreenTimeSession {
    /// RFC3339 local time.
    pub start: String,
    pub end: String,
    pub seconds: u64,
    /// Friendly app name derived from the bundle id.
    pub app: String,
    pub bundle_id: String,
    /// Biome device UUID ("this-mac" for the local-stream backup arm).
    pub device: String,
    /// "iphone" | "ipad" | "watch" | "mac" | "this-mac" | "ios" | "unknown".
    pub device_kind: String,
    /// Provenance: always "biome-infocus" here.
    #[serde(default)]
    pub source: String,
}

/// One decoded App.InFocus event: an app became (or stopped being) the
/// frontmost app on a device.
#[derive(Debug, Clone, PartialEq)]
pub struct InFocusEvent {
    /// Event time, µs since the Apple epoch (2001-01-01). Carried as integer
    /// µs so cursors round-trip exactly (the browser-cursor pattern).
    pub ts_us: i64,
    /// true = app came to the front, false = it left the front.
    pub starting: bool,
    pub bundle_id: String,
}

/// A paired session in cursor-native time.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSession {
    pub start_us: i64,
    pub end_us: i64,
    pub bundle_id: String,
}

/// One wall-clock listening session from a device's Now Playing stream,
/// stored one per line in `media/nowplaying/YYYY-MM-DD.jsonl` (keyed by
/// start day; flat across devices — each row carries its device).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowPlayingPlay {
    /// RFC3339 local time.
    pub start: String,
    pub end: String,
    /// Wall-clock seconds in the playing state (pauses excluded by the
    /// pairing, but seeks/scrubs are invisible — this is elapsed time).
    pub seconds: u64,
    /// Track or episode title.
    pub title: String,
    /// Artist or show.
    #[serde(default)]
    pub artist: String,
    /// Album (music) or show (podcasts duplicate it here).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album: String,
    /// Bundle id of the playing app (e.g. "com.apple.Music").
    #[serde(default)]
    pub app: String,
    /// Track/episode length in seconds; 0 when the app didn't report one.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub duration_secs: u64,
    /// Biome device UUID.
    pub device: String,
    /// "iphone" | "ipad" | "ios".
    pub device_kind: String,
    /// Provenance: always "iphone-nowplaying" here.
    #[serde(default)]
    pub source: String,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

/// One decoded Media.NowPlaying event: a playback-state transition with the
/// now-playing metadata at that moment.
#[derive(Debug, Clone, PartialEq)]
pub struct NowPlayingEvent {
    /// Event time, µs since the Apple epoch.
    pub ts_us: i64,
    /// true = playback running (state 1); paused/stopped/interrupted
    /// otherwise. Any event is end-evidence for the open session.
    pub playing: bool,
    /// Empty on bare state-change records (they still close sessions).
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Playing app bundle id.
    pub app: String,
    /// Reported length in seconds; 0 = unknown.
    pub duration_secs: u64,
}

/// A paired Now Playing session in cursor-native time.
#[derive(Debug, Clone, PartialEq)]
pub struct RawNowPlaying {
    pub start_us: i64,
    pub end_us: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub app: String,
    pub duration_secs: u64,
}

/// Incremental-sync state, persisted in `.trove/screen-time-sync.json`.
/// `cursors` is the real state (rebuildable from JSONL); `seg_mtimes` is
/// only a skip-work cache — losing it just re-parses, never duplicates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScreenTimeSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest emitted session end per device UUID, µs since Apple epoch.
    pub cursors: BTreeMap<String, i64>,
    /// Newest segment mtime seen per device (unix ms), to skip unchanged
    /// device dirs cheaply.
    #[serde(default)]
    pub seg_mtimes: BTreeMap<String, i64>,
}

/// One device in the `screen-time/devices.json` catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DeviceInfo {
    pub kind: String,
    /// Vault subdirectory name, e.g. "iphone-58a03ead". Fixed once chosen so
    /// file paths stay stable even if inference improves later.
    pub label: String,
    /// RFC3339 local time of the newest event seen from this device.
    #[serde(default)]
    pub last_seen: String,
    /// Raw `DevicePeer.platform` from Biome's sync.db, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<i64>,
    /// OS build string from sync.db (e.g. "25F80"), when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScreenTimeSyncStats {
    /// Device dirs with new data this pass.
    pub devices: u32,
    pub new_sessions: u64,
    /// New Now Playing listening sessions appended this pass.
    pub new_plays: u64,
    /// Records that failed framing or decoding — a few deleted records per
    /// segment are normal; a surge means format drift.
    pub skipped_records: u64,
}

/// Per-app usage over a range.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ScreenTimeAppUsage {
    pub app: String,
    pub bundle_id: String,
    pub seconds: u64,
}

/// Per-device usage over a range.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ScreenTimeDeviceUsage {
    pub device: String,
    pub kind: String,
    pub label: String,
    pub seconds: u64,
}

/// Aggregate of a date range for the Screen Time view.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ScreenTimeSummary {
    pub total_seconds: u64,
    /// Apps by time, descending.
    pub apps: Vec<ScreenTimeAppUsage>,
    /// Devices by time, descending.
    pub devices: Vec<ScreenTimeDeviceUsage>,
}

// ---------------------------------------------------------------------------
// Decoding: SEGB record payload → InFocusEvent

/// Decode one App.InFocus record payload. Field semantics (verified on real
/// iPhone/Watch/Mac segments, macOS 26): field 3 varint = transition
/// (1 = came frontmost, 0 = left), field 4 fixed64 = f64 event time in Apple
/// epoch seconds, field 6 = bundle id. Unknown fields are ignored, so new
/// fields in an OS update don't break decoding. `None` = not an event we
/// understand — callers count and skip.
pub fn decode_infocus(data: &[u8]) -> Option<InFocusEvent> {
    let fields = segb::proto_fields(data)?;
    let mut transition: Option<u64> = None;
    let mut ts: Option<f64> = None;
    let mut bundle: Option<&[u8]> = None;
    for (field, value) in &fields {
        match (field, value) {
            (3, ProtoValue::Varint(v)) => transition = Some(*v),
            (4, ProtoValue::Fixed64(bits)) => ts = Some(f64::from_bits(*bits)),
            (6, ProtoValue::Bytes(b)) => bundle = Some(b),
            _ => {}
        }
    }
    let ts = ts.filter(|t| t.is_finite() && *t > 0.0 && *t < 5e9)?;
    let bundle_id = std::str::from_utf8(bundle?).ok()?.to_string();
    if bundle_id.is_empty() {
        return None;
    }
    Some(InFocusEvent {
        ts_us: (ts * 1e6).round() as i64,
        starting: transition? == 1,
        bundle_id,
    })
}

/// Decode one Media.NowPlaying record payload. Field semantics (verified on
/// real iPhone/iPad/Watch segments, macOS 26): field 2 fixed64 = f64 event
/// time (Apple epoch), field 3 varint = playback state (1 playing; 0/2/3/4
/// stopped/paused/interrupted), field 4 = album/show, field 5 = artist,
/// field 6 varint = duration seconds, field 8 = title, field 15 = playing
/// app's bundle id. Title and metadata are absent on bare state-change
/// records — those still decode (they end sessions).
pub fn decode_nowplaying(data: &[u8]) -> Option<NowPlayingEvent> {
    let fields = segb::proto_fields(data)?;
    let mut ts: Option<f64> = None;
    let mut state: Option<u64> = None;
    let mut duration = 0u64;
    let (mut title, mut artist, mut album, mut app) = (None, None, None, None);
    for (field, value) in &fields {
        match (field, value) {
            (2, ProtoValue::Fixed64(bits)) => ts = Some(f64::from_bits(*bits)),
            (3, ProtoValue::Varint(v)) => state = Some(*v),
            (4, ProtoValue::Bytes(b)) => album = Some(*b),
            (5, ProtoValue::Bytes(b)) => artist = Some(*b),
            (6, ProtoValue::Varint(v)) => duration = *v,
            (8, ProtoValue::Bytes(b)) => title = Some(*b),
            (15, ProtoValue::Bytes(b)) => app = Some(*b),
            _ => {}
        }
    }
    let ts = ts.filter(|t| t.is_finite() && *t > 0.0 && *t < 5e9)?;
    let utf8 = |b: Option<&[u8]>| {
        b.and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or_default()
            .to_string()
    };
    Some(NowPlayingEvent {
        ts_us: (ts * 1e6).round() as i64,
        playing: state? == 1,
        title: utf8(title),
        artist: utf8(artist),
        album: utf8(album),
        app: utf8(app),
        duration_secs: duration,
    })
}

// ---------------------------------------------------------------------------
// Pairing: events → sessions (pure, unit-tested)

/// Pair foreground transitions into sessions. At most one app is frontmost,
/// so *any* event is evidence the open session ended at that moment: an end
/// event closes it (matching bundle or not — mismatches don't occur in
/// measured data), and a start event closes it and opens the next. A
/// trailing start stays open and is deliberately *not* emitted — its end
/// hasn't synced yet; the cursor stays behind it so the next pass picks it
/// up whole. Stray ends (open session emitted last pass) are ignored.
/// Zero-length sessions are dropped.
pub fn pair_sessions(mut events: Vec<InFocusEvent>) -> Vec<RawSession> {
    // Ends sort before starts at the same instant — app switches share a
    // timestamp (measured: ~28% of adjacent events), and end-then-start is
    // the real order.
    events.sort_by(|a, b| a.ts_us.cmp(&b.ts_us).then(a.starting.cmp(&b.starting)));
    let mut out = Vec::new();
    let mut open: Option<(i64, String)> = None;
    for ev in events {
        if let Some((start_us, bundle_id)) = open.take() {
            if ev.ts_us > start_us {
                out.push(RawSession {
                    start_us,
                    end_us: ev.ts_us,
                    bundle_id,
                });
            }
        }
        if ev.starting {
            open = Some((ev.ts_us, ev.bundle_id));
        }
    }
    out
}

/// Keep a session only if it clears the noise floor.
fn finalize_nowplaying(s: RawNowPlaying, out: &mut Vec<RawNowPlaying>) {
    if s.end_us - s.start_us >= NOWPLAYING_MIN_SECS * 1_000_000 {
        out.push(s);
    }
}

/// Whether the open title plays again within the bridge window after a stop —
/// the test that tells a momentary pause from a real end.
fn nowplaying_resumes(events: &[NowPlayingEvent], from: usize, after_ts: i64, app: &str, title: &str) -> bool {
    let bridge = NOWPLAYING_BRIDGE_SECS * 1_000_000;
    events[from..]
        .iter()
        .take_while(|e| e.ts_us <= after_ts + bridge)
        .any(|e| e.playing && e.app == app && !e.title.is_empty() && e.title == title)
}

/// Pair Now Playing transitions into listening sessions. A playing event with
/// content opens a session; later evidence of the same title extends it. The
/// model handles the real-world patterns the raw stream throws at us:
///
/// - **Pauses** — a stop of the playing title doesn't end the session if that
///   same title resumes within [`NOWPLAYING_BRIDGE_SECS`]; otherwise it closes
///   at the stop (billed up to that moment, never the pause gap).
/// - **Stale chapter stops** — some apps (e.g. Audible) emit a flurry of
///   play/pause records carrying *different* chapter titles under one album,
///   all within a second. A `false` for a different title of the same album
///   is churn, not a stop, and is ignored — without this it would slam the
///   open session shut at zero length, which is exactly why audiobook listens
///   were vanishing. Each chapter still becomes its own session (nothing is
///   dropped); the media layer labels them by the book.
/// - **Foreign events** — any event for a different item is end-evidence (one
///   thing plays at a time): it closes the open session, and a foreign *play*
///   opens the next.
///
/// Sub-[`NOWPLAYING_MIN_SECS`] sessions are dropped; a session still open at
/// the end (a play whose stop hasn't synced) is held for the next pass.
pub fn pair_nowplaying(mut events: Vec<NowPlayingEvent>) -> Vec<RawNowPlaying> {
    events.sort_by(|a, b| a.ts_us.cmp(&b.ts_us).then(a.playing.cmp(&b.playing)));
    let bridge = NOWPLAYING_BRIDGE_SECS * 1_000_000;
    let has_content = |e: &NowPlayingEvent| !e.title.is_empty() || !e.album.is_empty();

    let mut out = Vec::new();
    let mut open: Option<RawNowPlaying> = None;
    let mut last_play = 0i64; // last play that extended `open`

    for i in 0..events.len() {
        if let Some(o) = &open {
            let ev = &events[i];
            let same_app = ev.app == o.app;
            let same_title = same_app && !ev.title.is_empty() && ev.title == o.title;
            let gap = ev.ts_us - last_play;

            if same_title && gap <= bridge {
                // Same item still current.
                if ev.playing {
                    let (ts, dur) = (ev.ts_us, ev.duration_secs);
                    let o = open.as_mut().unwrap();
                    o.end_us = ts.max(o.end_us);
                    if o.duration_secs == 0 {
                        o.duration_secs = dur;
                    }
                    last_play = ts;
                } else if !nowplaying_resumes(&events, i + 1, ev.ts_us, &ev.app, &ev.title) {
                    let mut done = open.take().unwrap();
                    done.end_us = ev.ts_us.max(done.end_us);
                    finalize_nowplaying(done, &mut out);
                }
                continue;
            }

            // A stop carrying a *different* title of the same album is churn
            // from a metadata burst, not this session's stop — ignore it.
            if !ev.playing && same_app && !ev.album.is_empty() && ev.album == o.album && ev.title != o.title {
                continue;
            }

            // Foreign (or a far-later same-title stop): close at this instant.
            let mut done = open.take().unwrap();
            done.end_us = ev.ts_us.max(done.end_us);
            finalize_nowplaying(done, &mut out);
        }

        if open.is_none() && events[i].playing && has_content(&events[i]) {
            let ev = &events[i];
            open = Some(RawNowPlaying {
                start_us: ev.ts_us,
                end_us: ev.ts_us,
                title: ev.title.clone(),
                artist: ev.artist.clone(),
                album: ev.album.clone(),
                app: ev.app.clone(),
                duration_secs: ev.duration_secs,
            });
            last_play = ev.ts_us;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Time conversion (exact integer µs round trips, the browser pattern)

fn apple_us_to_local(us: i64) -> Option<DateTime<Local>> {
    let unix_us = us.checked_add(APPLE_EPOCH_OFFSET_S.checked_mul(1_000_000)?)?;
    if unix_us <= 0 {
        return None;
    }
    DateTime::from_timestamp_micros(unix_us).map(|t| t.with_timezone(&Local))
}

fn rfc3339_to_apple_us(time: &str) -> Option<i64> {
    let t = DateTime::parse_from_rfc3339(time).ok()?;
    t.timestamp_micros()
        .checked_sub(APPLE_EPOCH_OFFSET_S * 1_000_000)
}

// ---------------------------------------------------------------------------
// Device identity

/// What Biome's sync.db knows about a peer device.
#[derive(Debug, Clone, Default)]
pub(crate) struct PeerInfo {
    platform: Option<i64>,
    model: String,
    me: bool,
}

fn biome_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/Biome"))
}

/// The dir holding one subdir per Biome stream (`App.InFocus`,
/// `Media.NowPlaying`, …).
fn biome_streams_root() -> Option<PathBuf> {
    biome_root().map(|b| b.join("streams/restricted"))
}

/// Whether this process can read the Biome streams. False = no Full Disk
/// Access (or no Biome data at all). Same per-binary, no-prompt grant as
/// Safari history — the UI deep-links to System Settings.
pub fn screen_time_permission_ok() -> bool {
    biome_streams_root().is_some_and(|r| fs::read_dir(r.join("App.InFocus/remote")).is_ok())
}

/// Newest mtime across every App.InFocus and Media.NowPlaying segment — the
/// slow-tick gate: unchanged means no device synced anything new, so the
/// whole pass can be skipped. `None` when unreadable (no FDA) — callers
/// treat that as "don't sync".
pub fn screen_time_mtime() -> Option<std::time::SystemTime> {
    let root = biome_streams_root()?;
    let mut newest: Option<std::time::SystemTime> = None;
    for arm in [
        "App.InFocus/remote",
        "App.InFocus/local",
        "Media.NowPlaying/remote",
    ] {
        let Ok(entries) = fs::read_dir(root.join(arm)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let times = if path.is_dir() {
                segment_files(&path).iter().filter_map(file_mtime).max()
            } else {
                file_mtime(&path)
            };
            newest = newest.max(times);
        }
    }
    newest
}

fn file_mtime(path: impl AsRef<Path>) -> Option<std::time::SystemTime> {
    fs::metadata(path.as_ref()).and_then(|m| m.modified()).ok()
}

/// Segment files in a device dir: numeric names (µs-since-2001 creation
/// time, so the sort is chronological). Skips the `tombstone/` subdir and
/// anything else unexpected.
fn segment_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.path().is_file()
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Read `DevicePeer` out of Biome's sync.db (copy-then-read — it's SQLite).
/// Best effort: identity enrichment only, an empty map degrades to the
/// app-signature heuristic.
fn device_peers() -> HashMap<String, PeerInfo> {
    let Some(db) = biome_root().map(|b| b.join("sync/sync.db")) else {
        return HashMap::new();
    };
    if fs::File::open(&db).is_err() {
        return HashMap::new();
    }
    let stem = format!("trove-biome-sync-{}", std::process::id());
    crate::browser::import_via_copy(&db, &stem, |tmp| {
        let conn = rusqlite::Connection::open(tmp)?;
        let mut stmt = conn.prepare(
            "SELECT device_identifier, platform, CAST(model AS TEXT), me FROM DevicePeer",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = HashMap::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            out.insert(
                id,
                PeerInfo {
                    platform: row.get::<_, Option<i64>>(1)?,
                    model: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    me: row.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0,
                },
            );
        }
        Ok(out)
    })
    .unwrap_or_default()
}

/// Infer what kind of device a stream came from. sync.db's platform enum is
/// authoritative where present (verified on this machine: 1 = iPad,
/// 2 = iPhone, 3/4 = Mac; the `me` row is platform 3); the app-signature
/// heuristic covers devices sync.db doesn't list (the Watch syncs via the
/// phone and has no DevicePeer row).
pub(crate) fn infer_device_kind(platform: Option<i64>, bundles: &HashSet<String>) -> &'static str {
    match platform {
        Some(1) => return "ipad",
        Some(2) => return "iphone",
        Some(3) | Some(4) => return "mac",
        _ => {}
    }
    let any = |f: &dyn Fn(&str) -> bool| bundles.iter().any(|b| f(b));
    if any(&|b| b.starts_with("com.apple.Nano") || b.contains("carousel")) {
        return "watch";
    }
    if any(&|b| {
        matches!(
            b,
            "com.apple.finder" | "com.apple.dock" | "com.apple.Safari" | "com.googlecode.iterm2"
        )
    }) {
        return "mac";
    }
    if any(&|b| {
        b.starts_with("com.apple.springboard")
            || b.starts_with("com.apple.mobile")
            || b.starts_with("com.apple.Mobile")
    }) {
        return "ios";
    }
    "unknown"
}

/// Friendly display name for a bundle id: a small map for the system apps
/// whose ids don't read well, else the last dot-component capitalized
/// ("com.reddit.Reddit" → "Reddit").
pub(crate) fn friendly_app_name(bundle_id: &str) -> String {
    static KNOWN: &[(&str, &str)] = &[
        ("com.apple.mobilesafari", "Safari"),
        ("com.apple.MobileSMS", "Messages"),
        ("com.apple.mobilemail", "Mail"),
        ("com.apple.mobilecal", "Calendar"),
        ("com.apple.mobilenotes", "Notes"),
        ("com.apple.mobileslideshow", "Photos"),
        ("com.apple.mobiletimer", "Clock"),
        ("com.apple.MobileAddressBook", "Contacts"),
        ("com.apple.MobileStore", "iTunes Store"),
        ("com.apple.camera", "Camera"),
        ("com.apple.springboard.stand-by", "StandBy"),
        ("com.apple.springboard", "Home Screen"),
        ("com.apple.SleepLockScreen", "Lock Screen"),
        ("com.apple.Preferences", "Settings"),
        ("com.apple.AppStore", "App Store"),
        ("com.apple.Passbook", "Wallet"),
        ("com.apple.iBooks", "Books"),
        ("com.apple.podcasts", "Podcasts"),
        ("com.apple.facetime", "FaceTime"),
        ("com.apple.DocumentsApp", "Files"),
        ("com.apple.InCallService", "Phone Call"),
        ("com.apple.mobilephone", "Phone"),
        ("com.apple.Fitness", "Fitness"),
        ("com.apple.Health", "Health"),
        ("com.apple.weather", "Weather"),
        ("com.apple.reminders", "Reminders"),
        ("com.apple.shortcuts", "Shortcuts"),
        ("com.apple.findmy", "Find My"),
        ("com.apple.calculator", "Calculator"),
        ("com.apple.maps", "Maps"),
        ("com.apple.Maps", "Maps"),
        ("com.apple.Music", "Music"),
        ("com.apple.MobileAppStore", "App Store"),
        ("com.apple.NanoNowPlaying", "Now Playing (Watch)"),
        ("com.apple.NanoMusic", "Music (Watch)"),
        ("com.googlecode.iterm2", "iTerm2"),
    ];
    if let Some((_, name)) = KNOWN.iter().find(|(id, _)| *id == bundle_id) {
        return (*name).to_string();
    }
    let last = bundle_id.rsplit('.').next().unwrap_or(bundle_id);
    let mut chars = last.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => bundle_id.to_string(),
    }
}

/// "Idle-ish" foreground records: real captured data, but a lit screen
/// rather than usage — lock screens, StandBy, watch faces. Stored rows keep
/// them (full fidelity at write time); the reads hide them unless asked
/// (opinions at read time). Grown by observation, not exhaustive.
pub fn is_idle_bundle(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        "com.apple.loginwindow"                // Mac lock/login screen
            | "com.apple.springboard.stand-by" // iPhone StandBy
            | "com.apple.SleepLockScreen"      // iOS lock screen
    ) || bundle_id.ends_with("carousel.clock") // watch face
}

// ---------------------------------------------------------------------------
// Collector

/// Decode every live record in a device dir's segments with `decode`,
/// counting parse anomalies. Generic over the stream's event type.
fn read_device_events<T>(dir: &Path, decode: impl Fn(&[u8]) -> Option<T>) -> (Vec<T>, u64) {
    let mut events = Vec::new();
    let mut skipped = 0u64;
    for seg_path in segment_files(dir) {
        // Segments are append-only flat files — a direct read sees the same
        // bytes a copy would, and the per-record CRC rejects a torn tail.
        let bytes = match fs::read(&seg_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "trove screen-time: reading {} failed: {e}",
                    seg_path.display()
                );
                skipped += 1;
                continue;
            }
        };
        match segb::read_segb(&bytes) {
            Ok(seg) => {
                skipped += seg.anomalies as u64;
                for entry in &seg.entries {
                    match decode(&entry.data) {
                        Some(ev) => events.push(ev),
                        None => skipped += 1,
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "trove screen-time: segment {} unparseable (format drift?): {e:#}",
                    seg_path.display()
                );
                skipped += 1;
            }
        }
    }
    (events, skipped)
}

/// The per-device subdirs of a stream's `remote/` dir, sorted by UUID for
/// deterministic passes. Empty when the dir is missing or unreadable.
fn device_dirs(remote: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(remote) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort();
    out
}

/// Newest segment mtime in a device dir, unix milliseconds.
fn dir_seg_mtime_ms(dir: &Path) -> Option<i64> {
    segment_files(dir)
        .iter()
        .filter_map(file_mtime)
        .max()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

/// Which Biome arms a sync pass runs — each is its own hub toggle, consulted
/// by the caller (the browser chrome/safari split pattern).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BiomeArms {
    /// App.InFocus/remote — other devices, the unconditional default.
    pub infocus: bool,
    /// App.InFocus/local — this Mac's opt-in backup.
    pub this_mac: bool,
    /// Media.NowPlaying/remote — iPhone/iPad playback sessions.
    pub nowplaying: bool,
}

impl Vault {
    /// One sync pass over every device's Biome streams. Incremental via the
    /// persisted cursors; per-device failures are logged and skipped so one
    /// bad device never blocks the rest. Silently a no-op while the Biome
    /// paths are unreadable (no FDA — the hub shows the banner).
    pub fn collect_screen_time(&self) -> Result<ScreenTimeSyncStats> {
        let Some(root) = biome_streams_root() else {
            return Ok(ScreenTimeSyncStats::default());
        };
        let arms = BiomeArms {
            infocus: self.integration_enabled("screen-time"),
            this_mac: self.integration_enabled("screen-time-this-mac"),
            nowplaying: self.integration_enabled("nowplaying"),
        };
        self.collect_screen_time_from(&root, arms, &device_peers())
    }

    /// The pass itself, path- and identity-injected for tests. `streams_root`
    /// holds the `App.InFocus` and `Media.NowPlaying` stream dirs.
    pub(crate) fn collect_screen_time_from(
        &self,
        streams_root: &Path,
        arms: BiomeArms,
        peers: &HashMap<String, PeerInfo>,
    ) -> Result<ScreenTimeSyncStats> {
        let mut stats = ScreenTimeSyncStats::default();
        let remote = streams_root.join("App.InFocus/remote");
        if fs::read_dir(&remote).is_err() {
            // No FDA or no Biome data — silent skip, Safari-style.
            return Ok(stats);
        }
        let mut state = match self.read_screen_time_sync() {
            Some(s) => s,
            None => self.rebuild_screen_time_sync(),
        };
        let mut devices = self.screen_time_devices();

        // Remote devices: every synced device, captured unconditionally —
        // except this Mac's own remote alias, which mirrors local/ and is
        // owned by the opt-in backup arm below.
        if arms.infocus {
            for (uuid, dir) in device_dirs(&remote) {
                if peers.get(&uuid).is_some_and(|p| p.me) {
                    continue;
                }
                let peer = peers.get(&uuid).cloned().unwrap_or_default();
                self.sync_device_dir(
                    &dir,
                    &uuid,
                    &peer,
                    None,
                    &mut state,
                    &mut devices,
                    &mut stats,
                );
            }
        }

        // This Mac's own stream: opt-in backup for users without the live
        // activity watcher. The watcher wins at read time (see module docs).
        if arms.this_mac {
            let local = streams_root.join("App.InFocus/local");
            if local.is_dir() {
                self.sync_device_dir(
                    &local,
                    "this-mac",
                    &PeerInfo::default(),
                    Some("this-mac"),
                    &mut state,
                    &mut devices,
                    &mut stats,
                );
            }
        }

        // Now Playing: iPhone/iPad only (see module docs — Watch streams
        // mirror the phone, Mac streams duplicate the Mac scrobbler). Runs
        // after App.InFocus so the device catalog can vouch for kinds the
        // peers table doesn't know.
        if arms.nowplaying {
            for (uuid, dir) in device_dirs(&streams_root.join("Media.NowPlaying/remote")) {
                if peers.get(&uuid).is_some_and(|p| p.me) {
                    continue;
                }
                let kind = match peers.get(&uuid).and_then(|p| p.platform) {
                    Some(1) => "ipad".to_string(),
                    Some(2) => "iphone".to_string(),
                    Some(_) => continue,
                    None => match devices.get(&uuid).map(|d| d.kind.as_str()) {
                        Some(k @ ("iphone" | "ipad" | "ios")) => k.to_string(),
                        _ => continue, // not known to be an iPhone/iPad
                    },
                };
                self.sync_nowplaying_dir(&dir, &uuid, &kind, &mut state, &mut stats);
            }
        }

        state.updated = Local::now().to_rfc3339();
        self.write_screen_time_sync(&state)?;
        self.write_screen_time_devices(&devices)?;
        Ok(stats)
    }

    /// Sync one device dir: parse changed segments, pair, append, advance
    /// the cursor. Failures log and leave state untouched for a retry.
    #[allow(clippy::too_many_arguments)]
    fn sync_device_dir(
        &self,
        dir: &Path,
        uuid: &str,
        peer: &PeerInfo,
        forced_kind: Option<&str>,
        state: &mut ScreenTimeSyncState,
        devices: &mut BTreeMap<String, DeviceInfo>,
        stats: &mut ScreenTimeSyncStats,
    ) {
        // Unchanged segments → nothing new (stale device dirs from retired
        // hardware stay permanently skipped after their first pass).
        let Some(mtime_ms) = dir_seg_mtime_ms(dir) else {
            return; // no segments at all
        };
        if state.seg_mtimes.get(uuid) == Some(&mtime_ms) {
            return;
        }

        let (mut events, skipped) = read_device_events(dir, decode_infocus);
        stats.skipped_records += skipped;
        if events.is_empty() {
            state.seg_mtimes.insert(uuid.to_string(), mtime_ms);
            return;
        }

        // Device identity: an existing catalog entry wins (labels are file
        // paths — they must never move); otherwise infer and register.
        let bundles: HashSet<String> = events.iter().map(|e| e.bundle_id.clone()).collect();
        let last_ts = events.iter().map(|e| e.ts_us).max().unwrap_or(0);
        let (kind, label) = match devices.get(uuid) {
            Some(info) => (info.kind.clone(), info.label.clone()),
            None => {
                let kind = forced_kind
                    .unwrap_or_else(|| infer_device_kind(peer.platform, &bundles))
                    .to_string();
                let label = if forced_kind.is_some() {
                    kind.clone()
                } else {
                    let prefix: String =
                        uuid.chars().take(8).collect::<String>().to_lowercase();
                    format!("{kind}-{prefix}")
                };
                (kind, label)
            }
        };
        let last_seen = apple_us_to_local(last_ts)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        devices.insert(
            uuid.to_string(),
            DeviceInfo {
                kind: kind.clone(),
                label: label.clone(),
                last_seen,
                platform: peer.platform,
                model: peer.model.clone(),
            },
        );

        // Incremental: only events at/after the cursor (the cursor is the
        // last emitted session's end — re-seeing that boundary instant is
        // how a session that *starts* exactly there gets picked up; the
        // already-written session can't re-emit because its start is gone).
        let cursor = state.cursors.get(uuid).copied();
        if let Some(c) = cursor {
            events.retain(|e| e.ts_us >= c);
        }
        let sessions = pair_sessions(events);
        if sessions.is_empty() {
            state.seg_mtimes.insert(uuid.to_string(), mtime_ms);
            return;
        }

        let rows: Vec<ScreenTimeSession> = sessions
            .iter()
            .filter_map(|s| {
                let start = apple_us_to_local(s.start_us)?;
                let end = apple_us_to_local(s.end_us)?;
                let seconds = ((s.end_us - s.start_us) / 1_000_000).max(0) as u64;
                if seconds == 0 {
                    return None;
                }
                Some(ScreenTimeSession {
                    start: start.to_rfc3339(),
                    end: end.to_rfc3339(),
                    seconds,
                    app: friendly_app_name(&s.bundle_id),
                    bundle_id: s.bundle_id.clone(),
                    device: uuid.to_string(),
                    device_kind: kind.clone(),
                    source: SCREEN_TIME_SOURCE.to_string(),
                })
            })
            .collect();
        if let Err(e) = self.append_screen_time_sessions(&label, &rows) {
            eprintln!("trove screen-time: append for {label} failed: {e:#}");
            return; // cursor untouched → retried next pass, never lost
        }
        let new_cursor = sessions.iter().map(|s| s.end_us).max().unwrap();
        state.cursors.insert(uuid.to_string(), new_cursor);
        state.seg_mtimes.insert(uuid.to_string(), mtime_ms);
        stats.devices += 1;
        stats.new_sessions += rows.len() as u64;
    }

    /// Sync one device's Now Playing dir: the same shape as
    /// [`Vault::sync_device_dir`] with its own cursor/mtime namespace
    /// (`np/<uuid>`) and the `media/nowplaying/` store.
    fn sync_nowplaying_dir(
        &self,
        dir: &Path,
        uuid: &str,
        kind: &str,
        state: &mut ScreenTimeSyncState,
        stats: &mut ScreenTimeSyncStats,
    ) {
        let key = format!("np/{uuid}");
        let Some(mtime_ms) = dir_seg_mtime_ms(dir) else {
            return;
        };
        if state.seg_mtimes.get(&key) == Some(&mtime_ms) {
            return;
        }
        let (mut events, skipped) = read_device_events(dir, decode_nowplaying);
        stats.skipped_records += skipped;
        if let Some(c) = state.cursors.get(&key).copied() {
            events.retain(|e| e.ts_us >= c);
        }
        let sessions = pair_nowplaying(events);
        if sessions.is_empty() {
            state.seg_mtimes.insert(key, mtime_ms);
            return;
        }
        let rows: Vec<NowPlayingPlay> = sessions
            .iter()
            .filter_map(|s| {
                Some(NowPlayingPlay {
                    start: apple_us_to_local(s.start_us)?.to_rfc3339(),
                    end: apple_us_to_local(s.end_us)?.to_rfc3339(),
                    seconds: ((s.end_us - s.start_us) / 1_000_000).max(0) as u64,
                    title: s.title.clone(),
                    artist: s.artist.clone(),
                    album: s.album.clone(),
                    app: s.app.clone(),
                    duration_secs: s.duration_secs,
                    device: uuid.to_string(),
                    device_kind: kind.to_string(),
                    source: NOWPLAYING_SOURCE.to_string(),
                })
            })
            .collect();
        if let Err(e) = self.append_nowplaying_plays(&rows) {
            eprintln!("trove screen-time: nowplaying append for {uuid} failed: {e:#}");
            return; // cursor untouched → retried next pass
        }
        let new_cursor = sessions.iter().map(|s| s.end_us).max().unwrap();
        state.cursors.insert(key.clone(), new_cursor);
        state.seg_mtimes.insert(key, mtime_ms);
        stats.new_plays += rows.len() as u64;
    }

    /// Append Now Playing sessions to their day's JSONL log (keyed by local
    /// start day, flat across devices). Crate-visible for the media tests.
    pub(crate) fn append_nowplaying_plays(&self, plays: &[NowPlayingPlay]) -> Result<()> {
        self.stream("media/nowplaying", crate::store::Partition::Day).append(plays, |p| &p.start)
    }

    /// All Now Playing sessions for one local day, in start order — the
    /// unified media stream's `iphone-nowplaying` arm reads this.
    pub fn nowplaying_timeline(&self, date: &str) -> Result<Vec<NowPlayingPlay>> {
        let path = self.resolve(&format!("media/nowplaying/{date}.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)
            .with_context(|| format!("reading media/nowplaying/{date}.jsonl"))?;
        let mut rows: Vec<NowPlayingPlay> = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<NowPlayingPlay>(l).ok())
            .collect();
        rows.sort_by(|a, b| a.start.cmp(&b.start));
        Ok(rows)
    }

    /// Append sessions to their device's per-day JSONL logs (keyed by local
    /// start day). Single writer (the owner loop), so no flock needed.
    fn append_screen_time_sessions(
        &self,
        label: &str,
        sessions: &[ScreenTimeSession],
    ) -> Result<()> {
        self.stream(&format!("screen-time/{label}"), crate::store::Partition::Day)
            .append(sessions, |s| &s.start)
    }

    // -----------------------------------------------------------------------
    // Reads

    /// The device catalog (UUID → kind/label/last-seen). Rebuilt as devices
    /// appear; empty before the first sync.
    pub fn screen_time_devices(&self) -> BTreeMap<String, DeviceInfo> {
        let Ok(path) = self.resolve(DEVICES_FILE) else {
            return BTreeMap::new();
        };
        fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn write_screen_time_devices(&self, devices: &BTreeMap<String, DeviceInfo>) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(DEVICES_FILE)?, devices)
    }

    /// All sessions for one local day, sorted by start. `device` narrows to
    /// one device UUID (or "this-mac"); `include_idle: false` drops idle-ish
    /// rows ([`is_idle_bundle`]). Where the live activity watcher also
    /// covered a `this-mac` session, the watcher wins: the Biome backup row
    /// is dropped here (the raw file keeps it) so enabling the backup never
    /// double-counts Mac time.
    pub fn screen_time_timeline(
        &self,
        date: &str,
        device: Option<&str>,
        include_idle: bool,
    ) -> Result<Vec<ScreenTimeSession>> {
        let dir = self.root().join("screen-time");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut rows: Vec<ScreenTimeSession> = Vec::new();
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let path = entry.path().join(format!("{date}.jsonl"));
            if !path.exists() {
                continue;
            }
            let body = fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            rows.extend(
                body.lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str::<ScreenTimeSession>(l).ok()),
            );
        }
        if let Some(d) = device {
            rows.retain(|r| r.device == d);
        }
        if rows.iter().any(|r| r.device_kind == "this-mac") {
            let activity = self.activity_timeline(date).unwrap_or_default();
            rows = resolve_this_mac_overlap(rows, &activity);
        }
        if !include_idle {
            rows.retain(|r| !is_idle_bundle(&r.bundle_id));
        }
        rows.sort_by(|a, b| a.start.cmp(&b.start));
        Ok(rows)
    }

    /// Per-app and per-device time over an inclusive date range, with the
    /// same `device`/`include_idle` narrowing as the timeline.
    pub fn screen_time_summary(
        &self,
        from: &str,
        to: &str,
        device: Option<&str>,
        include_idle: bool,
    ) -> Result<ScreenTimeSummary> {
        let devices_catalog = self.screen_time_devices();
        let mut apps: HashMap<(String, String), u64> = HashMap::new();
        let mut devs: HashMap<String, (String, u64)> = HashMap::new();
        let mut total = 0u64;
        for date in days(from, to)? {
            for s in self.screen_time_timeline(&date, device, include_idle)? {
                total += s.seconds;
                *apps.entry((s.app, s.bundle_id)).or_default() += s.seconds;
                let d = devs.entry(s.device).or_insert((s.device_kind, 0));
                d.1 += s.seconds;
            }
        }
        let mut apps: Vec<ScreenTimeAppUsage> = apps
            .into_iter()
            .map(|((app, bundle_id), seconds)| ScreenTimeAppUsage {
                app,
                bundle_id,
                seconds,
            })
            .collect();
        apps.sort_by(|a, b| b.seconds.cmp(&a.seconds).then_with(|| a.app.cmp(&b.app)));
        let mut devices: Vec<ScreenTimeDeviceUsage> = devs
            .into_iter()
            .map(|(device, (kind, seconds))| {
                let label = devices_catalog
                    .get(&device)
                    .map(|i| i.label.clone())
                    .unwrap_or_else(|| kind.clone());
                ScreenTimeDeviceUsage {
                    device,
                    kind,
                    label,
                    seconds,
                }
            })
            .collect();
        devices.sort_by(|a, b| b.seconds.cmp(&a.seconds).then_with(|| a.label.cmp(&b.label)));
        Ok(ScreenTimeSummary {
            total_seconds: total,
            apps,
            devices,
        })
    }

    /// Screen-time hours per day over an inclusive range — a trend series
    /// for the chart, with the same `device`/`include_idle` narrowing as the
    /// timeline. Days with no data are omitted.
    pub fn screen_time_daily(
        &self,
        from: &str,
        to: &str,
        device: Option<&str>,
        include_idle: bool,
    ) -> Result<Vec<SeriesPoint>> {
        let mut out = Vec::new();
        for date in days(from, to)? {
            let secs: u64 = self
                .screen_time_timeline(&date, device, include_idle)?
                .iter()
                .map(|s| s.seconds)
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

    // -----------------------------------------------------------------------
    // Sync state

    /// The persisted sync state, if a sync has ever run.
    pub fn read_screen_time_sync(&self) -> Option<ScreenTimeSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_screen_time_sync(&self, state: &ScreenTimeSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Reconstruct cursors from the JSONL logs — a lost sync file never
    /// duplicates sessions. (The mtime cache is not rebuilt: re-parsing once
    /// is the cost of losing it.)
    fn rebuild_screen_time_sync(&self) -> ScreenTimeSyncState {
        let mut cursors: BTreeMap<String, i64> = BTreeMap::new();
        let dir = self.root().join("screen-time");
        let Ok(entries) = fs::read_dir(&dir) else {
            return ScreenTimeSyncState::default();
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(entry.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(body) = fs::read_to_string(&path) else {
                    continue;
                };
                for line in body.lines() {
                    let Ok(s) = serde_json::from_str::<ScreenTimeSession>(line) else {
                        continue;
                    };
                    let Some(end_us) = rfc3339_to_apple_us(&s.end) else {
                        continue;
                    };
                    let cur = cursors.entry(s.device).or_insert(i64::MIN);
                    *cur = (*cur).max(end_us);
                }
            }
        }
        // Now Playing cursors rebuild from their own store, namespaced so
        // the two arms can never collide on a device UUID.
        if let Ok(files) = fs::read_dir(self.root().join("media/nowplaying")) {
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(body) = fs::read_to_string(&path) else {
                    continue;
                };
                for line in body.lines() {
                    let Ok(p) = serde_json::from_str::<NowPlayingPlay>(line) else {
                        continue;
                    };
                    let Some(end_us) = rfc3339_to_apple_us(&p.end) else {
                        continue;
                    };
                    let cur = cursors.entry(format!("np/{}", p.device)).or_insert(i64::MIN);
                    *cur = (*cur).max(end_us);
                }
            }
        }
        ScreenTimeSyncState {
            updated: String::new(),
            cursors,
            seg_mtimes: BTreeMap::new(),
        }
    }
}

/// Drop `this-mac` Biome sessions that the live activity watcher already
/// covered: any overlap (±slack) with a non-AFK activity event means the
/// watcher was running and its richer record wins. Other devices are never
/// touched — simultaneous usage of two machines is real, not a duplicate.
fn resolve_this_mac_overlap(
    rows: Vec<ScreenTimeSession>,
    activity: &[crate::activity::ActivityEvent],
) -> Vec<ScreenTimeSession> {
    let spans: Vec<(i64, i64)> = activity
        .iter()
        .filter(|e| !e.afk)
        .filter_map(|e| {
            let s = DateTime::parse_from_rfc3339(&e.start).ok()?.timestamp();
            let t = DateTime::parse_from_rfc3339(&e.end).ok()?.timestamp();
            Some((s - OVERLAP_SLACK_SECS, t + OVERLAP_SLACK_SECS))
        })
        .collect();
    if spans.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|r| {
            if r.device_kind != "this-mac" {
                return true;
            }
            let (Ok(start), Ok(end)) = (
                DateTime::parse_from_rfc3339(&r.start),
                DateTime::parse_from_rfc3339(&r.end),
            ) else {
                return true;
            };
            let (start, end) = (start.timestamp(), end.timestamp());
            !spans.iter().any(|(a, b)| start <= *b && end >= *a)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::ActivityEvent;
    use chrono::TimeZone;

    const INFOCUS_ONLY: BiomeArms = BiomeArms {
        infocus: true,
        this_mac: false,
        nowplaying: false,
    };
    const WITH_THIS_MAC: BiomeArms = BiomeArms {
        infocus: true,
        this_mac: true,
        nowplaying: false,
    };
    const ALL_ARMS: BiomeArms = BiomeArms {
        infocus: true,
        this_mac: false,
        nowplaying: true,
    };

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-screentime-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Encode an App.InFocus protobuf payload the way iOS writes it.
    fn infocus_payload(ts: f64, starting: bool, bundle: &str) -> Vec<u8> {
        let mut out = vec![0x10, 0x01]; // field 2 = 1, as in real data
        out.push(0x18); // field 3 varint
        out.push(if starting { 1 } else { 0 });
        out.push(0x21); // field 4 fixed64
        out.extend_from_slice(&ts.to_le_bytes());
        out.push(0x32); // field 6 bytes
        out.push(bundle.len() as u8);
        out.extend_from_slice(bundle.as_bytes());
        out.extend_from_slice(&[0x4a, 0x03, b'1', b'.', b'0']); // field 9 version
        out.extend_from_slice(&[0x68, 0x01]); // field 13 varint
        out
    }

    /// Apple-epoch seconds for a fixed local datetime (timezone-stable tests).
    fn apple_s(d: u32, h: u32, m: u32, s: u32) -> f64 {
        let t = Local.with_ymd_and_hms(2026, 6, d, h, m, s).unwrap();
        (t.timestamp() - APPLE_EPOCH_OFFSET_S) as f64
    }

    fn ev(ts: f64, starting: bool, bundle: &str) -> InFocusEvent {
        InFocusEvent {
            ts_us: (ts * 1e6).round() as i64,
            starting,
            bundle_id: bundle.into(),
        }
    }

    /// Byte-parity contract for the four write paths (session append,
    /// nowplaying append, devices catalog, sync state): exact bytes, pinned
    /// before the port onto `store` and unchanged by it.
    #[test]
    fn writes_are_byte_identical() {
        let v = temp_vault("parity");
        let session = ScreenTimeSession {
            start: "2026-06-10T09:00:00-07:00".into(),
            end: "2026-06-10T09:10:00-07:00".into(),
            seconds: 600,
            app: "Safari".into(),
            bundle_id: "com.apple.mobilesafari".into(),
            device: "58A03EAD".into(),
            device_kind: "iphone".into(),
            source: "biome-infocus".into(),
        };
        v.append_screen_time_sessions("iphone-58a03ead", &[session.clone()]).unwrap();
        v.append_screen_time_sessions("iphone-58a03ead", &[session]).unwrap();
        let session_line = "{\"start\":\"2026-06-10T09:00:00-07:00\",\"end\":\"2026-06-10T09:10:00-07:00\",\"seconds\":600,\"app\":\"Safari\",\"bundle_id\":\"com.apple.mobilesafari\",\"device\":\"58A03EAD\",\"device_kind\":\"iphone\",\"source\":\"biome-infocus\"}\n";
        assert_eq!(
            fs::read_to_string(v.root().join("screen-time/iphone-58a03ead/2026-06-10.jsonl"))
                .unwrap(),
            format!("{session_line}{session_line}"),
            "per-device day append extends across calls"
        );

        v.append_nowplaying_plays(&[NowPlayingPlay {
            start: "2026-06-10T09:00:00-07:00".into(),
            end: "2026-06-10T09:05:00-07:00".into(),
            seconds: 300,
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            app: "com.apple.Music".into(),
            duration_secs: 200,
            device: "58A03EAD".into(),
            device_kind: "iphone".into(),
            source: "iphone-nowplaying".into(),
        }])
        .unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join("media/nowplaying/2026-06-10.jsonl")).unwrap(),
            "{\"start\":\"2026-06-10T09:00:00-07:00\",\"end\":\"2026-06-10T09:05:00-07:00\",\"seconds\":300,\"title\":\"Song\",\"artist\":\"Artist\",\"album\":\"Album\",\"app\":\"com.apple.Music\",\"duration_secs\":200,\"device\":\"58A03EAD\",\"device_kind\":\"iphone\",\"source\":\"iphone-nowplaying\"}\n"
        );

        let mut devices = BTreeMap::new();
        devices.insert(
            "58A03EAD".to_string(),
            DeviceInfo {
                kind: "iphone".into(),
                label: "iphone-58a03ead".into(),
                last_seen: "2026-06-10T09:10:00-07:00".into(),
                platform: Some(2),
                model: "25F80".into(),
            },
        );
        v.write_screen_time_devices(&devices).unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join("screen-time/devices.json")).unwrap(),
            "{\n  \"58A03EAD\": {\n    \"kind\": \"iphone\",\n    \"label\": \"iphone-58a03ead\",\n    \"last_seen\": \"2026-06-10T09:10:00-07:00\",\n    \"platform\": 2,\n    \"model\": \"25F80\"\n  }\n}"
        );

        let state = ScreenTimeSyncState {
            updated: "2026-06-10T09:10:00-07:00".into(),
            cursors: BTreeMap::from([("infocus/58A03EAD".to_string(), 800000000000000i64)]),
            seg_mtimes: BTreeMap::from([("infocus/58A03EAD".to_string(), 1700000000000i64)]),
        };
        v.write_screen_time_sync(&state).unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join(".trove/screen-time-sync.json")).unwrap(),
            "{\n  \"updated\": \"2026-06-10T09:10:00-07:00\",\n  \"cursors\": {\n    \"infocus/58A03EAD\": 800000000000000\n  },\n  \"seg_mtimes\": {\n    \"infocus/58A03EAD\": 1700000000000\n  }\n}"
        );
    }

    /// Write a synthetic device segment under `root/remote/<uuid>/`.
    fn write_segment(root: &Path, arm: &str, device: &str, events: &[(f64, bool, &str)]) {
        let dir = if device.is_empty() {
            root.join(arm)
        } else {
            root.join(arm).join(device)
        };
        fs::create_dir_all(dir.join("tombstone")).unwrap();
        let payloads: Vec<Vec<u8>> = events
            .iter()
            .map(|(ts, starting, bundle)| infocus_payload(*ts, *starting, bundle))
            .collect();
        let records: Vec<(i32, &[u8])> = payloads.iter().map(|p| (1, p.as_slice())).collect();
        // Numeric filename like the real µs-since-2001 segment names.
        fs::write(dir.join("800000000000000"), segb::build_segment(&records)).unwrap();
    }

    #[test]
    fn decodes_infocus_payloads() {
        let p = infocus_payload(800_000_000.5, true, "com.reddit.Reddit");
        let e = decode_infocus(&p).unwrap();
        assert!(e.starting);
        assert_eq!(e.bundle_id, "com.reddit.Reddit");
        assert_eq!(e.ts_us, 800_000_000_500_000);
        // Garbage, missing fields, absurd timestamps → None, not panic.
        assert!(decode_infocus(b"\xff\xff\xff").is_none());
        assert!(decode_infocus(&[0x18, 0x01]).is_none());
        assert!(decode_infocus(&infocus_payload(-5.0, true, "a.b")).is_none());
        assert!(decode_infocus(&infocus_payload(1e12, true, "a.b")).is_none());
    }

    #[test]
    fn fixture_decodes_all_live_records() {
        // The fixture is derived from a real device and is not shipped in the
        // public repository; the test is a no-op without it.
        let Ok(bytes) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/infocus-iphone.segb"
        )) else {
            eprintln!("skipping: tests/fixtures/infocus-iphone.segb not present");
            return;
        };
        let bytes: &[u8] = &bytes;
        let seg = segb::read_segb(bytes).unwrap();
        let events: Vec<InFocusEvent> =
            seg.entries.iter().filter_map(|e| decode_infocus(&e.data)).collect();
        // Real 13-day iPhone segment: every live record decodes, splitting
        // into near-perfectly paired transitions.
        assert_eq!(events.len(), 5330);
        assert_eq!(events.iter().filter(|e| e.starting).count(), 2666);
        let sessions = pair_sessions(events);
        assert_eq!(sessions.len(), 2665);
        // No session crosses more than a couple of hours (measured max 1.9h)
        // and none is negative — the pairing never invents time.
        assert!(sessions.iter().all(|s| s.end_us > s.start_us));
        assert!(sessions.iter().all(|s| s.end_us - s.start_us < 8 * 3600 * 1_000_000));
    }

    #[test]
    fn pairs_start_end_into_sessions() {
        let sessions = pair_sessions(vec![
            ev(100.0, true, "a.b.app"),
            ev(160.0, false, "a.b.app"),
            ev(160.0, true, "c.d.other"), // same instant: end sorts first
            ev(200.0, false, "c.d.other"),
        ]);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].bundle_id, "a.b.app");
        assert_eq!(sessions[0].end_us - sessions[0].start_us, 60_000_000);
        assert_eq!(sessions[1].bundle_id, "c.d.other");
        assert_eq!(sessions[1].start_us, 160_000_000);
    }

    #[test]
    fn pairing_edge_cases() {
        // Stray end (no open session) is ignored.
        assert!(pair_sessions(vec![ev(50.0, false, "a.b")]).is_empty());
        // A trailing open start is not emitted (its end hasn't synced yet).
        assert!(pair_sessions(vec![ev(50.0, true, "a.b")]).is_empty());
        // Missing end: the next start closes the open session.
        let s = pair_sessions(vec![
            ev(10.0, true, "a.b"),
            ev(30.0, true, "c.d"),
            ev(40.0, false, "c.d"),
        ]);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].bundle_id, "a.b");
        assert_eq!(s[0].end_us, 30_000_000);
        // Zero-length sessions are dropped; unsorted input is sorted.
        let s = pair_sessions(vec![
            ev(20.0, false, "a.b"),
            ev(20.0, true, "a.b"),
            ev(10.0, true, "x.y"),
        ]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].bundle_id, "x.y");
    }

    #[test]
    fn kind_inference() {
        let none = HashSet::new();
        assert_eq!(infer_device_kind(Some(2), &none), "iphone");
        assert_eq!(infer_device_kind(Some(1), &none), "ipad");
        assert_eq!(infer_device_kind(Some(3), &none), "mac");
        assert_eq!(infer_device_kind(Some(4), &none), "mac");
        let watch: HashSet<String> = ["com.apple.NanoMusic".to_string()].into();
        assert_eq!(infer_device_kind(None, &watch), "watch");
        let ios: HashSet<String> = ["com.apple.mobilesafari".to_string()].into();
        assert_eq!(infer_device_kind(Some(7), &ios), "ios");
        let mac: HashSet<String> = ["com.googlecode.iterm2".to_string()].into();
        assert_eq!(infer_device_kind(None, &mac), "mac");
        assert_eq!(infer_device_kind(None, &none), "unknown");
    }

    #[test]
    fn friendly_names() {
        assert_eq!(friendly_app_name("com.apple.mobilesafari"), "Safari");
        assert_eq!(friendly_app_name("com.apple.springboard.stand-by"), "StandBy");
        assert_eq!(friendly_app_name("com.reddit.Reddit"), "Reddit");
        assert_eq!(friendly_app_name("ai.topicfinder.podcastdiscovery"), "Podcastdiscovery");
    }

    #[test]
    fn collects_remote_device_incrementally() {
        let v = temp_vault("collect");
        let root = std::env::temp_dir()
            .join(format!("trove-st-root-{}-collect", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let uuid = "58A03EAD-F34E-4A88-B81A-E940159C82EE";
        write_segment(
            &root,
            "App.InFocus/remote",
            uuid,
            &[
                (apple_s(9, 10, 0, 0), true, "com.apple.mobilesafari"),
                (apple_s(9, 10, 5, 0), false, "com.apple.mobilesafari"),
                (apple_s(9, 10, 5, 0), true, "com.reddit.Reddit"),
                (apple_s(9, 10, 6, 30), false, "com.reddit.Reddit"),
                (apple_s(9, 10, 7, 0), true, "com.apple.MobileSMS"), // dangling
            ],
        );

        let stats = v.collect_screen_time_from(&root, INFOCUS_ONLY, &HashMap::new()).unwrap();
        assert_eq!(stats.devices, 1);
        assert_eq!(stats.new_sessions, 2, "dangling start held for next pass");

        let devices = v.screen_time_devices();
        let info = devices.get(uuid).unwrap();
        assert_eq!(info.kind, "ios"); // no sync.db in tests → signature
        assert_eq!(info.label, "ios-58a03ead");

        let day = v.screen_time_timeline("2026-06-09", None, true).unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].app, "Safari");
        assert_eq!(day[0].seconds, 300);
        assert_eq!(day[1].bundle_id, "com.reddit.Reddit");
        assert_eq!(day[1].device, uuid);

        // Second pass, unchanged file: mtime gate skips everything.
        let stats2 = v.collect_screen_time_from(&root, INFOCUS_ONLY, &HashMap::new()).unwrap();
        assert_eq!(stats2.devices, 0);
        assert_eq!(stats2.new_sessions, 0);

        // The segment grows (the dangling Messages session gets its end,
        // plus one more session). Only the new sessions land — no dupes.
        write_segment(
            &root,
            "App.InFocus/remote",
            uuid,
            &[
                (apple_s(9, 10, 0, 0), true, "com.apple.mobilesafari"),
                (apple_s(9, 10, 5, 0), false, "com.apple.mobilesafari"),
                (apple_s(9, 10, 5, 0), true, "com.reddit.Reddit"),
                (apple_s(9, 10, 6, 30), false, "com.reddit.Reddit"),
                (apple_s(9, 10, 7, 0), true, "com.apple.MobileSMS"),
                (apple_s(9, 10, 9, 0), false, "com.apple.MobileSMS"),
                (apple_s(10, 8, 0, 0), true, "com.apple.camera"),
                (apple_s(10, 8, 1, 0), false, "com.apple.camera"),
            ],
        );
        // Force the mtime gate open (same-second rewrites are invisible to it).
        let mut st = v.read_screen_time_sync().unwrap();
        st.seg_mtimes.clear();
        // (write back through the private writer via a fresh sync pass)
        fs::write(
            v.root().join(SYNC_FILE),
            serde_json::to_vec_pretty(&st).unwrap(),
        )
        .unwrap();

        let stats3 = v.collect_screen_time_from(&root, INFOCUS_ONLY, &HashMap::new()).unwrap();
        assert_eq!(stats3.new_sessions, 2);
        let day1 = v.screen_time_timeline("2026-06-09", None, true).unwrap();
        assert_eq!(day1.len(), 3, "no duplicates, Messages session arrived");
        assert_eq!(day1[2].app, "Messages");
        assert_eq!(day1[2].seconds, 120);
        let day2 = v.screen_time_timeline("2026-06-10", None, true).unwrap();
        assert_eq!(day2.len(), 1);
        assert_eq!(day2[0].app, "Camera");

        // Summary + daily aggregate across both days.
        let summary = v.screen_time_summary("2026-06-09", "2026-06-10", None, true).unwrap();
        assert_eq!(summary.total_seconds, 300 + 90 + 120 + 60);
        assert_eq!(summary.apps[0].app, "Safari");
        assert_eq!(summary.devices.len(), 1);
        assert_eq!(summary.devices[0].label, "ios-58a03ead");
        let daily = v.screen_time_daily("2026-06-08", "2026-06-11", None, true).unwrap();
        assert_eq!(daily.len(), 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cursor_rebuilds_from_jsonl() {
        let v = temp_vault("rebuild");
        let root =
            std::env::temp_dir().join(format!("trove-st-root-{}-rebuild", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let uuid = "AAAA1111-0000-0000-0000-000000000000";
        write_segment(
            &root,
            "App.InFocus/remote",
            uuid,
            &[
                (apple_s(9, 10, 0, 0), true, "a.b.app"),
                (apple_s(9, 10, 5, 0), false, "a.b.app"),
            ],
        );
        v.collect_screen_time_from(&root, INFOCUS_ONLY, &HashMap::new()).unwrap();
        let cursor = *v.read_screen_time_sync().unwrap().cursors.get(uuid).unwrap();

        // Lose the sync file: the rebuilt cursor matches, so a resync
        // appends nothing.
        fs::remove_file(v.root().join(SYNC_FILE)).unwrap();
        let stats = v.collect_screen_time_from(&root, INFOCUS_ONLY, &HashMap::new()).unwrap();
        assert_eq!(stats.new_sessions, 0);
        let rebuilt = v.read_screen_time_sync().unwrap();
        assert_eq!(rebuilt.cursors.get(uuid), Some(&cursor));
        assert_eq!(v.screen_time_timeline("2026-06-09", None, true).unwrap().len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn this_mac_arm_is_opt_in_and_watcher_wins_at_read() {
        let v = temp_vault("thismac");
        let root =
            std::env::temp_dir().join(format!("trove-st-root-{}-thismac", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write_segment(
            &root,
            "App.InFocus/local",
            "",
            &[
                (apple_s(9, 9, 0, 0), true, "com.googlecode.iterm2"),
                (apple_s(9, 9, 30, 0), false, "com.googlecode.iterm2"),
                (apple_s(9, 14, 0, 0), true, "com.apple.Safari"),
                (apple_s(9, 14, 10, 0), false, "com.apple.Safari"),
            ],
        );
        // Remote dir exists but is empty (collect requires remote/ readable).
        fs::create_dir_all(root.join("App.InFocus/remote")).unwrap();

        // Default: local stream untouched.
        let stats = v.collect_screen_time_from(&root, INFOCUS_ONLY, &HashMap::new()).unwrap();
        assert_eq!(stats.new_sessions, 0);
        assert!(v.screen_time_timeline("2026-06-09", None, true).unwrap().is_empty());

        // Opted in: it lands, tagged this-mac.
        let stats = v.collect_screen_time_from(&root, WITH_THIS_MAC, &HashMap::new()).unwrap();
        assert_eq!(stats.new_sessions, 2);
        let day = v.screen_time_timeline("2026-06-09", None, true).unwrap();
        assert_eq!(day.len(), 2);
        assert!(day.iter().all(|s| s.device_kind == "this-mac"));
        assert_eq!(v.screen_time_devices().get("this-mac").unwrap().label, "this-mac");

        // The activity watcher covered 9:00–9:45: its span wins, the
        // overlapping iTerm2 Biome session is dropped at read time; the
        // 14:00 Safari session (watcher off) survives. Raw files keep both.
        let watcher_span = ActivityEvent {
            start: Local.with_ymd_and_hms(2026, 6, 9, 9, 0, 0).unwrap().to_rfc3339(),
            end: Local.with_ymd_and_hms(2026, 6, 9, 9, 45, 0).unwrap().to_rfc3339(),
            seconds: 2700,
            app: "iTerm2".into(),
            bundle_id: String::new(),
            title: String::new(),
            afk: false,
        };
        v.append_activity_events(&[watcher_span]).unwrap();
        let day = v.screen_time_timeline("2026-06-09", None, true).unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].bundle_id, "com.apple.Safari");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_filter_by_device_and_idle() {
        let v = temp_vault("readfilter");
        let root =
            std::env::temp_dir().join(format!("trove-st-root-{}-readfilter", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let phone = "BBBB2222-0000-0000-0000-000000000000";
        let watch = "CCCC3333-0000-0000-0000-000000000000";
        write_segment(
            &root,
            "App.InFocus/remote",
            phone,
            &[
                (apple_s(9, 10, 0, 0), true, "com.apple.mobilesafari"),
                (apple_s(9, 10, 5, 0), false, "com.apple.mobilesafari"),
                (apple_s(9, 22, 0, 0), true, "com.apple.springboard.stand-by"),
                (apple_s(9, 22, 30, 0), false, "com.apple.springboard.stand-by"),
            ],
        );
        write_segment(
            &root,
            "App.InFocus/remote",
            watch,
            &[
                (apple_s(9, 11, 0, 0), true, "com.apple.carousel.clock"),
                (apple_s(9, 11, 10, 0), false, "com.apple.carousel.clock"),
                (apple_s(9, 11, 10, 0), true, "com.apple.NanoMail"),
                (apple_s(9, 11, 12, 0), false, "com.apple.NanoMail"),
            ],
        );
        v.collect_screen_time_from(&root, INFOCUS_ONLY, &HashMap::new()).unwrap();

        // Default view: lock screen / StandBy / watch face rows hidden.
        let day = v.screen_time_timeline("2026-06-09", None, false).unwrap();
        assert_eq!(day.len(), 2);
        assert!(day.iter().all(|s| !is_idle_bundle(&s.bundle_id)));
        // Idle rows are still in the files, shown on request.
        assert_eq!(v.screen_time_timeline("2026-06-09", None, true).unwrap().len(), 4);

        // Device narrowing applies everywhere, composed with the idle filter.
        let day = v.screen_time_timeline("2026-06-09", Some(watch), true).unwrap();
        assert_eq!(day.len(), 2);
        let summary = v
            .screen_time_summary("2026-06-09", "2026-06-09", Some(phone), false)
            .unwrap();
        assert_eq!(summary.total_seconds, 300);
        assert_eq!(summary.devices.len(), 1);
        assert_eq!(summary.apps.len(), 1);
        let daily = v
            .screen_time_daily("2026-06-09", "2026-06-09", Some(watch), false)
            .unwrap();
        assert_eq!(daily.len(), 1);
        assert!((daily[0].value - 120.0 / 3600.0).abs() < 1e-9);

        let _ = fs::remove_dir_all(&root);
    }


    /// Encode a Media.NowPlaying protobuf payload the way iOS writes it.
    fn nowplaying_payload(
        ts: f64,
        state: u64,
        title: &str,
        artist: &str,
        album: &str,
        dur: u64,
        app: &str,
    ) -> Vec<u8> {
        fn varint(out: &mut Vec<u8>, mut v: u64) {
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    break;
                }
                out.push(b | 0x80);
            }
        }
        fn string_field(out: &mut Vec<u8>, field: u8, s: &str) {
            if s.is_empty() {
                return;
            }
            out.push((field << 3) | 2);
            varint(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        let mut out = vec![0x11]; // field 2 fixed64
        out.extend_from_slice(&ts.to_le_bytes());
        out.push(0x18); // field 3 varint
        varint(&mut out, state);
        string_field(&mut out, 4, album);
        string_field(&mut out, 5, artist);
        if dur > 0 {
            out.push(0x30); // field 6 varint
            varint(&mut out, dur);
        }
        string_field(&mut out, 8, title);
        string_field(&mut out, 15, app);
        out
    }

    fn np(ts: f64, state: u64, title: &str) -> NowPlayingEvent {
        decode_nowplaying(&nowplaying_payload(
            ts,
            state,
            title,
            "Artist",
            "Album",
            300,
            "com.apple.Music",
        ))
        .unwrap()
    }

    #[test]
    fn decodes_nowplaying_payloads() {
        let e = np(800_000_100.25, 1, "Heartless");
        assert!(e.playing);
        assert_eq!(e.ts_us, 800_000_100_250_000);
        assert_eq!(e.title, "Heartless");
        assert_eq!(e.artist, "Artist");
        assert_eq!(e.album, "Album");
        assert_eq!(e.app, "com.apple.Music");
        assert_eq!(e.duration_secs, 300);
        // Bare state-change records (no metadata) still decode.
        let bare = decode_nowplaying(&nowplaying_payload(800_000_200.0, 2, "", "", "", 0, ""))
            .unwrap();
        assert!(!bare.playing);
        assert!(bare.title.is_empty());
        // Garbage and missing required fields do not.
        assert!(decode_nowplaying(b"\xff\xff").is_none());
        assert!(decode_nowplaying(&[0x18, 0x01]).is_none()); // state, no time
    }

    #[test]
    fn nowplaying_pairing() {
        // play → progress refresh (same track) → pause: one session.
        let s = pair_nowplaying(vec![
            np(100.0, 1, "Song A"),
            np(160.0, 1, "Song A"),
            np(220.0, 2, "Song A"),
        ]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].title, "Song A");
        assert_eq!(s[0].end_us - s[0].start_us, 120_000_000);

        // Track change closes the first and opens the second; a bare stop
        // (no title) closes the second.
        let bare_stop = decode_nowplaying(&nowplaying_payload(400.0, 3, "", "", "", 0, ""))
            .unwrap();
        let s = pair_nowplaying(vec![np(100.0, 1, "A"), np(250.0, 1, "B"), bare_stop]);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].title.as_str(), s[1].title.as_str()), ("A", "B"));
        assert_eq!(s[1].end_us, 400_000_000);

        // Sub-floor sessions are dropped; trailing open play is held.
        let s = pair_nowplaying(vec![np(100.0, 1, "A"), np(102.0, 2, "A"), np(110.0, 1, "B")]);
        assert!(s.is_empty(), "2s blip dropped, B still open");
    }

    #[test]
    fn nowplaying_audiobook_survives_chapter_churn() {
        // Audible's real iPhone shape: one book (album "Lying"), chapter
        // titles interleaved with stale per-chapter stops. The old pairing let
        // a stale "End Credits" stop slam the "Lying" session shut at zero
        // length, so the whole listen vanished. Each chapter must now survive
        // as its own session — nothing dropped — and the long one billed full.
        let book = |ts: f64, state: u64, chapter: &str| {
            decode_nowplaying(&nowplaying_payload(
                ts, state, chapter, "Sam Harris", "Lying", 4537, "com.audible.iphone",
            ))
            .unwrap()
        };
        let s = pair_nowplaying(vec![
            book(1000.0, 1, "End Credits"),
            book(1006.0, 1, "Opening Credits"),
            book(1006.5, 0, "End Credits"),  // stale stop of a past chapter
            book(1016.0, 1, "Lying"),
            book(1016.5, 0, "Opening Credits"), // stale stop of a past chapter
            book(1216.0, 0, "Lying"),        // the real stop of the current item
        ]);
        assert_eq!(s.len(), 3, "each chapter a session; none zeroed out");
        let lying = s.iter().find(|x| x.title == "Lying").expect("the long listen survives");
        assert_eq!(lying.end_us - lying.start_us, 200_000_000, "1016→1216 billed");
        assert!(s.iter().all(|x| x.album == "Lying" && x.app == "com.audible.iphone"));
        assert_eq!(
            crate::media::classify_nowplaying(&lying.app, &lying.artist, &lying.album, 4537),
            "audiobook",
        );
    }

    #[test]
    fn nowplaying_same_album_tracks_still_split() {
        // Two songs on the same album, a real (minutes-apart) track change —
        // the album-churn rule must NOT merge these; they stay two plays.
        let track = |ts: f64, state: u64, title: &str| {
            decode_nowplaying(&nowplaying_payload(
                ts, state, title, "Artist", "Greatest Hits", 200, "com.apple.Music",
            ))
            .unwrap()
        };
        let s = pair_nowplaying(vec![
            track(100.0, 1, "Song One"),
            track(280.0, 1, "Song Two"), // 180s later: a track change, not churn
            track(460.0, 2, "Song Two"),
        ]);
        assert_eq!(s.len(), 2, "real track changes split despite a shared album");
        assert_eq!((s[0].title.as_str(), s[1].title.as_str()), ("Song One", "Song Two"));
    }

    #[test]
    fn collects_nowplaying_for_ios_devices_only() {
        let v = temp_vault("nowplaying");
        let root = std::env::temp_dir()
            .join(format!("trove-st-root-{}-nowplaying", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let iphone = "AAAA0001-0000-0000-0000-000000000000";
        let watch = "BBBB0002-0000-0000-0000-000000000000";

        // InFocus stream so the device catalog learns kinds (no sync.db in
        // tests → signature heuristic: mobile* → ios, Nano* → watch).
        write_segment(
            &root,
            "App.InFocus/remote",
            iphone,
            &[
                (apple_s(9, 8, 0, 0), true, "com.apple.mobilesafari"),
                (apple_s(9, 8, 1, 0), false, "com.apple.mobilesafari"),
            ],
        );
        write_segment(
            &root,
            "App.InFocus/remote",
            watch,
            &[
                (apple_s(9, 8, 0, 0), true, "com.apple.NanoMusic"),
                (apple_s(9, 8, 1, 0), false, "com.apple.NanoMusic"),
            ],
        );
        // Both devices have NowPlaying streams (the watch mirrors the
        // phone) — only the iPhone's may land.
        for (uuid, title) in [(iphone, "Hard Fork"), (watch, "Hard Fork")] {
            let payloads = vec![
                nowplaying_payload(
                    apple_s(9, 9, 0, 0),
                    1,
                    title,
                    "Hard Fork",
                    "Hard Fork",
                    3681,
                    "ai.topicfinder.podcastdiscovery",
                ),
                nowplaying_payload(apple_s(9, 9, 30, 0), 2, "", "", "", 0, ""),
            ];
            let records: Vec<(i32, &[u8])> =
                payloads.iter().map(|p| (1, p.as_slice())).collect();
            let dir = root.join("Media.NowPlaying/remote").join(uuid);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("800000000000000"), segb::build_segment(&records)).unwrap();
        }

        let stats = v.collect_screen_time_from(&root, ALL_ARMS, &HashMap::new()).unwrap();
        assert_eq!(stats.new_plays, 1, "watch mirror excluded");
        let day = v.nowplaying_timeline("2026-06-09").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].title, "Hard Fork");
        assert_eq!(day[0].device, iphone);
        assert_eq!(day[0].device_kind, "ios");
        assert_eq!(day[0].seconds, 1800);
        assert_eq!(day[0].source, NOWPLAYING_SOURCE);

        // Incremental: unchanged → no-op; lost sync file → rebuilt cursor,
        // still no dupes.
        let again = v.collect_screen_time_from(&root, ALL_ARMS, &HashMap::new()).unwrap();
        assert_eq!(again.new_plays, 0);
        fs::remove_file(v.root().join(SYNC_FILE)).unwrap();
        let rebuilt = v.collect_screen_time_from(&root, ALL_ARMS, &HashMap::new()).unwrap();
        assert_eq!(rebuilt.new_plays, 0);
        assert_eq!(v.nowplaying_timeline("2026-06-09").unwrap().len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apple_epoch_round_trip() {
        let us = 800_000_000_123_456i64;
        let t = apple_us_to_local(us).unwrap();
        assert_eq!(rfc3339_to_apple_us(&t.to_rfc3339()), Some(us));
        assert!(apple_us_to_local(-(APPLE_EPOCH_OFFSET_S * 1_000_000) - 1).is_none());
    }
}

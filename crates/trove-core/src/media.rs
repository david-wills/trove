//! Unified media stream — every "play" across sources, merged at read time.
//!
//! Files-first: each source keeps its own vault files and write path; this
//! module is a pure read-time view over them, so it needs no storage, no
//! migration, and new sources (Spotify, Apple TV, …) join by adding one
//! mapping arm. Current arms:
//!
//! - **music** — Apple Music scrobbles from `music/plays/` (see
//!   [`crate::music`]): full plays and skips, with real per-play seconds.
//! - **podcasts** — `played`/`progress` diff events from `podcasts/events/`
//!   (see [`crate::podcasts`]). `added`/`removed` are library churn, not
//!   listening, and are excluded. Events are *observed* at snapshot time,
//!   so an item is timed by the episode's `last_played` when Podcasts
//!   recorded one (local plays) and falls back to the snapshot `ts` (iCloud
//!   plays arrive dateless); the events read extends a few days past the
//!   range so a listen observed at a later snapshot still lands on its day.
//!   Seconds are the event's playhead-delta estimate (0 = unknown).
//! - **web** — browser-extension spans from `browser/` that played audio
//!   (`audible`, see [`crate::browser_ext`]): YouTube, web players, anything
//!   the browser heard. Spans shorter than [`WEB_MIN_SECS`] are dropped —
//!   autoplay blips on news sites are noise, not media. History rows never
//!   qualify (only the extension observes audio).
//!
//! - **iphone-nowplaying** — wall-clock playback sessions from the
//!   iPhone/iPad Now Playing Biome stream, `media/nowplaying/` (see
//!   [`crate::screen_time`]): real "what played on the phone" with real
//!   timestamps. Kind follows the scrobbler's full-play judgment (≥50% of
//!   the reported duration or ≥4 min; unknown duration → ≥30s).
//! - **the write contract** — `media/plays/<source>/YYYY-MM.jsonl`,
//!   normalized [`MediaItem`] lines written by any collector in any
//!   language (Letterboxd, Trakt, Spotify exports, …). Source folders are
//!   discovered by scanning; nothing here needs editing to add one. Spec:
//!   `docs/vault-spec/domains/media-plays.md`.
//!
//! Apple Music *library* events ([`crate::music_library`]) are deliberately
//! not merged: a play bumps `play_count` there too, which would double-count
//! every scrobble. The library stream stays a verification/backfill source.
//! iPhone *podcast* listens can arrive twice — as a Now Playing session and
//! as a Podcasts snapshot-diff event — so where both name the same episode
//! on the same day, the Now Playing session wins and the podcast event is
//! dropped (richer: real start time and wall-clock seconds vs. an estimate
//! observed at the next snapshot).

use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::activity::days;
use crate::browser::domain_of;
use crate::correspondence::months;
use crate::health::SeriesPoint;
use crate::vault::Vault;

/// Audible web spans shorter than this are autoplay noise, not media.
pub const WEB_MIN_SECS: u64 = 30;

/// How many days past `to` to read podcast events: the snapshot observation
/// (the event `ts`, which the events read filters on) trails the listen's
/// `last_played` by the snapshot cadence plus a sync delay.
const PODCAST_OBSERVATION_SLACK_DAYS: i64 = 7;

/// One media play in the unified stream, normalized across sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct MediaItem {
    /// RFC3339 local time the play happened (started, for spans).
    pub ts: String,
    /// Provenance: which collector produced this — "music", "podcasts",
    /// "web", or "iphone-nowplaying" (later: "spotify", …).
    pub source: String,
    /// Content type: "music", "podcast", "audiobook", "video", or "other".
    /// Deterministic for the dedicated collectors; classified from the app +
    /// metadata for Now Playing (see [`classify_nowplaying`]). This is the
    /// axis the Media UI filters on.
    pub category: String,
    /// Device label the play came from — "iPhone"/"iPad" (per-device, e.g.
    /// "iPad (2)") for Now Playing sessions, "Mac" for the local scrobbler and
    /// browser extension, "" when unknown (the Podcasts snapshot is
    /// iCloud-synced and carries no device). The Media UI's second filter axis
    /// and the value is the filter key itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device: String,
    /// "play" (a real listen/watch) or "partial" (music skip, podcast
    /// progress without completion).
    pub kind: String,
    /// Track / episode / page title.
    pub title: String,
    /// Artist / show / domain — the grouping key for top charts.
    pub subtitle: String,
    /// Album / "" / full URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Seconds actually played — measured for music and web, a
    /// playhead-delta estimate for podcasts; 0 when unknown.
    pub seconds: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub favicon: String,
    /// Source-unique id for re-runnable imports (the media-plays write
    /// contract); empty for the read-time arms.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guid: String,
    /// Source-specific fields the normalized shape has no column for —
    /// full fidelity at write time, never dropped.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One artist/show/site's listening over a range.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct MediaUsage {
    /// The [`MediaItem::subtitle`] grouping key.
    pub name: String,
    /// Content type this row belongs to (the filter axis): "music",
    /// "podcast", "audiobook", "video", or "other".
    pub category: String,
    /// Device label this row's plays came from ("iPhone"/"iPad"/"Mac"/""), so
    /// the top chart filters by device too.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device: String,
    /// Full plays only (partials excluded, same rule as the music charts).
    pub plays: u64,
    /// Seconds played, partials included (podcast seconds are estimates).
    pub seconds: u64,
}

/// Headline numbers for the unified stream over a date range.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct MediaSummary {
    /// Full plays across all sources.
    pub plays: u64,
    /// Partial plays (music skips, podcast progress).
    pub partials: u64,
    /// Seconds played: measured for music + web, estimated for podcasts.
    pub seconds: u64,
    /// Items (plays + partials) per source.
    pub sources: BTreeMap<String, u64>,
    /// Items (plays + partials) per device kind ("iphone"/"ipad"/"mac"),
    /// the unknown-device "" bucket omitted — the device filter's chips.
    pub devices: BTreeMap<String, u64>,
    /// Artists/shows/sites by plays then seconds, descending.
    pub top: Vec<MediaUsage>,
}

impl Vault {
    /// The unified stream for one local day, chronological.
    pub fn media_timeline(&self, date: &str) -> Result<Vec<MediaItem>> {
        self.media_items(date, date)
    }

    /// Headline numbers + top artists/shows/sites over an inclusive range.
    pub fn media_summary(&self, from: &str, to: &str) -> Result<MediaSummary> {
        let items = self.media_items(from, to)?;
        let (mut plays, mut partials, mut seconds) = (0u64, 0u64, 0u64);
        let mut sources: BTreeMap<String, u64> = BTreeMap::new();
        let mut devices: BTreeMap<String, u64> = BTreeMap::new();
        let mut top: BTreeMap<(String, String, String), MediaUsage> = BTreeMap::new();
        for it in &items {
            seconds += it.seconds;
            // `sources` is a provenance breakdown (per collector); `devices`
            // feeds the device filter's chips (unknown bucket omitted). The
            // top chart groups by content type *and* device so each filter
            // axis can narrow it.
            *sources.entry(it.source.clone()).or_default() += 1;
            if !it.device.is_empty() {
                *devices.entry(it.device.clone()).or_default() += 1;
            }
            let u = top
                .entry((it.category.clone(), it.subtitle.clone(), it.device.clone()))
                .or_insert_with(|| MediaUsage {
                    name: it.subtitle.clone(),
                    category: it.category.clone(),
                    device: it.device.clone(),
                    plays: 0,
                    seconds: 0,
                });
            u.seconds += it.seconds;
            if it.kind == "play" {
                plays += 1;
                u.plays += 1;
            } else {
                partials += 1;
            }
        }
        let mut top: Vec<MediaUsage> = top.into_values().collect();
        top.sort_by(|a, b| {
            b.plays
                .cmp(&a.plays)
                .then(b.seconds.cmp(&a.seconds))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(MediaSummary {
            plays,
            partials,
            seconds,
            sources,
            devices,
            top,
        })
    }

    /// Full plays per day over an inclusive range — the trend series.
    /// Days with no plays are omitted (chart convention).
    pub fn media_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut per_day: BTreeMap<String, u64> = BTreeMap::new();
        for it in self.media_items(from, to)? {
            if it.kind == "play" {
                *per_day.entry(it.ts[..10].to_string()).or_default() += 1;
            }
        }
        Ok(per_day
            .into_iter()
            .map(|(date, n)| SeriesPoint {
                date,
                value: n as f64,
            })
            .collect())
    }

    /// All sources merged over an inclusive range, sorted by time.
    fn media_items(&self, from: &str, to: &str) -> Result<Vec<MediaItem>> {
        let mut out = Vec::new();
        // (day, lowercased title) of every Now Playing session — the dedupe
        // key that lets phone sessions outrank podcast snapshot events.
        let mut nowplaying_titles: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        // Resolve Now Playing device UUIDs to per-device labels once.
        let dev_labels = device_labels(&self.screen_time_devices());
        for date in days(from, to)? {
            for p in self.music_timeline(&date)? {
                out.push(MediaItem {
                    ts: p.start,
                    source: "music".into(),
                    category: "music".into(),
                    device: "Mac".into(),
                    kind: if p.full_play { "play" } else { "partial" }.into(),
                    title: p.track,
                    subtitle: p.artist,
                    detail: p.album,
                    seconds: p.seconds_played,
                    favicon: String::new(),
                    guid: String::new(),
                    extra: Map::new(),
                });
            }
            for v in self.browser_timeline(&date)? {
                if !v.audible || v.duration_secs < WEB_MIN_SECS {
                    continue;
                }
                let title = if v.title.is_empty() { v.url.clone() } else { v.title };
                out.push(MediaItem {
                    ts: v.time,
                    source: "web".into(),
                    category: "video".into(),
                    device: "Mac".into(),
                    kind: "play".into(),
                    title,
                    subtitle: domain_of(&v.url),
                    detail: v.url,
                    seconds: v.duration_secs,
                    favicon: v.favicon,
                    guid: String::new(),
                    extra: Map::new(),
                });
            }
            for np in self.nowplaying_timeline(&date)? {
                // The scrobbler's full-play judgment, on wall-clock seconds.
                let full = if np.duration_secs > 0 {
                    np.seconds * 2 >= np.duration_secs || np.seconds >= 240
                } else {
                    np.seconds >= 30
                };
                nowplaying_titles.insert((date.clone(), np.title.to_lowercase()));
                let category =
                    classify_nowplaying(&np.app, &np.artist, &np.album, np.duration_secs);
                let device = dev_labels
                    .get(&np.device)
                    .cloned()
                    .unwrap_or_else(|| pretty_kind(&np.device_kind));
                // Audiobooks are labelled by the book (album); the chapter
                // (the raw title) moves to the detail line.
                let (title, detail) = if category == "audiobook" && !np.album.is_empty() {
                    (np.album.clone(), np.title.clone())
                } else {
                    (np.title.clone(), np.album.clone())
                };
                out.push(MediaItem {
                    ts: np.start,
                    source: "iphone-nowplaying".into(),
                    category: category.into(),
                    device,
                    kind: if full { "play" } else { "partial" }.into(),
                    title,
                    subtitle: np.artist,
                    detail,
                    seconds: np.seconds,
                    favicon: String::new(),
                    guid: String::new(),
                    extra: Map::new(),
                });
            }
        }
        out.extend(
            self.podcast_media_items(from, to)?
                .into_iter()
                .filter(|p| {
                    let day = p.ts[..10.min(p.ts.len())].to_string();
                    !nowplaying_titles.contains(&(day, p.title.to_lowercase()))
                }),
        );
        out.extend(self.contract_media_items(from, to)?);
        out.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(out)
    }

    /// The generic arm: the **media-plays write contract**. Any collector —
    /// Rust module, script, or AI agent — joins the unified stream by
    /// writing normalized [`MediaItem`] lines to
    /// `media/plays/<source>/YYYY-MM.jsonl` (see
    /// `docs/vault-spec/domains/media-plays.md`); this scan picks every
    /// source folder up with no registration and no edit to this module.
    fn contract_media_items(&self, from: &str, to: &str) -> Result<Vec<MediaItem>> {
        let mut out = Vec::new();
        let dir = self.root().join("media/plays");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(out);
        };
        let mut sources: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        sources.sort();
        for source in sources {
            let stream =
                self.stream(&format!("media/plays/{source}"), crate::store::Partition::Month);
            for month in months(from, to)? {
                for mut it in stream.read::<MediaItem>(&month)? {
                    let day = &it.ts[..10.min(it.ts.len())];
                    if day < from || day > to {
                        continue;
                    }
                    // The folder names the source; a record may omit it.
                    if it.source.is_empty() {
                        it.source = source.clone();
                    }
                    out.push(it);
                }
            }
        }
        Ok(out)
    }

    /// Podcast listens timed by `last_played ?? ts`, filtered to the range.
    /// The events read is extended past `to` so a listen observed at a later
    /// snapshot still lands on its real day.
    fn podcast_media_items(&self, from: &str, to: &str) -> Result<Vec<MediaItem>> {
        let ext_to = shift_days(to, PODCAST_OBSERVATION_SLACK_DAYS);
        let mut out = Vec::new();
        for e in self.podcasts_events(from, &ext_to)? {
            let kind = match e.kind.as_str() {
                "played" => "play",
                "progress" => "partial",
                _ => continue, // added/removed: library churn, not listening
            };
            let ts = e.last_played.unwrap_or(e.ts);
            let day = &ts[..10.min(ts.len())];
            if day < from || day > to {
                continue;
            }
            out.push(MediaItem {
                ts,
                source: "podcasts".into(),
                category: "podcast".into(),
                device: String::new(), // iCloud-synced snapshot; device unknown
                kind: kind.into(),
                title: e.title,
                subtitle: e.show_title,
                detail: String::new(),
                seconds: e.seconds,
                favicon: String::new(),
                guid: String::new(),
                extra: Map::new(),
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Device identity

/// Human label for a device kind.
fn pretty_kind(kind: &str) -> String {
    match kind {
        "iphone" => "iPhone",
        "ipad" => "iPad",
        "mac" | "this-mac" => "Mac",
        "watch" => "Watch",
        "ios" => "iOS device",
        other => other,
    }
    .to_string()
}

/// Per-device display labels from the Screen Time device catalog, so the
/// Media view can name and filter by *which* device — telling two iPhones or
/// two iPads apart. A kind with one device is just "iPhone"; a second of the
/// same kind gets an ordinal ("iPad (2)"), oldest-last by last-seen. (Apple's
/// catalog carries no hardware model name — only the kind and an OS build —
/// so the label is kind-based, which is the precision actually available.)
fn device_labels(catalog: &BTreeMap<String, crate::screen_time::DeviceInfo>) -> BTreeMap<String, String> {
    let mut by_kind: BTreeMap<&str, Vec<(&String, &str)>> = BTreeMap::new();
    for (uuid, info) in catalog {
        by_kind
            .entry(info.kind.as_str())
            .or_default()
            .push((uuid, info.last_seen.as_str()));
    }
    let mut out = BTreeMap::new();
    for (kind, mut devs) in by_kind {
        // Most-recently-seen first → the active device keeps the bare label.
        devs.sort_by(|a, b| b.1.cmp(a.1));
        for (i, (uuid, _)) in devs.iter().enumerate() {
            let label = if i == 0 {
                pretty_kind(kind)
            } else {
                format!("{} ({})", pretty_kind(kind), i + 1)
            };
            out.insert((*uuid).clone(), label);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Now Playing content classification

/// Apps that play video (some also populate artist/album, so they're matched
/// before the audio families).
const VIDEO_APPS: &[&str] = &[
    "com.google.ios.youtube",
    "com.netflix.netflix",
    "com.wbd.stream", // Max / HBO
    "com.hulu.plus",
    "com.disney.disneyplus",
    "com.apple.tv",
    "com.apple.tvwatchlist",
    "tv.twitch",
    "com.amazon.aiv.avod",
    "com.peacocktv.peacock",
    "com.crunchyroll.iphone",
    "com.vimeo",
];

/// Audiobook apps.
const AUDIOBOOK_APPS: &[&str] = &[
    "com.apple.ibooks",
    "com.apple.books",
    "com.apple.bkagentservice",
    "com.audible.iphone",
    "com.audible.iphoneappdev",
    "com.audible.application",
    "com.overdrive.libby",
    "com.overdrive.overdrive",
    "com.librofm.librofm",
];

/// Podcast apps.
const PODCAST_APPS: &[&str] = &[
    "com.apple.podcasts",
    "ai.topicfinder.podcastdiscovery",
    "fm.overcast.overcast",
    "com.marco.overcast",
    "au.com.shiftyjelly.pocketcasts",
    "fm.castro.castro2",
    "com.spotify.podcasts",
];

/// Dedicated music apps (Spotify is handled separately — it hosts both).
const MUSIC_APPS: &[&str] = &[
    "com.apple.music",
    "com.apple.itunes",
    "com.google.ios.youtubemusic",
    "com.soundcloud.themobileapp",
    "com.pandora",
    "com.aspiro.tidal",
    "com.amazon.mp3",
];

/// Podcasts (and many audiobooks) repeat the show/book name in both `artist`
/// and `album`; music doesn't. The single most reliable metadata tell when
/// the app id is unknown or dual-content.
fn artist_equals_album(artist: &str, album: &str) -> bool {
    !artist.is_empty() && artist.eq_ignore_ascii_case(album)
}

/// Content type of a Now Playing session, from the playing app's bundle id
/// with metadata-shape fallbacks. Returns "music", "podcast", "audiobook",
/// "video", or "other".
///
/// The bundle id is authoritative when known. For unknown or dual-content
/// apps (Spotify hosts both music and podcasts) the metadata decides:
/// podcasts repeat the show name in `artist` and `album`, audiobooks run for
/// hours, songs are minutes. Ordering matters — video apps are matched first
/// because several also populate artist/album, and YouTube Music is rescued
/// before the broad `youtube` video match.
pub fn classify_nowplaying(app: &str, artist: &str, album: &str, duration_secs: u64) -> &'static str {
    let a = app.trim().to_ascii_lowercase();
    let is = |list: &[&str]| list.iter().any(|x| a == *x);

    // YouTube Music publishes under a youtube* id but is music, not video.
    if a.contains("youtubemusic") {
        return "music";
    }
    // Messages / FaceTime: inline clips, not a listening session.
    if a == "com.apple.mobilesms" || a == "com.apple.facetime" {
        return "other";
    }
    // Spotify hosts both music and podcasts — let the metadata tell decide.
    if a == "com.spotify.client" {
        return if artist_equals_album(artist, album) { "podcast" } else { "music" };
    }
    if is(VIDEO_APPS) || a.contains("youtube") || a.contains("netflix") {
        return "video";
    }
    if is(AUDIOBOOK_APPS) || a.contains("audible") || a.contains("audiobook") {
        return "audiobook";
    }
    if is(PODCAST_APPS) || a.contains("podcast") || a.contains("overcast") || a.contains("pocketcast")
    {
        return "podcast";
    }
    if is(MUSIC_APPS) || a.contains("music") || a.contains("spotify") {
        return "music";
    }

    // Unknown app — fall back to metadata shape.
    if artist_equals_album(artist, album) {
        return "podcast";
    }
    if duration_secs >= 2 * 3600 {
        return "audiobook";
    }
    if duration_secs >= 20 * 60 {
        return "podcast";
    }
    "music"
}

/// `date` plus `n` days, or `date` unchanged if it doesn't parse.
fn shift_days(date: &str, n: i64) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| (d + chrono::Duration::days(n)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| date.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::BrowserVisit;
    use crate::music::Play;
    use crate::podcasts::PodcastEpisode;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-media-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn play(day: &str, track: &str, artist: &str, secs: u64, full: bool) -> Play {
        Play {
            start: format!("{day}T09:00:00-07:00"),
            end: format!("{day}T09:05:00-07:00"),
            seconds_played: secs,
            track: track.into(),
            artist: artist.into(),
            album: "Album".into(),
            genre: "Pop".into(),
            duration_secs: Some(200.0),
            persistent_id: String::new(),
            full_play: full,
        }
    }

    fn visit(day: &str, url: &str, title: &str, secs: u64, audible: bool) -> BrowserVisit {
        BrowserVisit {
            time: format!("{day}T20:00:00-07:00"),
            url: url.into(),
            title: title.into(),
            browser: "chrome".into(),
            profile: String::new(),
            duration_secs: secs,
            source: "extension".into(),
            audible,
            foreground_secs: 0,
            favicon: "https://www.youtube.com/favicon.ico".into(),
            referrer: String::new(),
            transition: String::new(),
            tab_count: 0,
        }
    }

    fn episode(uuid: &str, title: &str) -> PodcastEpisode {
        PodcastEpisode {
            uuid: uuid.into(),
            title: title.into(),
            show_title: "The Indicator".into(),
            show_author: "NPR".into(),
            feed_url: String::new(),
            play_state: 1,
            playhead_secs: 100.0,
            duration_secs: 500.0,
            play_count: 0,
            last_played: None,
            pub_date: None,
        }
    }

    /// Baseline + one completion: yields a `played` event stamped `ts`
    /// carrying `last_played`.
    fn seed_podcast_play(v: &Vault, ts: &str, last_played: Option<&str>) {
        let ep = episode("A", "fish sticks");
        v.podcasts_snapshot(&[ep.clone()], "2026-06-01T04:00:00-07:00").unwrap();
        let mut done = ep;
        done.play_count = 1;
        done.last_played = last_played.map(Into::into);
        v.podcasts_snapshot(&[done], ts).unwrap();
    }

    #[test]
    fn merges_sources_in_time_order() {
        let v = temp_vault("merge");
        v.append_music_plays(&[play("2026-06-10", "Take On Me", "a-ha", 200, true)])
            .unwrap();
        v.append_browser_visits(&[visit(
            "2026-06-10",
            "https://www.youtube.com/watch?v=x",
            "Video",
            300,
            true,
        )])
        .unwrap();
        // Listened 06-10 evening, observed by the 06-11 snapshot: the item
        // must land on 06-10 via last_played.
        seed_podcast_play(
            &v,
            "2026-06-11T04:00:00-07:00",
            Some("2026-06-10T21:00:00-07:00"),
        );

        let day = v.media_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3);
        assert_eq!(
            day.iter().map(|i| i.source.as_str()).collect::<Vec<_>>(),
            vec!["music", "web", "podcasts"],
            "chronological: 09:00 music, 20:00 web, 21:00 podcast"
        );
        assert_eq!(day[0].kind, "play");
        assert_eq!(day[1].subtitle, "youtube.com");
        assert_eq!(day[1].detail, "https://www.youtube.com/watch?v=x");
        assert_eq!(day[2].title, "fish sticks");
        assert_eq!(day[2].subtitle, "The Indicator");
        assert_eq!(day[2].seconds, 400, "completion tail estimate flows through");
        // …and the observation day itself has no podcast item.
        assert!(v.media_timeline("2026-06-11").unwrap().is_empty());
    }

    #[test]
    fn dateless_podcast_play_falls_back_to_snapshot_time() {
        let v = temp_vault("dateless");
        seed_podcast_play(&v, "2026-06-11T04:00:00-07:00", None);
        let day = v.media_timeline("2026-06-11").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].ts, "2026-06-11T04:00:00-07:00");
        assert_eq!(day[0].kind, "play");
    }

    #[test]
    fn web_noise_is_filtered() {
        let v = temp_vault("noise");
        v.append_browser_visits(&[
            visit("2026-06-10", "https://news.example/", "Article", 600, false),
            visit("2026-06-10", "https://ads.example/", "Autoplay", 10, true),
            visit("2026-06-10", "https://www.youtube.com/watch?v=x", "Video", 90, true),
        ])
        .unwrap();
        let day = v.media_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 1, "silent + sub-floor spans dropped");
        assert_eq!(day[0].subtitle, "youtube.com");
    }

    #[test]
    fn summary_counts_and_top_across_sources() {
        let v = temp_vault("summary");
        v.append_music_plays(&[
            play("2026-06-09", "One", "Cannons", 200, true),
            play("2026-06-09", "Two", "Cannons", 20, false),
            play("2026-06-10", "Three", "Gorillaz", 300, true),
        ])
        .unwrap();
        v.append_browser_visits(&[visit(
            "2026-06-10",
            "https://www.youtube.com/watch?v=x",
            "Video",
            900,
            true,
        )])
        .unwrap();
        seed_podcast_play(
            &v,
            "2026-06-11T04:00:00-07:00",
            Some("2026-06-10T21:00:00-07:00"),
        );

        let s = v.media_summary("2026-06-09", "2026-06-10").unwrap();
        assert_eq!(s.plays, 4, "2 music full + 1 web + 1 podcast");
        assert_eq!(s.partials, 1, "the music skip");
        assert_eq!(
            s.seconds,
            200 + 20 + 300 + 900 + 400,
            "podcasts contribute their tail estimate (500 - 100)"
        );
        assert_eq!(s.sources.get("music"), Some(&3));
        assert_eq!(s.sources.get("web"), Some(&1));
        assert_eq!(s.sources.get("podcasts"), Some(&1));
        // Ties on plays break by seconds: youtube (900s) over the rest.
        assert_eq!(s.top[0].name, "youtube.com");
        assert_eq!(s.top[0].category, "video", "web audio folds into the video category");
        let cannons = s.top.iter().find(|u| u.name == "Cannons").unwrap();
        assert_eq!(cannons.plays, 1, "skip not a play");
        assert_eq!(cannons.seconds, 220, "skip seconds still count");
    }

    fn np_play(day: &str, hms: &str, title: &str, secs: u64, dur: u64) -> crate::screen_time::NowPlayingPlay {
        crate::screen_time::NowPlayingPlay {
            start: format!("{day}T{hms}-07:00"),
            end: format!("{day}T{hms}-07:00"),
            seconds: secs,
            title: title.into(),
            artist: "Hard Fork".into(),
            album: "Hard Fork".into(),
            app: "ai.topicfinder.podcastdiscovery".into(),
            duration_secs: dur,
            device: "AAAA-1111".into(),
            device_kind: "iphone".into(),
            source: crate::screen_time::NOWPLAYING_SOURCE.into(),
        }
    }

    #[test]
    fn iphone_nowplaying_joins_the_stream_and_outranks_podcast_events() {
        let v = temp_vault("nowplaying");
        // A phone listening session: 2000s of a 3681s episode → full play.
        v.append_nowplaying_plays(&[np_play("2026-06-10", "08:00:00", "fish sticks", 2000, 3681)])
            .unwrap();
        // The same episode also surfaces via the Podcasts snapshot diff a
        // day later (iCloud sync) — it must be dropped, not double-counted.
        seed_podcast_play(
            &v,
            "2026-06-11T04:00:00-07:00",
            Some("2026-06-10T08:05:00-07:00"),
        );

        let day = v.media_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 1, "podcast event deduped against the session");
        assert_eq!(day[0].source, "iphone-nowplaying");
        assert_eq!(day[0].kind, "play");
        assert_eq!(day[0].title, "fish sticks");
        assert_eq!(day[0].subtitle, "Hard Fork");
        assert_eq!(day[0].seconds, 2000);

        // A short session of an unrelated track is a partial, and an
        // unrelated podcast event still comes through.
        let v2 = temp_vault("nowplaying2");
        v2.append_nowplaying_plays(&[np_play("2026-06-10", "09:00:00", "Some Song", 40, 300)])
            .unwrap();
        seed_podcast_play(
            &v2,
            "2026-06-11T04:00:00-07:00",
            Some("2026-06-10T21:00:00-07:00"),
        );
        let day = v2.media_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].source, "iphone-nowplaying");
        assert_eq!(day[0].kind, "partial", "40s of 300s is not a full play");
        assert_eq!(day[1].source, "podcasts");
    }

    #[test]
    fn classifies_nowplaying_content_types() {
        let c = classify_nowplaying;
        // Known apps are authoritative.
        assert_eq!(c("com.apple.Music", "Kendrick Lamar", "untitled - Single", 146), "music");
        assert_eq!(c("ai.topicfinder.podcastdiscovery", "Show", "Show", 5082), "podcast");
        assert_eq!(c("com.apple.podcasts", "", "", 0), "podcast");
        assert_eq!(c("com.audible.iphone", "Author", "Some Book", 36000), "audiobook");
        assert_eq!(c("com.apple.Books", "Author", "Book", 0), "audiobook");
        assert_eq!(c("com.google.ios.youtube", "", "", 0), "video");
        assert_eq!(c("com.wbd.stream", "", "", 0), "video");
        // YouTube Music is rescued from the broad youtube→video match.
        assert_eq!(c("com.google.ios.youtubemusic", "Artist", "Album", 200), "music");
        // Messages inline audio is not a listening session.
        assert_eq!(c("com.apple.MobileSMS", "", "", 0), "other");
        // Spotify hosts both — the artist==album tell decides.
        assert_eq!(c("com.spotify.client", "Drake", "Scorpion", 250), "music");
        assert_eq!(c("com.spotify.client", "The Daily", "The Daily", 1500), "podcast");
        // Unknown app → metadata shape. artist==album ⇒ podcast; very long ⇒
        // audiobook; long-ish ⇒ podcast; short ⇒ music.
        assert_eq!(c("com.unknown.app", "NPR Up First", "NPR Up First", 600), "podcast");
        assert_eq!(c("com.unknown.app", "Author", "Book", 4 * 3600), "audiobook");
        assert_eq!(c("com.unknown.app", "Host", "Episode", 30 * 60), "podcast");
        assert_eq!(c("com.unknown.app", "Artist", "Single", 180), "music");
    }

    #[test]
    fn nowplaying_category_flows_into_timeline() {
        let v = temp_vault("np-category");
        // np_play uses the podcast app id and artist==album ("Hard Fork").
        v.append_nowplaying_plays(&[np_play("2026-06-10", "08:00:00", "Episode 1", 2000, 3681)])
            .unwrap();
        let day = v.media_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].category, "podcast");
        assert_eq!(day[0].source, "iphone-nowplaying", "provenance preserved");
        assert_eq!(day[0].device, "iPhone", "device label flows through");

        // The local scrobbler tags its plays as Mac, and the device shows up
        // in the summary's device breakdown (the filter's chips).
        v.append_music_plays(&[play("2026-06-10", "Song", "Artist", 200, true)])
            .unwrap();
        let s = v.media_summary("2026-06-10", "2026-06-10").unwrap();
        assert_eq!(s.devices.get("iPhone"), Some(&1));
        assert_eq!(s.devices.get("Mac"), Some(&1));
    }

    #[test]
    fn write_contract_sources_join_with_no_registration() {
        let v = temp_vault("contract");
        // A collector (any language) wrote normalized lines into its own
        // media/plays/<source>/ folder — including one sparse line that
        // omits `source` (the folder names it) and carries `extra`.
        let dir = v.root().join("media/plays/letterboxd");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2026-06.jsonl"),
            concat!(
                "{\"ts\":\"2026-06-10T21:00:00-07:00\",\"source\":\"letterboxd\",\"category\":\"video\",\"kind\":\"play\",\"title\":\"Heat\",\"subtitle\":\"Michael Mann\",\"seconds\":10260,\"guid\":\"lb-1\",\"extra\":{\"rating\":\"5\"}}\n",
                "{\"ts\":\"2026-06-10T18:00:00-07:00\",\"source\":\"\",\"category\":\"video\",\"kind\":\"play\",\"title\":\"Ronin\",\"subtitle\":\"John Frankenheimer\",\"seconds\":0}\n",
                "{\"ts\":\"2026-05-01T18:00:00-07:00\",\"source\":\"letterboxd\",\"category\":\"video\",\"kind\":\"play\",\"title\":\"Out of range\",\"subtitle\":\"X\",\"seconds\":0}\n",
            ),
        )
        .unwrap();
        // A native arm alongside, to prove the merge stays chronological.
        v.append_music_plays(&[play("2026-06-10", "Song", "Artist", 200, true)]).unwrap();

        let day = v.media_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3, "two contract rows + one scrobble; out-of-range dropped");
        assert_eq!(
            day.iter().map(|i| i.source.as_str()).collect::<Vec<_>>(),
            vec!["music", "letterboxd", "letterboxd"],
            "chronological, and the sparse row inherited its folder's source"
        );
        assert_eq!(day[2].title, "Heat");
        assert_eq!(day[2].extra.get("rating"), Some(&serde_json::json!("5")));

        let s = v.media_summary("2026-06-10", "2026-06-10").unwrap();
        assert_eq!(s.sources.get("letterboxd"), Some(&2));
        assert_eq!(s.top.iter().filter(|u| u.category == "video").count(), 2);
    }

    #[test]
    fn daily_counts_full_plays_per_day() {
        let v = temp_vault("daily");
        v.append_music_plays(&[
            play("2026-06-09", "One", "Cannons", 200, true),
            play("2026-06-09", "Skip", "Cannons", 20, false),
            play("2026-06-10", "Two", "Gorillaz", 300, true),
        ])
        .unwrap();
        let d = v.media_daily("2026-06-08", "2026-06-10").unwrap();
        assert_eq!(d.len(), 2, "empty day omitted");
        assert_eq!((d[0].date.as_str(), d[0].value), ("2026-06-09", 1.0));
        assert_eq!((d[1].date.as_str(), d[1].value), ("2026-06-10", 1.0));
    }
}

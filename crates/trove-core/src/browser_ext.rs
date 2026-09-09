//! Browser watcher extension support — the live, authoritative arm of the
//! `browser/` stream (the history import in [`crate::browser`] is the backup).
//!
//! A Trove WebExtension (repo `extension/`) snapshots the browser every few
//! seconds — the active tab of the focused window plus every audible tab —
//! and sends each snapshot over Chrome native messaging. Chrome spawns the
//! `troved` binary as the messaging host (one host process per running
//! profile); the host feeds snapshots into a [`TabTracker`], which merges
//! them into spans and appends closed spans to `browser/YYYY-MM-DD.jsonl`
//! stamped `source:"extension"`:
//!
//! ```json
//! {"time":"2026-06-10T14:03:01-07:00","url":"https://www.youtube.com/watch?v=x",
//!  "title":"…","browser":"chrome","profile":"","duration_secs":1840,
//!  "source":"extension","audible":true,"foreground_secs":95,
//!  "favicon":"https://www.youtube.com/favicon.ico","referrer":"https://www.google.com/",
//!  "transition":"link","tab_count":12}
//! ```
//!
//! Beyond engagement, each span also carries lightweight context the
//! extension can see cheaply: the tab `favicon`, the `referrer`/`transition`
//! of the navigation (active tab only, from `webNavigation`), and `tab_count`
//! (open tabs when the span opened). All optional — absent on history rows and
//! omitted when empty.
//!
//! **Engagement model.** A URL has an open span while it is *engaged*: the
//! active tab of the focused browser window (foreground browsing), or audible
//! in any tab (media playback — the whole reason this exists; background
//! YouTube keeps its span open while the user works elsewhere). A span
//! records both totals: `duration_secs` is the engaged wall time,
//! `foreground_secs` the part spent as the focused-active tab — so readers
//! can attribute media consumption without ever billing background playback
//! into focus time. `audible` marks spans that played audio at any point.
//!
//! Like [`crate::activity::Watcher`] and [`crate::music::Scrobbler`], the
//! tracker is an OS-free state machine: feed `(now, snapshot)`, get closed
//! spans — so it unit-tests with synthetic snapshots and injected timestamps.
//! Snapshot gaps beyond [`ExtConfig::gap_secs`] (service worker suspended,
//! machine asleep, browser crash) close every open span at its last
//! evidence rather than stretching across the gap.
//!
//! Extension rows can't carry a Chrome profile name (the tabs API doesn't
//! expose it), so `profile` stays empty — cursor rebuilds in the history
//! import only count `source:"history"` rows, so this never interferes.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::browser::BrowserVisit;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

// The extension publishes live sidecars while connected; their newest mtime
// is "last seen", with the shared stream as backup.
fn def_last_data(vault: &crate::vault::Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(".trove/live"))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "browser-extension",
        name: "Browser extension",
        kind: IntegrationKind::Live,
        default_on: true,
        description: "Live tab tracking from the Trove Chrome extension: foreground time, background audio, favicons, referrers. The history import below backfills anything it misses.",
        domain: "browser",
        vault_path: "browser/",
        toggleable: true,
        setup: &[
            "Run `troved install` so the native-messaging manifest points at the current binary.",
            "In each Chrome profile: chrome://extensions → enable Developer mode → Load unpacked → select the repo's extension/ folder.",
            "Watch for source:\"extension\" rows in browser/ (or the live badge on the Web tab).",
        ],
        caveats: "Chrome only for now (the Safari arm is planned). When the extension or troved is missing, nothing breaks — the history import covers those visits at lower fidelity.",
    },
    behavior: Behavior::NativeHost,
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Native messaging host name — the Chrome manifest filename stem and the
/// name the extension connects to. Shared with `troved install` (which writes
/// the manifest) so they can never drift.
pub const NATIVE_HOST_NAME: &str = "com.davidwills.trove";

/// The Trove extension's stable ID. Derived from the public key pinned in
/// `extension/manifest.json` ("key" field), so a load-unpacked install gets
/// the same ID on any machine. The native messaging manifest's
/// `allowed_origins` must list exactly this.
pub const EXTENSION_ID: &str = "inhhdcdmfoiodfkipnheoiejdegipgpb";

/// Where Chrome looks up the native messaging host manifest on macOS.
pub fn native_host_manifest_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library/Application Support/Google/Chrome/NativeMessagingHosts")
            .join(format!("{NATIVE_HOST_NAME}.json"))
    })
}

/// Seconds between extension snapshots (the extension's own cadence; the
/// tracker derives nothing from it except via [`ExtConfig::gap_secs`]).
pub const EXT_SNAPSHOT_SECS: u64 = 5;

/// One tab as the extension reports it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// `favIconUrl` if Chrome had one for the tab.
    #[serde(default)]
    pub favicon: String,
    /// The page this tab was navigated from (active tab only; from
    /// `webNavigation`). Empty for typed/reload/bookmark or after a worker
    /// restart dropped the per-tab map.
    #[serde(default)]
    pub referrer: String,
    /// How the navigation happened (active tab only): `link`, `typed`,
    /// `reload`, `form_submit`, … Empty when unknown.
    #[serde(default)]
    pub transition: String,
}

/// One browser snapshot from the extension: who is engaged right now.
/// Unknown fields (e.g. the extension's `"type":"snapshot"` tag) are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtSnapshot {
    /// Is any window of this browser the OS-focused window?
    #[serde(default)]
    pub focused: bool,
    /// The active tab of the last-focused window (counts as engaged only
    /// while `focused`).
    #[serde(default)]
    pub active: Option<TabInfo>,
    /// Every tab currently playing audio, focused or not.
    #[serde(default)]
    pub audible: Vec<TabInfo>,
    /// Total open tabs across all windows at snapshot time (rough "tab
    /// clutter" signal). Stamped onto each span when it opens.
    #[serde(default)]
    pub tab_count: u32,
}

/// One currently-open span, surfaced for live "watching now" display. This is
/// ephemeral UI state derived from [`TabTracker`]'s in-memory open spans — it
/// is *never* written to the day JSONL (which records closed spans only). See
/// [`TabTracker::live`] and [`crate::vault::Vault::write_browser_live`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct LiveSpan {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub favicon: String,
    /// Engaged wall-seconds so far (span start → now).
    pub duration_secs: u64,
    /// Of which, seconds spent as the focused-active tab.
    #[serde(default)]
    pub foreground_secs: u64,
    /// Played audio at some point in the span.
    #[serde(default)]
    pub audible: bool,
    /// Is this the focused-active tab right now (vs. background audio only)?
    #[serde(default)]
    pub foreground: bool,
}

/// A live sidecar: one browser host's open spans plus when it last wrote.
/// Readers drop sidecars older than a short TTL — the host died without
/// clearing its file. Ephemeral, rebuildable, safe to delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveState {
    /// "chrome", "safari", …
    pub browser: String,
    /// Unix seconds when the host wrote this (stamped by the vault on write).
    pub updated: u64,
    pub spans: Vec<LiveSpan>,
}

/// Tracker tuning.
#[derive(Debug, Clone)]
pub struct ExtConfig {
    /// A silence longer than this between snapshots is a gap: open spans are
    /// closed at their last evidence instead of stretched across it.
    pub gap_secs: i64,
    /// Spans shorter than this are jitter (tab flicked through) and dropped.
    pub min_span_secs: i64,
}

impl Default for ExtConfig {
    fn default() -> Self {
        Self {
            gap_secs: (EXT_SNAPSHOT_SECS * 4) as i64,
            min_span_secs: 1,
        }
    }
}

/// An in-progress span for one URL.
struct OpenSpan {
    start: DateTime<Local>,
    last_seen: DateTime<Local>,
    title: String,
    /// Latest non-empty favicon seen (loads late, like the title).
    favicon: String,
    /// Captured once at span open — how this URL was reached and from where.
    referrer: String,
    transition: String,
    /// Open tabs when the span opened.
    tab_count: u32,
    foreground_secs: i64,
    /// Was this the focused-active tab at the previous snapshot? The elapsed
    /// interval is billed to the *previous* state (same convention as
    /// [`crate::activity::Watcher`]).
    fg_last: bool,
    audible_any: bool,
}

/// One URL's engagement within a single snapshot, borrowed from it.
struct Engaged<'a> {
    title: &'a str,
    favicon: &'a str,
    referrer: &'a str,
    transition: &'a str,
    fg: bool,
    audible: bool,
}

/// The snapshot→span merge state machine. Holds no OS handles; the native
/// messaging host owns the I/O and feeds this.
pub struct TabTracker {
    config: ExtConfig,
    /// "chrome" (later: "safari" via its own extension host).
    browser: String,
    spans: HashMap<String, OpenSpan>,
    last_tick: Option<DateTime<Local>>,
}

impl TabTracker {
    pub fn new(browser: &str, config: ExtConfig) -> Self {
        Self {
            config,
            browser: browser.to_string(),
            spans: HashMap::new(),
            last_tick: None,
        }
    }

    /// Fold one snapshot in; returns spans that just closed (in no particular
    /// order — the vault append groups by day anyway).
    pub fn tick(&mut self, now: DateTime<Local>, snap: &ExtSnapshot) -> Vec<BrowserVisit> {
        let mut closed = Vec::new();
        let mut dt = self
            .last_tick
            .map(|p| (now - p).num_seconds())
            .unwrap_or(0);
        if dt > self.config.gap_secs || dt < 0 {
            // Collector was gone (worker suspended, sleep, crash) or the
            // clock jumped backward: engagement past last evidence is
            // unknowable, so close everything there and start fresh.
            for (url, s) in std::mem::take(&mut self.spans) {
                let end = s.last_seen;
                closed.extend(self.close(url, s, end));
            }
            dt = 0;
        }
        // Bill the elapsed interval to the previous snapshot's state.
        for s in self.spans.values_mut() {
            if s.fg_last {
                s.foreground_secs += dt;
            }
        }
        // Engaged now: the focused window's active tab + every audible tab.
        // A tab can be both; referrer/transition come from the active tab only
        // (the snapshot carries navigation info just for it).
        let mut engaged: HashMap<&str, Engaged> = HashMap::new();
        if snap.focused {
            if let Some(a) = &snap.active {
                if !a.url.is_empty() {
                    engaged.insert(
                        &a.url,
                        Engaged {
                            title: &a.title,
                            favicon: &a.favicon,
                            referrer: &a.referrer,
                            transition: &a.transition,
                            fg: true,
                            audible: false,
                        },
                    );
                }
            }
        }
        for t in &snap.audible {
            if t.url.is_empty() {
                continue;
            }
            engaged
                .entry(&t.url)
                .and_modify(|e| {
                    e.audible = true;
                    if e.favicon.is_empty() {
                        e.favicon = &t.favicon;
                    }
                })
                .or_insert(Engaged {
                    title: &t.title,
                    favicon: &t.favicon,
                    referrer: "",
                    transition: "",
                    fg: false,
                    audible: true,
                });
        }
        for (url, e) in &engaged {
            match self.spans.entry(url.to_string()) {
                Entry::Occupied(mut occ) => {
                    let s = occ.get_mut();
                    s.last_seen = now;
                    s.fg_last = e.fg;
                    s.audible_any |= e.audible;
                    if !e.title.is_empty() {
                        s.title = e.title.to_string();
                    }
                    if !e.favicon.is_empty() {
                        s.favicon = e.favicon.to_string();
                    }
                }
                Entry::Vacant(v) => {
                    v.insert(OpenSpan {
                        start: now,
                        last_seen: now,
                        title: e.title.to_string(),
                        favicon: e.favicon.to_string(),
                        referrer: e.referrer.to_string(),
                        transition: e.transition.to_string(),
                        tab_count: snap.tab_count,
                        foreground_secs: 0,
                        fg_last: e.fg,
                        audible_any: e.audible,
                    });
                }
            }
        }
        // Spans no longer engaged close at this snapshot (their final
        // interval was billed above, matching Watcher's boundary convention).
        let gone: Vec<String> = self
            .spans
            .keys()
            .filter(|u| !engaged.contains_key(u.as_str()))
            .cloned()
            .collect();
        for url in gone {
            let s = self.spans.remove(&url).expect("key from spans");
            closed.extend(self.close(url, s, now));
        }
        self.last_tick = Some(now);
        closed
    }

    /// Close out everything at its last evidence — call on shutdown (the
    /// browser disconnecting the host).
    pub fn flush(&mut self) -> Vec<BrowserVisit> {
        let mut closed = Vec::new();
        for (url, s) in std::mem::take(&mut self.spans) {
            let end = s.last_seen;
            closed.extend(self.close(url, s, end));
        }
        self.last_tick = None;
        closed
    }

    /// The currently-open spans, with elapsed durations computed against
    /// `now`, for live "watching right now" display. A pure read — closes
    /// nothing and mutates nothing. The result is ephemeral and must never be
    /// appended to the day JSONL (which holds closed spans only).
    pub fn live(&self, now: DateTime<Local>) -> Vec<LiveSpan> {
        // Interval since the last tick, not yet billed into `foreground_secs`;
        // attribute it to the current foreground state so the live number
        // tracks smoothly between snapshots.
        let pending = self
            .last_tick
            .map(|p| (now - p).num_seconds().max(0))
            .unwrap_or(0);
        self.spans
            .iter()
            .map(|(url, s)| {
                let secs = (now - s.start).num_seconds().max(0);
                let fg = s.foreground_secs + if s.fg_last { pending } else { 0 };
                LiveSpan {
                    url: url.clone(),
                    title: s.title.clone(),
                    favicon: s.favicon.clone(),
                    duration_secs: secs as u64,
                    foreground_secs: fg.min(secs) as u64,
                    audible: s.audible_any,
                    foreground: s.fg_last,
                }
            })
            .collect()
    }

    fn close(&self, url: String, s: OpenSpan, end: DateTime<Local>) -> Option<BrowserVisit> {
        let secs = (end - s.start).num_seconds();
        if secs < self.config.min_span_secs {
            return None;
        }
        Some(BrowserVisit {
            time: s.start.to_rfc3339(),
            url,
            title: s.title,
            browser: self.browser.clone(),
            profile: String::new(),
            duration_secs: secs as u64,
            source: "extension".into(),
            audible: s.audible_any,
            foreground_secs: s.foreground_secs.clamp(0, secs) as u64,
            favicon: s.favicon,
            referrer: s.referrer,
            transition: s.transition,
            tab_count: s.tab_count,
        })
    }
}

/// History rows within this slack of an extension span covering the same URL
/// are the same browsing, observed twice.
const OVERLAP_SLACK_SECS: i64 = 120;

/// Read-time precedence: where an extension span and a history visit cover
/// the same URL at the same time, the extension row wins (it is richer and
/// real-time). The raw JSONL keeps both — files-first; this filters reads
/// only. History rows with no covering span pass through, which is exactly
/// the backup contract: visits the extension missed still count once.
pub(crate) fn resolve_extension_overlap(rows: Vec<BrowserVisit>) -> Vec<BrowserVisit> {
    // url → [(window start, window end)] in unix seconds, slack included.
    let mut windows: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    for v in rows.iter().filter(|v| v.source == "extension") {
        let Ok(t) = DateTime::parse_from_rfc3339(&v.time) else {
            continue;
        };
        let start = t.timestamp();
        windows.entry(v.url.clone()).or_default().push((
            start - OVERLAP_SLACK_SECS,
            start + v.duration_secs as i64 + OVERLAP_SLACK_SECS,
        ));
    }
    if windows.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|v| {
            if v.source != "history" {
                return true;
            }
            let Some(spans) = windows.get(&v.url) else {
                return true;
            };
            let Ok(t) = DateTime::parse_from_rfc3339(&v.time) else {
                return true;
            };
            let t = t.timestamp();
            !spans.iter().any(|(a, b)| (*a..=*b).contains(&t))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Vault;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 10, h, m, s).unwrap()
    }

    fn tab(url: &str, title: &str) -> TabInfo {
        TabInfo {
            url: url.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn fg(url: &str, title: &str) -> ExtSnapshot {
        ExtSnapshot {
            focused: true,
            active: Some(tab(url, title)),
            audible: vec![],
            ..Default::default()
        }
    }

    fn tracker() -> TabTracker {
        TabTracker::new("chrome", ExtConfig::default())
    }

    #[test]
    fn foreground_span_extends_and_closes_on_url_change() {
        let mut t = tracker();
        assert!(t.tick(at(10, 0, 0), &fg("https://a.com/", "A")).is_empty());
        assert!(t.tick(at(10, 0, 5), &fg("https://a.com/", "A")).is_empty());
        let closed = t.tick(at(10, 0, 10), &fg("https://b.com/", "B"));
        assert_eq!(closed.len(), 1);
        let v = &closed[0];
        assert_eq!(v.url, "https://a.com/");
        assert_eq!(v.title, "A");
        assert_eq!(v.duration_secs, 10);
        assert_eq!(v.foreground_secs, 10);
        assert_eq!(v.source, "extension");
        assert!(!v.audible);
        assert_eq!(v.time, at(10, 0, 0).to_rfc3339());
    }

    #[test]
    fn background_audible_tab_tracks_without_foreground_time() {
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: true,
            active: Some(tab("https://docs.example/", "Doc")),
            audible: vec![tab("https://www.youtube.com/watch?v=x", "Song")],
            ..Default::default()
        };
        t.tick(at(12, 0, 0), &snap);
        t.tick(at(12, 0, 5), &snap);
        // Audio stops, doc stays: only the YouTube span closes.
        let closed = t.tick(at(12, 0, 10), &fg("https://docs.example/", "Doc"));
        assert_eq!(closed.len(), 1);
        let v = &closed[0];
        assert_eq!(v.url, "https://www.youtube.com/watch?v=x");
        assert!(v.audible);
        assert_eq!(v.duration_secs, 10);
        assert_eq!(v.foreground_secs, 0);
    }

    #[test]
    fn focused_audible_tab_is_one_span_with_both_totals() {
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: true,
            active: Some(tab("https://www.youtube.com/watch?v=x", "Video")),
            audible: vec![tab("https://www.youtube.com/watch?v=x", "Video")],
            ..Default::default()
        };
        t.tick(at(9, 0, 0), &snap);
        t.tick(at(9, 0, 5), &snap);
        let closed = t.flush();
        assert_eq!(closed.len(), 1);
        let v = &closed[0];
        assert!(v.audible);
        assert_eq!(v.duration_secs, 5);
        assert_eq!(v.foreground_secs, 5);
    }

    #[test]
    fn unfocused_active_tab_is_not_engaged() {
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: false,
            active: Some(tab("https://a.com/", "A")),
            audible: vec![],
            ..Default::default()
        };
        t.tick(at(8, 0, 0), &snap);
        t.tick(at(8, 0, 5), &snap);
        assert!(t.flush().is_empty(), "no engagement, no span");
    }

    #[test]
    fn gap_closes_at_last_evidence_not_across_it() {
        let mut t = tracker();
        t.tick(at(10, 0, 0), &fg("https://a.com/", "A"));
        t.tick(at(10, 0, 5), &fg("https://a.com/", "A"));
        // 10 minutes of silence (sleep): the old span must end at 10:00:05.
        let closed = t.tick(at(10, 10, 5), &fg("https://a.com/", "A"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].duration_secs, 5);
        // …and a fresh span is open from the new snapshot.
        let after = t.flush();
        assert!(after.is_empty(), "fresh span has zero length at flush");
    }

    #[test]
    fn sub_floor_spans_are_dropped() {
        let mut t = tracker();
        t.tick(at(10, 0, 0), &fg("https://a.com/", "A"));
        // Immediately flushed: zero-length, below the 1s floor.
        assert!(t.flush().is_empty());
    }

    #[test]
    fn title_updates_keep_the_latest() {
        let mut t = tracker();
        t.tick(at(10, 0, 0), &fg("https://a.com/", "Loading…"));
        t.tick(at(10, 0, 5), &fg("https://a.com/", "Real title"));
        let closed = t.tick(at(10, 0, 10), &fg("https://b.com/", "B"));
        assert_eq!(closed[0].title, "Real title");
    }

    #[test]
    fn audible_flag_sticks_after_audio_stops() {
        let mut t = tracker();
        let playing = ExtSnapshot {
            focused: true,
            active: Some(tab("https://a.com/", "A")),
            audible: vec![tab("https://a.com/", "A")],
            ..Default::default()
        };
        t.tick(at(10, 0, 0), &playing);
        // Audio stops but the tab stays focused — span continues, flag kept.
        t.tick(at(10, 0, 5), &fg("https://a.com/", "A"));
        let closed = t.tick(at(10, 0, 10), &fg("https://b.com/", "B"));
        assert!(closed[0].audible);
        assert_eq!(closed[0].duration_secs, 10);
    }

    #[test]
    fn live_reports_open_spans_without_closing_them() {
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: true,
            active: Some(tab("https://www.youtube.com/watch?v=x", "Video")),
            audible: vec![tab("https://www.youtube.com/watch?v=x", "Video")],
            ..Default::default()
        };
        // Six snapshots at the real 5s cadence — 30s into the video.
        for i in 0..=6 {
            assert!(t
                .tick(at(9, 0, 0) + chrono::Duration::seconds(i * 5), &snap)
                .is_empty());
        }
        // Still mid-video: nothing has been appended, but live() sees it.
        let live = t.live(at(9, 0, 30));
        assert_eq!(live.len(), 1);
        let s = &live[0];
        assert_eq!(s.url, "https://www.youtube.com/watch?v=x");
        assert_eq!(s.duration_secs, 30);
        assert_eq!(s.foreground_secs, 30);
        assert!(s.audible);
        assert!(s.foreground);
        // live() is a pure read: the span is still open afterward.
        assert!(t.tick(at(9, 0, 35), &snap).is_empty());
        // Foreground seconds never exceed elapsed even between snapshots.
        let mid = t.live(at(9, 0, 38));
        assert_eq!(mid[0].duration_secs, 38);
        assert_eq!(mid[0].foreground_secs, 38);
    }

    #[test]
    fn snapshot_json_shape_from_extension_parses() {
        // Exactly what background.js sends (incl. the ignored "type" tag and
        // the enrichment fields, which are all optional).
        let snap: ExtSnapshot = serde_json::from_str(
            r#"{"type":"snapshot","focused":true,"tab_count":7,
                "active":{"url":"https://a.com/","title":"A","favicon":"https://a.com/f.ico",
                          "referrer":"https://ref.com/","transition":"link"},
                "audible":[{"url":"https://b.com/","title":"B"}]}"#,
        )
        .unwrap();
        assert!(snap.focused);
        assert_eq!(snap.tab_count, 7);
        let a = snap.active.unwrap();
        assert_eq!(a.url, "https://a.com/");
        assert_eq!(a.favicon, "https://a.com/f.ico");
        assert_eq!(a.referrer, "https://ref.com/");
        assert_eq!(a.transition, "link");
        assert_eq!(snap.audible.len(), 1);
        // Older snapshots without the new fields still parse (defaults).
        let bare: ExtSnapshot =
            serde_json::from_str(r#"{"focused":false,"active":null,"audible":[]}"#).unwrap();
        assert_eq!(bare.tab_count, 0);
    }

    #[test]
    fn enrichment_fields_flow_onto_span() {
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: true,
            tab_count: 9,
            active: Some(TabInfo {
                url: "https://a.com/".into(),
                title: "A".into(),
                favicon: "https://a.com/f.ico".into(),
                referrer: "https://google.com/".into(),
                transition: "link".into(),
            }),
            audible: vec![],
        };
        t.tick(at(10, 0, 0), &snap);
        t.tick(at(10, 0, 5), &snap);
        let closed = t.tick(at(10, 0, 10), &fg("https://b.com/", "B"));
        assert_eq!(closed.len(), 1);
        let v = &closed[0];
        assert_eq!(v.favicon, "https://a.com/f.ico");
        assert_eq!(v.referrer, "https://google.com/");
        assert_eq!(v.transition, "link");
        assert_eq!(v.tab_count, 9);
        // The freshly-opened b.com span carries no navigation context.
        let last = t.flush();
        assert!(last.is_empty(), "b.com span is sub-floor at flush");
    }

    #[test]
    fn overlap_resolution_prefers_extension_rows() {
        let ext = BrowserVisit {
            time: at(10, 0, 0).to_rfc3339(),
            url: "https://a.com/".into(),
            title: "A".into(),
            browser: "chrome".into(),
            profile: String::new(),
            duration_secs: 300,
            source: "extension".into(),
            audible: false,
            foreground_secs: 300,
            favicon: String::new(),
            referrer: String::new(),
            transition: String::new(),
            tab_count: 0,
        };
        let covered = BrowserVisit {
            time: at(10, 1, 0).to_rfc3339(),
            duration_secs: 0,
            source: "history".into(),
            profile: "Default".into(),
            ..ext.clone()
        };
        let other_url = BrowserVisit {
            url: "https://b.com/".into(),
            ..covered.clone()
        };
        let later = BrowserVisit {
            time: at(11, 0, 0).to_rfc3339(),
            ..covered.clone()
        };
        let out = resolve_extension_overlap(vec![
            ext.clone(),
            covered,
            other_url.clone(),
            later.clone(),
        ]);
        assert_eq!(out.len(), 3);
        assert!(out.iter().any(|v| v.source == "extension"));
        assert!(out.iter().any(|v| v.url == "https://b.com/"), "different URL survives");
        assert!(
            out.iter().any(|v| v.time == later.time && v.source == "history"),
            "history visit outside the span survives"
        );
    }

    #[test]
    fn vault_round_trip_dedupes_at_read_time() {
        let dir = std::env::temp_dir().join(format!(
            "trove-browserext-roundtrip-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();

        let mut t = tracker();
        // Five minutes on one page at the real snapshot cadence.
        for i in 0..=60 {
            let now = at(10, 0, 0) + chrono::Duration::seconds(i * EXT_SNAPSHOT_SECS as i64);
            assert!(t.tick(now, &fg("https://a.com/", "A")).is_empty());
        }
        let mut rows = t.flush();
        assert_eq!(rows.len(), 1);
        // The same visit as Chrome's history will record it, plus one the
        // extension never saw (e.g. captured while troved was down).
        rows.push(BrowserVisit {
            time: at(10, 0, 2).to_rfc3339(),
            url: "https://a.com/".into(),
            title: "A".into(),
            browser: "chrome".into(),
            profile: "Default".into(),
            duration_secs: 0,
            source: "history".into(),
            audible: false,
            foreground_secs: 0,
            favicon: String::new(),
            referrer: String::new(),
            transition: String::new(),
            tab_count: 0,
        });
        rows.push(BrowserVisit {
            time: at(15, 0, 0).to_rfc3339(),
            url: "https://c.com/".into(),
            title: "C".into(),
            browser: "chrome".into(),
            profile: "Default".into(),
            duration_secs: 0,
            source: "history".into(),
            audible: false,
            foreground_secs: 0,
            favicon: String::new(),
            referrer: String::new(),
            transition: String::new(),
            tab_count: 0,
        });
        v.append_browser_visits(&rows).unwrap();

        // Raw file keeps all three lines (files-first)…
        let raw = std::fs::read_to_string(v.root().join("browser/2026-06-10.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 3);
        // …and the extension row's false/zero extras are not serialized.
        assert!(!raw.contains("\"audible\""));

        // Reads resolve the overlap in the extension's favor.
        let day = v.browser_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
        assert!(day.iter().any(|r| r.source == "extension" && r.duration_secs == 300));
        assert!(day.iter().any(|r| r.url == "https://c.com/"));
    }
}

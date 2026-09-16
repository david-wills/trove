//! Browser extension support — the read side of the live, authoritative arm
//! of the `browser/` stream (the history import in [`crate::browser`] is the
//! backup).
//!
//! The Trove Chrome extension snapshots the browser every few seconds — the
//! active tab of the focused window plus every audible tab — and sends each
//! snapshot over native messaging to the external `trove-collector` program,
//! which merges them into spans and appends closed spans to
//! `browser/YYYY-MM-DD.jsonl` stamped `source:"extension"`:
//!
//! ```json
//! {"time":"2026-06-10T14:03:01-07:00","url":"https://www.youtube.com/watch?v=x",
//!  "title":"…","browser":"chrome","profile":"","duration_secs":1840,
//!  "source":"extension","audible":true,"foreground_secs":95,
//!  "favicon":"https://www.youtube.com/favicon.ico","referrer":"https://www.google.com/",
//!  "transition":"link","tab_count":12}
//! ```
//!
//! **Engagement model.** A URL has an open span while it is *engaged*: the
//! active tab of the focused browser window (foreground browsing), or audible
//! in any tab (media playback — background YouTube keeps its span open while
//! the user works elsewhere). A span records both totals: `duration_secs` is
//! the engaged wall time, `foreground_secs` the part spent as the
//! focused-active tab — so readers can attribute media consumption without
//! ever billing background playback into focus time. `audible` marks spans
//! that played audio at any point. Each span also carries lightweight
//! context: the tab `favicon`, the `referrer`/`transition` of the navigation,
//! and `tab_count` (open tabs when the span opened). All optional — absent on
//! history rows and omitted when empty.
//!
//! What stays in this crate: the def, the live "watching now" sidecar shape
//! the collector publishes under `.trove/live/` (read by the Web tab), and
//! the read-time overlap rule between extension and history rows.
//! `docs/vault-spec/domains/browser-visits.md` is the contract.

use std::collections::HashMap;

use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::browser::BrowserVisit;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};

// The collector publishes live sidecars while the extension is connected;
// their newest mtime is "last seen", with the shared stream as backup.
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
            "Install trove-collector (github.com/david-wills/trove-collector); its install step registers the native-messaging host the extension talks to.",
            "In each Chrome profile: chrome://extensions → enable Developer mode → Load unpacked → select the collector repo's extension/ folder.",
            "Watch for source:\"extension\" rows in browser/ (or the live badge on the Web tab).",
        ],
        caveats: "Chrome only for now (the Safari arm is planned). When the extension or the collector is missing, nothing breaks — the history import covers those visits at lower fidelity.",
    },
    behavior: Behavior::External { collector: "trove-collector" },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// One currently-open span, surfaced for live "watching now" display. This is
/// ephemeral state the collector derives from its in-memory open spans — it
/// is *never* written to the day JSONL (which records closed spans only). See
/// [`crate::vault::Vault::read_browser_live`].
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
    /// Unix seconds when the host wrote this.
    pub updated: u64,
    pub spans: Vec<LiveSpan>,
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
    use chrono::{Local, TimeZone};

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 10, h, m, s).unwrap()
    }

    fn history(time: DateTime<Local>, url: &str, title: &str) -> BrowserVisit {
        BrowserVisit {
            time: time.to_rfc3339(),
            url: url.into(),
            title: title.into(),
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
        }
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
        let covered = history(at(10, 1, 0), "https://a.com/", "A");
        let other_url = history(at(10, 1, 0), "https://b.com/", "A");
        let later = history(at(11, 0, 0), "https://a.com/", "A");
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

        // Five minutes on one page as the collector writes it, the same
        // visit as Chrome's history records it, plus one the extension never
        // saw (e.g. captured while the collector was down).
        let rows = vec![
            BrowserVisit {
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
            },
            history(at(10, 0, 2), "https://a.com/", "A"),
            history(at(15, 0, 0), "https://c.com/", "C"),
        ];
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

    #[test]
    fn live_sidecar_shape_from_collector_parses() {
        // The exact sidecar the collector writes under .trove/live/.
        let body = r#"{"browser":"chrome","updated":1780000000,"spans":[{"url":"https://a.com/","title":"A","duration_secs":12,"foreground_secs":12,"foreground":true}]}"#;
        let state: LiveState = serde_json::from_str(body).unwrap();
        assert_eq!(state.browser, "chrome");
        assert_eq!(state.spans.len(), 1);
        assert!(state.spans[0].foreground);
        assert!(!state.spans[0].audible);
        assert!(state.spans[0].favicon.is_empty());
    }
}

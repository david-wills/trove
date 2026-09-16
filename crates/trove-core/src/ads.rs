//! Ad observation stream: the read side of `browser/ads/`.
//!
//! The extension's opt-in page observer (docs/page-observer-spec.md) reports
//! the display ads it sees to the external `trove-collector` program, which
//! derives `network`/`advertiser` and appends one record per ad to a JSONL
//! file per local day, keyed by the day the ad record *closed*:
//! `browser/ads/YYYY-MM-DD.jsonl`:
//!
//! ```json
//! {"ts":"2026-06-11T10:23:45-07:00","end":"2026-06-11T10:24:10-07:00",
//!  "page_url":"https://example.com/article",
//!  "frame_url":"https://googleads.g.doubleclick.net/...",
//!  "landing_url":"https://advertiser.com/promo","network":"doubleclick.net",
//!  "advertiser":"advertiser.com","viewed_secs":12.5,"viewable":true,
//!  "w":300,"h":250,"source":"extension"}
//! ```
//!
//! Off by default twice over: the extension only observes after a
//! Chrome-mediated permission grant, and the collector only appends while
//! the `browser-ads` integration is enabled here. `viewable` follows the MRC
//! display standard the events were measured with: ≥50% of pixels in the
//! viewport for ≥1 continuous second. `docs/vault-spec/domains/ads.md` is
//! the contract; this crate reads the stream and aggregates it for the Ads
//! view.

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
    crate::registry::newest_stem(&vault.root().join("browser/ads"))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "browser-ads",
        name: "Ad observation",
        kind: IntegrationKind::Live,
        default_on: true,
        description: "Records the display ads the extension's opt-in page observer sees — network, advertiser, size, and how long each was actually on screen. Pure observation: never blocks ads, never captures page text or creative images.",
        domain: "browser",
        vault_path: "browser/ads/",
        toggleable: true,
        setup: &[
            "Install the browser extension above first.",
            "In the extension's options (chrome://extensions → Trove Browser Watcher → Details → Extension options): enable \"Page observation\" and accept Chrome's site-access prompt, then enable \"Ad observation\".",
            "This toggle is the vault-side gate on top of that — both must be on for records to land.",
        ],
        caveats: "Off by default in the extension; enabling is a Chrome-mediated permission grant, revocable in chrome://extensions, with per-site exclusions via Chrome's own site-access controls. Ads rendered into srcdoc frames get viewability but no advertiser (there is no frame URL to inspect).",
    },
    behavior: Behavior::External { collector: "trove-collector" },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Opt-in enrichment of the ad stream: resolves *who paid* for each observed
/// ad. Off by default and the lone exception to Ads being pure local
/// observation — when on, the collector fetches the Google ad-transparency
/// page named by the creative's own AdChoices link and reads the "Paid for
/// by" advertiser. No stream of its own; it rides `browser-ads`, and the
/// collector consults this toggle per ad batch.
///
/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static IDENTIFY_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "browser-ads-identify",
        name: "Advertiser identity lookup",
        kind: IntegrationKind::Live,
        default_on: false,
        description: "Names who paid for each ad you saw by fetching Google's public ad-transparency (\"Why this ad?\") page and reading the \"Paid for by\" line. The only part of Ads that makes a network request — off by default.",
        domain: "browser",
        vault_path: "browser/ads/",
        toggleable: true,
        setup: &[
            "Enable Ad observation first — this enriches that stream; on its own it does nothing.",
            "Turning this on lets the collector fetch Google's own ad-transparency page for ads you observed, to name the advertiser. Only that Google URL is requested — no page content, creative, or your identity is sent.",
        ],
        caveats: "Makes outbound requests to Google's ad-transparency pages — the one exception to Ads being pure local observation. Only Google-served ads carry a transparency link; others stay labeled by network. The named payer is the legal advertiser of record, often the brand's media-buying agency rather than the brand in the creative.",
    },
    behavior: Behavior::External { collector: "trove-collector" },
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};

/// One stored ad record (a line in `browser/ads/YYYY-MM-DD.jsonl`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AdRecord {
    /// Detection time, RFC3339 local.
    pub ts: String,
    /// Close time, RFC3339 local. The file is keyed by this day.
    pub end: String,
    pub page_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub frame_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub landing_url: String,
    /// Serving network (registrable domain of `frame_url`, or the slot
    /// family for srcdoc ads — e.g. "google"). "unknown" when neither
    /// signal yields anything; honest fields beat guessed ones.
    pub network: String,
    /// Advertiser: registrable domain of `landing_url`, or — when the opt-in
    /// `browser-ads-identify` resolver is on and the ad carried a Google
    /// transparency link — the human "Paid for by" name (e.g. "Hearts &
    /// Science LLC"). Empty if unjoined and unresolved.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub advertiser: String,
    /// Google ad-transparency advertiser id (e.g. "AR0405…") when the resolver
    /// named `advertiser` — a stable link to adstransparency.google.com.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub advertiser_id: String,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub viewed_secs: f64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub viewable: bool,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub w: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub h: u32,
    #[serde(default = "ext_source")]
    pub source: String,
}

fn ext_source() -> String {
    "extension".into()
}

fn is_zero_f64(n: &f64) -> bool {
    *n == 0.0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// Per-network or per-advertiser aggregate over a date range.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AdUsage {
    pub name: String,
    pub count: u64,
    /// How many of `count` met the MRC viewability bar.
    pub viewable: u64,
    pub viewed_secs: f64,
}

/// Aggregate of a date range for the Ads view.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AdsSummary {
    pub ads: u64,
    pub viewable: u64,
    pub viewed_secs: f64,
    /// By total viewed time, descending.
    pub networks: Vec<AdUsage>,
    /// Same, advertisers only (records the inspector managed to attribute).
    pub advertisers: Vec<AdUsage>,
}

/// Two day-series for trends: ads seen and ad-viewing seconds.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AdsDaily {
    pub seen: Vec<SeriesPoint>,
    pub viewed_secs: Vec<SeriesPoint>,
}

impl Vault {
    /// Append derived records to their close-day JSONL logs under the
    /// per-day flock discipline (keyed by the *close* day, not detection
    /// time). The byte-parity reference writer for tests and imports; the
    /// live stream is written by the external collector.
    pub fn append_ad_records(&self, records: &[AdRecord]) -> Result<()> {
        self.stream("browser/ads", crate::store::Partition::Day)
            .with_flock()
            .append(records, |r| &r.end)
    }

    /// All ad records that closed on one local day (YYYY-MM-DD), file order.
    pub fn ads_timeline(&self, date: &str) -> Result<Vec<AdRecord>> {
        let path = self.resolve(&format!("browser/ads/{date}.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)
            .with_context(|| format!("reading browser/ads/{date}.jsonl"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<AdRecord>(l).ok())
            .collect())
    }

    /// Totals plus per-network and per-advertiser aggregates over an
    /// inclusive date range. No derived index — volume is tiny (tens to a
    /// few hundred records a day), so aggregate on the fly like activity.
    pub fn ads_summary(&self, from: &str, to: &str) -> Result<AdsSummary> {
        let mut networks: HashMap<String, AdUsage> = HashMap::new();
        let mut advertisers: HashMap<String, AdUsage> = HashMap::new();
        let mut ads = 0u64;
        let mut viewable = 0u64;
        let mut viewed_secs = 0f64;
        let bump = |map: &mut HashMap<String, AdUsage>, name: &str, r: &AdRecord| {
            let u = map.entry(name.to_string()).or_insert_with(|| AdUsage {
                name: name.to_string(),
                count: 0,
                viewable: 0,
                viewed_secs: 0.0,
            });
            u.count += 1;
            u.viewable += r.viewable as u64;
            u.viewed_secs += r.viewed_secs;
        };
        for date in days(from, to)? {
            for r in self.ads_timeline(&date)? {
                ads += 1;
                viewable += r.viewable as u64;
                viewed_secs += r.viewed_secs;
                bump(&mut networks, &r.network, &r);
                if !r.advertiser.is_empty() {
                    bump(&mut advertisers, &r.advertiser, &r);
                }
            }
        }
        let rank = |map: HashMap<String, AdUsage>| {
            let mut v: Vec<AdUsage> = map.into_values().collect();
            v.sort_by(|a, b| {
                b.viewed_secs
                    .total_cmp(&a.viewed_secs)
                    .then_with(|| b.count.cmp(&a.count))
                    .then_with(|| a.name.cmp(&b.name))
            });
            v
        };
        Ok(AdsSummary {
            ads,
            viewable,
            viewed_secs,
            networks: rank(networks),
            advertisers: rank(advertisers),
        })
    }

    /// Ads seen and ad-viewing seconds per day over an inclusive range —
    /// trend series. Days with no data are omitted.
    pub fn ads_daily(&self, from: &str, to: &str) -> Result<AdsDaily> {
        let mut out = AdsDaily {
            seen: Vec::new(),
            viewed_secs: Vec::new(),
        };
        for date in days(from, to)? {
            let rows = self.ads_timeline(&date)?;
            if rows.is_empty() {
                continue;
            }
            let secs: f64 = rows.iter().map(|r| r.viewed_secs).sum();
            out.seen.push(SeriesPoint {
                date: date.clone(),
                value: rows.len() as f64,
            });
            out.viewed_secs.push(SeriesPoint { date, value: secs });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-ads-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn rfc(d: u32, h: u32, m: u32) -> String {
        Local.with_ymd_and_hms(2026, 6, d, h, m, 0).unwrap().to_rfc3339()
    }

    fn record(d: u32, h: u32, network: &str, advertiser: &str, viewed: f64) -> AdRecord {
        AdRecord {
            ts: rfc(d, h, 0),
            end: rfc(d, h, 2),
            page_url: "https://example.com/article".into(),
            frame_url: String::new(),
            landing_url: String::new(),
            network: network.into(),
            advertiser: advertiser.into(),
            advertiser_id: String::new(),
            viewed_secs: viewed,
            viewable: viewed >= 1.0,
            w: 300,
            h: 250,
            source: ext_source(),
        }
    }

    #[test]
    fn append_and_read_round_trip() {
        let v = temp_vault("roundtrip");
        v.append_ad_records(&[
            record(10, 9, "doubleclick.net", "", 0.0),
            record(11, 10, "taboola.com", "advertiser.com", 12.5),
        ])
        .unwrap();

        // Keyed by close day.
        let d10 = v.ads_timeline("2026-06-10").unwrap();
        assert_eq!(d10.len(), 1);
        assert_eq!(d10[0].network, "doubleclick.net");
        assert_eq!(d10[0].advertiser, "");
        assert!(!d10[0].viewable);

        let d11 = v.ads_timeline("2026-06-11").unwrap();
        assert_eq!(d11.len(), 1);
        assert_eq!(d11[0].network, "taboola.com");
        assert_eq!(d11[0].advertiser, "advertiser.com");
        assert_eq!(d11[0].viewed_secs, 12.5);
        assert!(d11[0].viewable);
        assert_eq!(d11[0].source, "extension");
    }

    /// Byte-parity contract for the stream: the exact line shape the
    /// external collector writes and this reader expects.
    #[test]
    fn append_writes_byte_identical_jsonl() {
        let v = temp_vault("parity");
        let mut full = record(10, 9, "doubleclick.net", "advertiser.com", 12.5);
        full.frame_url = "https://googleads.g.doubleclick.net/x".into();
        full.landing_url = "https://www.advertiser.com/promo".into();
        let bare = record(10, 9, "unknown", "", 0.0);
        v.append_ad_records(&[full, bare.clone()]).unwrap();
        v.append_ad_records(&[bare]).unwrap(); // second call extends

        let ts = rfc(10, 9, 0);
        let end = rfc(10, 9, 2);
        let full_line = format!(
            "{{\"ts\":\"{ts}\",\"end\":\"{end}\",\"page_url\":\"https://example.com/article\",\
             \"frame_url\":\"https://googleads.g.doubleclick.net/x\",\
             \"landing_url\":\"https://www.advertiser.com/promo\",\
             \"network\":\"doubleclick.net\",\"advertiser\":\"advertiser.com\",\
             \"viewed_secs\":12.5,\"viewable\":true,\"w\":300,\"h\":250,\"source\":\"extension\"}}"
        );
        let bare_line = format!(
            "{{\"ts\":\"{ts}\",\"end\":\"{end}\",\"page_url\":\"https://example.com/article\",\
             \"network\":\"unknown\",\"w\":300,\"h\":250,\"source\":\"extension\"}}"
        );
        let raw = fs::read_to_string(v.root().join("browser/ads/2026-06-10.jsonl")).unwrap();
        assert_eq!(raw, format!("{full_line}\n{bare_line}\n{bare_line}\n"));
    }

    #[test]
    fn optional_fields_are_skipped_and_reparse() {
        let v = temp_vault("serde");
        v.append_ad_records(&[record(10, 9, "doubleclick.net", "", 0.0)]).unwrap();
        let raw = fs::read_to_string(v.root().join("browser/ads/2026-06-10.jsonl")).unwrap();
        // Empty/zero/false fields stay off the line — same convention as
        // BrowserVisit.
        assert!(!raw.contains("landing_url"));
        assert!(!raw.contains("advertiser"));
        assert!(!raw.contains("viewed_secs"));
        assert!(!raw.contains("viewable"));
        assert!(raw.contains("\"network\":\"doubleclick.net\""));

        // A minimal stored line (older writer / hand edit) still parses.
        let r: AdRecord = serde_json::from_str(
            r#"{"ts":"2026-06-10T09:00:00-07:00","end":"2026-06-10T09:02:00-07:00",
                "page_url":"https://example.com/","network":"google"}"#,
        )
        .unwrap();
        assert_eq!(r.source, "extension");
        assert_eq!(r.viewed_secs, 0.0);
        assert!(!r.viewable);
    }

    #[test]
    fn summary_and_daily_aggregate() {
        let v = temp_vault("summary");
        v.append_ad_records(&[
            record(10, 9, "doubleclick.net", "", 2.0),
            record(10, 10, "googlesyndication.com", "example.com", 5.0),
            record(11, 9, "taboola.com", "example.com", 30.0),
        ])
        .unwrap();

        let s = v.ads_summary("2026-06-10", "2026-06-11").unwrap();
        assert_eq!(s.ads, 3);
        assert_eq!(s.viewable, 3);
        assert_eq!(s.viewed_secs, 37.0);
        assert_eq!(s.networks[0].name, "taboola.com"); // by viewed time
        assert_eq!(s.networks.len(), 3);
        assert_eq!(s.advertisers.len(), 1);
        assert_eq!(s.advertisers[0].name, "example.com");
        assert_eq!(s.advertisers[0].count, 2);

        let daily = v.ads_daily("2026-06-09", "2026-06-12").unwrap();
        assert_eq!(daily.seen.len(), 2);
        assert_eq!(daily.seen[0].value, 2.0);
        assert_eq!(daily.viewed_secs[1].value, 30.0);
    }
}

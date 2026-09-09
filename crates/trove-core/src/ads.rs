//! Ad observation stream — the first feature of the extension's opt-in page
//! observer (docs/page-observer-spec.md). Off by default twice over: the
//! extension only observes after a Chrome-mediated permission grant, and the
//! host only appends while the `browser-ads` integration is enabled.
//!
//! Source of truth is one JSONL file per local day, keyed by the day the ad
//! record *closed*: `browser/ads/YYYY-MM-DD.jsonl`, one record per line:
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
//! The extension is a dumb sensor shipping raw URLs and epoch timestamps
//! ([`AdEvent`]); `network`/`advertiser` are derived here at append time —
//! derivation logic lives where it's tested and reusable, not in the
//! content script. Viewability accrual happened browser-side (the
//! IntersectionObserver is the only honest clock for it); the host only
//! sanity-bounds timestamps against arrival time.
//!
//! `viewable` follows the MRC display standard the events were measured
//! with: ≥50% of pixels in the viewport for ≥1 continuous second.

use std::collections::{BTreeMap, HashMap};
use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::activity::days;
use crate::browser::domain_of;
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
    behavior: Behavior::NativeHost,
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Opt-in enrichment of the ad stream: resolves *who paid* for each observed
/// ad. Off by default and the lone exception to Ads being pure local
/// observation — when on, [`Vault::ingest_ad_events`] fetches the Google
/// ad-transparency page named by the creative's own AdChoices link and reads
/// the "Paid for by" advertiser. No collector of its own; it rides the
/// `browser-ads` stream, so it is `Behavior::NativeHost` like its parent —
/// the native host consults [`Vault::integration_enabled`] for it per ad
/// batch.
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
            "Turning this on lets Trove fetch Google's own ad-transparency page for ads you observed, to name the advertiser. Only that Google URL is requested — no page content, creative, or your identity is sent.",
        ],
        caveats: "Makes outbound requests to Google's ad-transparency pages — the one exception to Ads being pure local observation. Only Google-served ads carry a transparency link; others stay labeled by network. The named payer is the legal advertiser of record, often the brand's media-buying agency rather than the brand in the creative.",
    },
    behavior: Behavior::NativeHost,
    permission: None,
    last_data: None,
    connection: None,
    pull: None,
};

/// One closed ad record as the extension ships it (wire shape, inside a
/// `{type:"ads", events:[…]}` native message). Epoch-ms timestamps and raw
/// URLs only — all derivation happens host-side.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdEvent {
    /// Detection time, Unix epoch milliseconds.
    #[serde(default)]
    pub ts_ms: i64,
    /// Close time (iframe removed / page unloaded), epoch milliseconds.
    #[serde(default)]
    pub end_ms: i64,
    /// Top-frame URL the ad appeared on, captured at detection.
    #[serde(default)]
    pub page_url: String,
    /// The ad iframe's `src`. Empty for srcdoc/about:blank ads (detected by
    /// slot naming instead).
    #[serde(default)]
    pub frame_url: String,
    /// Click-through landing URL from the inspector join; empty if unjoined.
    #[serde(default)]
    pub landing_url: String,
    /// Matched ad-slot name prefix (`google_ads_iframe`, `aswift_`,
    /// `div-gpt-ad`) — the network fallback when `frame_url` is empty.
    #[serde(default)]
    pub slot: String,
    /// Seconds ≥50%-visible in a visible tab (IntersectionObserver-accrued).
    #[serde(default)]
    pub viewed_secs: f64,
    /// MRC display standard: ≥50% visible for ≥1 continuous second.
    #[serde(default)]
    pub viewable: bool,
    /// Largest observed iframe size, CSS pixels.
    #[serde(default)]
    pub w: u32,
    #[serde(default)]
    pub h: u32,
    /// Google's "Why this ad?" transparency URL scraped from the creative's
    /// AdChoices link, if present. Wire-only: never stored — it carries a
    /// one-time `reasons` token and is consumed at append time by the opt-in
    /// advertiser resolver (`browser-ads-identify`). Empty for non-Google ads.
    #[serde(default)]
    pub why_url: String,
}

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

/// Aggregate of a date range for the (future) Ads view.
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

/// Slot-prefix → network family, mirroring the extension's
/// `TROVE_AD_SLOT_PREFIXES` (all current slot patterns are Google's).
const SLOT_NETWORKS: &[(&str, &str)] = &[
    ("google_ads_iframe", "google"),
    ("aswift_", "google"),
    ("div-gpt-ad", "google"),
];

/// Two-part public suffixes seen on ad/advertiser domains, so eTLD+1
/// reduction doesn't mangle "foo.co.uk" into "co.uk". Deliberately small —
/// a wrong-but-stable grouping beats hauling in a full public-suffix list.
const TWO_PART_SUFFIXES: &[&str] = &[
    "co.uk", "com.au", "co.jp", "co.kr", "com.br", "co.in", "com.tr", "net.au", "co.nz",
];

/// Registrable domain (eTLD+1-ish) of a URL: `domain_of` host, reduced.
/// "googleads.g.doubleclick.net" → "doubleclick.net".
fn registrable_of(url: &str) -> String {
    let host = domain_of(url);
    // domain_of collapses non-web schemes to the scheme name; not a domain.
    if !host.contains('.') {
        return String::new();
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let two = labels[labels.len() - 2..].join(".");
    let take = if TWO_PART_SUFFIXES.contains(&two.as_str()) { 3 } else { 2 };
    labels[labels.len() - take..].join(".")
}

/// The serving network for a record: registrable domain of the frame URL,
/// falling back to the slot family for srcdoc/about:blank ads.
fn network_of(frame_url: &str, slot: &str) -> String {
    let from_frame = registrable_of(frame_url);
    if !from_frame.is_empty() {
        return from_frame;
    }
    for (prefix, network) in SLOT_NETWORKS {
        if slot.starts_with(prefix) {
            return (*network).into();
        }
    }
    "unknown".into()
}

fn ms_to_local(ms: i64) -> Option<DateTime<Local>> {
    DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&Local))
}

// ── Advertiser identity resolution (the `browser-ads-identify` opt-in) ──────
//
// Google's display creatives carry an AdChoices link to a "Why this ad?"
// transparency page whose server-rendered HTML names the legal payer
// ("Paid for by …") and links its canonical ad-transparency advertiser id.
// The resolver fetches that one Google URL — never the advertiser, never a
// click tracker (which would register a fake click) — and reads those two
// markers. Results are cached by the page's `reasons` token so a repeated
// campaign is fetched once. This is the lone networked path in Ads.

/// A resolved advertiser identity. The name is the legal "Paid for by"
/// entity, which is often a brand's media-buying agency, not the brand itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AdIdentity {
    /// "Paid for by" display name, e.g. "Hearts & Science LLC". Empty means a
    /// resolved-but-unattributable page — cached negatively to avoid refetch.
    name: String,
    /// Canonical ad-transparency advertiser id (`AR…`), when present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ad_id: String,
}

/// HTTP timeout per transparency fetch. The page is large (~800 KB) but the
/// markers are near the top; a tight bound keeps a slow page from stalling the
/// host's ad batch.
const WTA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// A real browser UA — the transparency endpoint serves a JS shell to unknown
/// clients, but the server-rendered identity markers we read are present
/// either way; a normal UA just avoids edge-case gating.
const WTA_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Cap on transparency fetches per ad batch, so a flood of fresh ads can't
/// fan out into hundreds of synchronous requests. Cached tokens don't count.
const MAX_RESOLVE_PER_BATCH: usize = 25;

/// Vault-relative cache of resolved identities, keyed by [`why_token`].
const IDENTITY_CACHE: &str = "browser/ads/.identities.json";

/// Stable cache key for a transparency URL: its `reasons` token (shared by
/// repeat impressions of one campaign) if present, else the whole URL.
fn why_token(why_url: &str) -> String {
    match why_url.find("reasons=") {
        Some(i) => {
            let rest = &why_url[i + "reasons=".len()..];
            let tok = rest.split('&').next().unwrap_or(rest);
            if tok.is_empty() { why_url.to_string() } else { tok.to_string() }
        }
        None => why_url.to_string(),
    }
}

/// The slice of `s` between the first `start` and the next `end` after it.
fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

/// Decode the handful of HTML entities Google emits in advertiser names.
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Parse the advertiser identity out of a transparency page's HTML. Reads the
/// server-rendered "Paid for by …" name (required) and the adjacent
/// ad-transparency advertiser id (optional). `None` if the page names no
/// payer (unverified/blank advertiser).
fn parse_identity(html: &str) -> Option<AdIdentity> {
    let name = html_unescape(between(html, "Paid for by ", "<")?.trim());
    if name.is_empty() {
        return None;
    }
    let ad_id = between(html, "adstransparency.google.com/advertiser/", "?")
        .or_else(|| between(html, "adstransparency.google.com/advertiser/", "\""))
        .map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        })
        .filter(|s| s.starts_with("AR"))
        .unwrap_or_default();
    Some(AdIdentity { name, ad_id })
}

/// Fetch and parse one transparency page. Any network/parse failure is a
/// `None` — resolution is best-effort enrichment, never a gate.
fn fetch_identity(why_url: &str) -> Option<AdIdentity> {
    let body = ureq::get(why_url)
        .timeout(WTA_TIMEOUT)
        .set("User-Agent", WTA_UA)
        .call()
        .ok()?
        .into_string()
        .ok()?;
    parse_identity(&body)
}

impl AdEvent {
    /// Derive the stored record, sanity-bounding timestamps against the
    /// host's arrival time: the extension batches at most ~30s after close,
    /// so `end` beyond `received` means a wrong browser clock — clamp rather
    /// than file records in the future.
    fn into_record(&self, received: DateTime<Local>) -> AdRecord {
        let end = ms_to_local(self.end_ms).unwrap_or(received).min(received);
        let ts = ms_to_local(self.ts_ms).unwrap_or(end).min(end);
        let span_secs = (end - ts).num_milliseconds() as f64 / 1000.0;
        AdRecord {
            ts: ts.to_rfc3339(),
            end: end.to_rfc3339(),
            page_url: self.page_url.clone(),
            frame_url: self.frame_url.clone(),
            landing_url: self.landing_url.clone(),
            network: network_of(&self.frame_url, &self.slot),
            advertiser: registrable_of(&self.landing_url),
            advertiser_id: String::new(),
            viewed_secs: self.viewed_secs.clamp(0.0, span_secs.max(0.0)),
            viewable: self.viewable,
            w: self.w,
            h: self.h,
            source: ext_source(),
        }
    }
}

impl Vault {
    /// Append ad events to their close-day JSONL log. `received` is the
    /// host's arrival timestamp, used only as a sanity bound.
    ///
    /// Same flock-per-day-file discipline as [`Vault::append_browser_visits`]
    /// and for the same reason: one host process per running Chrome profile
    /// writes this stream concurrently.
    pub fn append_ad_events(&self, events: &[AdEvent], received: DateTime<Local>) -> Result<()> {
        self.ingest_ad_events(events, received, false)
    }

    /// Derive and append ad records, optionally resolving each advertiser's
    /// identity off Google's transparency page first (`resolve` mirrors the
    /// `browser-ads-identify` opt-in). Resolution is the only path that
    /// touches the network; with `resolve` false this is exactly
    /// [`Vault::append_ad_events`].
    pub fn ingest_ad_events(
        &self,
        events: &[AdEvent],
        received: DateTime<Local>,
        resolve: bool,
    ) -> Result<()> {
        let mut records: Vec<AdRecord> = events.iter().map(|e| e.into_record(received)).collect();
        if resolve {
            self.resolve_advertisers(&mut records, events);
        }
        self.write_ad_records(&records)
    }

    /// Overlay each record's `advertiser`/`advertiser_id` with the identity
    /// named by its transparency URL, fetching uncached pages (bounded per
    /// batch) and caching results by `reasons` token. Best-effort: a fetch
    /// miss leaves the domain-derived advertiser untouched. `events` is
    /// index-aligned with `records` (both built from the same batch).
    fn resolve_advertisers(&self, records: &mut [AdRecord], events: &[AdEvent]) {
        let path = self.root().join(IDENTITY_CACHE);
        let mut cache: BTreeMap<String, AdIdentity> = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut changed = false;
        let mut fetched = 0usize;
        for (rec, ev) in records.iter_mut().zip(events) {
            // A landing-derived advertiser (apple.com, nike.com) is the clearer
            // signal — keep it. Only spend a fetch when the click chain named
            // no advertiser, which is exactly when the transparency page earns
            // its keep (e.g. a creative that dead-ends on a click tracker).
            if ev.why_url.is_empty() || !rec.advertiser.is_empty() {
                continue;
            }
            let key = why_token(&ev.why_url);
            let ident = match cache.get(&key) {
                Some(c) => c.clone(),
                None if fetched < MAX_RESOLVE_PER_BATCH => {
                    fetched += 1;
                    // Negative results cache as a default (empty name) too, so
                    // an unattributable page isn't refetched every impression.
                    let got = fetch_identity(&ev.why_url).unwrap_or_default();
                    cache.insert(key, got.clone());
                    changed = true;
                    got
                }
                None => continue,
            };
            if !ident.name.is_empty() {
                rec.advertiser = ident.name;
                rec.advertiser_id = ident.ad_id;
            }
        }
        if changed {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(s) = serde_json::to_string_pretty(&cache) {
                let _ = fs::write(&path, s);
            }
        }
    }

    /// Append derived records to their close-day JSONL logs under the
    /// per-day flock discipline (keyed by the *close* day, not detection
    /// time). Split out so [`Vault::ingest_ad_events`] can enrich records
    /// between derivation and write.
    fn write_ad_records(&self, records: &[AdRecord]) -> Result<()> {
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
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-ads-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Epoch ms for a fixed local datetime, so day grouping is deterministic
    /// in any timezone.
    fn ms(d: u32, h: u32, m: u32) -> i64 {
        Local
            .with_ymd_and_hms(2026, 6, d, h, m, 0)
            .unwrap()
            .timestamp_millis()
    }

    fn event(d: u32, h: u32, frame: &str, landing: &str, viewed: f64) -> AdEvent {
        AdEvent {
            ts_ms: ms(d, h, 0),
            end_ms: ms(d, h, 2),
            page_url: "https://example.com/article".into(),
            frame_url: frame.into(),
            landing_url: landing.into(),
            slot: String::new(),
            viewed_secs: viewed,
            viewable: viewed >= 1.0,
            w: 300,
            h: 250,
            why_url: String::new(),
        }
    }

    fn received() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 12, 0, 0, 0).unwrap()
    }

    #[test]
    fn registrable_reduction() {
        assert_eq!(
            registrable_of("https://googleads.g.doubleclick.net/pagead/x"),
            "doubleclick.net"
        );
        assert_eq!(registrable_of("https://www.advertiser.com/promo?x=1"), "advertiser.com");
        assert_eq!(registrable_of("https://shop.example.co.uk/p"), "example.co.uk");
        assert_eq!(registrable_of(""), "");
        assert_eq!(registrable_of("about:blank"), "");
    }

    #[test]
    fn network_falls_back_to_slot() {
        assert_eq!(network_of("https://tpc.googlesyndication.com/sf", ""), "googlesyndication.com");
        assert_eq!(network_of("", "google_ads_iframe"), "google");
        assert_eq!(network_of("", "aswift_"), "google");
        assert_eq!(network_of("", ""), "unknown");
    }

    #[test]
    fn append_and_read_round_trip() {
        let v = temp_vault("roundtrip");
        v.append_ad_events(
            &[
                event(10, 9, "https://googleads.g.doubleclick.net/x", "", 0.0),
                event(11, 10, "https://cdn.taboola.com/y", "https://www.advertiser.com/promo", 12.5),
            ],
            received(),
        )
        .unwrap();

        // Keyed by close day, derivation applied.
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

    /// Byte-parity contract for the append path: exact file bytes, pinned
    /// before the port onto `store::JsonlStream` and unchanged by it.
    #[test]
    fn append_writes_byte_identical_jsonl() {
        let v = temp_vault("parity");
        let full = event(
            10,
            9,
            "https://googleads.g.doubleclick.net/x",
            "https://www.advertiser.com/promo",
            12.5,
        );
        let bare = event(10, 9, "", "", 0.0);
        v.append_ad_events(&[full, bare.clone()], received()).unwrap();
        v.append_ad_events(&[bare], received()).unwrap(); // second call extends

        let ts = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap().to_rfc3339();
        let end = Local.with_ymd_and_hms(2026, 6, 10, 9, 2, 0).unwrap().to_rfc3339();
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
        v.append_ad_events(
            &[event(10, 9, "https://securepubads.g.doubleclick.net/x", "", 0.0)],
            received(),
        )
        .unwrap();
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
    fn timestamps_are_sanity_bounded() {
        let v = temp_vault("bounds");
        let mut e = event(10, 9, "https://googleads.g.doubleclick.net/x", "", 9999.0);
        e.end_ms = ms(13, 9, 0); // "from the future" relative to arrival
        v.append_ad_events(&[e], received()).unwrap();
        // Clamped to the arrival day, viewed_secs capped at the real span.
        let rows = v.ads_timeline("2026-06-12").unwrap();
        assert_eq!(rows.len(), 1);
        let span_secs = 2.0 * 24.0 * 3600.0 - 9.0 * 3600.0; // 06-10 09:00 → 06-12 00:00
        assert!(rows[0].viewed_secs <= span_secs + 1.0);
    }

    #[test]
    fn summary_and_daily_aggregate() {
        let v = temp_vault("summary");
        v.append_ad_events(
            &[
                event(10, 9, "https://googleads.g.doubleclick.net/a", "", 2.0),
                event(10, 10, "https://tpc.googlesyndication.com/b", "https://shoes.example.com/s", 5.0),
                event(11, 9, "https://cdn.taboola.com/c", "https://shoes.example.com/s2", 30.0),
            ],
            received(),
        )
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

    #[test]
    fn why_token_extracts_reasons() {
        assert_eq!(
            why_token("https://adssettings.google.com/whythisad?source=display&reasons=ABC123&x=1"),
            "ABC123"
        );
        // No token → the whole URL keys the cache.
        assert_eq!(why_token("https://x/whythisad?source=display"), "https://x/whythisad?source=display");
    }

    #[test]
    fn parse_identity_from_real_transparency_page() {
        // A faithful slice of a live adssettings.google.com "Why this ad?"
        // page (the Star Wars / Hearts & Science campaign that prompted this).
        let html = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ads/whythisad.html"
        ))
        .unwrap();
        let id = parse_identity(&html).expect("names a payer");
        assert_eq!(id.name, "Hearts & Science LLC"); // entity-decoded
        assert_eq!(id.ad_id, "AR04055566952792326145");

        // A page that names no payer resolves to None (cached negatively).
        assert!(parse_identity("<html><body>About this ad</body></html>").is_none());
    }

    #[test]
    fn resolve_overlays_cached_identity_without_network() {
        let v = temp_vault("resolve");
        // Pre-seed the identity cache so resolution is a pure cache hit — no
        // fetch, deterministic, offline.
        let dir = v.root().join("browser/ads");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(".identities.json"),
            r#"{"TOK42":{"name":"Hearts & Science LLC","ad_id":"AR0405"}}"#,
        )
        .unwrap();

        let mut e = event(10, 9, "https://googleads.g.doubleclick.net/x", "", 2.0);
        e.why_url = "https://adssettings.google.com/whythisad?source=display&reasons=TOK42".into();
        v.ingest_ad_events(&[e], received(), true).unwrap();

        let rows = v.ads_timeline("2026-06-10").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].advertiser, "Hearts & Science LLC");
        assert_eq!(rows[0].advertiser_id, "AR0405");

        // With resolution off, the same event keeps its domain-derived
        // advertiser (here empty) and never reads the cache.
        let mut e2 = event(11, 9, "https://googleads.g.doubleclick.net/x", "", 2.0);
        e2.why_url = "https://adssettings.google.com/whythisad?source=display&reasons=TOK42".into();
        v.ingest_ad_events(&[e2], received(), false).unwrap();
        let rows = v.ads_timeline("2026-06-11").unwrap();
        assert_eq!(rows[0].advertiser, "");
    }

    #[test]
    fn landing_domain_wins_over_transparency() {
        let v = temp_vault("landing-wins");
        let dir = v.root().join("browser/ads");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(".identities.json"),
            r#"{"TOK42":{"name":"Hearts & Science LLC","ad_id":"AR0405"}}"#,
        )
        .unwrap();

        // A clean landing (apple.com) AND a transparency link are both present;
        // resolution is on. The recognizable advertiser domain must win — no
        // override to the agency name, and no fetch spent (cache hit aside).
        let mut e = event(10, 9, "https://googleads.g.doubleclick.net/x", "https://www.apple.com/privacy/?cid=x", 2.0);
        e.why_url = "https://adssettings.google.com/whythisad?source=display&reasons=TOK42".into();
        v.ingest_ad_events(&[e], received(), true).unwrap();

        let rows = v.ads_timeline("2026-06-10").unwrap();
        assert_eq!(rows[0].advertiser, "apple.com");
        assert_eq!(rows[0].advertiser_id, ""); // not the resolved AR id
    }
}

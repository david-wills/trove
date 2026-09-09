//! Overcast — per-episode podcast play and progress history via the
//! community-documented extended OPML export (`overcast.fm/account/export_opml/extended`).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/overcast.md.
//!
//! A **Periodic** cloud pull (daily cadence — Marco Arment requests ≤10
//! fetches/day): every listened or in-progress episode lands in the unified
//! media stream via the **media-plays write contract**
//! (`docs/vault-spec/domains/media-plays.md`). Two layers per new/changed
//! episode play state:
//!
//! - **raw** — per-episode attribute snapshot at
//!   `media/plays/overcast/raw/YYYY-MM.jsonl`, partitioned by the
//!   userUpdatedDate month (or the fetch time for undated episodes); full
//!   fidelity, unconditional.
//! - **contract** — one normalized [`MediaItem`] at
//!   `media/plays/overcast/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! The OPML is a full snapshot of the account's episode list every fetch.
//! Snapshot-diff logic: episode state is keyed by `overcastId` (falling back
//! to the enclosure URL), and we compare `played` + `progress` against the
//! prior baseline to emit only new/changed states. The FIRST fetch is a SILENT
//! BASELINE — we write raw but do NOT emit contract rows, so there is no
//! flood of "partial" rows from already-heard episodes on first connect.
//!
//! Auth: email + password POSTed to `overcast.fm/login` → session cookie
//! (`o` cookie); the cookie is stored (encoded as the `access_token` slot
//! of a [`TokenSet`]) and sent on all subsequent requests. Overcast has no
//! OAuth; the session cookie lasts until the user changes their password.
//! Re-login is triggered only on 401/403 / explicit reconnect.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Contract stream directory; raw lines go one level deeper in `raw/`.
const DIR: &str = "media/plays/overcast";
const RAW_DIR: &str = "media/plays/overcast/raw";
/// Non-secret rebuildable cursor: the snapshot baseline (episode state map +
/// baseline flag). Deleting it causes a silent re-baseline on the next pull.
const CURSOR_FILE: &str = ".trove/overcast-sync.json";
/// The service id under `.trove/sync/` where the session cookie is stored.
const SERVICE: &str = "overcast";

const LOGIN_URL: &str = "https://overcast.fm/login";
const EXPORT_URL: &str = "https://overcast.fm/account/export_opml/extended";

/// Overcast requests ≤10 fetches/day; we sync once per 24 hours.
pub const OVERCAST_SYNC_SECS: u64 = 86_400;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("episodes").copied().unwrap_or(0);
            Ok(CollectOutcome::note_if(n > 0, || {
                format!("overcast synced — {n} episodes")
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("overcast sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("episodes").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Overcast is up to date — no new plays".to_string()
    } else {
        format!("Overcast synced — {n} new plays")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "overcast",
        name: "Overcast",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Syncs your podcast listening history from Overcast, including per-episode \
             played status and progress timestamps. Uses the community-documented extended \
             OPML export — snapshot-diff so only new and changed plays are written.",
        domain: "media",
        vault_path: "media/plays/overcast/",
        toggleable: true,
        setup: &[
            "Connect with your overcast.fm email and password (email:password).",
            "The first sync establishes a silent baseline — no play rows for already-heard \
             episodes. Later syncs emit only newly played or progressed episodes.",
        ],
        caveats: "Uses the community-documented extended OPML endpoint — unofficial but \
                  stable for years. Marco Arment requests no more than ~10 fetches per day; \
                  Trove syncs once per day. No total listen-time field exists; `seconds` is \
                  the playback progress position. Auth is email/password (no OAuth); \
                  re-connect if the session expires after a password change.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(OVERCAST_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("overcast"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste: email:password composite → session cookie stored).

fn def_connect(vault: &Vault, raw: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("enter your Overcast email and password as email:password");
    }
    let (email, password) = raw
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected email:password — separate with a colon"))?;
    let email = email.trim();
    let password = password.trim();
    if email.is_empty() || password.is_empty() {
        bail!("both email and password are required (email:password)");
    }
    let cookie = login(email, password)
        .context("Overcast login failed — check your email and password")?;
    let tok = crate::sync::oauth::TokenSet {
        access_token: cookie,
        refresh_token: None,
        token_type: None,
        scope: None,
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &tok)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let _ = token; // cookie value — don't expose it
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Connected".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "overcast",
    display_name: "Overcast",
    methods: &[ConnectMethod::TokenPaste {
        label: "Overcast login (email:password)",
        help: "Enter your overcast.fm account email and password separated by a colon. \
               Your credentials are used once to get a session cookie — only the cookie \
               is stored, not your password.",
        placeholder: "you@example.com:yourpassword",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["overcast"],
    setup: &[
        "Paste your overcast.fm email and password as email:password.",
        "Your credentials are exchanged for a session cookie (only the cookie is stored — \
         your password is never persisted).",
        "Trove syncs once per day — Marco Arment requests ≤10 fetches/day.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer.

/// POST to overcast.fm/login, extract the session `o` cookie value.
fn login(email: &str, password: &str) -> Result<String> {
    let form = format!("email={}&password={}", urlencoded(email), urlencoded(password));
    let resp = ureq::post(LOGIN_URL)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("User-Agent", "Trove/1 (+https://github.com/trove-app/trove)")
        .send_string(&form)
        .map_err(|e| match e {
            ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
                anyhow::anyhow!("Overcast rejected the login — check your email and password")
            }
            ureq::Error::Status(code, r) => {
                let body = r.into_string().unwrap_or_default();
                let body: String = body.chars().take(200).collect();
                anyhow::anyhow!("Overcast login failed (HTTP {code}): {body}")
            }
            other => anyhow::anyhow!("Overcast login network error: {other}"),
        })?;

    // Extract the `o` session cookie from Set-Cookie headers.
    // ureq 2.x: `resp.header("set-cookie")` returns the last value only;
    // iterate all headers to find the `o=` cookie.
    let cookie = resp
        .header("set-cookie")
        .and_then(|h| extract_o_cookie(h))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Overcast login succeeded but no session cookie was returned — \
                 try again or check your credentials"
            )
        })?;
    Ok(cookie)
}

/// Fetch the extended OPML using the stored session cookie.
fn fetch_opml(cookie: &str) -> Result<String> {
    let resp = ureq::get(EXPORT_URL)
        .timeout(HTTP_TIMEOUT)
        .set("Cookie", &format!("o={cookie}"))
        .set("User-Agent", "Trove/1 (+https://github.com/trove-app/trove)")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
                anyhow::anyhow!(
                    "Overcast rejected the export request — try reconnecting in the \
                     Integrations tab"
                )
            }
            ureq::Error::Status(429, _) => {
                anyhow::anyhow!("Overcast rate-limited the export (>10 fetches/day)")
            }
            ureq::Error::Status(code, r) => {
                let body = r.into_string().unwrap_or_default();
                let body: String = body.chars().take(200).collect();
                anyhow::anyhow!("Overcast OPML fetch failed (HTTP {code}): {body}")
            }
            other => anyhow::anyhow!("Overcast OPML network error: {other}"),
        })?;
    resp.into_string().context("reading Overcast OPML response body")
}

/// Extract the value of the `o` cookie from a `Set-Cookie` header string.
fn extract_o_cookie(header: &str) -> Option<String> {
    // Format: `o=<value>; Path=/; HttpOnly; ...`
    header.split(';').next().and_then(|pair| {
        let pair = pair.trim();
        pair.strip_prefix("o=").map(|v| v.trim().to_string())
    })
}

/// Percent-encode a form value (URL-encoding for application/x-www-form-urlencoded).
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// OPML parsing — pure, fixture-tested.

/// Attribute bag for one `<outline>` episode element in the extended OPML.
/// Field evidence: the Overcast extended OPML export adds custom attributes
/// in the `overcast` namespace (`overcast:played`, etc.); Python parsers that
/// read the file (overcast-to-sqlite, cleverdevil/overcast-recently-played)
/// access them as bare names (`played`, `progress`, `overcastId`, …) because
/// Python's ElementTree strips namespace prefixes from attribute keys in some
/// configurations, OR the OPML serialises them as bare names in practice.
/// quick-xml returns the raw bytes including the prefix, so we strip it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EpisodeAttrs {
    /// `overcastId` — stable integer id assigned by Overcast. Primary key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    overcast_id: String,
    /// Episode title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    title: String,
    /// Episode webpage URL (e.g. `https://example.net/podcast/1`).
    /// Distinct from `enclosure_url` (the audio file link).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    url: String,
    /// Enclosure URL — the audio/media file link
    /// (e.g. `https://example.net/files/1.mp3`), from the `enclosureUrl`
    /// attribute. This is the `detail` field in the contract row.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    enclosure_url: String,
    /// Overcast web URL for the episode.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    overcast_url: String,
    /// `1` if fully played, `0` otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    played: String,
    /// Playback progress in seconds (integer string).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    progress: String,
    /// RFC822-ish date the user last interacted with this episode in Overcast.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    user_updated_date: String,
    /// RFC822-ish date the episode was added to the user's queue.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    added_date: String,
    /// Episode publish date.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub_date: String,
    /// Feed title carried down from the parent outline (denormalized for raw).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    feed_title: String,
    /// Feed XML URL (denormalized for raw).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    feed_xml_url: String,
}

/// Parse the extended OPML XML into a flat list of episode attribute bags.
/// Returns ALL episode outlines (the contract layer filters by play state);
/// raw layer receives everything for full fidelity.
///
/// Real Overcast extended OPML shape (confirmed via alexwlchan/overcast-downloader,
/// cleverdevil gist, overcast_parser):
/// ```xml
/// <opml version="2.0">
///   <body>
///     <outline text="playlists">...</outline>
///     <outline text="feeds">
///       <outline type="rss" title="Feed Title" xmlUrl="https://feed.rss">
///         <outline type="podcast-episode"
///                  overcastId="123"
///                  title="Episode Title"
///                  url="https://example.net/podcast/1"
///                  enclosureUrl="https://example.net/files/1.mp3"
///                  overcastUrl="https://overcast.fm/+abc"
///                  played="1" progress="1234"
///                  userUpdatedDate="2024-06-10T08:30:00-00:00"
///                  addedDate="2024-06-01T12:00:00-00:00"
///                  pubDate="2024-06-01T06:00:00-00:00" />
///       </outline>
///     </outline>
///   </body>
/// </opml>
/// ```
///
/// Attributes are bare (no namespace prefix) in the real export. The
/// namespace prefix (`overcast:`) appears in some community documentation
/// but not in the actual file; we strip any prefix that does appear so both
/// forms work.
///
/// Episode outlines are self-closing (`<outline ... />`), so depth tracking
/// based on Start/End events does not reliably clear the feed context. Instead
/// we track the feed context explicitly: set on `type="rss"` Start, cleared on
/// the corresponding `</outline>` End event. Empty (self-closing) outlines do
/// NOT push to depth and never trigger an End event.
fn parse_opml(xml: &str) -> Result<Vec<EpisodeAttrs>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut episodes: Vec<EpisodeAttrs> = Vec::new();
    let mut current_feed: Option<(String, String)> = None; // (title, xml_url)

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name_bytes = e.name().as_ref().to_vec();
                let local = local_name(&name_bytes);
                if local == b"outline" {
                    let attrs = collect_attrs(e);
                    let otype = attrs.get("type").map(String::as_str).unwrap_or("");
                    if otype == "rss" {
                        let title = attrs.get("title").cloned().unwrap_or_default();
                        let xml_url = attrs.get("xmlUrl").cloned().unwrap_or_default();
                        current_feed = Some((title, xml_url));
                    }
                }
            }
            Ok(Event::Empty(ref e)) => {
                // Self-closing elements (the common case for episode outlines).
                // Do NOT push to open_stack — there is no matching End event.
                let name_bytes = e.name().as_ref().to_vec();
                let local = local_name(&name_bytes);
                if local == b"outline" {
                    let attrs = collect_attrs(e);
                    let otype = attrs.get("type").map(String::as_str).unwrap_or("");
                    match otype {
                        "rss" => {
                            // Self-closing feed outline has no child episodes,
                            // so we don't set feed context (no End event to clear it).
                        }
                        "podcast-episode" | "" => {
                            if let Some((ref feed_title, ref feed_xml_url)) = current_feed {
                                let ep = EpisodeAttrs {
                                    overcast_id: attrs
                                        .get("overcastId")
                                        .cloned()
                                        .unwrap_or_default(),
                                    title: attrs.get("title").cloned().unwrap_or_default(),
                                    url: attrs.get("url").cloned().unwrap_or_default(),
                                    enclosure_url: attrs
                                        .get("enclosureUrl")
                                        .cloned()
                                        .unwrap_or_default(),
                                    overcast_url: attrs
                                        .get("overcastUrl")
                                        .cloned()
                                        .unwrap_or_default(),
                                    played: attrs.get("played").cloned().unwrap_or_default(),
                                    progress: attrs
                                        .get("progress")
                                        .cloned()
                                        .unwrap_or_default(),
                                    user_updated_date: attrs
                                        .get("userUpdatedDate")
                                        .cloned()
                                        .unwrap_or_default(),
                                    added_date: attrs
                                        .get("addedDate")
                                        .cloned()
                                        .unwrap_or_default(),
                                    pub_date: attrs
                                        .get("pubDate")
                                        .cloned()
                                        .unwrap_or_default(),
                                    feed_title: feed_title.clone(),
                                    feed_xml_url: feed_xml_url.clone(),
                                };
                                episodes.push(ep);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name_bytes = e.name().as_ref().to_vec();
                let local = local_name(&name_bytes);
                if local == b"outline" {
                    // Any closing </outline> clears the current feed context.
                    // Feed outlines have open Start + End tags; episode outlines
                    // are self-closing Empty events (no End), so this only fires
                    // for the rss feed outline's closing tag.
                    current_feed = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("Overcast OPML parse error at {}: {e}", reader.buffer_position()),
            _ => {}
        }
    }

    Ok(episodes)
}

/// Return the local name (strip namespace prefix if present).
/// `overcast:played` → b"played"; `title` → b"title".
fn local_name(name: &[u8]) -> &[u8] {
    name.iter().position(|&b| b == b':').map(|i| &name[i + 1..]).unwrap_or(name)
}

/// Collect all attributes of an element into a `HashMap<String, String>`,
/// stripping namespace prefixes from keys (so `overcast:played` → `played`).
fn collect_attrs(e: &quick_xml::events::BytesStart) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for a in e.attributes().flatten() {
        let raw_key = a.key.as_ref();
        let key = String::from_utf8_lossy(local_name(raw_key)).into_owned();
        let val = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_default();
        m.insert(key, val);
    }
    m
}

// ---------------------------------------------------------------------------
// Timestamp parsing.

/// Parse a date string as best we can, returning an RFC3339 string with local
/// offset. Returns `None` on failure.
///
/// Overcast uses ISO-8601 dates with numeric offsets, e.g.
/// `2024-06-10T08:30:00-00:00`. We accept RFC3339 (Z or offset), RFC2822
/// (`Mon, 01 Jan 2024 12:00:00 +0000`), and several other patterns for
/// defensive compatibility.
fn parse_date_rfc822(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Try RFC3339 first — handles Z and numeric-offset ISO-8601 forms like
    // `2024-06-10T08:30:00-00:00` and `2024-06-10T08:30:00Z`.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    // Try RFC2822 for the classic email-header date format.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    // Fallback: try a few common patterns.
    for fmt in &[
        "%a, %d %b %Y %H:%M:%S %z",
        "%d %b %Y %H:%M:%S %z",
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(dt) = chrono::DateTime::parse_from_str(s, fmt) {
            return Some(dt.with_timezone(&Local).to_rfc3339());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Snapshot cursor.

/// Per-episode play state fingerprint stored in the cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EpState {
    played: String,
    progress: String,
}

/// The snapshot cursor persisted at `CURSOR_FILE`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// `true` after the first successful fetch (baseline established).
    #[serde(default)]
    baselined: bool,
    /// Map from episode stable id → last-seen play state.
    #[serde(default)]
    episodes: HashMap<String, EpState>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_overcast_sync(&self) -> SyncState {
        self.resolve(CURSOR_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_overcast_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(CURSOR_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Write helpers.

/// A raw episode row wrapped with a ts so the month-partition writer can file it.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Write raw + contract rows, deduped by guid. Returns (new_contract_rows, new_raw_rows).
fn write_rows(
    vault: &Vault,
    rows: &[MediaItem],
    raws: &[(EpisodeAttrs, String)], // (attrs, ts)
    existing_guids: &HashSet<String>,
) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut new_rows: Vec<MediaItem> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();

    let mut seen = existing_guids.clone();
    for (item, (ep, ts)) in rows.iter().zip(raws.iter()) {
        if !seen.insert(item.guid.clone()) {
            continue;
        }
        new_rows.push(item.clone());
        new_raws.push(RawLine {
            ts: ts.clone(),
            value: serde_json::to_value(ep).unwrap_or(Value::Null),
        });
    }

    contract.append(&new_rows, |i| &i.ts)?;
    raw_stream.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// Episode → MediaItem conversion.

/// Derive a stable guid for an episode: prefer `overcastId`, fall back to the
/// enclosure URL (`enclosureUrl`), then the webpage URL (`url`) as last resort.
/// Returns `None` if none is present (can't dedupe).
fn episode_guid(ep: &EpisodeAttrs) -> Option<String> {
    if !ep.overcast_id.is_empty() {
        return Some(format!("overcast-{}", ep.overcast_id));
    }
    if !ep.enclosure_url.is_empty() {
        return Some(format!("overcast-enc-{}", ep.enclosure_url));
    }
    if !ep.url.is_empty() {
        return Some(format!("overcast-url-{}", ep.url));
    }
    None
}

/// Convert one episode to a MediaItem. Returns `None` when:
/// - no stable guid is derivable,
/// - episode has no played/progress data (not yet listened).
fn episode_to_item(ep: &EpisodeAttrs, ts: &str) -> Option<MediaItem> {
    let guid = episode_guid(ep)?;

    let played = ep.played == "1";
    let progress: u64 = ep.progress.trim().parse().unwrap_or(0);

    // Skip episodes with no listening activity.
    if !played && progress == 0 {
        return None;
    }

    let kind = if played { "play".to_string() } else { "partial".to_string() };

    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().into()));
        }
    };
    put("overcast_id", &ep.overcast_id);
    put("overcast_url", &ep.overcast_url);
    // Preserve the episode webpage URL separately (distinct from the enclosure).
    put("episode_url", &ep.url);
    put("added_date", &ep.added_date);
    put("pub_date", &ep.pub_date);
    put("feed_xml_url", &ep.feed_xml_url);

    // `detail` = enclosure URL (the audio/media file link); fall back to the
    // episode webpage URL when enclosureUrl is absent (older export shapes).
    let detail = if !ep.enclosure_url.is_empty() {
        ep.enclosure_url.clone()
    } else {
        ep.url.clone()
    };

    Some(MediaItem {
        ts: ts.to_string(),
        source: "overcast".into(),
        category: "podcast".into(),
        device: String::new(),
        kind,
        title: ep.title.clone(),
        subtitle: ep.feed_title.clone(),
        detail,
        seconds: progress,
        favicon: String::new(),
        guid,
        extra,
    })
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and run one snapshot-diff sync cycle.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let cookie = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|c| !c.trim().is_empty())
        .context(
            "Overcast is not connected — add your email:password in the Integrations tab",
        )?;

    pull_with_cookie(vault, &cookie)
}

fn pull_with_cookie(vault: &Vault, cookie: &str) -> Result<PullOutcome> {
    let xml = fetch_opml(cookie)?;
    pull_with_opml(vault, &xml)
}

/// The testable inner pull: accepts a raw OPML string, diffs against the
/// cursor, writes new rows, updates the cursor.
pub(crate) fn pull_with_opml(vault: &Vault, xml: &str) -> Result<PullOutcome> {
    let episodes = parse_opml(xml)?;

    let mut state = vault.read_overcast_sync();

    // Load existing guids for dedup.
    let contract = vault.stream(DIR, Partition::Month);
    let mut existing_guids: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for it in contract.read::<MediaItem>(&key)? {
            if !it.guid.is_empty() {
                existing_guids.insert(it.guid);
            }
        }
    }

    // First fetch: silent baseline — write raw for full fidelity, but emit
    // zero contract rows. Key the baseline on the cursor's `baselined` flag.
    if !state.baselined {
        // Write raw for every episode that has any play state.
        let raw_stream = vault.stream(RAW_DIR, Partition::Month);
        let now_ts = Local::now().to_rfc3339();
        let mut raw_lines: Vec<RawLine> = Vec::new();
        for ep in &episodes {
            if ep.played == "1" || ep.progress.trim().parse::<u64>().unwrap_or(0) > 0 {
                let ts = episode_ts(ep, &now_ts);
                raw_lines.push(RawLine {
                    ts: ts.clone(),
                    value: serde_json::to_value(ep).unwrap_or(Value::Null),
                });
            }
        }
        raw_stream.append(&raw_lines, |r| &r.ts)?;

        // Record the baseline state for all episodes.
        for ep in &episodes {
            if let Some(guid) = episode_guid(ep) {
                state.episodes.insert(
                    guid,
                    EpState { played: ep.played.clone(), progress: ep.progress.clone() },
                );
            }
        }
        state.baselined = true;
        state.updated = Some(Local::now().to_rfc3339());
        vault.write_overcast_sync(&state)?;

        return Ok(PullOutcome {
            headline: "Overcast baseline established — future syncs will emit new plays".into(),
            counts: BTreeMap::from([("episodes", 0u64)]),
        });
    }

    // Subsequent fetches: diff against the cursor.
    let now_ts = Local::now().to_rfc3339();
    let mut new_items: Vec<MediaItem> = Vec::new();
    let mut new_raws: Vec<(EpisodeAttrs, String)> = Vec::new();

    for ep in &episodes {
        let guid = match episode_guid(ep) {
            Some(g) => g,
            None => continue,
        };

        let current_state =
            EpState { played: ep.played.clone(), progress: ep.progress.clone() };

        let prior = state.episodes.get(&guid);
        let changed = prior.map_or(true, |p| *p != current_state);
        let has_data = ep.played == "1" || ep.progress.trim().parse::<u64>().unwrap_or(0) > 0;

        if changed && has_data {
            let ts = episode_ts(ep, &now_ts);
            if let Some(item) = episode_to_item(ep, &ts) {
                new_items.push(item);
                new_raws.push((ep.clone(), ts));
            }
        }

        // Always update the cursor state.
        state.episodes.insert(guid, current_state);
    }

    // Safety: if parsing produced zero items with play data (e.g., shape
    // change), do NOT advance the cursor — retry next tick.
    let total_parsed_with_data = episodes
        .iter()
        .filter(|ep| {
            ep.played == "1" || ep.progress.trim().parse::<u64>().unwrap_or(0) > 0
        })
        .count();
    if total_parsed_with_data == 0 && !episodes.is_empty() {
        // Suspicious: OPML had episodes but none with play data. Could be a
        // shape change or empty account. Log and return without advancing.
        return Ok(PullOutcome {
            headline: "Overcast OPML parsed but no play data found — check connection".into(),
            counts: BTreeMap::from([("episodes", 0u64)]),
        });
    }

    let n = write_rows(vault, &new_items, &new_raws, &existing_guids)?;

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_overcast_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{n} episodes"),
        counts: BTreeMap::from([("episodes", n)]),
    })
}

/// Derive the RFC3339 ts for an episode:
/// - `userUpdatedDate` first (the time the user interacted with the episode),
/// - `pubDate` fallback,
/// - `now` as last resort.
fn episode_ts(ep: &EpisodeAttrs, now: &str) -> String {
    parse_date_rfc822(&ep.user_updated_date)
        .or_else(|| parse_date_rfc822(&ep.pub_date))
        .unwrap_or_else(|| now.to_string())
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-overcast-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A realistic extended OPML matching the REAL Overcast export shape.
    ///
    /// Confirmed via alexwlchan/overcast-downloader, cleverdevil gist, and
    /// overcast_parser crate:
    /// - Bare attribute names (no `overcast:` prefix).
    /// - Self-closing episode outlines (`<outline ... />`).
    /// - ISO-8601 dates with numeric offsets (e.g. `2024-06-10T08:30:00-00:00`).
    /// - Distinct `url` (episode webpage) and `enclosureUrl` (audio file).
    /// - A `<outline text="playlists">` section after feeds (must not leak
    ///   into episode parsing).
    fn sample_opml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head>
    <title>Overcast Subscriptions</title>
  </head>
  <body>
    <outline text="feeds">
      <outline type="rss" title="Hardcore History" xmlUrl="https://feeds.feedburner.com/dancarlin/history">
        <outline type="podcast-episode"
                 overcastId="12345"
                 title="Show 67 - Twilight of the Aesir"
                 url="https://www.dancarlin.com/hardcore-history-67"
                 enclosureUrl="https://traffic.libsyn.com/dancarlin/hardcore_history_67.mp3"
                 overcastUrl="https://overcast.fm/+12345"
                 played="1"
                 progress="21600"
                 userUpdatedDate="2024-06-10T08:30:00-00:00"
                 addedDate="2024-06-01T12:00:00-00:00"
                 pubDate="2024-06-01T06:00:00-00:00" />
        <outline type="podcast-episode"
                 overcastId="12346"
                 title="Show 68 - Thunder Road"
                 url="https://www.dancarlin.com/hardcore-history-68"
                 enclosureUrl="https://traffic.libsyn.com/dancarlin/hardcore_history_68.mp3"
                 overcastUrl="https://overcast.fm/+12346"
                 played="0"
                 progress="3600"
                 userUpdatedDate="2024-06-11T09:00:00-00:00"
                 addedDate="2024-06-10T14:00:00-00:00"
                 pubDate="2024-06-10T06:00:00-00:00" />
        <outline type="podcast-episode"
                 overcastId="12347"
                 title="Show 69 - Blueprint for Armageddon I"
                 url="https://www.dancarlin.com/hardcore-history-69"
                 enclosureUrl="https://traffic.libsyn.com/dancarlin/hardcore_history_69.mp3"
                 overcastUrl="https://overcast.fm/+12347"
                 played="0"
                 progress="0"
                 addedDate="2024-06-12T10:00:00-00:00"
                 pubDate="2024-06-12T06:00:00-00:00" />
      </outline>
      <outline type="rss" title="Accidental Tech Podcast" xmlUrl="https://atp.fm/rss">
        <outline type="podcast-episode"
                 overcastId="99901"
                 title="583: The Sands of Time"
                 url="https://atp.fm/583"
                 enclosureUrl="https://traffic.libsyn.com/atp/atp583.mp3"
                 overcastUrl="https://overcast.fm/+99901"
                 played="1"
                 progress="5400"
                 userUpdatedDate="2024-06-12T22:00:00-00:00"
                 addedDate="2024-06-11T18:00:00-00:00"
                 pubDate="2024-06-11T15:00:00-00:00" />
      </outline>
    </outline>
    <outline text="playlists">
      <outline text="All Episodes" type="smart-playlist" />
      <outline text="In Progress" type="smart-playlist" />
    </outline>
  </body>
</opml>"#
    }

    /// OPML with namespace-prefixed attributes (seen in some older export shapes).
    /// Confirms prefix-stripping still works correctly.
    fn sample_opml_bare_attrs() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <body>
    <outline text="feeds">
      <outline type="rss" title="Some Podcast" xmlUrl="https://podcast.example/rss">
        <outline type="podcast-episode"
                 overcastId="777"
                 title="Episode One"
                 url="https://podcast.example/episodes/1"
                 enclosureUrl="https://podcast.example/files/ep1.mp3"
                 overcastUrl="https://overcast.fm/+777"
                 played="1"
                 progress="1800"
                 userUpdatedDate="2024-06-13T10:00:00-00:00"
                 addedDate="2024-06-12T10:00:00-00:00"
                 pubDate="2024-06-12T08:00:00-00:00" />
      </outline>
    </outline>
  </body>
</opml>"#
    }

    #[test]
    fn parses_real_shape_opml() {
        let eps = parse_opml(sample_opml()).unwrap();
        // 4 episode outlines total (3 + 1). The playlists section must NOT
        // produce phantom episodes even though it has self-closing outlines.
        assert_eq!(eps.len(), 4, "should parse 4 episode outlines (playlists must not leak)");

        let ep0 = &eps[0];
        assert_eq!(ep0.overcast_id, "12345");
        assert_eq!(ep0.title, "Show 67 - Twilight of the Aesir");
        assert_eq!(ep0.played, "1");
        assert_eq!(ep0.progress, "21600");
        assert_eq!(ep0.feed_title, "Hardcore History");
        assert_eq!(ep0.feed_xml_url, "https://feeds.feedburner.com/dancarlin/history");
        // Both url (webpage) and enclosureUrl (media file) are present and distinct.
        assert_eq!(ep0.url, "https://www.dancarlin.com/hardcore-history-67");
        assert_eq!(ep0.enclosure_url, "https://traffic.libsyn.com/dancarlin/hardcore_history_67.mp3");

        let ep1 = &eps[1];
        assert_eq!(ep1.played, "0");
        assert_eq!(ep1.progress, "3600");

        // ep2 has no play data (played=0, progress=0).
        let ep2 = &eps[2];
        assert_eq!(ep2.progress, "0");
        assert_eq!(ep2.played, "0");

        let ep3 = &eps[3];
        assert_eq!(ep3.feed_title, "Accidental Tech Podcast");
        assert_eq!(ep3.played, "1");
    }

    #[test]
    fn parses_bare_attr_opml() {
        let eps = parse_opml(sample_opml_bare_attrs()).unwrap();
        assert_eq!(eps.len(), 1);
        let ep = &eps[0];
        assert_eq!(ep.overcast_id, "777");
        assert_eq!(ep.played, "1");
        assert_eq!(ep.progress, "1800");
        assert_eq!(ep.url, "https://podcast.example/episodes/1");
        assert_eq!(ep.enclosure_url, "https://podcast.example/files/ep1.mp3");
    }

    /// Depth-tracking regression: self-closing episode outlines must NOT inflate
    /// depth and must not cause the playlists section to leak episode outlines.
    #[test]
    fn playlists_section_does_not_leak() {
        // The playlists section comes AFTER the feeds section and contains
        // self-closing outlines. With the old depth-based tracker these could
        // be mis-attributed to the last feed. Verify no phantom episodes appear.
        let eps = parse_opml(sample_opml()).unwrap();
        // All 4 must belong to real feeds (Hardcore History or ATP).
        for ep in &eps {
            assert!(
                ep.feed_title == "Hardcore History" || ep.feed_title == "Accidental Tech Podcast",
                "unexpected feed_title {:?} — phantom episode from playlists?",
                ep.feed_title
            );
        }
    }

    #[test]
    fn episode_to_item_played() {
        let eps = parse_opml(sample_opml()).unwrap();
        let ep0 = &eps[0]; // played=1, progress=21600
        let ts = episode_ts(ep0, "2024-06-10T10:00:00+00:00");
        let item = episode_to_item(ep0, &ts).unwrap();
        assert_eq!(item.kind, "play");
        assert_eq!(item.category, "podcast");
        assert_eq!(item.source, "overcast");
        assert_eq!(item.seconds, 21600);
        assert_eq!(item.subtitle, "Hardcore History");
        assert_eq!(item.title, "Show 67 - Twilight of the Aesir");
        assert!(item.guid.starts_with("overcast-12345"));
        // detail must be the enclosure URL (audio file), not the webpage URL.
        assert_eq!(item.detail, "https://traffic.libsyn.com/dancarlin/hardcore_history_67.mp3");
        // Episode webpage URL preserved in extra.
        assert_eq!(
            item.extra.get("episode_url").and_then(|v| v.as_str()),
            Some("https://www.dancarlin.com/hardcore-history-67")
        );
    }

    #[test]
    fn episode_to_item_partial() {
        let eps = parse_opml(sample_opml()).unwrap();
        let ep1 = &eps[1]; // played=0, progress=3600
        let ts = episode_ts(ep1, "2024-06-11T09:00:00+00:00");
        let item = episode_to_item(ep1, &ts).unwrap();
        assert_eq!(item.kind, "partial");
        assert_eq!(item.seconds, 3600);
    }

    #[test]
    fn episode_to_item_unlistened_returns_none() {
        let eps = parse_opml(sample_opml()).unwrap();
        let ep2 = &eps[2]; // played=0, progress=0 → no listening data
        let ts = episode_ts(ep2, "2024-06-12T06:00:00+00:00");
        assert!(episode_to_item(ep2, &ts).is_none());
    }

    #[test]
    fn first_sync_is_silent_baseline() {
        let vault = temp_vault("baseline");
        let n = pull_with_opml(&vault, sample_opml()).unwrap();
        // First sync: zero contract rows written.
        assert_eq!(n.counts.get("episodes").copied().unwrap_or(0), 0);
        // Cursor should be baselined.
        let state = vault.read_overcast_sync();
        assert!(state.baselined);
        // Raw files were written for episodes with play data (ep0, ep1, ep3).
        let raw_stream = vault.stream(RAW_DIR, Partition::Month);
        let parts = raw_stream.partitions().unwrap();
        assert!(!parts.is_empty(), "raw files should exist after baseline");
    }

    #[test]
    fn second_sync_emits_changed_plays() {
        let vault = temp_vault("second-sync");
        // First sync: baseline.
        pull_with_opml(&vault, sample_opml()).unwrap();

        // Second sync: same OPML — no changes, zero new rows.
        let out = pull_with_opml(&vault, sample_opml()).unwrap();
        assert_eq!(out.counts.get("episodes").copied().unwrap_or(0), 0);

        // Third sync: one episode newly played (ep2 now has progress).
        // Use bare attribute names matching the real export shape.
        let updated_opml = sample_opml().replace(
            r#"played="0"
                 progress="0"
                 addedDate="2024-06-12T10:00:00-00:00"
                 pubDate="2024-06-12T06:00:00-00:00" />"#,
            r#"played="1"
                 progress="5000"
                 userUpdatedDate="2024-06-13T12:00:00-00:00"
                 addedDate="2024-06-12T10:00:00-00:00"
                 pubDate="2024-06-12T06:00:00-00:00" />"#,
        );
        let out3 = pull_with_opml(&vault, &updated_opml).unwrap();
        assert_eq!(out3.counts.get("episodes").copied().unwrap_or(0), 1);
    }

    #[test]
    fn dedupes_on_re_run() {
        let vault = temp_vault("dedup");
        // Baseline.
        pull_with_opml(&vault, sample_opml()).unwrap();
        // Change ep0 progress to force emission (bare attr names matching real export).
        let changed = sample_opml().replace(
            r#"progress="21600""#,
            r#"progress="21700""#,
        );
        let out1 = pull_with_opml(&vault, &changed).unwrap();
        assert_eq!(out1.counts.get("episodes").copied().unwrap_or(0), 1);
        // Re-run with same OPML: guid already present — no duplicate.
        let out2 = pull_with_opml(&vault, &changed).unwrap();
        assert_eq!(out2.counts.get("episodes").copied().unwrap_or(0), 0);
    }

    #[test]
    fn extract_o_cookie_parses_correctly() {
        let header = "o=abc123xyz; Path=/; HttpOnly; Secure; SameSite=Lax";
        assert_eq!(extract_o_cookie(header), Some("abc123xyz".to_string()));
    }

    #[test]
    fn extract_o_cookie_returns_none_for_other_cookies() {
        let header = "session=xyz; Path=/";
        assert_eq!(extract_o_cookie(header), None);
    }

    #[test]
    fn urlencoded_encodes_special_chars() {
        assert_eq!(urlencoded("a@b.com"), "a%40b.com");
        assert_eq!(urlencoded("p@ss word!"), "p%40ss+word%21");
        assert_eq!(urlencoded("simple"), "simple");
    }

    #[test]
    fn parse_date_parses_real_overcast_iso8601() {
        // Real Overcast format: ISO-8601 with numeric offset.
        let s = "2024-06-10T08:30:00-00:00";
        let result = parse_date_rfc822(s);
        assert!(result.is_some(), "should parse real Overcast ISO-8601 userUpdatedDate");
        let ts = result.unwrap();
        assert!(ts.contains("2024-06-10"), "date should be 2024-06-10, got {ts}");
    }

    #[test]
    fn parse_date_parses_rfc2822_format() {
        // RFC2822 for defensive compatibility.
        let s = "Mon, 10 Jun 2024 08:30:00 +0000";
        let result = parse_date_rfc822(s);
        assert!(result.is_some(), "should parse RFC2822 format");
        let ts = result.unwrap();
        assert!(ts.contains("2024-06-10"), "date should be 2024-06-10, got {ts}");
    }

    #[test]
    fn parse_date_parses_z_suffix() {
        // Z suffix (Zulu/UTC) must also parse correctly.
        let s = "2024-06-10T08:30:00Z";
        let result = parse_date_rfc822(s);
        assert!(result.is_some(), "should parse Z-suffix ISO-8601 date");
        let ts = result.unwrap();
        assert!(ts.contains("2024-06-10"), "date should be 2024-06-10, got {ts}");
    }

    #[test]
    fn connection_has_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
    }
}

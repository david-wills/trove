//! Audible audiobook library and listening history.
//! Brief: docs/integrations/audible.md.
//!
//! A **Periodic** cloud pull: Audible's unofficial internal REST API is
//! polled for library snapshots; raw items are written unconditionally.
//!
//! # Parser status: PARKED — needs real sample
//!
//! The position-delta engine that would derive media-plays contract rows from
//! successive snapshots is **parked** (PARSER_ACTIVE = false).  The reason:
//!
//! - `last_position_heard` is NOT a valid `response_group` for `GET /1.0/library`
//!   (it is only valid on `/1.0/content/{asin}/licenserequest`).  Verified
//!   against mkb79/Audible docs/source/misc/external_api.rst.
//! - The valid progress-related response_groups for `/1.0/library` are
//!   `listening_status` and `percent_complete`, but neither their field names
//!   nor their nested shapes are documented in the primary source.
//! - Without a real captured library response we cannot assert the right key
//!   paths; shipping an active parser wired to invented field names would
//!   silently write zero contract rows while advancing the cursor.
//!
//! Until a real account spike captures a sample and confirms the field paths,
//! the pull writes only the raw layer (full library snapshot every hour).
//! The contract-layer derivation will be activated once the field names are
//! confirmed.
//!
//! Two vault layers:
//!
//! - **raw** — full library API response items at
//!   `media/audible/raw/YYYY-MM.jsonl`, partitioned by the poll month.
//!   Each line includes a `_polled_at` RFC3339 field so snapshots are
//!   timestamped and the watermark is rebuildable from raw.
//! - **contract** (PARKED) — will derive listening spans at
//!   `media/plays/audible/YYYY-MM.jsonl` per the media-plays write contract
//!   once the progress-field shape is confirmed by a real-account spike.
//!
//! # Authentication
//!
//! Audible uses Amazon's device-registration flow: a browser-step OAuth login
//! yields an authorization code, which is exchanged for tokens via a device
//! registration endpoint (`POST /auth/register`) that returns an access token,
//! refresh token, ADP token, and RSA device private key.  Subsequent requests
//! must be signed with the RSA key + ADP token (`x-adp-token`,
//! `x-adp-alg`, `x-adp-signature` headers).
//!
//! Reimplementing this auth fully in Rust requires a real Audible account to
//! spike and validate the flow.  The `CONNECTION` here accepts a pre-serialized
//! JSON credential blob from a separate spike/helper tool (stored in the
//! `access_token` slot of the vault's TokenSet) so the raw-layer collector
//! can be built and tested independently.  See [`AudibleCreds`] for the
//! expected JSON shape.
//!
//! **Needs-David flag**: completing the auth spike requires a real Audible
//! account.  The pull degrades gracefully to a no-op when not connected.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

/// Raw library snapshot directory.
const RAW_DIR: &str = "media/audible/raw";
/// Service key under `.trove/sync/` for the stored credential blob.
const SERVICE: &str = "audible";

/// Default API base URL (US marketplace).
const DEFAULT_API_BASE: &str = "https://api.audible.com";

/// Max items per library page.
const PAGE_SIZE: u32 = 1000;
/// HTTP timeout; kept short so a hung connection doesn't stall the watcher.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs in the watcher loop.  Hourly — library data changes
/// relatively slowly and the progress-field shape is unconfirmed.
pub const AUDIBLE_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(RAW_DIR))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("library_items").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("audible synced — {n} library items snapshotted (parser parked: needs real sample)")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "audible sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("library_items").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Audible is up to date — library snapshot written (parser parked: needs real sample)".to_string()
    } else {
        format!("Audible synced — {n} library items snapshotted (parser parked: needs real sample)")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "audible",
        name: "Audible",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Your Audible library snapshotted via Audible's device API. \
                      Raw library data is collected; listening-span derivation is \
                      parked pending a real-account sample to confirm progress field names.",
        domain: "media",
        vault_path: "media/audible/raw/",
        toggleable: true,
        setup: &[
            "Connect with your Audible account credentials on this card.",
            "First sync snapshots your library. Listening-span derivation will be \
             activated once the progress-field shape is confirmed by a real-account spike.",
        ],
        caveats: "Uses an unofficial device-registration API (reverse-engineered, same \
                  as Libation/OpenAudible); Amazon may change this without notice. \
                  Only raw library snapshots are collected until the progress field \
                  shape is confirmed. A stable internet connection is required at sync time.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(AUDIBLE_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("audible"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection.
//
// The full Amazon device-registration flow (OAuth login → PKCE code →
// POST /auth/register → RSA-signed requests) requires a dedicated spike with
// a real account.  For now the CONNECTION accepts a pre-serialized JSON
// credential blob (produced by an external spike/helper) pasted into the
// `access_token` field of the vault's TokenSet.  The JSON blob shape is
// [`AudibleCreds`] serialized to a string.
//
// The `help` text explains what form this credential takes and points at the
// external path until the spike is complete.

fn def_connect(vault: &Vault, cred_json: &str) -> Result<()> {
    let cred_json = cred_json.trim();
    if cred_json.is_empty() {
        bail!("empty credential — paste the JSON credential blob");
    }
    // Validate the shape so the user gets an early clear error.
    let creds: AudibleCreds = serde_json::from_str(cred_json)
        .context("invalid credential blob — must be a JSON object with access_token, \
                  refresh_token, adp_token, device_private_key, and api_url")?;
    if creds.access_token.trim().is_empty() {
        bail!("access_token is empty in the credential blob");
    }
    if creds.adp_token.trim().is_empty() {
        bail!("adp_token is empty in the credential blob");
    }
    // Store the whole JSON blob in the access_token slot.  The blob carries
    // the full typed shape including the adp_token and RSA key that the
    // normal TokenSet has no fields for.
    let tok = crate::sync::oauth::TokenSet {
        access_token: cred_json.to_string(),
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
    if let Some(tok) = vault.load_sync_token(SERVICE)? {
        let label = if let Ok(creds) = serde_json::from_str::<AudibleCreds>(&tok.access_token) {
            // Show the marketplace as the account label.
            format!("Audible ({})", creds.api_url.trim_start_matches("https://api."))
        } else {
            "Audible (connected)".to_string()
        };
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
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
    id: "audible",
    display_name: "Audible",
    methods: &[ConnectMethod::TokenPaste {
        label: "Audible credential blob (JSON)",
        help: "Paste the JSON credential blob produced by the audible-auth spike tool. \
               The blob must contain access_token, refresh_token, adp_token, \
               device_private_key, and api_url fields obtained from Audible's \
               device-registration flow.",
        placeholder: "{\"access_token\":\"...\",\"adp_token\":\"...\"}",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["audible"],
    setup: &[
        "Run the audible-auth spike tool with your Audible username and password to obtain the credential blob.",
        "Paste the JSON blob here. Your actual credentials are never stored — only the resulting device tokens.",
        "Audible's device-registration flow (used by Libation and OpenAudible) is not an official API; \
         Amazon may change it without notice.",
    ],
};

// ---------------------------------------------------------------------------
// Credential blob (stored in the TokenSet's access_token field as JSON).

/// The shape of the credential JSON blob stored in the vault.  This mirrors
/// the token fields Audible's device-registration returns, plus the
/// marketplace API base URL so requests use the right regional endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleCreds {
    /// Short-lived bearer token (60 min).  Used in `Authorization: Bearer`.
    pub access_token: String,
    /// Long-lived token for refreshing the access token.
    pub refresh_token: String,
    /// ADP token for RSA-signed requests.
    pub adp_token: String,
    /// PEM-encoded RSA device private key for request signing.
    pub device_private_key: String,
    /// Marketplace API base URL, e.g. `https://api.audible.com` (US) or
    /// `https://api.audible.co.uk` (UK).
    pub api_url: String,
}

// ---------------------------------------------------------------------------
// Library item shape (from the Audible /1.0/library API).
//
// NOTE: progress fields (listening_status, percent_complete) are intentionally
// captured in `extra` rather than typed struct fields.  The valid response
// groups for /1.0/library are: listening_status and percent_complete (NOT
// last_position_heard, which is only valid on /1.0/content/{asin}/licenserequest).
// The exact field names/shapes inside listening_status and percent_complete
// are undocumented in the primary source (mkb79/Audible external_api.rst) and
// have not been confirmed against a real API response.  Until a spike captures
// the real shape, all fields land in `extra` for full raw fidelity.

/// One Audible library item as returned by the API.
/// Unknown / progress fields land in `extra` for full fidelity in the raw layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryItem {
    pub asin: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<Author>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purchase_date: Option<String>,
    /// Whether the title has been fully consumed (from is_finished response_group).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_finished: bool,
    /// Runtime in minutes (from product_attrs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_length_min: Option<u64>,
    /// All remaining fields (including listening_status, percent_complete)
    /// captured for full raw fidelity; shapes unconfirmed until real-account spike.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
}

// ---------------------------------------------------------------------------
// Raw snapshot row: embeds the poll timestamp so snapshots are dated and the
// watermark is rebuildable from raw.

/// One raw library item with its observation timestamp embedded.
/// `_polled_at` is a real serialized field (RFC3339) so each raw row carries
/// the poll time; the `ts` field is also used (without serializing separately)
/// to drive the store partition key.
#[derive(Serialize)]
struct RawLine {
    /// RFC3339 poll timestamp — written into every raw row so the cursor is
    /// rebuildable from raw snapshots.  Also used as the partition key by the
    /// store (via the `ts` accessor below).
    #[serde(rename = "_polled_at")]
    ts: String,
    /// The full API item, flattened into the row alongside `_polled_at`.
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Library parser — pure, testable, no network.

/// Parse a raw library response body (Value) into a list of [`LibraryItem`]s.
/// The API wraps items under `items` or (older shape) `library`.
pub fn parse_library(body: &Value) -> Vec<LibraryItem> {
    let arr = body
        .get("items")
        .or_else(|| body.get("library"))
        .and_then(Value::as_array);
    let Some(arr) = arr else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for v in arr {
        match serde_json::from_value::<LibraryItem>(v.clone()) {
            Ok(item) if !item.asin.is_empty() => out.push(item),
            _ => {} // skip unparseable items
        }
    }
    out
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable for tests).

/// Fetches one page of the Audible library.
///
/// `NOTE` — the actual HTTP implementation here is a **scaffold**: it
/// constructs the request URL and response_groups string correctly per the
/// documented API but the RSA-signed `x-adp-*` header generation is deferred
/// (it requires the device's RSA private key and correct SHA256withRSA signing,
/// which needs a real account to validate).
///
/// The injectable [`LibraryFetcher`] trait lets the raw-layer collector be
/// fully tested with fixture data regardless of the auth stub.
pub trait LibraryFetcher {
    fn fetch_library(&self, page: u32) -> Result<Value>;
}

/// Real HTTP client.  Auth is bearer only (access_token as Bearer) since the
/// RSA-signing path is parked; this may work for some Audible account types
/// but is not validated against a real account.
struct AudibleClient {
    api_base: String,
    access_token: String,
}

impl AudibleClient {
    fn new(api_base: String, access_token: String) -> Self {
        AudibleClient { api_base, access_token }
    }
}

impl LibraryFetcher for AudibleClient {
    fn fetch_library(&self, page: u32) -> Result<Value> {
        let url = format!("{}/1.0/library", self.api_base);
        // response_groups: only groups valid for /1.0/library per mkb79/Audible docs.
        // listening_status and percent_complete are included for progress data;
        // last_position_heard is NOT valid here (only on licenserequest endpoint).
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.access_token))
            .set("Accept", "application/json")
            .query("response_groups", "product_desc,product_attrs,contributors,listening_status,is_finished,percent_complete")
            .query("num_results", &PAGE_SIZE.to_string())
            .query("page", &page.to_string())
            .query("sort_by", "-PurchaseDate")
            .call()
            .context("Audible library request failed")?;
        resp.into_json::<Value>().context("parsing Audible library response")
    }
}

// ---------------------------------------------------------------------------
// The pull (main logic).

/// Load the credential blob from the vault.  Returns an error if Audible is
/// not connected.
fn load_creds(vault: &Vault) -> Result<AudibleCreds> {
    let tok = vault
        .load_sync_token(SERVICE)?
        .context("Audible is not connected — paste your credential blob in the Integrations tab")?;
    serde_json::from_str::<AudibleCreds>(&tok.access_token)
        .context("stored Audible credential is malformed — reconnect with a fresh credential blob")
}

/// Pull the Audible library.  Runs with a real [`AudibleClient`]; the
/// testable seam is [`pull_with`].
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let creds = load_creds(vault)?;
    let api_base = if creds.api_url.trim().is_empty() {
        DEFAULT_API_BASE.to_string()
    } else {
        creds.api_url.trim().to_string()
    };
    let client = AudibleClient::new(api_base, creds.access_token.clone());
    pull_with(vault, &client)
}

/// Testable seam — the entire pull body over an injected fetcher.
pub fn pull_with(vault: &Vault, fetcher: &impl LibraryFetcher) -> Result<PullOutcome> {
    let poll_ts = Local::now().to_rfc3339();

    // Drain all pages.  Audible returns up to PAGE_SIZE items per page; we
    // stop when the RAW array length is < PAGE_SIZE (not the parsed count,
    // so a single unparseable item on a full page cannot truncate the drain).
    let mut all_items: Vec<LibraryItem> = Vec::new();
    let mut all_raws: Vec<Value> = Vec::new();
    let mut page: u32 = 1;
    loop {
        let body = fetcher.fetch_library(page)?;
        let items = parse_library(&body);
        // Collect raw values for the raw layer — use the raw array length
        // (before parsing) to decide pagination so parse drops do not stop drain early.
        let raw_arr = body
            .get("items")
            .or_else(|| body.get("library"))
            .and_then(Value::as_array);
        let raw_len = raw_arr.map(|a| a.len()).unwrap_or(0);
        if let Some(arr) = raw_arr {
            all_raws.extend(arr.iter().cloned());
        }
        let is_last = raw_len < PAGE_SIZE as usize;
        all_items.extend(items);
        if is_last {
            break;
        }
        page += 1;
    }

    // Write raw snapshot rows (full fidelity, unconditionally).
    // Each row embeds `_polled_at` so snapshots are dated and the watermark
    // is rebuildable from raw; the ts accessor drives the store partition key.
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let raw_rows: Vec<RawLine> =
        all_raws.into_iter().map(|v| RawLine { ts: poll_ts.clone(), value: v }).collect();
    raw_stream.append(&raw_rows, |r| &r.ts)?;

    // Contract-layer derivation (position-delta → media-plays spans) is PARKED.
    // Reason: the progress field shape (listening_status / percent_complete)
    // is undocumented in the primary source and unconfirmed against a real
    // account response.  Activate when a real spike confirms the field names.

    Ok(PullOutcome {
        headline: format!(
            "{} library items snapshotted (parser parked: needs real sample)",
            all_items.len()
        ),
        counts: BTreeMap::from([("library_items", all_items.len() as u64)]),
    })
}

// ---------------------------------------------------------------------------
// Tests — parser + raw layer (pure, no network, unique temp dirs).

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-audible-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Minimal library API response fixture using only documented /1.0/library
    /// response_groups (no last_position_heard — not valid on library endpoint).
    /// listening_status and percent_complete land in `extra` (shapes unconfirmed).
    fn sample_library() -> Value {
        serde_json::json!({
            "items": [
                {
                    "asin": "B002V5GK42",
                    "title": "The Hitchhiker's Guide to the Galaxy",
                    "authors": [{"name": "Douglas Adams", "asin": "B000AQ3OEI"}],
                    "purchase_date": "2023-04-01T00:00:00.000Z",
                    "is_finished": false,
                    "runtime_length_min": 333,
                    "percent_complete": 18,
                    "listening_status": {"is_downloaded": false}
                },
                {
                    "asin": "B08G9PRS1K",
                    "title": "Project Hail Mary",
                    "authors": [{"name": "Andy Weir"}],
                    "purchase_date": "2024-01-10T00:00:00.000Z",
                    "is_finished": false,
                    "runtime_length_min": 696,
                    "percent_complete": 0,
                    "listening_status": {"is_downloaded": false}
                }
            ]
        })
    }

    #[test]
    fn parse_library_extracts_items() {
        let items = parse_library(&sample_library());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].asin, "B002V5GK42");
        assert_eq!(items[0].title, "The Hitchhiker's Guide to the Galaxy");
        assert_eq!(items[0].authors.len(), 1);
        assert_eq!(items[0].authors[0].name, "Douglas Adams");
        // progress fields land in extra (shapes unconfirmed from docs)
        assert!(items[0].extra.contains_key("percent_complete"));
        assert!(items[0].extra.contains_key("listening_status"));
        assert_eq!(items[1].asin, "B08G9PRS1K");
    }

    #[test]
    fn parse_library_tolerates_empty_body() {
        assert!(parse_library(&serde_json::json!({})).is_empty());
        assert!(parse_library(&serde_json::json!({"items": []})).is_empty());
    }

    #[test]
    fn parse_library_handles_legacy_library_key() {
        let body = serde_json::json!({
            "library": [
                {"asin": "LEGACY001", "title": "Old Shape Book",
                 "percent_complete": 50}
            ]
        });
        let items = parse_library(&body);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].asin, "LEGACY001");
    }

    /// Fixture-level stub: returns different pages based on call count.
    struct StubFetcher {
        pages: Vec<Value>,
        call: std::cell::Cell<usize>,
    }
    impl StubFetcher {
        fn single(page: Value) -> Self {
            StubFetcher { pages: vec![page], call: std::cell::Cell::new(0) }
        }
        fn multi(pages: Vec<Value>) -> Self {
            StubFetcher { pages, call: std::cell::Cell::new(0) }
        }
    }
    impl LibraryFetcher for StubFetcher {
        fn fetch_library(&self, _page: u32) -> Result<Value> {
            let i = self.call.get();
            self.call.set(i + 1);
            Ok(self.pages[i.min(self.pages.len() - 1)].clone())
        }
    }

    #[test]
    fn pull_with_writes_raw_layer_unconditionally() {
        let v = temp_vault("raw");
        let f = StubFetcher::single(sample_library());
        let out = pull_with(&v, &f).unwrap();
        assert_eq!(out.counts.get("library_items"), Some(&2));

        let raw = v.root().join("media/audible/raw");
        assert!(raw.is_dir(), "raw/ created");
        let entries: Vec<_> = std::fs::read_dir(&raw).unwrap().collect();
        assert!(!entries.is_empty(), "raw partition written");
        // Check raw content is full-fidelity (contains asin + _polled_at).
        let content: String = entries
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .collect();
        assert!(content.contains("B002V5GK42"), "raw contains asin");
        assert!(content.contains("Douglas Adams"), "raw contains author");
        // _polled_at must be present so snapshots are dated (rebuildable watermark).
        assert!(content.contains("_polled_at"), "raw rows embed poll timestamp");
    }

    #[test]
    fn pull_with_embeds_poll_timestamp_in_every_raw_row() {
        let v = temp_vault("rawts");
        let f = StubFetcher::single(sample_library());
        pull_with(&v, &f).unwrap();

        let raw = v.root().join("media/audible/raw");
        let content: String = std::fs::read_dir(&raw)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .collect();
        // Every non-empty line must have _polled_at.
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let row: Value = serde_json::from_str(line)
                .expect("raw line is valid JSON");
            assert!(
                row.get("_polled_at").and_then(Value::as_str).is_some(),
                "_polled_at missing from raw row: {line}"
            );
        }
    }

    #[test]
    fn pagination_drain_uses_raw_array_length() {
        // A page that has PAGE_SIZE raw items but one is unparseable (empty asin).
        // The drain must not stop early due to the parse drop.
        // Build a page with PAGE_SIZE (1000) items: 999 valid + 1 empty-asin.
        let mut items: Vec<Value> = (0..999_u32)
            .map(|i| serde_json::json!({
                "asin": format!("ASIN{i:05}"),
                "title": format!("Book {i}"),
                "authors": []
            }))
            .collect();
        // Item with empty asin — will be dropped by parse_library.
        items.push(serde_json::json!({"asin": "", "title": "Bad", "authors": []}));
        let full_page = serde_json::json!({"items": items});

        // Single-page short library (< PAGE_SIZE raw items) for the second call.
        let short_page = serde_json::json!({"items": [
            {"asin": "FINALBOOK", "title": "Final", "authors": []}
        ]});

        let v = temp_vault("paginate");
        let f = StubFetcher::multi(vec![full_page, short_page]);
        let out = pull_with(&v, &f).unwrap();
        // Should have fetched both pages: 999 parsed from first + 1 from second.
        assert_eq!(
            out.counts.get("library_items"),
            Some(&1000),
            "both pages drained: 999 valid + 1 on second page"
        );
    }

    #[test]
    fn credential_blob_round_trips_through_connect_and_status() {
        let v = temp_vault("creds");
        let creds = AudibleCreds {
            access_token: "Atna|test-access-token".into(),
            refresh_token: "refresh-token-value".into(),
            adp_token: "adp-token-value".into(),
            device_private_key: "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----".into(),
            api_url: "https://api.audible.com".into(),
        };
        let blob = serde_json::to_string(&creds).unwrap();

        def_connect(&v, &blob).unwrap();
        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert!(status.accounts[0].label.contains("audible.com"));
        assert!(!status.accounts[0].needs_reconnect);

        def_disconnect(&v, "audible").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn connect_rejects_empty_and_malformed_blobs() {
        let v = temp_vault("badcreds");
        assert!(def_connect(&v, "").is_err(), "empty blob rejected");
        assert!(def_connect(&v, "not json").is_err(), "bad JSON rejected");
        assert!(
            def_connect(&v, r#"{"access_token":""}"#).is_err(),
            "empty access_token rejected"
        );
    }

    #[test]
    fn pull_errors_clearly_when_not_connected() {
        let v = temp_vault("noconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected") || err.contains("Integrations"), "clear error: {err}");
    }
}

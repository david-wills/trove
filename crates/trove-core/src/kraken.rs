//! Kraken cryptocurrency exchange — ledger sync via the official private REST
//! API (`/0/private/Ledgers`), plus CSV export for historical backfill.
//!
//! The **Ledgers** endpoint is the single source of truth for a Kraken account:
//! it covers trades, deposits, withdrawals, staking rewards, earn/yield entries,
//! transfers, and more.  The raw trade-fill shape (pair, price, vol) rides in
//! each ledger entry's `extra`; Trove does not separately drain TradesHistory.
//!
//! # Auth
//!
//! The user generates a **read-only** API key + secret at
//! kraken.com → Security → API → Add Key.  Required permissions: **Query Ledger
//! Entries** only — never deposit/trade/withdraw permissions.  Both are pasted as
//! `KEY:SECRET`; Trove stores them 0600 under `.trove/sync/kraken.json`.
//!
//! Every request is signed with Kraken's two-level scheme:
//! 1. `SHA256(nonce_string + encoded_body)` → 32-byte digest
//! 2. `HMAC-SHA512(endpoint_path + digest, base64_decode(secret))` → 64-byte tag
//! 3. Base64-encode the tag → `API-Sign` header
//!
//! Source: <https://docs.kraken.com/api/docs/guides/spot-rest-auth>
//!
//! # Endpoints
//!
//! - `POST /0/private/Ledgers` — all ledger entries, newest-first, 50/page.
//!   Request body: `nonce=<ms_epoch>&ofs=<offset>` (form-encoded POST body).
//!   Response: `{ "result": { "ledger": { "<id>": {...} }, "count": N }, "error": [] }`.
//!
//! # Vault layout
//!
//! - **Raw layer** (unconditional): `finance/kraken/raw/YYYY-MM.jsonl` —
//!   verbatim API ledger objects, full fidelity.
//! - **Contract layer**: `finance/purchases/kraken/YYYY-MM.jsonl` —
//!   [`crate::finance::LineItem`] rows keyed by the Kraken ledger `id`.
//!
//! # Cursor
//!
//! `.trove/kraken-sync.json` — a rebuildable non-secret JSON file holding the
//! ledger id of the newest entry drained in the previous run.  On the next sync
//! the drain collects entries until it encounters the saved id, then stops.
//! Guid dedupe makes the one-entry overlap idempotent.  Deleting the cursor
//! triggers a full re-backfill.
//!
//! # Contract mapping (finance-purchases)
//!
//! A Kraken ledger entry is a dated value transfer — the finance-purchases
//! contract was designed for exactly this.
//!
//! | LineItem field | Kraken source field                                        |
//! |----------------|------------------------------------------------------------|
//! | `guid`         | ledger entry id (stable, the dedupe key)                   |
//! | `ts`           | `time` (Unix seconds float) → RFC3339 local                |
//! | `merchant`     | `"Kraken"`                                                 |
//! | `item`         | `type` (trade/deposit/withdrawal/staking/…)                |
//! | `amount`       | `amount` (decimal string → f64)                            |
//! | `currency`     | `asset` (Kraken symbol, e.g. `"XXBT"`, `"ZUSD"`)           |
//! | `status`       | `""` (Kraken ledgers are always final)                     |
//! | `extra`        | `refid`, `subtype`, `aclass`, `fee`, `balance`, `time_raw` |

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Local, TimeZone};
use hmac::Hmac;
use hmac::Mac as HmacMac;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256, Sha512};

use crate::finance::LineItem;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract-layer directory.
const DIR: &str = "finance/purchases/kraken";
/// Raw-layer directory.
const RAW_DIR: &str = "finance/kraken/raw";
/// Secret-store service name (key in `.trove/sync/kraken.json`).
const SERVICE: &str = "kraken";
/// Rebuildable cursor file (non-secret; not in `sync/`).
const SYNC_FILE: &str = ".trove/kraken-sync.json";
/// Kraken API base URL.
const API_BASE: &str = "https://api.kraken.com";
/// Ledgers endpoint path.
const LEDGERS_PATH: &str = "/0/private/Ledgers";
/// Entries returned per page (Kraken default; max is also 50 for this endpoint).
const PAGE_SIZE: u64 = 50;
/// Maximum pages per sync pass (50/page × 2000 = 100k entries; well beyond any
/// personal account, but bounds the drain loop on runaway API responses).
const MAX_PAGES: usize = 2000;
/// Seconds between periodic syncs: daily (ledger activity on a personal account
/// is low; the incremental offset walk is cheap once the cursor is in place).
const KRAKEN_SYNC_SECS: u64 = 86_400;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let entries = out.counts.get("entries").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(entries > 0, || {
                format!("kraken synced — {entries} new ledger entries")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("kraken sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let entries = out.counts.get("entries").copied().unwrap_or(0);
    let headline = if entries == 0 {
        "Kraken is up to date — no new ledger entries".to_string()
    } else {
        format!("Kraken synced — {entries} new ledger entries")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "kraken",
        name: "Kraken",
        kind: IntegrationKind::CloudSync,
        // 🔒 financial detail — ships opt-in with explicit acknowledgement.
        default_on: false,
        description: "Syncs your Kraken ledger history (trades, deposits, withdrawals, \
                      staking rewards, and earn entries) using a read-only API key. \
                      First sync backfills the full history; later syncs fetch only what's new. \
                      A Kraken CSV export (Documents → Create Export) can be imported for deep \
                      backfill before the API sync begins.",
        domain: "finance",
        vault_path: "finance/purchases/kraken/",
        toggleable: true,
        setup: &[
            "At kraken.com, go to Security → API → Add Key.",
            "Enable only Query Ledger Entries — never deposit, trade, or withdrawal permissions.",
            "Copy both the API key and the secret, then paste them here as KEY:SECRET.",
        ],
        caveats: "Financial data. Generating a CSV export on Kraken's side can take minutes to \
                  several days for large accounts; the API sync is automatic and incremental.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(KRAKEN_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("kraken"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = API key + secret as "KEY:SECRET").

/// Parse the pasted `KEY:SECRET` into `(key, secret)`.  Strips surrounding
/// whitespace from both halves; the colon must be present.
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty credential — paste your Kraken API key and secret as KEY:SECRET");
    }
    match pasted.split_once(':') {
        Some((key, secret)) => {
            let key = key.trim();
            let secret = secret.trim();
            if key.is_empty() {
                bail!("API key is empty — paste as KEY:SECRET (the key before the colon)");
            }
            if secret.is_empty() {
                bail!("API secret is empty — paste as KEY:SECRET (the secret after the colon)");
            }
            Ok((key.to_string(), secret.to_string()))
        }
        None => bail!("no colon found — paste your Kraken API credentials as KEY:SECRET"),
    }
}

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (key, secret) = parse_credentials(pasted)?;
    let client = KrakenClient::new(key, secret);
    connect_with(vault, &client, pasted)
}

/// The testable connect body: probe the API to verify the key, then store it.
fn connect_with(vault: &Vault, client: &impl KrakenApi, pasted: &str) -> Result<()> {
    // Probe with ofs=0, count=1 — proves auth and ledger permission.
    match client.get_ledgers(0) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Kraken rejected the key or secret — check they are correct and that \
             Query Ledger Entries permission is enabled"
        ),
        Err(e) => bail!("Kraken connection check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: pasted.trim().to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        if let Ok((key, _)) = parse_credentials(&token.access_token) {
            accounts.push(ConnectedAccount {
                key: SERVICE.to_string(),
                label: format!("API key {}", key_summary(&key)),
                connected_at: None,
                expires_at: None,
                needs_reconnect: false,
                extra: BTreeMap::new(),
            });
        }
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Short display of the API key: first 4 chars + ellipsis.
fn key_summary(key: &str) -> String {
    let prefix: String = key.chars().take(4).collect();
    if key.len() > 4 { format!("{prefix}\u{2026}") } else { prefix }
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "kraken",
    display_name: "Kraken",
    methods: &[ConnectMethod::TokenPaste {
        label: "Kraken API key and secret",
        help: "Paste your Kraken API key and secret as KEY:SECRET (separated by a colon). \
               Generate a key at kraken.com → Security → API → Add Key. \
               Enable only Query Ledger Entries — never deposit, trade, or withdrawal permissions.",
        placeholder: "AbCdEfGhIj\u{2026}:aBcDeFgHiJ\u{2026}",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["kraken"],
    setup: &[
        "At kraken.com, go to Security → API → Add Key.",
        "Enable only the 'Query Ledger Entries' permission.",
        "Copy both the key and secret, then paste them as KEY:SECRET here.",
    ],
};

// ---------------------------------------------------------------------------
// Request signing — Kraken two-level HMAC-SHA512 scheme.
//
// Source: https://docs.kraken.com/api/docs/guides/spot-rest-auth
//
// Steps:
// 1. `nonce_str` = decimal string of the nonce (milliseconds since epoch).
// 2. `encoded_body` = URL-encoded POST body (e.g. `"nonce=1234567890&ofs=0"`).
// 3. `digest` = SHA256(nonce_str + encoded_body) → 32-byte binary.
// 4. `message` = endpoint_path_bytes + digest_bytes.
// 5. `secret_bytes` = base64_decode(api_secret).
// 6. `signature` = HMAC-SHA512(message, secret_bytes) → 64-byte binary.
// 7. `api_sign` = base64_encode(signature).

fn sign(secret_b64: &str, nonce: u64, path: &str, body: &str) -> Result<String> {
    // Step 3: SHA256(nonce_string + body).
    let nonce_str = nonce.to_string();
    let mut hasher = Sha256::new();
    hasher.update(nonce_str.as_bytes());
    hasher.update(body.as_bytes());
    let sha256_digest = hasher.finalize(); // [u8; 32]

    // Step 4: path bytes + SHA256 digest bytes.
    let mut message: Vec<u8> = Vec::with_capacity(path.len() + 32);
    message.extend_from_slice(path.as_bytes());
    message.extend_from_slice(&sha256_digest);

    // Step 5: base64-decode the secret key.
    let secret_bytes = B64
        .decode(secret_b64.trim())
        .context("Kraken API secret is not valid base64 — re-paste your credentials")?;

    // Step 6: HMAC-SHA512.
    let mut mac: Hmac<Sha512> =
        HmacMac::new_from_slice(&secret_bytes).expect("HMAC accepts any key length");
    HmacMac::update(&mut mac, &message);
    let tag = HmacMac::finalize(mac).into_bytes();

    // Step 7: base64-encode.
    Ok(B64.encode(tag))
}

/// Milliseconds since the Unix epoch — Kraken's recommended nonce source.
fn nonce_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable trait for testing.

#[derive(Debug)]
enum FetchError {
    /// HTTP 403 / EAPI:Invalid key — bad key or secret.
    Unauthorized,
    /// HTTP 429 / `EAPI:Rate limit exceeded` — back off.
    RateLimited,
    /// Everything else.
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized — check key, secret, and ledger permission"),
            FetchError::RateLimited => write!(f, "rate limited — will retry on next sync"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The Kraken endpoints this collector uses.
trait KrakenApi {
    /// `POST /0/private/Ledgers` with `ofs` offset.  Returns the parsed
    /// `result.ledger` object (a map of id → entry) or an error.
    fn get_ledgers(&self, ofs: u64) -> Result<Value, FetchError>;
}

/// Thin ureq client — key and (base64) secret injected at construction.
struct KrakenClient {
    key: String,
    secret: String,
}

impl KrakenClient {
    fn new(key: String, secret: String) -> Self {
        KrakenClient { key, secret }
    }

    /// POST with a form-encoded body and the two Kraken auth headers.
    fn post_private(&self, path: &str, extra_body: &str) -> Result<Value, FetchError> {
        let nonce = nonce_ms();
        // Body: always includes nonce; caller may append extra params.
        let body = if extra_body.is_empty() {
            format!("nonce={nonce}")
        } else {
            format!("nonce={nonce}&{extra_body}")
        };

        let sig = sign(&self.secret, nonce, path, &body)
            .map_err(|e| FetchError::Other(e.to_string()))?;

        let url = format!("{API_BASE}{path}");
        let result = ureq::post(&url)
            .set("API-Key", &self.key)
            .set("API-Sign", &sig)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body);

        match result {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("JSON parse: {e}")))?;
                // Kraken embeds API errors in `error: [...]` even on HTTP 200.
                if let Some(errors) = v.get("error").and_then(Value::as_array) {
                    if !errors.is_empty() {
                        let msg = errors
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ");
                        // EAPI:Invalid key (403-equivalent in Kraken's scheme).
                        if msg.contains("Invalid key") || msg.contains("Invalid nonce") {
                            return Err(FetchError::Unauthorized);
                        }
                        if msg.contains("Rate limit") {
                            return Err(FetchError::RateLimited);
                        }
                        return Err(FetchError::Other(format!("Kraken API error: {msg}")));
                    }
                }
                Ok(v)
            }
            Err(ureq::Error::Status(403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429 | 503, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl KrakenApi for KrakenClient {
    fn get_ledgers(&self, ofs: u64) -> Result<Value, FetchError> {
        let body_suffix = format!("ofs={ofs}");
        let resp = self.post_private(LEDGERS_PATH, &body_suffix)?;
        // Extract the `result.ledger` map.
        Ok(resp
            .get("result")
            .and_then(|r| r.get("ledger"))
            .cloned()
            .unwrap_or(Value::Object(Map::new())))
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Id of the newest ledger entry already stored.  The drain stops when it
    /// encounters this id again (overlap-idempotent via guid dedupe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    newest_id: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_kraken_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_kraken_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Wire shapes.

/// One Kraken ledger entry as returned by the API.  We deserialize only what
/// we need for the contract mapping; the full raw object is written verbatim.
#[derive(Debug, Clone)]
struct LedgerEntry {
    id: String,
    /// The full raw JSON object (verbatim, for the raw layer).
    raw: Value,
}

/// Parse the `result.ledger` map (id → entry object) into a Vec of entries,
/// sorted newest-first by `time` (Kraken returns them newest-first already, but
/// explicit sort makes the ordering deterministic in tests).
fn parse_ledger_page(ledger_obj: &Value) -> Vec<LedgerEntry> {
    let map = match ledger_obj.as_object() {
        Some(m) => m,
        None => return Vec::new(),
    };
    let mut entries: Vec<LedgerEntry> = map
        .iter()
        .filter_map(|(id, obj)| {
            if id.is_empty() {
                return None;
            }
            Some(LedgerEntry { id: id.clone(), raw: obj.clone() })
        })
        .collect();
    // Sort newest-first by time for a stable cursor (the oldest entry last).
    entries.sort_by(|a, b| {
        let ta = a.raw.get("time").and_then(Value::as_f64).unwrap_or(0.0);
        let tb = b.raw.get("time").and_then(Value::as_f64).unwrap_or(0.0);
        tb.partial_cmp(&ta).unwrap_or(std::cmp::Ordering::Equal)
    });
    entries
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// One Kraken ledger entry → a contract [`LineItem`].
/// Returns `None` when `time` is absent/zero (can't be partitioned).
fn entry_to_line_item(id: &str, obj: &Value) -> Option<LineItem> {
    // `time` is a Unix timestamp (float seconds).
    let time_secs = obj.get("time").and_then(Value::as_f64).filter(|&t| t > 0.0)?;
    let ts = unix_secs_to_local(time_secs)?;
    // Verify the ts is month-partitionable before proceeding.
    Partition::Month.key(&ts)?;

    let entry_type = obj.get("type").and_then(Value::as_str).unwrap_or("").to_string();
    let asset = obj.get("asset").and_then(Value::as_str).unwrap_or("").to_string();
    let amount: Option<f64> = obj
        .get("amount")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok());

    // Extra: everything the contract's named fields don't carry.
    let mut extra = Map::new();
    // Raw float timestamp for full-fidelity readers (the RFC3339 ts already
    // represents the same instant, but the raw float is the source of truth).
    extra.insert("time_raw".into(), Value::from(time_secs));
    for key in &["refid", "subtype", "aclass", "fee", "balance"] {
        if let Some(v) = obj.get(*key) {
            if !v.is_null() && v.as_str().map(|s| !s.is_empty()).unwrap_or(true) {
                extra.insert((*key).to_string(), v.clone());
            }
        }
    }

    let mut li = LineItem::new("kraken", id, &ts, "Kraken");
    if !entry_type.is_empty() {
        li.item = entry_type;
    }
    li.amount = amount;
    if !asset.is_empty() {
        li.currency = asset;
    }
    li.extra = extra;
    Some(li)
}

/// Unix timestamp (float seconds) → RFC3339 local string.
fn unix_secs_to_local(secs: f64) -> Option<String> {
    Local.timestamp_opt(secs as i64, 0).single().map(|dt| dt.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Raw line shape.

/// The on-disk raw line: the verbatim Kraken ledger entry tagged with `ts` for
/// month partitioning (`#[serde(skip)]` keeps the tag out of the JSONL output;
/// only the `value` is serialized).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid.

fn write_layer(vault: &Vault, rows: Vec<(LineItem, Value)>) -> Result<u64> {
    use std::collections::HashSet;
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Load existing guids to dedupe (guid = Kraken ledger id, stable).
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                if !g.is_empty() {
                    seen.insert(g.to_string());
                }
            }
        }
    }

    let mut new_rows: Vec<LineItem> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (row, raw_val) in rows {
        if row.guid.is_empty() || !seen.insert(row.guid.clone()) {
            continue;
        }
        new_raws.push(RawLine { ts: row.ts.clone(), value: raw_val });
        new_rows.push(row);
    }

    contract.append(&new_rows, |r| &r.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Load the stored KEY:SECRET and run the sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let pasted = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|s| !s.trim().is_empty())
        .context("Kraken is not connected — add your API key in the Integrations tab")?;
    let (key, secret) = parse_credentials(&pasted)?;
    let client = KrakenClient::new(key, secret);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl KrakenApi) -> Result<PullOutcome> {
    let state = vault.read_kraken_sync();
    let stop_at = state.newest_id.clone();

    // Drain the full new window before advancing the cursor; a crash re-drains
    // (guid dedupe makes the overlap idempotent).
    let (collected, newest_id) = drain_ledgers(api, stop_at.as_deref())?;

    // Map to contract rows; entries without a parseable time are skipped (they
    // can't be month-partitioned; in practice Kraken always populates time).
    let rows: Vec<(LineItem, Value)> = collected
        .iter()
        .filter_map(|e| {
            let mut raw_obj = e.raw.clone();
            // Tag the raw object with its stable id so readers can identify it.
            if let Some(obj) = raw_obj.as_object_mut() {
                obj.insert("id".into(), Value::String(e.id.clone()));
            }
            entry_to_line_item(&e.id, &e.raw).map(|li| (li, raw_obj))
        })
        .collect();

    let written = write_layer(vault, rows)?;

    // Advance the cursor only after a successful write.
    let mut new_state = vault.read_kraken_sync();
    if let Some(id) = newest_id {
        new_state.newest_id = Some(id);
    }
    new_state.updated = Some(Local::now().to_rfc3339());
    vault.write_kraken_sync(&new_state)?;

    Ok(PullOutcome {
        headline: format!("{written} ledger entries"),
        counts: BTreeMap::from([("entries", written)]),
    })
}

/// Drain the ledger from offset 0 forward, stopping when `stop_at` id is
/// encountered (or when a short/empty page arrives).  Returns all new entries
/// (newest-first) and the id of the overall newest entry (the new cursor).
fn drain_ledgers(
    api: &impl KrakenApi,
    stop_at: Option<&str>,
) -> Result<(Vec<LedgerEntry>, Option<String>)> {
    let mut all: Vec<LedgerEntry> = Vec::new();
    let mut newest_id: Option<String> = None;
    let mut ofs: u64 = 0;
    let mut pages = 0;

    loop {
        let page_val = api
            .get_ledgers(ofs)
            .map_err(|e| anyhow::anyhow!("Kraken Ledgers fetch at ofs={ofs}: {e}"))?;
        let entries = parse_ledger_page(&page_val);

        if entries.is_empty() {
            break;
        }

        // The first entry of the first page is the overall newest.
        if newest_id.is_none() {
            if let Some(first) = entries.first() {
                newest_id = Some(first.id.clone());
            }
        }

        let mut hit_cursor = false;
        for entry in entries {
            // Stop when we encounter the saved cursor id.
            if stop_at == Some(entry.id.as_str()) {
                hit_cursor = true;
                break;
            }
            all.push(entry);
        }

        pages += 1;
        if hit_cursor || pages >= MAX_PAGES {
            break;
        }

        // Short-page detection: if fewer than PAGE_SIZE entries arrived on
        // this page, we've reached the end of history.
        let page_count = page_val.as_object().map(|m| m.len()).unwrap_or(0) as u64;
        if page_count < PAGE_SIZE {
            break;
        }

        ofs += PAGE_SIZE;
    }

    Ok((all, newest_id))
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-kraken-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — synthesized from the documented Kraken Ledgers API shape
    // (https://docs.kraken.com/api/docs/rest-api/get-ledgers-info).
    // Fields: refid (string), time (float seconds), type, subtype, aclass,
    // asset, amount (decimal string), fee (decimal string), balance (string).

    fn ledger_map(entries: &[(&str, Value)]) -> Value {
        let mut map = Map::new();
        for (id, obj) in entries {
            map.insert((*id).to_string(), obj.clone());
        }
        Value::Object(map)
    }

    fn entry_trade() -> Value {
        json!({
            "refid": "OXXXXX-YYYYY-ZZZZZ1",
            "time": 1748415600.0_f64,   // 2026-05-28T00:00:00 UTC
            "type": "trade",
            "subtype": "",
            "aclass": "currency",
            "asset": "XXBT",
            "amount": "0.01250000",
            "fee": "0.00002500",
            "balance": "0.08750000"
        })
    }

    fn entry_deposit() -> Value {
        json!({
            "refid": "OXXXXX-YYYYY-ZZZZZ2",
            "time": 1749020400.0_f64,   // 2026-06-04T05:00:00 UTC
            "type": "deposit",
            "subtype": "",
            "aclass": "currency",
            "asset": "ZUSD",
            "amount": "500.00",
            "fee": "0.00",
            "balance": "1500.00"
        })
    }

    fn entry_staking() -> Value {
        json!({
            "refid": "OXXXXX-YYYYY-ZZZZZ3",
            "time": 1749106800.0_f64,   // 2026-06-05T05:00:00 UTC
            "type": "staking",
            "subtype": "stakingtospot",
            "aclass": "currency",
            "asset": "ETH2.S",
            "amount": "0.00012345",
            "fee": "0.00000000",
            "balance": "3.04562890"
        })
    }

    /// An entry with time=0 (degenerate, should be skipped).
    fn entry_no_time() -> Value {
        json!({
            "refid": "OXXXXX-YYYYY-ZZZZZ4",
            "time": 0.0_f64,
            "type": "trade",
            "aclass": "currency",
            "asset": "XXBT",
            "amount": "0.00100000",
            "fee": "0.00000200",
            "balance": "0.08850000"
        })
    }

    // -----------------------------------------------------------------------
    // credential parsing

    #[test]
    fn parse_credentials_ok() {
        let (k, s) = parse_credentials("mykey:mysecret").unwrap();
        assert_eq!(k, "mykey");
        assert_eq!(s, "mysecret");
    }

    #[test]
    fn parse_credentials_trims_whitespace() {
        let (k, s) = parse_credentials("  mykey : mysecret  ").unwrap();
        assert_eq!(k, "mykey");
        assert_eq!(s, "mysecret");
    }

    #[test]
    fn parse_credentials_no_colon_errors() {
        assert!(parse_credentials("keyonly").is_err());
    }

    #[test]
    fn parse_credentials_empty_errors() {
        assert!(parse_credentials("").is_err());
        assert!(parse_credentials(":secret").is_err());
        assert!(parse_credentials("key:").is_err());
    }

    // -----------------------------------------------------------------------
    // signing

    #[test]
    fn sign_produces_base64_output() {
        // A known-valid base64 secret (32 zero bytes).
        let secret_b64 = B64.encode([0u8; 32]);
        let sig = sign(&secret_b64, 1_748_000_000_000, LEDGERS_PATH, "nonce=1748000000000&ofs=0")
            .unwrap();
        // Must be a valid base64 string and non-empty.
        assert!(!sig.is_empty());
        B64.decode(&sig).expect("API-Sign must be valid base64");
    }

    #[test]
    fn sign_differs_by_nonce() {
        let secret_b64 = B64.encode([1u8; 32]);
        let body1 = "nonce=1000&ofs=0";
        let body2 = "nonce=1001&ofs=0";
        let s1 = sign(&secret_b64, 1000, LEDGERS_PATH, body1).unwrap();
        let s2 = sign(&secret_b64, 1001, LEDGERS_PATH, body2).unwrap();
        assert_ne!(s1, s2, "different nonce → different signature");
    }

    #[test]
    fn sign_bad_secret_errors() {
        let err = sign("not!base64!!", 1000, LEDGERS_PATH, "nonce=1000").unwrap_err();
        assert!(err.to_string().contains("base64"), "clear error: {err}");
    }

    // -----------------------------------------------------------------------
    // parse_ledger_page

    #[test]
    fn parse_ledger_page_extracts_entries_sorted_newest_first() {
        let page = ledger_map(&[
            ("L-TRADE1", entry_trade()),    // time 1748415600
            ("L-DEPOSIT1", entry_deposit()), // time 1749020400 (newer)
        ]);
        let entries = parse_ledger_page(&page);
        assert_eq!(entries.len(), 2);
        // deposit (newer time) must come first.
        assert_eq!(entries[0].id, "L-DEPOSIT1");
        assert_eq!(entries[1].id, "L-TRADE1");
    }

    #[test]
    fn parse_ledger_page_empty_map_returns_empty() {
        let page = Value::Object(Map::new());
        assert!(parse_ledger_page(&page).is_empty());
    }

    // -----------------------------------------------------------------------
    // entry_to_line_item

    #[test]
    fn maps_trade_entry_to_line_item() {
        let li = entry_to_line_item("L-TRADE1", &entry_trade()).unwrap();
        assert_eq!(li.source, "kraken");
        assert_eq!(li.guid, "L-TRADE1");
        assert_eq!(li.merchant, "Kraken");
        assert_eq!(li.item, "trade");
        assert_eq!(li.currency, "XXBT");
        assert_eq!(li.amount, Some(0.01250000));
        // ts is RFC3339 local for the Unix timestamp.
        let parsed = DateTime::parse_from_rfc3339(&li.ts).unwrap();
        assert_eq!(parsed.timestamp(), 1748415600);
        // extra carries fee and refid.
        assert!(li.extra.contains_key("fee"));
        assert_eq!(li.extra.get("refid"), Some(&json!("OXXXXX-YYYYY-ZZZZZ1")));
        // time_raw is the original float for full-fidelity.
        assert!(li.extra.contains_key("time_raw"));
    }

    #[test]
    fn maps_staking_entry() {
        let li = entry_to_line_item("L-STAKING1", &entry_staking()).unwrap();
        assert_eq!(li.item, "staking");
        assert_eq!(li.currency, "ETH2.S");
        assert_eq!(li.amount, Some(0.00012345));
        // subtype is preserved in extra.
        assert_eq!(li.extra.get("subtype"), Some(&json!("stakingtospot")));
    }

    #[test]
    fn entry_with_zero_time_returns_none() {
        assert!(
            entry_to_line_item("L-NOTIME", &entry_no_time()).is_none(),
            "zero time → not filed"
        );
    }

    #[test]
    fn entry_to_line_item_negative_amount() {
        // A trade sell has a negative amount (funds leave the account).
        let mut e = entry_trade();
        e["amount"] = json!("-0.01250000");
        e["type"] = json!("trade");
        let li = entry_to_line_item("L-SELL1", &e).unwrap();
        assert_eq!(li.amount, Some(-0.01250000));
    }

    // -----------------------------------------------------------------------
    // Mock API

    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct MockApi {
        // Each call to get_ledgers pops the next response from the queue.
        pages: RefCell<VecDeque<Result<Value, FetchError>>>,
        calls: RefCell<Vec<u64>>, // recorded ofs values
    }

    impl MockApi {
        fn new() -> Self {
            MockApi { pages: RefCell::new(VecDeque::new()), calls: RefCell::new(Vec::new()) }
        }
        fn push(self, page: Value) -> Self {
            self.pages.borrow_mut().push_back(Ok(page));
            self
        }
    }

    impl KrakenApi for MockApi {
        fn get_ledgers(&self, ofs: u64) -> Result<Value, FetchError> {
            self.calls.borrow_mut().push(ofs);
            self.pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(Value::Object(Map::new())))
        }
    }

    // -----------------------------------------------------------------------
    // pull_with

    #[test]
    fn pull_writes_both_layers_and_advances_cursor() {
        let v = temp_vault("pullbasic");
        let page = ledger_map(&[
            ("L-DEP1", entry_deposit()),
            ("L-TRD1", entry_trade()),
        ]);
        let api = MockApi::new().push(page);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&2));

        // Contract rows exist on disk.
        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<Value> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<Value>(&key).unwrap());
        }
        assert_eq!(all.len(), 2);

        // Raw layer exists.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_all: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_all.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_all.len(), 2);

        // Cursor advanced to the newest entry (deposit, time 1749020400).
        let state = v.read_kraken_sync();
        assert_eq!(state.newest_id.as_deref(), Some("L-DEP1"));
        assert!(state.updated.is_some());
    }

    #[test]
    fn pull_dedupes_on_re_pull() {
        let v = temp_vault("dedup");
        let page = ledger_map(&[("L-TRD1", entry_trade())]);
        let out1 = pull_with(&v, &MockApi::new().push(page.clone())).unwrap();
        assert_eq!(out1.counts.get("entries"), Some(&1));

        // Second pull with same data → guid dedupe, zero new writes.
        let out2 = pull_with(&v, &MockApi::new().push(page)).unwrap();
        assert_eq!(out2.counts.get("entries"), Some(&0));
    }

    #[test]
    fn pull_stops_at_saved_cursor() {
        let v = temp_vault("cursor");
        // First sync: one deposit entry.
        let page1 = ledger_map(&[("L-DEP1", entry_deposit())]);
        pull_with(&v, &MockApi::new().push(page1)).unwrap();
        // Cursor is now "L-DEP1".
        assert_eq!(v.read_kraken_sync().newest_id.as_deref(), Some("L-DEP1"));

        // Build a second entry with a strictly later timestamp so it sorts newer
        // than L-DEP1 (time=1749020400) and appears first in the page.
        let mut newer_entry = entry_deposit();
        newer_entry["time"] = json!(1_749_200_000.0_f64); // later than deposit
        newer_entry["asset"] = json!("ZUSD");
        newer_entry["amount"] = json!("100.00");

        // Second sync: two entries — the new one (later time) and the old cursor.
        // Sorted newest-first: L-TRD2 (time 1749200000) first, L-DEP1 second.
        let page2 = ledger_map(&[
            ("L-TRD2", newer_entry),       // newer — new entry
            ("L-DEP1", entry_deposit()),   // the cursor — signals stop
        ]);
        let out = pull_with(&v, &MockApi::new().push(page2)).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&1));

        // Cursor advanced to the new newest entry.
        assert_eq!(v.read_kraken_sync().newest_id.as_deref(), Some("L-TRD2"));
    }

    #[test]
    fn pull_empty_window_keeps_cursor() {
        let v = temp_vault("empty");
        let page = ledger_map(&[("L-TRD1", entry_trade())]);
        pull_with(&v, &MockApi::new().push(page)).unwrap();

        // Empty next page → cursor unchanged.
        let out = pull_with(&v, &MockApi::new()).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&0));
        assert_eq!(v.read_kraken_sync().newest_id.as_deref(), Some("L-TRD1"));
    }

    #[test]
    fn pagination_advances_ofs_on_full_pages() {
        // Two pages: first has PAGE_SIZE=50 entries (full), second has 1 (short → stop).
        let mut page1_entries: Vec<(&str, Value)> = Vec::new();
        // We need 50 entries; use a boxed vec to keep ids alive.
        let ids: Vec<String> = (0..50).map(|i| format!("L-P1-{i:02}")).collect();
        let mut objs: Vec<Value> = Vec::new();
        for i in 0..50 {
            let mut e = entry_trade();
            // Spread times so sort order is predictable.
            e["time"] = json!(1_749_000_000.0 + i as f64);
            objs.push(e);
        }
        for i in 0..50 {
            page1_entries.push((ids[i].as_str(), objs[i].clone()));
        }
        let page1 = ledger_map(&page1_entries);
        let page2 = ledger_map(&[("L-P2-00", entry_staking())]);

        let v = temp_vault("paginate");
        let api = MockApi::new().push(page1).push(page2);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&51), "50 + 1 across two pages");

        // Two get_ledgers calls: ofs=0, ofs=50.
        let calls = api.calls.borrow();
        assert_eq!(calls.as_slice(), &[0, 50]);
    }

    #[test]
    fn connection_stores_and_status_summarizes() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "MYKEY1234:MYSECRET".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].key, "kraken");
        assert!(status.accounts[0].label.contains("MYKE"), "label shows key prefix");

        def_disconnect(&v, "kraken").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn pull_needs_connection() {
        let v = temp_vault("needsconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn connection_has_token_paste_method_and_links_def() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "kraken");
        assert_eq!(DEF.connection, Some("kraken"));
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.newest_id.is_none());
        assert!(empty.updated.is_none());

        let partial: SyncState =
            serde_json::from_str(r#"{"updated":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert!(partial.newest_id.is_none());
        assert_eq!(partial.updated.as_deref(), Some("2026-06-01T00:00:00-07:00"));
    }
}

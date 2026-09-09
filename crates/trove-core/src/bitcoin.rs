//! Bitcoin wallet transaction history via Blockstream Esplora — the keyless
//! public chain API (`blockstream.info/api`).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/bitcoin.md.
//! **First collector in the `finance-purchases` domain** — this build binds the
//! contract (see [`crate::finance::purchases`] / [`crate::contracts`]).
//!
//! A **Periodic** cloud pull. The user pastes one or more Bitcoin **addresses**
//! (never private keys); Trove reads the public chain. There is no account and
//! no token — the only credential is the public address list, stored exactly
//! like a public username (the [`crate::boardgamegeek`] / [`crate::listenbrainz`]
//! pattern): in the `access_token` slot of a never-expiring
//! [`crate::sync::oauth::TokenSet`] under `.trove/sync/bitcoin.json`. A custom
//! Esplora base URL (for a self-hosted instance) may be appended after a `|`.
//!
//! ## Endpoints (Esplora REST, confirmed against the official API.md)
//!
//! - `GET /address/:addr/txs` → newest-first tx history: up to 50 mempool txs
//!   **first**, then the first 25 confirmed txs. (Mempool entries lead the page;
//!   the confirmed-chain cursor below is therefore derived from the first
//!   *confirmed* tx, skipping any leading mempool ones — see [`drain_address`].)
//! - `GET /address/:addr/txs/chain/:last_seen_txid` → the next page of 25
//!   **confirmed** txs after `:last_seen_txid` (confirmed-chain only) — followed
//!   until a short/empty page.
//!
//! Each tx object carries `txid`, `fee`, `status` (`confirmed`, `block_height`,
//! `block_hash`, `block_time` — a Unix timestamp in **seconds**), and `vin`/
//! `vout` arrays. Inputs carry `prevout {scriptpubkey_address, value}`; outputs
//! carry `{scriptpubkey_address, value}`. **All amounts are in satoshis**
//! (integers; 1 BTC = 100_000_000 sats).
//!
//! ## Mapping → the `finance-purchases` contract
//!
//! A Bitcoin transaction is a dated value transfer — an event in the wallet's
//! ledger. For each tx we compute the wallet's **net** value change across all
//! of the user's known addresses: `Σ(owned vout value) − Σ(owned vin prevout
//! value)`, in satoshis. That signed net (positive = received, negative = sent;
//! a self-transfer nets to just the fee) becomes one [`crate::finance::LineItem`]
//! under `finance/purchases/bitcoin/YYYY-MM.jsonl`:
//!
//! - `guid` = the `txid` (stable, the dedupe key — one row per tx regardless of
//!   how many owned addresses it touches, so a self-transfer isn't double-counted),
//! - `ts` = `block_time` (Unix seconds) → RFC3339 **local**, its month the
//!   partition key,
//! - `merchant` = `"Bitcoin"` (the network/asset — the honest "where", since the
//!   counterparty payer/payee is not observable from the chain),
//! - `item` = `"Received"` / `"Sent"` (the direction),
//! - `amount` = the net change as **BTC** (a `number`; sats / 1e8), `currency` =
//!   `"BTC"`,
//! - `extra` carries the exact integer sats (`value_sats`, `fee_sats`), the
//!   direction, the block height, and the counterparty addresses — nothing the
//!   API returned is lost.
//!
//! ## Two layers
//!
//! - **raw** — the verbatim Esplora tx object under
//!   `finance/purchases/bitcoin/raw/YYYY-MM.jsonl` (full fidelity, unconditional,
//!   every input/output and script preserved).
//! - **contract** — the normalized [`crate::finance::LineItem`] rows, deduped by
//!   `guid` (txid) against what's already on disk.
//!
//! The watermark — per-address `last_seen_txid` (the confirmed-chain cursor) —
//! lives in a rebuildable, non-secret cursor at `.trove/bitcoin-sync.json`. The
//! whole new window is drained (paged to a short/empty page) before the cursor
//! advances, so a crash re-drains rather than skips; guid dedupe makes the
//! overlap idempotent.
//!
//! Plain addresses only (xpub-level scanning is out of scope for v1). Cash App's
//! exported Bitcoin rows overlap this source — different account folders by
//! design; reconciliation on txid happens at read time, never at write time.

use std::collections::{BTreeMap, HashSet};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::finance::LineItem;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract-layer line-item stream; raw under `raw/`.
const DIR: &str = "finance/purchases/bitcoin";
const RAW_DIR: &str = "finance/purchases/bitcoin/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that's for 0600
/// secrets). Deleting it just re-walks the whole history on the next sync (guid
/// dedupe keeps the re-walk idempotent).
const SYNC_FILE: &str = ".trove/bitcoin-sync.json";

/// Service id under `.trove/sync/` where the address list is stored (the
/// public-username slot: the addresses ride a never-expiring [`TokenSet`]'s
/// `access_token`, exactly like boardgamegeek/listenbrainz — they are public,
/// not secret, but the slot is the convenient single-string home).
const SERVICE: &str = "bitcoin";

/// Default Esplora base. A user may override per-connection (self-hosted) by
/// appending `|https://my-esplora.example/api` to the pasted address string.
const DEFAULT_BASE: &str = "https://blockstream.info/api";

/// Satoshis per BTC.
const SATS_PER_BTC: f64 = 100_000_000.0;

/// Esplora serves 25 confirmed txs per chain page.
const PAGE_SIZE: usize = 25;
/// Runaway guard on the per-address pagination walk (25/page × 4000 = 100k txs,
/// far beyond any personal wallet; bounds the loop if a short page never comes).
const MAX_PAGES: usize = 4000;
/// Politeness throttle between requests to the public instance.
const REQ_INTERVAL: Duration = Duration::from_millis(400);
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between syncs in the watcher loop. Hourly: chain activity for a
/// personal wallet trickles in and the incremental cursor poll is cheap.
pub const BITCOIN_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// a missing address or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let txs = out.counts.get("transactions").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(txs > 0, || {
                format!("bitcoin synced — {txs} new transactions")
            }))
        }
        // Not connected / transient network: stay silent, retry next tick.
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("bitcoin sync skipped: {e}"))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let txs = out.counts.get("transactions").copied().unwrap_or(0);
    let headline = if txs == 0 {
        "Bitcoin is up to date — no new transactions".to_string()
    } else {
        format!("Bitcoin synced — {txs} new transactions")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bitcoin",
        name: "Bitcoin Wallet (Blockstream)",
        kind: IntegrationKind::CloudSync,
        // 🔒 financial detail + querying discloses addresses to the API host —
        // ships opt-in with explicit acknowledgement (default-off).
        default_on: false,
        description: "Fetches transaction history for your Bitcoin addresses from the public, \
                      keyless Blockstream Esplora API and records each as a dated value transfer. \
                      No keys or account needed — you supply public addresses only (never private \
                      keys). The first sync backfills the whole history.",
        domain: "finance",
        vault_path: "finance/purchases/bitcoin/",
        toggleable: true,
        setup: &[
            "Connect with one or more public Bitcoin addresses on this card (comma- or \
             space-separated).",
            "First sync backfills the whole transaction history; later syncs fetch only what's new.",
        ],
        caveats: "Querying a public API reveals your addresses to Blockstream (which states it keeps \
                  no persistent logs); append |https://your-esplora/api to the addresses to use a \
                  self-hosted instance for full privacy. Plain addresses only — extended-public-key \
                  (xpub) wallet scanning is not supported in v1. Amounts are the net change to your \
                  wallet across the addresses you supplied; the exact satoshi values and the \
                  counterparty addresses are kept in each row's extra and raw layer.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(BITCOIN_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("bitcoin"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the public Bitcoin address(es), keyless).

/// Store the pasted address string under `.trove/sync/bitcoin.json` (0600). The
/// addresses are public, but the secret store's single-string slot is the
/// convenient home (the boardgamegeek username precedent). Reads are keyless, so
/// we verify at least one address is well-formed and reachable with a cheap
/// probe; a clear error surfaces to the connect UI when nothing parses.
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (addrs, base) = parse_connect_input(pasted);
    if addrs.is_empty() {
        bail!("no Bitcoin address found — paste one or more public addresses, comma- or space-separated");
    }
    let client = EsploraClient::new(base);
    connect_with(vault, &client, pasted)
}

/// The connect body over an injected fetcher — the testable seam (tests verify
/// against a stub, never the network).
fn connect_with(vault: &Vault, client: &impl EsploraApi, pasted: &str) -> Result<()> {
    let (addrs, _base) = parse_connect_input(pasted);
    if addrs.is_empty() {
        bail!("no Bitcoin address found — paste one or more public addresses, comma- or space-separated");
    }
    // Keyless verification: a cheap first-page probe on the first address. An
    // explicit bad-address rejection (Esplora 400) blocks; a network blip or
    // anything else doesn't (the user may be briefly offline) — the pull retries.
    match client.address_txs(&addrs[0], None) {
        Ok(_) => {}
        Err(FetchError::BadAddress) => {
            bail!("Blockstream rejected the address {:?} — check it's a valid Bitcoin address", addrs[0])
        }
        Err(_) => {} // transient/other: store anyway, the pull will retry
    }
    // The whole pasted string (addresses + optional |base) rides in the
    // access_token slot of a never-expiring token.
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

/// Forget the stored addresses. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// `configured` is always true: public reads need no app credentials, so
/// connecting is just pasting addresses. The connected account, if any, shows a
/// compact summary of the addresses (never the full list when it's long).
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let (addrs, _base) = parse_connect_input(&token.access_token);
        if !addrs.is_empty() {
            accounts.push(ConnectedAccount {
                key: SERVICE.to_string(),
                label: address_summary(&addrs),
                connected_at: None, // the secret store doesn't record it
                expires_at: None,   // an address never expires
                needs_reconnect: false,
                extra: BTreeMap::new(),
            });
        }
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste one
/// or more public addresses. No api_key — Esplora's public REST is keyless.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "bitcoin",
    display_name: "Bitcoin Wallet (Blockstream)",
    methods: &[ConnectMethod::TokenPaste {
        label: "Bitcoin address(es)",
        help: "Paste one or more PUBLIC Bitcoin addresses (comma- or space-separated) — never a \
               private key or seed phrase. Transactions are read from the public chain via \
               Blockstream's keyless API. To use a self-hosted Esplora instead, append \
               |https://your-esplora/api after the addresses.",
        placeholder: "bc1q… , 3J98t… , 1A1zP…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["bitcoin"],
    setup: &[
        "Paste one or more of your public Bitcoin addresses (comma- or space-separated).",
        "Only public addresses — never private keys or seed phrases.",
        "Optional: append |https://your-esplora/api to read from a self-hosted Esplora instance.",
    ],
};

// ---------------------------------------------------------------------------
// Connect-input parsing — pure.

/// Split the pasted connect string into (addresses, base URL). The format is a
/// comma/whitespace-separated address list, optionally followed by
/// `|<base-url>` to override the Esplora host. Addresses are kept verbatim
/// (trimmed); a custom base has any trailing slash stripped.
fn parse_connect_input(pasted: &str) -> (Vec<String>, String) {
    let (addr_part, base) = match pasted.split_once('|') {
        Some((a, b)) => {
            let b = b.trim().trim_end_matches('/');
            (a, if b.is_empty() { DEFAULT_BASE.to_string() } else { b.to_string() })
        }
        None => (pasted, DEFAULT_BASE.to_string()),
    };
    let mut seen = HashSet::new();
    let addrs: Vec<String> = addr_part
        .split([',', ' ', '\n', '\t', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        // De-dup while preserving order (a user might paste the same address twice).
        .filter(|s| seen.insert(s.to_string()))
        .map(str::to_string)
        .collect();
    (addrs, base)
}

/// A compact label for the connected-account row: the first address, plus a
/// "+N more" when several were supplied (never dumps a long list into the UI).
fn address_summary(addrs: &[String]) -> String {
    match addrs.len() {
        0 => String::new(),
        1 => addrs[0].clone(),
        n => format!("{} +{} more", addrs[0], n - 1),
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch outcomes. A 400 (bad address) is distinguished so connect
/// can reject cleanly; 429/503 backs off; everything else is a message.
#[derive(Debug)]
enum FetchError {
    /// HTTP 400 — Esplora rejected the address as malformed.
    BadAddress,
    /// HTTP 429 / 503 — back off and retry.
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::BadAddress => write!(f, "bad or malformed address (HTTP 400)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429/503)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The Esplora endpoints this collector uses. A trait so tests drive the
/// mapping/persist logic against fixtures, never the network.
trait EsploraApi {
    /// `GET /address/:addr/txs` (first page, newest-first) when `after` is
    /// `None`, else `GET /address/:addr/txs/chain/:after` (the next confirmed
    /// page after `:after`). Returns the parsed array of tx objects.
    fn address_txs(&self, addr: &str, after: Option<&str>) -> Result<Vec<Value>, FetchError>;
}

/// Thin client; base URL injected (the github/lastfm/boardgamegeek pattern).
struct EsploraClient {
    base: String,
}

impl EsploraClient {
    fn new(base: String) -> Self {
        EsploraClient { base }
    }

    fn finish(result: Result<ureq::Response, ureq::Error>) -> Result<Vec<Value>, FetchError> {
        match result {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok(match v {
                    Value::Array(a) => a,
                    _ => Vec::new(),
                })
            }
            Err(ureq::Error::Status(400, _)) => Err(FetchError::BadAddress),
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

impl EsploraApi for EsploraClient {
    fn address_txs(&self, addr: &str, after: Option<&str>) -> Result<Vec<Value>, FetchError> {
        let url = match after {
            Some(txid) => format!("{}/address/{addr}/txs/chain/{txid}", self.base),
            None => format!("{}/address/{addr}/txs", self.base),
        };
        Self::finish(ureq::get(&url).timeout(HTTP_TIMEOUT).call())
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Per-address confirmed-chain cursor: the newest *confirmed* `txid` already
    /// drained for that address (never a mempool txid — the `…/txs/chain/<txid>`
    /// endpoint pages the confirmed chain). The next sync pages
    /// `…/txs/chain/<txid>` forward from it. A new address simply isn't present
    /// and backfills from scratch.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    last_seen_txid: BTreeMap<String, String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_bitcoin_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_bitcoin_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity tx object). The on-disk line is the verbatim
// Esplora object (flattened), tagged with the contract ts purely so the
// month-partition writer files it under the right month. Only `value` is
// serialized.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// A `vin`/`vout` entry's `value` in satoshis (an integer). Coinbase inputs and
/// pruned prevouts may be absent → 0.
fn sats_of(entry: &Value, key: &str) -> i64 {
    entry.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// The `scriptpubkey_address` of an object (a `vout`, or a `vin`'s `prevout`).
/// Empty when absent (a coinbase input, a non-address script).
fn address_of(obj: &Value) -> &str {
    obj.get("scriptpubkey_address").and_then(Value::as_str).unwrap_or("")
}

/// Whether a tx is confirmed (`status.confirmed == true`). A mempool tx has
/// `confirmed: false`. **Load-bearing for the cursor**: `GET /address/:addr/txs`
/// returns up to 50 mempool txs FIRST, then the first 25 confirmed txs (per the
/// Esplora API.md), but the `…/txs/chain/:last_seen_txid` cursor pages the
/// CONFIRMED chain — so the watermark must only ever be a confirmed txid, or the
/// next sync's `/chain/<mempool-txid>` request strands every tx that confirms in
/// the gap. We therefore pick the watermark/page cursors from confirmed txs only.
fn is_confirmed(tx: &Value) -> bool {
    tx.get("status").and_then(|s| s.get("confirmed")).and_then(Value::as_bool) == Some(true)
}

/// The `txid` of a tx, when present and non-empty.
fn txid_of(tx: &Value) -> Option<&str> {
    tx.get("txid").and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Convert satoshis (signed) to a BTC `number`. f64 represents every realistic
/// satoshi count exactly as an integer; dividing by 1e8 is the standard BTC
/// rendering. The exact integer sats are preserved in `extra`/raw, so this is a
/// presentation amount, never the lossless record.
fn sats_to_btc(sats: i64) -> f64 {
    sats as f64 / SATS_PER_BTC
}

/// A Unix timestamp (seconds) → RFC3339 local. `None` when the value is out of
/// range (an unconfirmed/mempool tx has no `block_time`).
fn block_time_to_local(secs: i64) -> Option<String> {
    Local.timestamp_opt(secs, 0).single().map(|dt| dt.to_rfc3339())
}

/// One Esplora tx → a contract [`LineItem`], given the set of the user's owned
/// addresses. Computes the wallet's net value change (Σ owned vout − Σ owned vin
/// prevout, in sats). `None` when the tx has no usable `block_time` (mempool /
/// unconfirmed — it can't be partitioned; it'll land once confirmed) or no
/// `txid` (can't dedupe).
fn line_item_from(tx: &Value, owned: &HashSet<String>) -> Option<LineItem> {
    let txid = tx.get("txid").and_then(Value::as_str).filter(|s| !s.is_empty())?;

    // block_time (Unix seconds) lives under status.
    let status = tx.get("status");
    let block_time = status.and_then(|s| s.get("block_time")).and_then(Value::as_i64)?;
    let ts = block_time_to_local(block_time)?;
    // Must yield a month partition; otherwise the row can't be filed.
    Partition::Month.key(&ts)?;

    // Net = owned outputs received − owned inputs spent (satoshis).
    let mut received_sats: i64 = 0;
    let mut sent_sats: i64 = 0;
    let mut counterparties_in: Vec<Value> = Vec::new();
    let mut counterparties_out: Vec<Value> = Vec::new();

    if let Some(vout) = tx.get("vout").and_then(Value::as_array) {
        for o in vout {
            let addr = address_of(o);
            let val = sats_of(o, "value");
            if owned.contains(addr) {
                received_sats += val;
            } else if !addr.is_empty() {
                counterparties_out.push(Value::String(addr.to_string()));
            }
        }
    }
    if let Some(vin) = tx.get("vin").and_then(Value::as_array) {
        for i in vin {
            let prevout = i.get("prevout").unwrap_or(i);
            let addr = address_of(prevout);
            let val = sats_of(prevout, "value");
            if owned.contains(addr) {
                sent_sats += val;
            } else if !addr.is_empty() {
                counterparties_in.push(Value::String(addr.to_string()));
            }
        }
    }

    let net_sats = received_sats - sent_sats;
    let fee_sats = sats_of(tx, "fee");
    let direction = if net_sats >= 0 { "received" } else { "sent" };

    let mut extra = Map::new();
    extra.insert("direction".into(), Value::String(direction.into()));
    extra.insert("value_sats".into(), Value::from(net_sats));
    extra.insert("received_sats".into(), Value::from(received_sats));
    extra.insert("sent_sats".into(), Value::from(sent_sats));
    if fee_sats > 0 {
        extra.insert("fee_sats".into(), Value::from(fee_sats));
        extra.insert("fee_btc".into(), Value::from(sats_to_btc(fee_sats)));
    }
    if let Some(h) = status.and_then(|s| s.get("block_height")).and_then(Value::as_i64) {
        extra.insert("block_height".into(), Value::from(h));
    }
    if let Some(bh) = status.and_then(|s| s.get("block_hash")).and_then(Value::as_str) {
        if !bh.is_empty() {
            extra.insert("block_hash".into(), Value::String(bh.to_string()));
        }
    }
    // The non-owned addresses on the other side of the transfer (the closest
    // observable thing to a counterparty — kept for read-time reconciliation).
    if direction == "received" && !counterparties_in.is_empty() {
        extra.insert("from_addresses".into(), Value::Array(counterparties_in));
    }
    if direction == "sent" && !counterparties_out.is_empty() {
        extra.insert("to_addresses".into(), Value::Array(counterparties_out));
    }

    let mut li = LineItem::new("bitcoin", txid, ts, "Bitcoin");
    li.item = if direction == "received" { "Received".into() } else { "Sent".into() };
    li.amount = Some(sats_to_btc(net_sats));
    li.currency = "BTC".into();
    // `category` is a *source-native* department (Electronics, Productivity); the
    // chain emits none, so we leave it empty (an honest absence per the domain's
    // omit-empty rule) rather than inventing a synthetic "Crypto" label.
    if status.and_then(|s| s.get("confirmed")).and_then(Value::as_bool) == Some(true) {
        li.status = "confirmed".into();
    }
    li.extra = extra;
    Some(li)
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid against what's already on disk.

/// Append new contract + raw rows, deduped by guid (txid). Returns the count of
/// new contract rows written. Raw lines partition by the same month as their
/// contract row.
fn write_layer(vault: &Vault, rows: Vec<(LineItem, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids in the contract stream — re-runnable: a re-pull of an
    // overlapping window never duplicates (the readwise/letterboxd pattern).
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
            continue; // no id, or already stored (incl. a tx touching 2 owned addrs)
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

/// Resolve the addresses and sync. Missing address ⇒ a quiet skip on the
/// periodic path (mirror boardgamegeek/todoist), a clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let pasted = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|s| !s.trim().is_empty())
        .context("Bitcoin is not connected — add your address(es) in the Integrations tab")?;
    let (_addrs, base) = parse_connect_input(&pasted);
    let client = EsploraClient::new(base);
    pull_with(vault, &client, &pasted)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl EsploraApi, pasted: &str) -> Result<PullOutcome> {
    let (addrs, _base) = parse_connect_input(pasted);
    if addrs.is_empty() {
        bail!("Bitcoin has no usable address — re-connect with a public address");
    }
    let owned: HashSet<String> = addrs.iter().cloned().collect();
    let mut state = vault.read_bitcoin_sync();

    // Drain every address's whole new confirmed window FIRST (collecting raw tx
    // objects + their next-cursor), before any watermark advances — a partial
    // drain must not move a cursor, so a crash re-drains. guid dedupe across the
    // collected set means a tx that touches two owned addresses is written once.
    let mut collected: Vec<Value> = Vec::new();
    let mut next_cursor: BTreeMap<String, String> = BTreeMap::new();

    for addr in &addrs {
        let from = state.last_seen_txid.get(addr).cloned();
        let (txs, newest) = drain_address(api, addr, from.as_deref())
            .map_err(|e| fetch_err(addr, e))?;
        // The newest txid for this address becomes its next cursor — but only
        // committed into `state` after the whole pull's writes succeed (below).
        if let Some(n) = newest {
            next_cursor.insert(addr.clone(), n);
        } else if let Some(prev) = from {
            // No new confirmed txs this window — keep the existing cursor.
            next_cursor.insert(addr.clone(), prev);
        }
        collected.extend(txs);
        thread::sleep(REQ_INTERVAL);
    }

    // Map → contract rows (skipping mempool/unconfirmed with no block_time), and
    // write both layers deduped by txid.
    let rows: Vec<(LineItem, Value)> = collected
        .iter()
        .filter_map(|tx| line_item_from(tx, &owned).map(|li| (li, tx.clone())))
        .collect();
    let written = write_layer(vault, rows)?;

    // Advance cursors only after the writes succeed (forward progress committed
    // once, so a crash mid-write re-drains).
    for (addr, txid) in next_cursor {
        state.last_seen_txid.insert(addr, txid);
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_bitcoin_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{written} transactions"),
        counts: BTreeMap::from([("transactions", written)]),
    })
}

/// Drain one address's confirmed-chain history from `after` forward, following
/// `…/txs/chain/:last_seen_txid` until a short/empty page. Returns
/// `(all tx objects newer than the cursor, the newest CONFIRMED txid seen)`.
///
/// **The watermark is the newest CONFIRMED txid, never a mempool one.** The first
/// page (`GET /address/:addr/txs`) lists up to 50 mempool txs FIRST, then the
/// first 25 confirmed (per the Esplora API.md), all newest-first — so the newest
/// confirmed tx is the first entry whose `status.confirmed` is true (the leading
/// mempool entries are skipped). The `…/txs/chain/:last_seen_txid` cursor pages
/// the confirmed chain only, so persisting a mempool txid would strand every tx
/// that confirms before the next sync. The page cursor (the `:last_seen_txid` for
/// the *next* request) is likewise the last CONFIRMED txid of the page. Mempool
/// txs are still collected and returned (they map to nothing until confirmed, but
/// keeping them is harmless — `line_item_from` skips any without a `block_time`).
/// Pagination stops on a page shorter than the 25/page size (the last page) or
/// empty (past the end).
fn drain_address(
    api: &impl EsploraApi,
    addr: &str,
    after: Option<&str>,
) -> Result<(Vec<Value>, Option<String>), FetchError> {
    let mut all: Vec<Value> = Vec::new();
    let mut newest: Option<String> = None;
    // The chain cursor for the *next* page request: the last CONFIRMED txid of
    // the page we just consumed. Starts at the caller's saved cursor so we only
    // pull txs confirmed AFTER it.
    let mut page_cursor: Option<String> = after.map(str::to_string);
    let mut pages = 0;

    loop {
        let page = fetch_page(api, addr, page_cursor.as_deref())?;
        if page.is_empty() {
            break;
        }
        // The watermark is the newest CONFIRMED txid — the first confirmed entry
        // on the (newest-first) first page, skipping any leading mempool txs.
        if newest.is_none() {
            newest = page.iter().find(|t| is_confirmed(t)).and_then(txid_of).map(str::to_string);
        }
        let short = page.len() < PAGE_SIZE;
        // The next chain page starts after the last CONFIRMED txid of this page
        // (the `/chain/` endpoint pages confirmed txs, so the cursor must be a
        // confirmed txid — never a leading mempool one).
        let last_txid =
            page.iter().rev().find(|t| is_confirmed(t)).and_then(txid_of).map(str::to_string);
        all.extend(page);
        pages += 1;
        if short || pages >= MAX_PAGES {
            break;
        }
        match last_txid {
            Some(t) => page_cursor = Some(t),
            None => break, // no confirmed txid to advance past (e.g. an all-mempool page)
        }
        thread::sleep(REQ_INTERVAL);
    }

    Ok((all, newest))
}

/// One page fetch with a single rate-limit back-off-and-retry (429/503).
fn fetch_page(
    api: &impl EsploraApi,
    addr: &str,
    after: Option<&str>,
) -> Result<Vec<Value>, FetchError> {
    match api.address_txs(addr, after) {
        Ok(v) => Ok(v),
        Err(FetchError::RateLimited) => {
            thread::sleep(Duration::from_secs(2));
            api.address_txs(addr, after)
        }
        Err(e) => Err(e),
    }
}

/// Map a [`FetchError`] at the top of an address drain into an anyhow error.
fn fetch_err(addr: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::BadAddress => anyhow::anyhow!(
            "Blockstream rejected the address {addr:?} (400) — re-connect with a valid address"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Blockstream rate limited the {addr:?} request (429/503) — it'll retry on the next sync"
        ),
        other => anyhow::anyhow!("Bitcoin fetch failed for {addr:?}: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-bitcoin-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures: synthesized from the CONFIRMED Esplora API.md tx shape ---
    // (txid, fee, status{confirmed,block_height,block_hash,block_time}, vin with
    // prevout{scriptpubkey_address,value}, vout{scriptpubkey_address,value};
    // ALL values in satoshis, block_time a Unix timestamp in seconds).

    const MY_ADDR: &str = "bc1qmyaddrreceiverxxxxxxxxxxxxxxxxxxxxxxxx";
    const MY_CHANGE: &str = "bc1qmychangexxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    const EXT_ADDR: &str = "bc1qexternalpartyxxxxxxxxxxxxxxxxxxxxxxxxx";

    /// A RECEIVE: an external input pays 0.05 BTC (5_000_000 sats) to my address;
    /// change goes back to the sender. block_time 1748415600 = 2026-05-28 UTC.
    fn tx_receive() -> Value {
        json!({
            "txid": "aaaa1111receive",
            "version": 2,
            "locktime": 0,
            "size": 225,
            "weight": 561,
            "fee": 1500,
            "status": {
                "confirmed": true,
                "block_height": 842000,
                "block_hash": "0000000000000000000abc",
                "block_time": 1748415600
            },
            "vin": [
                {
                    "txid": "prevtx0",
                    "vout": 0,
                    "is_coinbase": false,
                    "sequence": 4294967295u32,
                    "prevout": {
                        "scriptpubkey": "0014deadbeef",
                        "scriptpubkey_address": EXT_ADDR,
                        "value": 8000000
                    }
                }
            ],
            "vout": [
                {
                    "scriptpubkey": "0014myaddr",
                    "scriptpubkey_address": MY_ADDR,
                    "value": 5000000
                },
                {
                    "scriptpubkey": "0014extchange",
                    "scriptpubkey_address": EXT_ADDR,
                    "value": 2998500
                }
            ]
        })
    }

    /// A SEND: my address spends 5_000_000 sats; 1_200_000 goes to an external
    /// payee and the rest (minus fee) returns to my change address.
    fn tx_send() -> Value {
        json!({
            "txid": "bbbb2222send",
            "version": 2,
            "locktime": 0,
            "fee": 2000,
            "status": {
                "confirmed": true,
                "block_height": 842500,
                "block_hash": "0000000000000000000def",
                "block_time": 1749020400
            },
            "vin": [
                {
                    "txid": "aaaa1111receive",
                    "vout": 0,
                    "is_coinbase": false,
                    "prevout": {
                        "scriptpubkey_address": MY_ADDR,
                        "value": 5000000
                    }
                }
            ],
            "vout": [
                {
                    "scriptpubkey_address": EXT_ADDR,
                    "value": 1200000
                },
                {
                    "scriptpubkey_address": MY_CHANGE,
                    "value": 3798000
                }
            ]
        })
    }

    /// A mempool (unconfirmed) tx — NO block_time. Must be skipped (can't be
    /// partitioned); it'll land once it confirms.
    fn tx_mempool() -> Value {
        json!({
            "txid": "cccc3333mempool",
            "fee": 1000,
            "status": { "confirmed": false },
            "vin": [{ "prevout": { "scriptpubkey_address": EXT_ADDR, "value": 1000000 } }],
            "vout": [{ "scriptpubkey_address": MY_ADDR, "value": 999000 }]
        })
    }

    fn owned() -> HashSet<String> {
        [MY_ADDR.to_string(), MY_CHANGE.to_string()].into_iter().collect()
    }

    // --- connect-input parsing ---

    #[test]
    fn parses_address_list_and_optional_base() {
        let (a, b) = parse_connect_input("bc1qaaa, bc1qbbb  bc1qccc");
        assert_eq!(a, vec!["bc1qaaa", "bc1qbbb", "bc1qccc"]);
        assert_eq!(b, DEFAULT_BASE, "no override → default base");

        let (a2, b2) = parse_connect_input("bc1qaaa | https://my-esplora.example/api/");
        assert_eq!(a2, vec!["bc1qaaa"]);
        assert_eq!(b2, "https://my-esplora.example/api", "trailing slash stripped");

        // Duplicate addresses collapse; empties are dropped.
        let (a3, _) = parse_connect_input(" bc1qaaa , , bc1qaaa ");
        assert_eq!(a3, vec!["bc1qaaa"], "deduped, order preserved");

        assert!(parse_connect_input("   ").0.is_empty());
    }

    #[test]
    fn address_summary_is_compact() {
        assert_eq!(address_summary(&["bc1qaaa".into()]), "bc1qaaa");
        assert_eq!(
            address_summary(&["bc1qaaa".into(), "bc1qbbb".into(), "bc1qccc".into()]),
            "bc1qaaa +2 more"
        );
    }

    // --- pure mapping ---

    #[test]
    fn maps_a_receive_to_a_positive_line_item() {
        let li = line_item_from(&tx_receive(), &owned()).unwrap();
        assert_eq!(li.source, "bitcoin");
        assert_eq!(li.guid, "aaaa1111receive", "guid is the txid (stable)");
        assert_eq!(li.merchant, "Bitcoin");
        assert_eq!(li.item, "Received");
        assert_eq!(li.currency, "BTC");
        assert_eq!(li.category, "", "no source-native category — left empty, not a synthetic label");
        assert_eq!(li.status, "confirmed");
        // Net = 5_000_000 received − 0 sent = +0.05 BTC.
        assert_eq!(li.amount, Some(0.05));
        assert_eq!(li.extra.get("direction"), Some(&json!("received")));
        assert_eq!(li.extra.get("value_sats"), Some(&json!(5_000_000)));
        assert_eq!(li.extra.get("fee_sats"), Some(&json!(1500)));
        assert_eq!(li.extra.get("block_height"), Some(&json!(842000)));
        // The external input shows up as a from-address.
        assert_eq!(li.extra.get("from_addresses"), Some(&json!([EXT_ADDR])));
        // ts is the block_time (Unix secs) rendered in local tz — same instant.
        assert_eq!(DateTime::parse_from_rfc3339(&li.ts).unwrap().timestamp(), 1748415600);
    }

    #[test]
    fn maps_a_send_to_a_negative_net_line_item() {
        let li = line_item_from(&tx_send(), &owned()).unwrap();
        assert_eq!(li.guid, "bbbb2222send");
        assert_eq!(li.item, "Sent");
        // received_sats counts only owned outputs (the change), sent_sats the
        // owned input. Net = 3_798_000 − 5_000_000 = −1_202_000 sats.
        assert_eq!(li.extra.get("received_sats"), Some(&json!(3_798_000)), "owned change");
        assert_eq!(li.extra.get("sent_sats"), Some(&json!(5_000_000)), "owned input");
        assert_eq!(li.extra.get("value_sats"), Some(&json!(-1_202_000)));
        assert_eq!(li.amount, Some(sats_to_btc(-1_202_000)));
        assert_eq!(li.extra.get("direction"), Some(&json!("sent")));
        // The external payee is a to-address.
        assert_eq!(li.extra.get("to_addresses"), Some(&json!([EXT_ADDR])));
    }

    #[test]
    fn mempool_tx_without_block_time_is_skipped() {
        assert!(line_item_from(&tx_mempool(), &owned()).is_none(), "no block_time → not filed");
    }

    #[test]
    fn sats_to_btc_is_exact_for_realistic_values() {
        assert_eq!(sats_to_btc(100_000_000), 1.0);
        assert_eq!(sats_to_btc(5_000_000), 0.05);
        assert_eq!(sats_to_btc(0), 0.0);
    }

    // --- a scripted mock API ---

    struct MockApi {
        // addr → queue of pages (each an array of tx objects), consumed in order.
        pages: RefCell<BTreeMap<String, VecDeque<Result<Vec<Value>, FetchError>>>>,
        // record the (addr, after) calls so cursor logic can be asserted.
        calls: RefCell<Vec<(String, Option<String>)>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi { pages: RefCell::new(BTreeMap::new()), calls: RefCell::new(Vec::new()) }
        }
        fn push(self, addr: &str, page: Vec<Value>) -> Self {
            self.pages.borrow_mut().entry(addr.to_string()).or_default().push_back(Ok(page));
            self
        }
    }

    impl EsploraApi for MockApi {
        fn address_txs(&self, addr: &str, after: Option<&str>) -> Result<Vec<Value>, FetchError> {
            self.calls.borrow_mut().push((addr.to_string(), after.map(str::to_string)));
            self.pages
                .borrow_mut()
                .get_mut(addr)
                .and_then(|q| q.pop_front())
                .unwrap_or(Ok(Vec::new()))
        }
    }

    #[test]
    fn full_pull_writes_both_layers_dedupes_and_advances_cursor() {
        let v = temp_vault("fullpull");
        // One address, one page with a receive + a send (+ a mempool tx that's
        // skipped). Newest-first: the receive is first → it becomes the cursor.
        let api = MockApi::new().push(MY_ADDR, vec![tx_receive(), tx_send(), tx_mempool()]);

        let out = pull_with(&v, &api, MY_ADDR).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&2), "mempool tx skipped");

        // Contract rows partitioned by block_time month — read them all back.
        let contract = v.stream(DIR, Partition::Month);
        let mut all_rows: Vec<Value> = Vec::new();
        for key in contract.partitions().unwrap() {
            all_rows.extend(contract.read::<Value>(&key).unwrap());
        }
        assert_eq!(all_rows.len(), 2);
        let guids: HashSet<String> = all_rows
            .iter()
            .map(|r| r.get("guid").and_then(Value::as_str).unwrap_or("").to_string())
            .collect();
        assert!(guids.contains("aaaa1111receive") && guids.contains("bbbb2222send"));
        // A row carries the contract amount as a number and BTC currency.
        let receive = all_rows.iter().find(|r| r["guid"] == "aaaa1111receive").unwrap();
        assert_eq!(receive["amount"], json!(0.05));
        assert_eq!(receive["currency"], "BTC");
        assert_eq!(receive["merchant"], "Bitcoin");

        // Raw layer preserves a field the contract drops (vin prevout value).
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_rows: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_rows.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_rows.len(), 2, "raw mirrors contract count");
        let raw_recv = raw_rows.iter().find(|r| r["txid"] == "aaaa1111receive").unwrap();
        assert_eq!(raw_recv["vin"][0]["prevout"]["value"], json!(8000000), "raw keeps full input value");

        // Cursor advanced to the newest txid (first of the newest-first page).
        let state = v.read_bitcoin_sync();
        assert_eq!(state.last_seen_txid.get(MY_ADDR).map(String::as_str), Some("aaaa1111receive"));
        assert!(state.updated.is_some());

        // Re-run with the same page → guid dedupe, byte-identical contract files.
        let before: BTreeMap<String, String> = contract
            .partitions()
            .unwrap()
            .into_iter()
            .map(|k| (k.clone(), std::fs::read_to_string(v.root().join(format!("{DIR}/{k}.jsonl"))).unwrap()))
            .collect();
        let again =
            pull_with(&v, &MockApi::new().push(MY_ADDR, vec![tx_receive(), tx_send()]), MY_ADDR).unwrap();
        assert_eq!(again.counts.get("transactions"), Some(&0), "all dedup'd");
        for (k, body) in &before {
            let after = std::fs::read_to_string(v.root().join(format!("{DIR}/{k}.jsonl"))).unwrap();
            assert_eq!(body, &after, "contract file byte-identical after re-run");
        }
    }

    #[test]
    fn self_transfer_across_two_owned_addresses_is_one_row() {
        // tx_send spends MY_ADDR and pays change to MY_CHANGE — both owned. If we
        // scanned per-address we'd write it twice; the global guid dedupe writes
        // it once even when BOTH owned addresses are connected and BOTH return it.
        let v = temp_vault("selftransfer");
        let api = MockApi::new()
            .push(MY_ADDR, vec![tx_send()])
            .push(MY_CHANGE, vec![tx_send()]);
        let input = format!("{MY_ADDR}, {MY_CHANGE}");
        let out = pull_with(&v, &api, &input).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&1), "one row for the tx, not two");

        let contract = v.stream(DIR, Partition::Month);
        let mut rows = 0;
        for key in contract.partitions().unwrap() {
            rows += contract.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(rows, 1);
        // Both addresses got their cursor set to the tx.
        let state = v.read_bitcoin_sync();
        assert_eq!(state.last_seen_txid.get(MY_ADDR).map(String::as_str), Some("bbbb2222send"));
        assert_eq!(state.last_seen_txid.get(MY_CHANGE).map(String::as_str), Some("bbbb2222send"));
    }

    #[test]
    fn pagination_follows_chain_cursor_until_short_page() {
        // 25 txs (a full page) then a short page of 1 → two requests; the second
        // uses the last txid of the first page as the chain cursor.
        let mut page1: Vec<Value> = Vec::new();
        for i in 0..PAGE_SIZE {
            let mut tx = tx_receive();
            tx["txid"] = json!(format!("page1-{i:02}"));
            page1.push(tx);
        }
        let last_of_page1 = format!("page1-{:02}", PAGE_SIZE - 1);
        let mut tail = tx_send();
        tail["txid"] = json!("page2-tail");

        let v = temp_vault("paginate");
        let api = MockApi::new().push(MY_ADDR, page1).push(MY_ADDR, vec![tail]);
        let out = pull_with(&v, &api, MY_ADDR).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&26), "25 + 1 across two pages");

        // Two calls: page 1 (no cursor), page 2 (chain cursor = last txid of p1).
        let calls = api.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], (MY_ADDR.to_string(), None));
        assert_eq!(calls[1], (MY_ADDR.to_string(), Some(last_of_page1)));
        // Cursor advanced to the newest txid (first of page 1).
        let state = v.read_bitcoin_sync();
        assert_eq!(state.last_seen_txid.get(MY_ADDR).map(String::as_str), Some("page1-00"));
    }

    #[test]
    fn incremental_pull_resumes_from_saved_cursor() {
        let v = temp_vault("incremental");
        // First sync: one tx, cursor saved.
        pull_with(&v, &MockApi::new().push(MY_ADDR, vec![tx_receive()]), MY_ADDR).unwrap();
        assert_eq!(
            v.read_bitcoin_sync().last_seen_txid.get(MY_ADDR).map(String::as_str),
            Some("aaaa1111receive")
        );

        // Second sync: the chain request must carry the saved cursor as `after`.
        let api2 = MockApi::new().push(MY_ADDR, vec![tx_send()]);
        let out = pull_with(&v, &api2, MY_ADDR).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&1));
        let calls = api2.calls.borrow();
        assert_eq!(
            calls[0],
            (MY_ADDR.to_string(), Some("aaaa1111receive".to_string())),
            "resumed from cursor"
        );
        // Cursor moved forward to the new newest.
        assert_eq!(
            v.read_bitcoin_sync().last_seen_txid.get(MY_ADDR).map(String::as_str),
            Some("bbbb2222send")
        );
    }

    #[test]
    fn empty_window_keeps_existing_cursor_and_writes_nothing() {
        let v = temp_vault("empty");
        // Seed a cursor by a first sync.
        pull_with(&v, &MockApi::new().push(MY_ADDR, vec![tx_receive()]), MY_ADDR).unwrap();
        // Now an empty window (no new confirmed txs).
        let out = pull_with(&v, &MockApi::new(), MY_ADDR).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&0));
        // The existing cursor is preserved (not cleared by the empty drain).
        assert_eq!(
            v.read_bitcoin_sync().last_seen_txid.get(MY_ADDR).map(String::as_str),
            Some("aaaa1111receive")
        );
    }

    #[test]
    fn cursor_is_newest_confirmed_txid_when_mempool_leads_the_page() {
        // THE REAL ORDERING: `GET /address/:addr/txs` returns up to 50 mempool
        // txs FIRST, then the first 25 confirmed (Esplora API.md). So the first
        // tx of the first page is a MEMPOOL txid whenever an unconfirmed tx is
        // pending (common on an hourly poll of an active wallet). The persisted
        // watermark must be the newest CONFIRMED txid — never the mempool one —
        // or the next sync's `…/txs/chain/<mempool-txid>` request strands every
        // tx that confirms in the gap (silent financial-data loss).
        let v = temp_vault("mempoolfirst");
        // Page as the API actually returns it: mempool tx FIRST, then confirmed.
        let api = MockApi::new().push(MY_ADDR, vec![tx_mempool(), tx_receive(), tx_send()]);

        let out = pull_with(&v, &api, MY_ADDR).unwrap();
        // Only the two confirmed txs are filed; the mempool tx has no block_time.
        assert_eq!(out.counts.get("transactions"), Some(&2), "mempool tx skipped");

        // The cursor is the newest CONFIRMED txid (tx_receive, first confirmed on
        // the page) — NOT the leading mempool txid "cccc3333mempool".
        let state = v.read_bitcoin_sync();
        assert_eq!(
            state.last_seen_txid.get(MY_ADDR).map(String::as_str),
            Some("aaaa1111receive"),
            "watermark is the newest CONFIRMED txid, not the leading mempool one"
        );

        // And it self-heals on the next sync: when the pending tx later confirms,
        // a `…/txs/chain/aaaa1111receive` page returns it and it gets filed —
        // proving the cursor is a usable confirmed-chain cursor, not a dead end.
        let mut confirmed_now = tx_mempool();
        confirmed_now["status"] = json!({
            "confirmed": true,
            "block_height": 842600,
            "block_hash": "0000000000000000000fed",
            "block_time": 1749106800
        });
        let api2 = MockApi::new().push(MY_ADDR, vec![confirmed_now]);
        let out2 = pull_with(&v, &api2, MY_ADDR).unwrap();
        assert_eq!(out2.counts.get("transactions"), Some(&1), "the now-confirmed tx lands");
        // The chain request carried the saved CONFIRMED cursor as `after`.
        assert_eq!(
            api2.calls.borrow()[0],
            (MY_ADDR.to_string(), Some("aaaa1111receive".to_string())),
            "resumed from the confirmed watermark"
        );
        // Cursor advanced to the freshly-confirmed tx.
        assert_eq!(
            v.read_bitcoin_sync().last_seen_txid.get(MY_ADDR).map(String::as_str),
            Some("cccc3333mempool")
        );
    }

    #[test]
    fn page_cursor_skips_trailing_mempool_when_paging() {
        // A full first page whose LAST entries are mempool must page the chain
        // from the last CONFIRMED txid, not a trailing mempool one (the `/chain/`
        // endpoint pages confirmed txs only). Construct a >25-entry first page:
        // 25 confirmed followed by a mempool tail, so page.last() is mempool.
        let mut page1: Vec<Value> = Vec::new();
        for i in 0..PAGE_SIZE {
            let mut tx = tx_receive();
            tx["txid"] = json!(format!("confd-{i:02}"));
            page1.push(tx);
        }
        let last_confirmed = format!("confd-{:02}", PAGE_SIZE - 1);
        // A mempool entry can ride the same first-page response; here it trails.
        page1.push(tx_mempool());
        let mut tail = tx_send();
        tail["txid"] = json!("page2-tail");

        let v = temp_vault("trailingmempool");
        let api = MockApi::new().push(MY_ADDR, page1).push(MY_ADDR, vec![tail]);
        let out = pull_with(&v, &api, MY_ADDR).unwrap();
        // 25 confirmed + 1 tail confirmed = 26 filed (the mempool tail is skipped).
        assert_eq!(out.counts.get("transactions"), Some(&26));

        // The second request's chain cursor is the last CONFIRMED txid, never the
        // trailing mempool one.
        let calls = api.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], (MY_ADDR.to_string(), Some(last_confirmed)));
        // The watermark is still the newest confirmed (first confirmed entry).
        assert_eq!(
            v.read_bitcoin_sync().last_seen_txid.get(MY_ADDR).map(String::as_str),
            Some("confd-00")
        );
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        // An empty cursor file deserializes to all-default (a first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_seen_txid.is_empty());
        assert!(empty.updated.is_none());
        // An older cursor with only `updated` set still deserializes (additive).
        let partial: SyncState =
            serde_json::from_str(r#"{"updated":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert!(partial.last_seen_txid.is_empty());
        assert_eq!(partial.updated.as_deref(), Some("2026-06-01T00:00:00-07:00"));
    }

    // --- connection ---

    #[test]
    fn connection_stores_addresses_and_status_summarizes() {
        let v = temp_vault("conn");
        // Store directly (def_connect needs the network for the probe).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "bc1qaaa, bc1qbbb".into(),
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
        assert_eq!(status.accounts[0].key, "bitcoin");
        assert_eq!(status.accounts[0].label, "bc1qaaa +1 more");

        def_disconnect(&v, "bitcoin").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn connect_with_rejects_bad_address_and_stores_good_one() {
        let v = temp_vault("connect");
        // A stub that returns BadAddress → connect bails, nothing stored.
        struct BadApi;
        impl EsploraApi for BadApi {
            fn address_txs(&self, _a: &str, _b: Option<&str>) -> Result<Vec<Value>, FetchError> {
                Err(FetchError::BadAddress)
            }
        }
        assert!(connect_with(&v, &BadApi, "not-an-address").is_err());
        assert!(v.load_sync_token(SERVICE).unwrap().is_none(), "bad address not stored");

        // A stub that returns an OK page → connect stores the pasted string.
        let ok = MockApi::new().push("bc1qgood", vec![tx_receive()]);
        connect_with(&v, &ok, "bc1qgood").unwrap();
        assert_eq!(
            v.load_sync_token(SERVICE).unwrap().map(|t| t.access_token).as_deref(),
            Some("bc1qgood")
        );
    }

    #[test]
    fn pull_needs_connection() {
        let v = temp_vault("needsconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method_and_links_def() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "bitcoin");
        assert_eq!(DEF.connection, Some("bitcoin"));
    }
}

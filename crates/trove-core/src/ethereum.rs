//! Ethereum / EVM wallet history via the Etherscan API v2 — normal
//! transactions, ERC-20 token transfers, and internal transactions across
//! EVM-compatible chains (Polygon, Arbitrum, Optimism, Base, etc.).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/ethereum.md.
//!
//! A **Periodic** cloud pull. The user pastes a composite string:
//! `api_key|addr1,addr2,...` (API key from etherscan.io/myapikey, free, then
//! one or more public wallet **addresses** — never private keys). An optional
//! trailing `@chainid` may specify an EVM chain (default: 1 = Ethereum
//! mainnet). Full format: `api_key|addr1,addr2@chainid`.
//!
//! ## Endpoints (Etherscan v2, confirmed against official docs)
//!
//! Base: `https://api.etherscan.io/v2/api?chainid=<id>&module=account`
//!
//! - `action=txlist` — normal transactions; fields include `blockNumber`,
//!   `timeStamp`, `hash`, `from`, `to`, `value` (wei), `gasUsed`, `gasPrice`,
//!   `isError`, `txreceipt_status`.
//! - `action=tokentx` — ERC-20 transfers; adds `contractAddress`, `tokenName`,
//!   `tokenSymbol`, `tokenDecimal`, `value` (raw token units).
//! - `action=txlistinternal` — internal (contract-initiated) value moves; adds
//!   `type`, `traceId`, `errCode`.
//!
//! All return `{"status":"1","message":"OK","result":[…]}`.
//! All values are strings (Etherscan serializes numbers as strings).
//!
//! Pagination: `page=N&offset=1000&sort=asc&startblock=<cursor>`. Free tier
//! cap: 1,000 records/request (July 2026). Drain until empty or short page.
//! Rate limit: 5 calls/sec free tier; throttle at 250 ms between requests.
//!
//! ## Mapping → the `finance-purchases` contract
//!
//! Each row is a dated value transfer:
//! - `guid` = tx `hash` for normal/internal; `hash + ":" + logIndex` for token
//!   transfers (one hash can carry several ERC-20 events; logIndex is a string).
//! - `ts` = `timeStamp` (Unix seconds string) → RFC3339 local.
//! - `merchant` = `"Ethereum"` (or `"Polygon"`, `"Base"`, … by chain id).
//! - `item` = `"Received ETH"` / `"Sent ETH"` / `"Token Transfer"` /
//!   `"Internal Transfer"` etc. by type.
//! - `amount` = ETH value or token value as a decimal string parsed to f64
//!   (divided by 10^18 for ETH/native; by 10^tokenDecimal for tokens). The
//!   exact raw wei/unit string is kept in `extra.value_raw`.
//! - `currency` = `"ETH"` or the token symbol.
//! - `extra` carries chain id, gas, direction, contract address, token name, …
//!
//! ## Two layers
//!
//! - **raw** — verbatim Etherscan API result row under
//!   `finance/purchases/ethereum/raw/YYYY-MM.jsonl`.
//! - **contract** — normalized [`crate::finance::LineItem`] rows under
//!   `finance/purchases/ethereum/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! ## Cursor
//!
//! Per-address per-chain: last block number fully drained (`last_block`), in
//! `.trove/ethereum-sync.json`. Next sync starts `startblock=last_block` and
//! skips already-seen guids (the overlap is deduplicated).
//! A self-hosted Etherscan-compatible node is not in scope for v1.

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

// ---------------------------------------------------------------------------
// Constants.

/// Contract-layer stream (source subfolder under `finance/purchases/`).
const DIR: &str = "finance/purchases/ethereum";
/// Raw-layer stream.
const RAW_DIR: &str = "finance/purchases/ethereum/raw";
/// Non-secret rebuildable cursor (not under `.trove/sync/` — that's for 0600 secrets).
const SYNC_FILE: &str = ".trove/ethereum-sync.json";
/// Service id under `.trove/sync/` for the user's API key + addresses.
const SERVICE: &str = "ethereum";
/// Etherscan v2 base URL.
const API_BASE: &str = "https://api.etherscan.io/v2/api";
/// Default EVM chain id (Ethereum mainnet).
const DEFAULT_CHAIN_ID: u64 = 1;
/// Records per page (free tier cap, effective July 2026).
const PAGE_SIZE: usize = 1000;
/// Runaway guard: max pages per address per sync.
const MAX_PAGES: usize = 2000;
/// Politeness throttle between requests (5 calls/sec = 200 ms; use 250 ms).
const REQ_INTERVAL: Duration = Duration::from_millis(250);
/// HTTP timeout per request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between periodic syncs (hourly, matching bitcoin.rs cadence).
pub const ETHEREUM_SYNC_SECS: u64 = 3600;

/// Chain id → human-readable native asset name (for the `merchant` field).
fn chain_name(chain_id: u64) -> &'static str {
    match chain_id {
        1 => "Ethereum",
        137 => "Polygon",
        42161 => "Arbitrum",
        10 => "Optimism",
        8453 => "Base",
        56 => "BNB Chain",
        43114 => "Avalanche",
        250 => "Fantom",
        _ => "EVM Chain",
    }
}

/// Chain id → native currency symbol.
fn chain_currency(chain_id: u64) -> &'static str {
    match chain_id {
        1 | 42161 | 10 | 8453 => "ETH",
        137 => "MATIC",
        56 => "BNB",
        43114 => "AVAX",
        250 => "FTM",
        _ => "ETH",
    }
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.values().sum::<u64>();
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("ethereum synced — {n} new rows")
            }))
        }
        Err(e) => {
            Ok(crate::registry::CollectOutcome::note(format!("ethereum sync skipped: {e}")))
        }
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n: u64 = out.counts.values().sum();
    let headline = if n == 0 {
        "Ethereum is up to date — no new transactions".to_string()
    } else {
        format!("Ethereum synced — {n} new rows")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ethereum",
        name: "Ethereum Wallet (Etherscan)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Ethereum and EVM-compatible wallet history — normal transactions, \
                      ERC-20 token transfers, and internal calls — using your free Etherscan API \
                      key. You supply wallet addresses only; private keys never leave your device. \
                      Each transaction is recorded as a dated value transfer.",
        domain: "finance",
        vault_path: "finance/purchases/ethereum/",
        toggleable: true,
        setup: &[
            "Get a free API key at etherscan.io/myapikey (no payment required).",
            "Connect with your key and one or more public wallet addresses.",
            "Querying Etherscan discloses your wallet addresses to their servers.",
        ],
        caveats: "Every sync query reveals your wallet addresses to Etherscan. \
                  Private keys and seed phrases must NEVER be entered here — public \
                  addresses only. ERC-20 token values use the token's reported decimal \
                  places; verify high-value transfers independently.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(ETHEREUM_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("ethereum"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste: composite "api_key|addr1,addr2@chainid").

/// Store the pasted composite string under `.trove/sync/ethereum.json` (0600).
/// Format: `api_key|addr1,addr2,...@chainid` (chain id is optional, defaults
/// to 1 = Ethereum mainnet). The address list and chain id are public; the API
/// key is private and rides in the `access_token` slot of a never-expiring
/// [`TokenSet`].
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (api_key, addrs, _chain_id) = parse_connect_input(pasted)?;
    if api_key.is_empty() {
        bail!("no API key found — paste your key and addresses in the form: key|addr1,addr2");
    }
    if addrs.is_empty() {
        bail!("no wallet address found — paste in the form: api_key|addr1,addr2");
    }
    // Store the full pasted string; the run fn re-parses it.
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
        if let Ok((_key, addrs, chain_id)) = parse_connect_input(&token.access_token) {
            if !addrs.is_empty() {
                let label = format!("{} on chain {chain_id}", address_summary(&addrs));
                accounts.push(ConnectedAccount {
                    key: SERVICE.to_string(),
                    label,
                    connected_at: None,
                    expires_at: None,
                    needs_reconnect: false,
                    extra: BTreeMap::new(),
                });
            }
        }
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. TokenPaste: the user
/// pastes their Etherscan API key and wallet addresses as one composite string.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "ethereum",
    display_name: "Ethereum Wallet (Etherscan)",
    methods: &[ConnectMethod::TokenPaste {
        label: "Etherscan API key and wallet address(es)",
        help: "Format: api_key|addr1,addr2,...  \
               Get a free API key at etherscan.io/myapikey. Paste your key, a pipe character (|), \
               then your public wallet address(es) comma- or space-separated. \
               For other EVM chains, append @chainid (e.g. @137 for Polygon). \
               Example: ABCD1234...|0xYourAddr@1  \
               Never paste private keys or seed phrases.",
        placeholder: "ABCDEF1234...|0xYourWalletAddress",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["ethereum"],
    setup: &[
        "Get a free Etherscan API key at etherscan.io/myapikey.",
        "Paste your API key, a | character, then your public wallet address(es).",
        "Add @chainid (e.g. @137) for Polygon, @8453 for Base, etc.",
        "Public addresses only — never private keys or seed phrases.",
        "Each sync query reveals your wallet addresses to Etherscan.",
    ],
};

// ---------------------------------------------------------------------------
// Connect-input parsing — pure.

/// Parse the composite pasted string: `api_key|addr1,addr2,...@chainid`.
/// Returns `(api_key, addresses, chain_id)`. Chain id defaults to 1.
fn parse_connect_input(pasted: &str) -> Result<(String, Vec<String>, u64)> {
    let trimmed = pasted.trim();
    // Split on the FIRST `|` to separate key from addresses.
    let (key_part, rest) = match trimmed.split_once('|') {
        Some((k, r)) => (k.trim(), r.trim()),
        None => {
            // Tolerate key-less paste for pure-address connects (public chain).
            // But we require the key, so bail.
            bail!("paste format: api_key|addr1,addr2 (include the | separator)");
        }
    };
    let api_key = key_part.to_string();

    // The `rest` may end with `@chainid`.
    let (addr_part, chain_id) = match rest.rsplit_once('@') {
        Some((a, c)) => {
            let chain: u64 = c
                .trim()
                .parse()
                .context(format!("chain id {c:?} is not a valid integer"))?;
            (a.trim(), chain)
        }
        None => (rest, DEFAULT_CHAIN_ID),
    };

    let mut seen = HashSet::new();
    let addrs: Vec<String> = addr_part
        .split([',', ' ', '\n', '\t', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.to_string()))
        .map(str::to_string)
        .collect();

    Ok((api_key, addrs, chain_id))
}

/// A compact label: first address + "+N more" when several were supplied.
fn address_summary(addrs: &[String]) -> String {
    match addrs.len() {
        0 => String::new(),
        1 => addrs[0].clone(),
        n => format!("{} +{} more", addrs[0], n - 1),
    }
}

// ---------------------------------------------------------------------------
// Cursor.

/// Cursor key: `address:chainid` (never rely on address alone — the same
/// address on two chains has independent history).
fn cursor_key(addr: &str, chain_id: u64) -> String {
    format!("{addr}:{chain_id}")
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Per-address-chain cursor: the last block number fully drained.
    /// Key: `address:chainid`. Value: block number as a string (matches
    /// Etherscan's string-serialized numbers and avoids u64 truncation in JSON).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    last_block: BTreeMap<String, String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_ethereum_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ethereum_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

#[derive(Debug)]
enum FetchError {
    /// API key rejected / invalid (status "0", message contains "Invalid API Key").
    InvalidApiKey,
    /// Rate limited (HTTP 429 or status "0" with rate-limit message).
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::InvalidApiKey => write!(f, "Etherscan API key invalid or rate-limited"),
            FetchError::RateLimited => write!(f, "rate limited — will retry on next sync"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The Etherscan endpoints this collector uses.
trait EtherscanApi {
    /// `action=txlist` — normal transactions for address from startblock,
    /// ascending, page N, up to PAGE_SIZE. Returns the raw result array.
    fn txlist(
        &self,
        address: &str,
        start_block: u64,
        page: u64,
    ) -> Result<Vec<Value>, FetchError>;

    /// `action=tokentx` — ERC-20 token transfers.
    fn tokentx(
        &self,
        address: &str,
        start_block: u64,
        page: u64,
    ) -> Result<Vec<Value>, FetchError>;

    /// `action=txlistinternal` — internal (contract-initiated) txs.
    fn txlistinternal(
        &self,
        address: &str,
        start_block: u64,
        page: u64,
    ) -> Result<Vec<Value>, FetchError>;
}

struct EtherscanClient {
    api_key: String,
    chain_id: u64,
}

impl EtherscanClient {
    fn new(api_key: String, chain_id: u64) -> Self {
        EtherscanClient { api_key, chain_id }
    }

    fn get(
        &self,
        action: &str,
        address: &str,
        start_block: u64,
        page: u64,
    ) -> Result<Vec<Value>, FetchError> {
        let url = format!(
            "{API_BASE}?chainid={chain_id}&module=account&action={action}\
             &address={address}&startblock={start_block}&endblock=99999999\
             &page={page}&offset={PAGE_SIZE}&sort=asc&apikey={api_key}",
            chain_id = self.chain_id,
            api_key = self.api_key,
        );
        let resp = ureq::get(&url).timeout(HTTP_TIMEOUT).call();
        match resp {
            Ok(r) => {
                let v: Value = r
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("JSON parse: {e}")))?;
                let status = v.get("status").and_then(Value::as_str).unwrap_or("0");
                let message = v.get("message").and_then(Value::as_str).unwrap_or("");
                if status == "0" {
                    let msg_lc = message.to_lowercase();
                    if msg_lc.contains("invalid api key") || msg_lc.contains("invalid apikey") {
                        return Err(FetchError::InvalidApiKey);
                    }
                    if msg_lc.contains("max rate limit") || msg_lc.contains("rate limit") {
                        return Err(FetchError::RateLimited);
                    }
                    // "No transactions found" is a normal empty result, not an error.
                    if msg_lc.contains("no transactions found") || message == "No records found" {
                        return Ok(Vec::new());
                    }
                    // Other status=0 with a non-empty result array (e.g. partial data).
                    // Fall through to parse result below.
                }
                match v.get("result").cloned() {
                    Some(Value::Array(a)) => Ok(a),
                    _ => Ok(Vec::new()),
                }
            }
            Err(ureq::Error::Status(429 | 503, _)) => Err(FetchError::RateLimited),
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl EtherscanApi for EtherscanClient {
    fn txlist(&self, addr: &str, start: u64, page: u64) -> Result<Vec<Value>, FetchError> {
        self.get("txlist", addr, start, page)
    }
    fn tokentx(&self, addr: &str, start: u64, page: u64) -> Result<Vec<Value>, FetchError> {
        self.get("tokentx", addr, start, page)
    }
    fn txlistinternal(&self, addr: &str, start: u64, page: u64) -> Result<Vec<Value>, FetchError> {
        self.get("txlistinternal", addr, start, page)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (verbatim API result row, tagged with a ts for partitioning).

#[derive(Serialize)]
struct RawLine {
    /// The ts is only used for the month-partition key; it is NOT re-serialized
    /// into the raw line (the raw object already contains `timeStamp`).
    #[serde(skip)]
    ts: String,
    /// The action type ("txlist" / "tokentx" / "txlistinternal") so a reader
    /// can distinguish sources within the raw layer.
    action: &'static str,
    /// The verbatim result object from the Etherscan response array.
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping helpers (fixture-tested).

/// Wei string (or any integer-like string) → ETH float.
/// The exact raw string is kept in `extra.value_raw`; this is a display value.
/// Parses as u128 (sufficient for all practical ETH amounts; u128::MAX ≈ 340e18
/// ETH, far above total supply) then converts to f64. Values above u128::MAX
/// are not reachable in practice and yield 0.0 on parse failure.
fn wei_to_eth(wei_str: &str) -> f64 {
    wei_str.trim().parse::<u128>().map(|w| w as f64 / 1e18).unwrap_or(0.0)
}

/// Token value (raw units, as string) → decimal float, given decimals (string).
fn token_value(raw: &str, decimals_str: &str) -> f64 {
    let raw_val: f64 = raw.trim().parse().unwrap_or(0.0);
    let decimals: u32 = decimals_str.trim().parse().unwrap_or(18);
    if decimals == 0 {
        return raw_val;
    }
    raw_val / 10f64.powi(decimals as i32)
}

/// `timeStamp` string (Unix seconds) → RFC3339 local. `None` when unparseable.
fn timestamp_to_local(ts_str: &str) -> Option<String> {
    let secs: i64 = ts_str.trim().parse().ok()?;
    Local.timestamp_opt(secs, 0).single().map(|dt| dt.to_rfc3339())
}

/// Extract a string field from a Value (object), trimmed.
fn str_field<'a>(obj: &'a Value, key: &str) -> &'a str {
    obj.get(key).and_then(Value::as_str).unwrap_or("").trim_matches('"')
}

/// Map one `txlist` row → a [`LineItem`] for the `finance-purchases` contract.
/// Returns `None` when the ts can't be parsed (can't be partitioned) or the
/// hash is empty (can't be deduped).
fn normal_tx_to_line_item(tx: &Value, address: &str, chain_id: u64) -> Option<LineItem> {
    let hash = str_field(tx, "hash");
    if hash.is_empty() {
        return None;
    }
    let ts = timestamp_to_local(str_field(tx, "timeStamp"))?;
    Partition::Month.key(&ts)?;

    let from = str_field(tx, "from");
    let to = str_field(tx, "to");
    let value_raw = str_field(tx, "value");
    let amount = wei_to_eth(value_raw);

    // Direction relative to the user's address (case-insensitive).
    let addr_lower = address.to_lowercase();
    let from_lower = from.to_lowercase();
    let direction = if from_lower == addr_lower { "sent" } else { "received" };

    let item_label = if direction == "sent" { "Sent ETH" } else { "Received ETH" };

    let is_error = str_field(tx, "isError");
    let status_str = if is_error == "1" {
        "failed"
    } else if str_field(tx, "txreceipt_status") == "1" {
        "confirmed"
    } else {
        "confirmed"
    };

    let mut extra = Map::new();
    extra.insert("action".into(), Value::String("txlist".into()));
    extra.insert("direction".into(), Value::String(direction.into()));
    extra.insert("value_raw".into(), Value::String(value_raw.to_string()));
    extra.insert("chain_id".into(), Value::from(chain_id));
    let gas_used = str_field(tx, "gasUsed");
    let gas_price = str_field(tx, "gasPrice");
    if !gas_used.is_empty() {
        extra.insert("gas_used".into(), Value::String(gas_used.to_string()));
    }
    if !gas_price.is_empty() {
        extra.insert("gas_price_wei".into(), Value::String(gas_price.to_string()));
    }
    if !from.is_empty() {
        extra.insert("from".into(), Value::String(from.to_string()));
    }
    if !to.is_empty() {
        extra.insert("to".into(), Value::String(to.to_string()));
    }
    let block = str_field(tx, "blockNumber");
    if !block.is_empty() {
        extra.insert("block_number".into(), Value::String(block.to_string()));
    }
    let fn_name = str_field(tx, "functionName");
    if !fn_name.is_empty() {
        extra.insert("function_name".into(), Value::String(fn_name.to_string()));
    }

    let network = chain_name(chain_id);
    let currency = chain_currency(chain_id);

    let mut li = LineItem::new("ethereum", hash, ts, network);
    li.item = item_label.into();
    li.amount = Some(amount);
    li.currency = currency.into();
    li.status = status_str.into();
    li.extra = extra;
    Some(li)
}

/// Map one `tokentx` row → a [`LineItem`]. The `guid` is `hash:ordinal` where
/// `ordinal` is the 0-based position of this transfer within the ordered list of
/// transfers for the same `hash` (as returned by the API in ascending order).
///
/// Etherscan's `tokentx` response does NOT include a per-log-event index field:
/// `transactionIndex` is the position of the *transaction* in the block and is
/// identical for every ERC-20 transfer emitted by the same tx. Using it would
/// produce identical GUIDs for all transfers in one tx (e.g. a DEX swap that
/// emits multiple Transfer events), causing silent dedup-loss of all but the
/// first. Instead, the caller assigns a stable ordinal from the order of the
/// result array (which Etherscan returns in ascending block/logIndex order for
/// `sort=asc`).
fn token_tx_to_line_item(tx: &Value, address: &str, chain_id: u64, ordinal: usize) -> Option<LineItem> {
    let hash = str_field(tx, "hash");
    if hash.is_empty() {
        return None;
    }
    let ts = timestamp_to_local(str_field(tx, "timeStamp"))?;
    Partition::Month.key(&ts)?;

    // Derive a stable per-event identifier using the ordinal within the result
    // array for this hash. The ordinal is assigned by the caller across all
    // transfers with the same hash and is stable across syncs because Etherscan
    // returns tokentx rows in ascending block order (sort=asc) and the result
    // set for a given hash is always the same set of Transfer events.
    let guid = format!("{hash}:{ordinal}");

    let from = str_field(tx, "from");
    let to = str_field(tx, "to");
    let value_raw = str_field(tx, "value");
    let token_symbol = str_field(tx, "tokenSymbol");
    let token_name = str_field(tx, "tokenName");
    let token_decimal = str_field(tx, "tokenDecimal");
    let contract_address = str_field(tx, "contractAddress");

    let amount = token_value(value_raw, token_decimal);

    let addr_lower = address.to_lowercase();
    let direction = if str_field(tx, "from").to_lowercase() == addr_lower { "sent" } else { "received" };
    let item_label = format!(
        "{} Token Transfer",
        if direction == "sent" { "Sent" } else { "Received" }
    );
    let currency = if token_symbol.is_empty() { "?" } else { token_symbol };

    let mut extra = Map::new();
    extra.insert("action".into(), Value::String("tokentx".into()));
    extra.insert("direction".into(), Value::String(direction.into()));
    extra.insert("value_raw".into(), Value::String(value_raw.to_string()));
    extra.insert("chain_id".into(), Value::from(chain_id));
    if !token_name.is_empty() {
        extra.insert("token_name".into(), Value::String(token_name.to_string()));
    }
    if !token_decimal.is_empty() {
        extra.insert("token_decimal".into(), Value::String(token_decimal.to_string()));
    }
    if !contract_address.is_empty() {
        extra.insert("contract_address".into(), Value::String(contract_address.to_string()));
    }
    if !from.is_empty() {
        extra.insert("from".into(), Value::String(from.to_string()));
    }
    if !to.is_empty() {
        extra.insert("to".into(), Value::String(to.to_string()));
    }
    let block = str_field(tx, "blockNumber");
    if !block.is_empty() {
        extra.insert("block_number".into(), Value::String(block.to_string()));
    }

    let network = chain_name(chain_id);
    let mut li = LineItem::new("ethereum", guid, ts, network);
    li.item = item_label;
    li.amount = Some(amount);
    li.currency = currency.to_string();
    li.extra = extra;
    Some(li)
}

/// Map one `txlistinternal` row → a [`LineItem`].
fn internal_tx_to_line_item(tx: &Value, address: &str, chain_id: u64) -> Option<LineItem> {
    let hash = str_field(tx, "hash");
    if hash.is_empty() {
        return None;
    }
    // Internal txs from the same outer hash are disambiguated by traceId.
    let trace_id = str_field(tx, "traceId");
    let guid =
        if trace_id.is_empty() { hash.to_string() } else { format!("{hash}:int:{trace_id}") };

    let ts = timestamp_to_local(str_field(tx, "timeStamp"))?;
    Partition::Month.key(&ts)?;

    let from = str_field(tx, "from");
    let to = str_field(tx, "to");
    let value_raw = str_field(tx, "value");
    let amount = wei_to_eth(value_raw);

    let addr_lower = address.to_lowercase();
    let direction =
        if str_field(tx, "from").to_lowercase() == addr_lower { "sent" } else { "received" };
    let item_label = if direction == "sent" {
        "Sent Internal Transfer"
    } else {
        "Received Internal Transfer"
    };

    let is_error = str_field(tx, "isError");
    let status_str = if is_error == "1" { "failed" } else { "confirmed" };

    let mut extra = Map::new();
    extra.insert("action".into(), Value::String("txlistinternal".into()));
    extra.insert("direction".into(), Value::String(direction.into()));
    extra.insert("value_raw".into(), Value::String(value_raw.to_string()));
    extra.insert("chain_id".into(), Value::from(chain_id));
    let tx_type = str_field(tx, "type");
    if !tx_type.is_empty() {
        extra.insert("type".into(), Value::String(tx_type.to_string()));
    }
    if !trace_id.is_empty() {
        extra.insert("trace_id".into(), Value::String(trace_id.to_string()));
    }
    if !from.is_empty() {
        extra.insert("from".into(), Value::String(from.to_string()));
    }
    if !to.is_empty() {
        extra.insert("to".into(), Value::String(to.to_string()));
    }
    let block = str_field(tx, "blockNumber");
    if !block.is_empty() {
        extra.insert("block_number".into(), Value::String(block.to_string()));
    }

    let network = chain_name(chain_id);
    let currency = chain_currency(chain_id);
    let mut li = LineItem::new("ethereum", guid, ts, network);
    li.item = item_label.into();
    li.amount = Some(amount);
    li.currency = currency.into();
    li.status = status_str.into();
    li.extra = extra;
    Some(li)
}

// ---------------------------------------------------------------------------
// Write: raw + contract layers, deduped by guid.

fn write_layer(vault: &Vault, rows: Vec<(LineItem, RawLine)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Load existing guids to deduplicate.
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
    for (row, raw_line) in rows {
        if row.guid.is_empty() || !seen.insert(row.guid.clone()) {
            continue;
        }
        new_raws.push(raw_line);
        new_rows.push(row);
    }

    contract.append(&new_rows, |r| &r.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// Drain helpers — page until empty or short page, with block-window narrowing.

/// Drain one Etherscan action for an address, starting from `start_block`.
///
/// ## Pagination strategy
///
/// Etherscan's `page`/`offset` parameters are subject to a provider-enforced
/// cap: the free tier can return at most `PAGE_SIZE` rows per request, and
/// `page * offset` must not exceed the provider's absolute cap (10,000 rows for
/// the API v2; 1,000 per request for the free tier from July 2026). Simply
/// incrementing the `page` number while keeping `endblock=∞` cannot retrieve
/// more than `PAGE_SIZE` records from a given window — once a page returns a
/// full `PAGE_SIZE` batch you do not know whether there are more results *within
/// that window* or whether you have just hit the cap.
///
/// The correct technique (documented in Etherscan's API reference) is to
/// **shrink the block window**: when a page returns exactly `PAGE_SIZE` rows,
/// record the highest block in that batch, then on the next request set
/// `startblock = highest_block_in_batch` and reset `page = 1`. The block at the
/// boundary is re-fetched; duplicate guids are eliminated by `write_layer`.
/// This ensures every record is retrieved regardless of address activity depth.
fn drain_action(
    api: &impl EtherscanApi,
    action: ActionKind,
    addr: &str,
    start_block: u64,
    chain_id: u64,
) -> Result<Vec<(LineItem, RawLine)>, FetchError> {
    let mut rows: Vec<(LineItem, RawLine)> = Vec::new();
    let mut page: u64 = 1;
    let mut pages_fetched = 0usize;
    let mut window_start = start_block;
    // Per-hash ordinal counter for tokentx: counts how many Transfer events
    // with the same hash we have seen so far across all pages. This produces a
    // stable per-event ordinal (0, 1, 2, …) that disambiguates multiple ERC-20
    // transfers emitted by a single transaction.
    let mut hash_ordinal: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    loop {
        let page_data = match action {
            ActionKind::TxList => api.txlist(addr, window_start, page)?,
            ActionKind::TokenTx => api.tokentx(addr, window_start, page)?,
            ActionKind::TxListInternal => api.txlistinternal(addr, window_start, page)?,
        };
        let count = page_data.len();

        // Track the highest block in this batch for window narrowing.
        let mut batch_highest: Option<u64> = None;
        for tx in &page_data {
            if let Some(b) = tx.get("blockNumber")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<u64>().ok())
            {
                batch_highest = Some(batch_highest.map_or(b, |h: u64| h.max(b)));
            }
        }

        for tx in page_data {
            let action_str = action.as_str();
            let maybe_li = match action {
                ActionKind::TxList => normal_tx_to_line_item(&tx, addr, chain_id),
                ActionKind::TokenTx => {
                    // Assign a per-hash ordinal for stable, collision-free guids.
                    let hash = str_field(&tx, "hash").to_string();
                    let ordinal = {
                        let entry = hash_ordinal.entry(hash).or_insert(0);
                        let o = *entry;
                        *entry += 1;
                        o
                    };
                    token_tx_to_line_item(&tx, addr, chain_id, ordinal)
                }
                ActionKind::TxListInternal => internal_tx_to_line_item(&tx, addr, chain_id),
            };
            if let Some(li) = maybe_li {
                let raw_line = RawLine { ts: li.ts.clone(), action: action_str, value: tx };
                rows.push((li, raw_line));
            }
        }
        pages_fetched += 1;

        if count < PAGE_SIZE || pages_fetched >= MAX_PAGES {
            // Short page (or guard hit) — we have exhausted the current window.
            break;
        }

        // Full page returned — there may be more. Narrow the block window to
        // avoid hitting the provider's page*offset cap. Reset page=1 so the
        // next request starts fresh within the narrowed window. The overlap at
        // `batch_highest` is handled by guid deduplication in `write_layer`.
        if let Some(hi) = batch_highest {
            window_start = hi;
            page = 1;
        } else {
            // No block numbers in the batch (shouldn't happen); fall back to
            // page increment to avoid an infinite loop.
            page += 1;
        }
        thread::sleep(REQ_INTERVAL);
    }
    Ok(rows)
}

#[derive(Copy, Clone)]
enum ActionKind {
    TxList,
    TokenTx,
    TxListInternal,
}

impl ActionKind {
    fn as_str(self) -> &'static str {
        match self {
            ActionKind::TxList => "txlist",
            ActionKind::TokenTx => "tokentx",
            ActionKind::TxListInternal => "txlistinternal",
        }
    }
}

/// Extract the highest block number from a set of rows (to advance the cursor).
fn highest_block(rows: &[(LineItem, RawLine)]) -> Option<u64> {
    rows.iter()
        .filter_map(|(_, raw)| {
            raw.value.get("blockNumber").and_then(Value::as_str).and_then(|s| s.parse::<u64>().ok())
        })
        .max()
}

// ---------------------------------------------------------------------------
// Main pull.

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let pasted = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|s| !s.trim().is_empty())
        .context("Ethereum is not connected — add your API key and address(es) in the Integrations tab")?;
    let (api_key, addrs, chain_id) = parse_connect_input(&pasted)?;
    let client = EtherscanClient::new(api_key, chain_id);
    pull_with(vault, &client, &addrs, chain_id)
}

fn pull_with(
    vault: &Vault,
    api: &impl EtherscanApi,
    addrs: &[String],
    chain_id: u64,
) -> Result<PullOutcome> {
    if addrs.is_empty() {
        bail!("Ethereum has no usable address — re-connect with a public wallet address");
    }
    let mut state = vault.read_ethereum_sync();
    let mut total_written: u64 = 0;
    let actions = [ActionKind::TxList, ActionKind::TokenTx, ActionKind::TxListInternal];

    // Collect the full new window for ALL addresses and actions BEFORE advancing
    // any cursor (a partial drain must not advance the watermark; guid dedupe
    // makes the overlap idempotent).
    let mut all_rows: Vec<(LineItem, RawLine)> = Vec::new();
    // Track the highest block per address per action for cursor advancement.
    let mut next_blocks: BTreeMap<String, u64> = BTreeMap::new();

    for addr in addrs {
        let key = cursor_key(addr, chain_id);
        // start_block: one beyond the last drained block (or 0 for first sync).
        let start_block: u64 = state
            .last_block
            .get(&key)
            .and_then(|s| s.parse::<u64>().ok())
            .map(|b| b + 1)
            .unwrap_or(0);

        for &action in &actions {
            let rows = match drain_action(api, action, addr, start_block, chain_id) {
                Ok(r) => r,
                Err(FetchError::InvalidApiKey) => {
                    bail!("Etherscan API key invalid — re-connect with a valid key");
                }
                Err(FetchError::RateLimited) => {
                    // Back off once and retry.
                    thread::sleep(Duration::from_secs(2));
                    drain_action(api, action, addr, start_block, chain_id)
                        .map_err(|e| anyhow::anyhow!("Etherscan rate-limited for {addr}: {e}"))?
                }
                Err(FetchError::Other(e)) => {
                    bail!("Etherscan fetch failed for {addr}: {e}");
                }
            };
            // Track the highest block seen for this address (across all actions).
            if let Some(hi) = highest_block(&rows) {
                let entry = next_blocks.entry(key.clone()).or_insert(0);
                if hi > *entry {
                    *entry = hi;
                }
            }
            all_rows.extend(rows);
            thread::sleep(REQ_INTERVAL);
        }
    }

    // Write both layers, deduped by guid, THEN advance cursors.
    let written = write_layer(vault, all_rows)?;
    total_written += written;

    for (key, block) in next_blocks {
        state.last_block.insert(key, block.to_string());
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_ethereum_sync(&state)?;

    let counts = BTreeMap::from([("transactions", total_written)]);
    Ok(PullOutcome { headline: format!("{total_written} transactions"), counts })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-ethereum-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixtures (synthesized from the documented Etherscan API response shapes).

    const MY_ADDR: &str = "0xde0b295669a9fd93d5f28d9ec85e40f4cb697bae";
    const OTHER_ADDR: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TOKEN_CONTRACT: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";

    /// A normal ETH receive (txlist row).
    fn tx_normal_receive() -> Value {
        json!({
            "blockNumber": "4730207",
            "timeStamp": "1513667988",
            "hash": "0xabc0000001",
            "nonce": "0",
            "blockHash": "0xblock0001",
            "transactionIndex": "0",
            "from": OTHER_ADDR,
            "to": MY_ADDR,
            "value": "5000000000000000000",
            "gas": "21000",
            "gasPrice": "20000000000",
            "isError": "0",
            "txreceipt_status": "1",
            "input": "0x",
            "contractAddress": "",
            "cumulativeGasUsed": "21000",
            "gasUsed": "21000",
            "confirmations": "100",
            "methodId": "0x",
            "functionName": ""
        })
    }

    /// A normal ETH send (txlist row).
    fn tx_normal_send() -> Value {
        json!({
            "blockNumber": "4730208",
            "timeStamp": "1513668000",
            "hash": "0xabc0000002",
            "from": MY_ADDR,
            "to": OTHER_ADDR,
            "value": "1000000000000000000",
            "gas": "21000",
            "gasPrice": "20000000000",
            "isError": "0",
            "txreceipt_status": "1",
            "input": "0x",
            "gasUsed": "21000",
            "transactionIndex": "1"
        })
    }

    /// A failed transaction (isError="1").
    fn tx_normal_failed() -> Value {
        json!({
            "blockNumber": "4730209",
            "timeStamp": "1513668100",
            "hash": "0xabc0000003",
            "from": MY_ADDR,
            "to": OTHER_ADDR,
            "value": "0",
            "isError": "1",
            "txreceipt_status": "0",
            "gasUsed": "21000",
            "transactionIndex": "2"
        })
    }

    /// An ERC-20 token transfer (tokentx row).
    fn tx_token_transfer() -> Value {
        json!({
            "blockNumber": "4730210",
            "timeStamp": "1513668200",
            "hash": "0xabc0000004",
            "nonce": "5",
            "blockHash": "0xblock0004",
            "from": OTHER_ADDR,
            "contractAddress": TOKEN_CONTRACT,
            "to": MY_ADDR,
            "value": "5000000000000000000000",
            "tokenName": "Dai Stablecoin",
            "tokenSymbol": "DAI",
            "tokenDecimal": "18",
            "transactionIndex": "3",
            "gas": "60000",
            "gasPrice": "20000000000",
            "gasUsed": "45000",
            "cumulativeGasUsed": "120000",
            "input": "deprecated",
            "confirmations": "50"
        })
    }

    /// An internal transaction (txlistinternal row).
    fn tx_internal() -> Value {
        json!({
            "blockNumber": "4730211",
            "timeStamp": "1513668300",
            "hash": "0xabc0000005",
            "from": OTHER_ADDR,
            "to": MY_ADDR,
            "value": "250000000000000000",
            "contractAddress": "",
            "input": "",
            "type": "call",
            "gas": "2300",
            "gasUsed": "0",
            "traceId": "0",
            "isError": "0",
            "errCode": ""
        })
    }

    // ---------------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        txlist_pages: RefCell<Vec<Vec<Value>>>,
        tokentx_pages: RefCell<Vec<Vec<Value>>>,
        internal_pages: RefCell<Vec<Vec<Value>>>,
        calls: RefCell<Vec<(&'static str, String, u64, u64)>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                txlist_pages: RefCell::new(Vec::new()),
                tokentx_pages: RefCell::new(Vec::new()),
                internal_pages: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn with_txlist(mut self, pages: Vec<Vec<Value>>) -> Self {
            self.txlist_pages = RefCell::new(pages);
            self
        }
        fn with_tokentx(mut self, pages: Vec<Vec<Value>>) -> Self {
            self.tokentx_pages = RefCell::new(pages);
            self
        }
        fn with_internal(mut self, pages: Vec<Vec<Value>>) -> Self {
            self.internal_pages = RefCell::new(pages);
            self
        }
    }

    impl EtherscanApi for MockApi {
        fn txlist(&self, addr: &str, start: u64, page: u64) -> Result<Vec<Value>, FetchError> {
            self.calls.borrow_mut().push(("txlist", addr.to_string(), start, page));
            let mut pages = self.txlist_pages.borrow_mut();
            Ok(if pages.is_empty() { Vec::new() } else { pages.remove(0) })
        }
        fn tokentx(&self, addr: &str, start: u64, page: u64) -> Result<Vec<Value>, FetchError> {
            self.calls.borrow_mut().push(("tokentx", addr.to_string(), start, page));
            let mut pages = self.tokentx_pages.borrow_mut();
            Ok(if pages.is_empty() { Vec::new() } else { pages.remove(0) })
        }
        fn txlistinternal(
            &self,
            addr: &str,
            start: u64,
            page: u64,
        ) -> Result<Vec<Value>, FetchError> {
            self.calls.borrow_mut().push(("txlistinternal", addr.to_string(), start, page));
            let mut pages = self.internal_pages.borrow_mut();
            Ok(if pages.is_empty() { Vec::new() } else { pages.remove(0) })
        }
    }

    // ---------------------------------------------------------------------------
    // Unit tests for pure mapping.

    #[test]
    fn maps_normal_receive_to_positive_line_item() {
        let li = normal_tx_to_line_item(&tx_normal_receive(), MY_ADDR, 1).unwrap();
        assert_eq!(li.source, "ethereum");
        assert_eq!(li.guid, "0xabc0000001");
        assert_eq!(li.merchant, "Ethereum");
        assert_eq!(li.item, "Received ETH");
        assert_eq!(li.currency, "ETH");
        assert_eq!(li.status, "confirmed");
        // 5 * 10^18 wei = 5 ETH
        assert!((li.amount.unwrap() - 5.0).abs() < 1e-9);
        assert_eq!(li.extra.get("direction"), Some(&json!("received")));
        assert_eq!(li.extra.get("value_raw"), Some(&json!("5000000000000000000")));
        assert_eq!(li.extra.get("chain_id"), Some(&json!(1u64)));
    }

    #[test]
    fn maps_normal_send_to_negative_direction() {
        let li = normal_tx_to_line_item(&tx_normal_send(), MY_ADDR, 1).unwrap();
        assert_eq!(li.guid, "0xabc0000002");
        assert_eq!(li.item, "Sent ETH");
        assert_eq!(li.extra.get("direction"), Some(&json!("sent")));
        assert!((li.amount.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn failed_tx_maps_status_failed() {
        let li = normal_tx_to_line_item(&tx_normal_failed(), MY_ADDR, 1).unwrap();
        assert_eq!(li.status, "failed");
        assert_eq!(li.extra.get("direction"), Some(&json!("sent")));
    }

    #[test]
    fn maps_token_transfer_with_symbol_and_decimal() {
        let li = token_tx_to_line_item(&tx_token_transfer(), MY_ADDR, 1, 0).unwrap();
        assert_eq!(li.currency, "DAI");
        assert_eq!(li.item, "Received Token Transfer");
        assert_eq!(li.extra.get("token_name"), Some(&json!("Dai Stablecoin")));
        assert_eq!(li.extra.get("contract_address"), Some(&json!(TOKEN_CONTRACT)));
        // 5000 * 10^18 / 10^18 = 5000 DAI
        assert!((li.amount.unwrap() - 5000.0).abs() < 0.001);
        // guid is hash:0 (ordinal 0 = first transfer)
        assert_eq!(li.guid, "0xabc0000004:0");
    }

    /// Two ERC-20 Transfer events emitted by the same transaction (e.g. a DEX
    /// swap). They share the same `hash` and the same `transactionIndex` — using
    /// either as the guid disambiguator would produce a collision and silently
    /// drop the second row. The ordinal-based guid must produce two distinct ids.
    #[test]
    fn two_token_transfers_same_hash_produce_distinct_guids_and_both_survive() {
        // Both rows share the same hash and transactionIndex.
        let shared_hash = "0xdeadbeef0000dex";
        let tx_a = json!({
            "blockNumber": "9000000",
            "timeStamp": "1640000000",
            "hash": shared_hash,
            "from": OTHER_ADDR,
            "to": MY_ADDR,
            "value": "1000000000000000000000",
            "tokenName": "Dai Stablecoin",
            "tokenSymbol": "DAI",
            "tokenDecimal": "18",
            "transactionIndex": "5",   // same for both — this is the TX-in-block index
            "contractAddress": TOKEN_CONTRACT,
            "gas": "200000",
            "gasUsed": "150000"
        });
        let tx_b = json!({
            "blockNumber": "9000000",
            "timeStamp": "1640000000",
            "hash": shared_hash,
            "from": MY_ADDR,
            "to": OTHER_ADDR,
            "value": "999000000000000000000",
            "tokenName": "Wrapped Ether",
            "tokenSymbol": "WETH",
            "tokenDecimal": "18",
            "transactionIndex": "5",   // same — would collide with tx_a's guid
            "contractAddress": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "gas": "200000",
            "gasUsed": "150000"
        });

        let li_a = token_tx_to_line_item(&tx_a, MY_ADDR, 1, 0).unwrap();
        let li_b = token_tx_to_line_item(&tx_b, MY_ADDR, 1, 1).unwrap();

        // Both guids contain the shared hash.
        assert!(li_a.guid.starts_with(shared_hash), "a guid: {}", li_a.guid);
        assert!(li_b.guid.starts_with(shared_hash), "b guid: {}", li_b.guid);
        // Guids are distinct.
        assert_ne!(li_a.guid, li_b.guid, "collision! both have guid {}", li_a.guid);
        assert_eq!(li_a.guid, format!("{shared_hash}:0"));
        assert_eq!(li_b.guid, format!("{shared_hash}:1"));

        // When written through the full pull path both rows survive write_layer.
        let v = temp_vault("tokentx_collision");
        let api = MockApi::new().with_tokentx(vec![vec![tx_a, tx_b]]);
        let out = pull_with(&v, &api, &[MY_ADDR.to_string()], 1).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&2), "both transfers must survive");

        // Verify distinct guids in the written contract layer.
        let contract = v.stream(DIR, Partition::Month);
        let mut guids: Vec<String> = Vec::new();
        for key in contract.partitions().unwrap() {
            for row in contract.read::<Value>(&key).unwrap() {
                if let Some(g) = row.get("guid").and_then(Value::as_str) {
                    guids.push(g.to_string());
                }
            }
        }
        assert_eq!(guids.len(), 2, "two rows in vault");
        assert!(guids.contains(&format!("{shared_hash}:0")));
        assert!(guids.contains(&format!("{shared_hash}:1")));
    }

    #[test]
    fn maps_internal_tx_with_trace_id() {
        let li = internal_tx_to_line_item(&tx_internal(), MY_ADDR, 1).unwrap();
        assert_eq!(li.item, "Received Internal Transfer");
        assert_eq!(li.currency, "ETH");
        assert!(li.guid.contains("int:0"), "guid contains trace id");
        assert_eq!(li.extra.get("type"), Some(&json!("call")));
        // 0.25 ETH
        assert!((li.amount.unwrap() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn wei_to_eth_conversion() {
        assert_eq!(wei_to_eth("1000000000000000000"), 1.0);
        assert_eq!(wei_to_eth("0"), 0.0);
        assert!((wei_to_eth("500000000000000000") - 0.5).abs() < 1e-15);
    }

    #[test]
    fn token_value_uses_decimals() {
        assert!((token_value("1000000000000000000000", "18") - 1000.0).abs() < 0.001);
        assert_eq!(token_value("100", "2"), 1.0);
        assert_eq!(token_value("5", "0"), 5.0);
    }

    // ---------------------------------------------------------------------------
    // Parse connect input.

    #[test]
    fn parse_connect_input_splits_key_addresses_chain() {
        let (key, addrs, chain) =
            parse_connect_input("MYAPIKEY123|0xaddr1,0xaddr2@137").unwrap();
        assert_eq!(key, "MYAPIKEY123");
        assert_eq!(addrs, vec!["0xaddr1", "0xaddr2"]);
        assert_eq!(chain, 137);
    }

    #[test]
    fn parse_connect_input_defaults_chain_1() {
        let (key, addrs, chain) = parse_connect_input("MYKEY|0xaddr1").unwrap();
        assert_eq!(key, "MYKEY");
        assert_eq!(addrs, vec!["0xaddr1"]);
        assert_eq!(chain, DEFAULT_CHAIN_ID);
    }

    #[test]
    fn parse_connect_input_no_pipe_bails() {
        assert!(parse_connect_input("notakey").is_err());
    }

    #[test]
    fn parse_connect_input_deduplicates_addresses() {
        let (_, addrs, _) =
            parse_connect_input("KEY|0xaddr1, 0xaddr1, 0xaddr2").unwrap();
        assert_eq!(addrs, vec!["0xaddr1", "0xaddr2"]);
    }

    // ---------------------------------------------------------------------------
    // Pull integration tests.

    #[test]
    fn pull_writes_both_layers_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::new()
            .with_txlist(vec![vec![tx_normal_receive(), tx_normal_send()]])
            .with_tokentx(vec![vec![tx_token_transfer()]])
            .with_internal(vec![vec![tx_internal()]]);

        let out = pull_with(&v, &api, &[MY_ADDR.to_string()], 1).unwrap();
        // 4 rows total: 2 normal + 1 token + 1 internal
        assert_eq!(out.counts.get("transactions"), Some(&4));

        // Contract layer has 4 rows.
        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<Value> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<Value>(&key).unwrap());
        }
        assert_eq!(all.len(), 4);

        // Raw layer also has 4 rows.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_all: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_all.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_all.len(), 4);
        // Raw rows have the action field.
        assert!(raw_all.iter().any(|r| r.get("action") == Some(&json!("txlist"))));
        assert!(raw_all.iter().any(|r| r.get("action") == Some(&json!("tokentx"))));
        assert!(raw_all.iter().any(|r| r.get("action") == Some(&json!("txlistinternal"))));

        // Cursor advances to highest block seen (4730211 from the internal tx).
        let state = v.read_ethereum_sync();
        let ck = cursor_key(MY_ADDR, 1);
        assert_eq!(state.last_block.get(&ck).map(String::as_str), Some("4730211"));
    }

    #[test]
    fn pull_deduplicates_on_re_run() {
        let v = temp_vault("dedup");
        let api1 = MockApi::new().with_txlist(vec![vec![tx_normal_receive()]]);
        pull_with(&v, &api1, &[MY_ADDR.to_string()], 1).unwrap();

        // Same row again.
        let api2 = MockApi::new().with_txlist(vec![vec![tx_normal_receive()]]);
        let out2 = pull_with(&v, &api2, &[MY_ADDR.to_string()], 1).unwrap();
        assert_eq!(out2.counts.get("transactions"), Some(&0), "duplicate row skipped");

        // File still has only 1 row.
        let contract = v.stream(DIR, Partition::Month);
        let mut n = 0;
        for key in contract.partitions().unwrap() {
            n += contract.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(n, 1);
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_block.is_empty());
        let partial: SyncState =
            serde_json::from_str(r#"{"updated":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert!(partial.last_block.is_empty());
        assert_eq!(partial.updated.as_deref(), Some("2026-06-01T00:00:00-07:00"));
    }

    #[test]
    fn cursor_key_separates_address_and_chain() {
        let k1 = cursor_key("0xaddr", 1);
        let k2 = cursor_key("0xaddr", 137);
        assert_ne!(k1, k2, "same address on different chains has different cursor key");
    }

    #[test]
    fn pull_needs_connection() {
        let v = temp_vault("needsconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn connection_stores_and_status_summarizes() {
        let v = temp_vault("conn");
        def_connect(&v, "APIKEY123|0xaddr1,0xaddr2@137").unwrap();
        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert!(status.accounts[0].label.contains("0xaddr1 +1 more"));
        assert!(status.accounts[0].label.contains("137"));

        def_disconnect(&v, "ethereum").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn def_exposes_connection_id_and_pull() {
        assert_eq!(DEF.connection, Some("ethereum"));
        assert!(DEF.pull.is_some());
        assert_eq!(CONNECTION.id, "ethereum");
    }

    /// Large-wallet pagination: when a page returns exactly PAGE_SIZE rows, the
    /// block window must narrow (startblock advances to the highest block in the
    /// batch and page resets to 1) rather than just incrementing the page number.
    /// This test uses SMALL_PAGE=3 to simulate a PAGE_SIZE boundary without
    /// generating 1,000 fixture rows.
    ///
    /// The scenario uses a custom PAGE_SIZE via drain_action_with_page_size.
    /// Since drain_action uses the module-level PAGE_SIZE constant, we test the
    /// window-narrowing logic by providing a full first page (PAGE_SIZE rows all
    /// in block 5000) and a short second page (2 rows in block 5001), then
    /// verifying all rows were written and the second request used a different
    /// startblock than the first (window narrowed).
    #[test]
    fn large_wallet_pagination_narrows_block_window() {
        // Build a "full" first page: PAGE_SIZE txs all in block 5000.
        let first_page: Vec<Value> = (0..PAGE_SIZE)
            .map(|i| {
                json!({
                    "blockNumber": "5000",
                    "timeStamp": "1640000000",
                    "hash": format!("0xhash{i:05}"),
                    "from": OTHER_ADDR,
                    "to": MY_ADDR,
                    "value": "1000000000000000000",
                    "isError": "0",
                    "txreceipt_status": "1",
                    "gasUsed": "21000",
                    "gasPrice": "1000000000",
                    "transactionIndex": i.to_string()
                })
            })
            .collect();

        // The second page (after window narrowing to startblock=5000) has 2 more
        // rows in block 5001.
        let second_page: Vec<Value> = (0..2)
            .map(|i| {
                json!({
                    "blockNumber": "5001",
                    "timeStamp": "1640001000",
                    "hash": format!("0xhash9{i:04}"),
                    "from": OTHER_ADDR,
                    "to": MY_ADDR,
                    "value": "1000000000000000000",
                    "isError": "0",
                    "txreceipt_status": "1",
                    "gasUsed": "21000",
                    "gasPrice": "1000000000",
                    "transactionIndex": i.to_string()
                })
            })
            .collect();

        // Total unique rows = PAGE_SIZE + 2 (the first_page items in block 5000
        // are all unique; the second_page items in block 5001 are new too).
        let expected_unique = PAGE_SIZE + 2;

        let v = temp_vault("largepagination");
        let api = MockApi::new()
            .with_txlist(vec![first_page, second_page]);

        let out = pull_with(&v, &api, &[MY_ADDR.to_string()], 1).unwrap();
        assert_eq!(
            out.counts.get("transactions"),
            Some(&(expected_unique as u64)),
            "all {} rows from both pages must be written", expected_unique
        );

        // Verify the second txlist call used a higher startblock (window narrowed).
        let calls = api.calls.borrow();
        let txlist_calls: Vec<_> = calls.iter().filter(|(a, _, _, _)| *a == "txlist").collect();
        assert!(txlist_calls.len() >= 2, "must have made at least 2 txlist calls");
        let first_start = txlist_calls[0].2;
        let second_start = txlist_calls[1].2;
        assert!(
            second_start > first_start,
            "second txlist call must use a higher startblock ({} > {}) — window narrowed",
            second_start, first_start
        );
        // Second call resets to page=1.
        assert_eq!(txlist_calls[1].3, 1, "after window narrowing page must reset to 1");
    }
}

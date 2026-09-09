//! Interactive Brokers Flex Query API — trade fills, dividends, and corporate
//! actions via the IBKR Flex Web Service.
//!
//! **Brief:** docs/integrations/interactive-brokers.md
//!
//! ## Auth
//!
//! TokenPaste: the user pastes a composite string `<token>|<query_id>`. The
//! Flex Web Service token is created in Client Portal → Settings →
//! Reports & Statements → Flex Queries → "Create Token". The query id comes
//! from a user-defined Flex Query (same section). Both are user-owned — no
//! Trove-held app credentials.
//!
//! ## Two-step HTTP API
//!
//! Step 1: POST `https://ndcdyn.interactivebrokers.com/AccountManagement/FlexWebService/SendRequest`
//!   - query params: `t=<token>&q=<query_id>&v=3`
//!   - response XML: `<FlexStatementResponse><Status>Success</Status><ReferenceCode>…</ReferenceCode>…`
//!
//! Step 2: POST (poll) `https://gdcdyn.interactivebrokers.com/AccountManagement/FlexWebService/GetStatement`
//!   - query params: `t=<token>&q=<reference_code>&v=3`
//!   - response: either another `FlexStatementResponse` (with code 1019/1018 = not ready;
//!     retry with backoff), or the full `FlexQueryResponse` XML.
//!
//! ## Vault layout
//!
//! - Raw (unconditional): `finance/interactive-brokers/raw/YYYY-MM.jsonl`
//!   Full fidelity JSON serialisation of each parsed record (Trades,
//!   CashTransactions, CorporateActions, OpenPositions).
//! - Contract (Trades + CashTransactions → `crate::finance::LineItem`):
//!   `finance/purchases/interactive-brokers/YYYY-MM.jsonl`
//!   One row per trade/cash event, `guid` = `transactionID` (Trades) or
//!   `transactionID` (CashTransactions). Deduped on re-pull.
//! - Positions (raw-only, finance-holdings draft):
//!   `finance/interactive-brokers/positions/YYYY-MM-DD.jsonl`
//!   Dated snapshot from `OpenPositions` section, full fidelity.
//!
//! ## Key XML field names (confirmed from ibflex Types.py)
//!
//! Trade: `transactionID`, `tradeDate`, `symbol`, `quantity`, `tradePrice`,
//!   `tradeMoney`, `proceeds`, `netCash`, `ibCommission`, `buySell`,
//!   `assetCategory`, `accountId`, `currency`, `description`.
//! CashTransaction: `transactionID`, `dateTime`, `amount`, `type`,
//!   `symbol`, `currency`, `description`, `accountId`.
//! CorporateAction: `transactionID` (primary unique id per IBKR docs),
//!   `actionID` (secondary, may be absent), `dateTime`, `symbol`, `quantity`,
//!   `amount`, `type`, `currency`, `accountId`, `description`.
//! OpenPosition: `reportDate`, `symbol`, `position`, `markPrice`,
//!   `positionValue`, `costBasisPrice`, `costBasisMoney`, `currency`,
//!   `accountId`, `assetCategory`.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use quick_xml::events::{BytesText, Event};
use quick_xml::Reader;
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

/// Contract layer — trades/cash as LineItem rows.
const DIR: &str = "finance/purchases/interactive-brokers";
/// Raw layer — verbatim parsed records.
const RAW_DIR: &str = "finance/interactive-brokers/raw";
/// Positions snapshot dir (raw-only, finance-holdings draft).
const POS_DIR: &str = "finance/interactive-brokers/positions";
/// Cursor file (non-secret, rebuildable).
const SYNC_FILE: &str = ".trove/interactive-brokers-sync.json";
/// Service id for token storage.
const SERVICE: &str = "interactive-brokers";

/// Step 1 endpoint: request generation of a Flex statement.
const SEND_REQUEST_URL: &str = "https://ndcdyn.interactivebrokers.com/AccountManagement/FlexWebService/SendRequest";
/// Step 2 endpoint: retrieve the generated statement by reference code.
const GET_STATEMENT_URL: &str = "https://gdcdyn.interactivebrokers.com/AccountManagement/FlexWebService/GetStatement";

/// Flex Web Service API version.
const FLEX_VERSION: &str = "3";
/// How long to wait for the statement to generate (code 1019/1018 = not ready).
const MAX_RETRIES: u32 = 12;
/// Base backoff between GetStatement polls (5 seconds as recommended by IBKR).
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// HTTP timeout per request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between periodic syncs: daily (4 h × 6 = 24 h).
pub const IB_SYNC_SECS: u64 = 24 * 3600;

// ---------------------------------------------------------------------------
// Registry hooks.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("transactions").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("Interactive Brokers synced — {n} new records")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Interactive Brokers sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "interactive-brokers",
        name: "Interactive Brokers",
        kind: IntegrationKind::CloudSync,
        // 🔒 Financial detail — ships opt-in with explicit acknowledgement.
        default_on: false,
        description: "Syncs detailed brokerage records from Interactive Brokers — trade fills, \
                      lot-level cost basis, dividends, and corporate actions — using the IBKR \
                      Flex Web Service. You define a Flex Query in Client Portal; Trove polls it \
                      daily. No Trove-held app credentials required.",
        domain: "finance",
        vault_path: "finance/purchases/interactive-brokers/",
        toggleable: true,
        setup: &[
            "In IBKR Client Portal, go to Settings → Reports & Statements → Flex Queries.",
            "Create a new Activity Flex Query. Enable at minimum: Trades, Cash Transactions, \
             Corporate Actions, Open Positions. Set the date range to 'Last N Calendar Days: 365' \
             and the format to XML. In the Trades section, set Level of Detail = Executions \
             (the default) — this avoids double-counting if summary rows are also enabled.",
            "Still in Flex Queries settings, generate a Flex Web Service token (one token \
             covers all queries for your account).",
            "Paste your token and query id here, separated by a pipe: <token>|<query_id> \
             (e.g. 1234567890abcdef|1000123).",
            "First sync backfills up to the date range set in your Flex Query. Later syncs \
             are incremental (new records are deduped by transaction id).",
        ],
        caveats: "The Flex Web Service uses an unusual two-step API: Trove requests statement \
                  generation, then polls until ready (up to ~1 minute). Positions are stored \
                  raw-only until the holdings contract is ratified. Options, futures, and forex \
                  trades are included when your Flex Query covers them.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(IB_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some(SERVICE),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste: composite "<token>|<query_id>").

/// Parse the composite pasted string: `<token>|<query_id>`.
/// Returns `(token, query_id)`. The separator is `|`.
fn parse_credentials(pasted: &str) -> Option<(String, String)> {
    let s = pasted.trim();
    let (token, qid) = s.split_once('|')?;
    let token = token.trim().to_string();
    let qid = qid.trim().to_string();
    if token.is_empty() || qid.is_empty() {
        None
    } else {
        Some((token, qid))
    }
}

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    parse_credentials(pasted)
        .context("paste your Flex Web Service token and query id separated by a pipe: token|query_id")?;
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
        if let Some((_, qid)) = parse_credentials(&token.access_token) {
            accounts.push(ConnectedAccount {
                key: SERVICE.to_string(),
                label: format!("Query {qid}"),
                connected_at: None,
                expires_at: None,
                needs_reconnect: false,
                extra: BTreeMap::new(),
            });
        }
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: SERVICE,
    display_name: "Interactive Brokers",
    methods: &[ConnectMethod::TokenPaste {
        label: "Flex token | query id",
        help: "In Client Portal → Settings → Reports & Statements → Flex Queries, create an \
               Activity Flex Query (XML format, covering Trades + Cash Transactions + Corporate \
               Actions + Open Positions) and generate a Flex Web Service token. Paste both \
               separated by a pipe character: <your-token>|<your-query-id>",
        placeholder: "1234567890abcdef|1000123",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["interactive-brokers"],
    setup: &[
        "Log in to Client Portal at https://www.interactivebrokers.com/portal",
        "Navigate to Settings → Reports & Statements → Flex Queries.",
        "Click 'Create' to make a new Activity Flex Query. Enable sections: Trades, \
         Cash Transactions, Corporate Actions, Open Positions. Set format = XML, \
         date range = Last 365 Calendar Days (or your preferred range). In the Trades \
         section set Level of Detail = Executions (avoids double-counting summary rows).",
        "In the same Flex Queries page, click 'Generate Token' to create your Flex Web \
         Service token. Copy the token (a long alphanumeric string).",
        "Copy the Query Id from the Flex Query you just created (shown in the query list).",
        "Paste both here separated by |: <token>|<query_id>",
    ],
};

// ---------------------------------------------------------------------------
// Sync cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful sync (informational only —
    /// the pull always re-fetches the full configured Flex window and dedups
    /// by guid on disk, so no high-water cursor is needed or maintained).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_ib_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ib_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Parsed record types (XML attribute maps → Value).

/// A record kind in the Flex XML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordKind {
    Trade,
    CashTransaction,
    CorporateAction,
    OpenPosition,
}

/// A parsed XML element: kind + map of attribute name → value string.
#[derive(Debug, Clone)]
struct FlexRecord {
    kind: RecordKind,
    attrs: BTreeMap<String, String>,
}

impl FlexRecord {
    fn get(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }

    /// Convert to a raw JSON Value (all fields preserved as strings/numbers).
    fn to_json(&self) -> Value {
        let kind_str = match self.kind {
            RecordKind::Trade => "Trade",
            RecordKind::CashTransaction => "CashTransaction",
            RecordKind::CorporateAction => "CorporateAction",
            RecordKind::OpenPosition => "OpenPosition",
        };
        let mut m = Map::new();
        m.insert("_record_type".into(), Value::String(kind_str.to_string()));
        for (k, v) in &self.attrs {
            // Try numeric coercion for known numeric fields, fall back to string.
            let jv = if let Ok(n) = v.parse::<i64>() {
                Value::from(n)
            } else if let Ok(f) = v.parse::<f64>() {
                Value::from(f)
            } else {
                Value::String(v.clone())
            };
            m.insert(k.clone(), jv);
        }
        Value::Object(m)
    }

    /// The ts string for partitioning (the relevant date field, converted to
    /// a local RFC3339 midnight). Returns `None` when the date is unparseable.
    fn ts(&self) -> Option<String> {
        // Prefer tradeDate for trades, dateTime for cash/corp actions,
        // reportDate for positions.
        let raw = match self.kind {
            RecordKind::Trade => self.get("tradeDate"),
            RecordKind::CashTransaction => self.get("dateTime").or_else(|| self.get("reportDate")),
            RecordKind::CorporateAction => self.get("dateTime").or_else(|| self.get("reportDate")),
            RecordKind::OpenPosition => self.get("reportDate"),
        }?;
        parse_ibkr_date_to_local(raw)
    }

    /// The stable dedup key for the contract layer. `None` = skip.
    fn guid(&self) -> Option<String> {
        match self.kind {
            RecordKind::Trade => {
                let tid = self.get("transactionID").filter(|s| !s.is_empty())?;
                Some(format!("trade:{tid}"))
            }
            RecordKind::CashTransaction => {
                let tid = self.get("transactionID").filter(|s| !s.is_empty())?;
                Some(format!("cash:{tid}"))
            }
            RecordKind::CorporateAction => {
                // transactionID is the documented unique id per IBKR's Flex Statement
                // reference ("Unique ID for this transaction"). actionID is present on
                // some statement configurations but may be absent or empty. Use
                // transactionID first, falling back to actionID so neither is lost.
                let id = self
                    .get("transactionID")
                    .filter(|s| !s.is_empty())
                    .or_else(|| self.get("actionID").filter(|s| !s.is_empty()))?;
                Some(format!("corp:{id}"))
            }
            RecordKind::OpenPosition => None, // positions are snapshots, not ledger rows
        }
    }
}

// ---------------------------------------------------------------------------
// XML helpers.

/// Decode a quick-xml 0.40 `BytesText` element into a String.
/// In quick-xml 0.40, `decode()` handles encoding/EOL conversion but leaves
/// XML entities; `unescape()` is not available on `BytesText` — instead we
/// call `decode()` then `quick_xml::escape::unescape()`.
fn text_decode(e: &BytesText) -> String {
    match e.decode() {
        Ok(decoded) => match quick_xml::escape::unescape(&decoded) {
            Ok(unescaped) => unescaped.into_owned(),
            Err(_) => decoded.into_owned(),
        },
        Err(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// XML parsing.

/// Parse the Flex XML body, extracting records we care about.
/// The Flex XML has a shape like:
/// ```xml
/// <FlexQueryResponse>
///   <FlexStatements count="N">
///     <FlexStatement accountId="…" …>
///       <Trades>
///         <Trade transactionID="…" tradeDate="…" … />
///       </Trades>
///       <CashTransactions>
///         <CashTransaction transactionID="…" dateTime="…" … />
///       </CashTransactions>
///       <CorporateActions>
///         <CorporateAction actionID="…" dateTime="…" … />
///       </CorporateActions>
///       <OpenPositions>
///         <OpenPosition reportDate="…" … />
///       </OpenPositions>
///     </FlexStatement>
///   </FlexStatements>
/// </FlexQueryResponse>
/// ```
fn parse_flex_xml(xml: &str) -> Result<Vec<FlexRecord>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut records = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Empty(ref e) | Event::Start(ref e) => {
                let tag = e.name();
                let tag_str = std::str::from_utf8(tag.as_ref())
                    .unwrap_or("")
                    .to_string();
                let kind = match tag_str.as_str() {
                    "Trade" => Some(RecordKind::Trade),
                    "CashTransaction" => Some(RecordKind::CashTransaction),
                    "CorporateAction" => Some(RecordKind::CorporateAction),
                    "OpenPosition" => Some(RecordKind::OpenPosition),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let mut attrs = BTreeMap::new();
                    for attr in e.attributes().flatten() {
                        let key = std::str::from_utf8(attr.key.as_ref())
                            .unwrap_or("")
                            .to_string();
                        let val = attr
                            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .map(|v| v.into_owned())
                            .unwrap_or_default();
                        if !key.is_empty() {
                            attrs.insert(key, val);
                        }
                    }
                    records.push(FlexRecord { kind, attrs });
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(records)
}

// ---------------------------------------------------------------------------
// Date parsing helpers.

/// Parse an IBKR date string to a local RFC3339 timestamp.
/// Handles: "YYYY-MM-DD", "YYYYMMDD", "YYYY-MM-DD;HH:MM:SS" (dateTime with semicolon),
/// "YYYY-MM-DD,HH:MM:SS", and plain "YYYYMMDD;HHMMSS".
fn parse_ibkr_date_to_local(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s == "0" {
        return None;
    }

    // Strip a time component after ';' or ',' — we only need the date for partitioning.
    let date_part = s.split_once(';').map(|(d, _)| d).unwrap_or(s);
    let date_part = date_part.split_once(',').map(|(d, _)| d).unwrap_or(date_part);

    // "YYYY-MM-DD"
    if let Ok(d) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
        let dt = d
            .and_hms_opt(0, 0, 0)?
            .and_local_timezone(Local)
            .single()?;
        return Some(dt.to_rfc3339());
    }
    // "YYYYMMDD"
    if let Ok(d) = NaiveDate::parse_from_str(date_part, "%Y%m%d") {
        let dt = d
            .and_hms_opt(0, 0, 0)?
            .and_local_timezone(Local)
            .single()?;
        return Some(dt.to_rfc3339());
    }
    None
}

// ---------------------------------------------------------------------------
// Contract mapping.

/// Map a FlexRecord to a LineItem. Returns `None` for records that cannot be
/// mapped (OpenPositions, missing required fields).
fn to_line_item(rec: &FlexRecord) -> Option<LineItem> {
    let guid = rec.guid()?;
    let ts = rec.ts()?;
    // Verify the ts partitions.
    Partition::Month.key(&ts)?;

    match rec.kind {
        RecordKind::Trade => {
            let symbol = rec.get("symbol").filter(|s| !s.is_empty()).unwrap_or("Unknown");
            let buy_sell = rec.get("buySell").unwrap_or("").to_string();
            let asset_cat = rec.get("assetCategory").unwrap_or("").to_string();
            let currency = rec.get("currency").unwrap_or("USD").to_string();

            // netCash is the proceeds minus commissions; tradeMoney is the gross.
            let net_cash: Option<f64> = rec.get("netCash").and_then(|v| v.parse().ok());
            let proceeds: Option<f64> = rec.get("proceeds").and_then(|v| v.parse().ok());
            let commission: Option<f64> = rec.get("ibCommission").and_then(|v| v.parse().ok());
            let qty: Option<f64> = rec.get("quantity").and_then(|v| v.parse().ok());
            let price: Option<f64> = rec.get("tradePrice").and_then(|v| v.parse().ok());

            // Use netCash as the contract amount (closest to "what hit my account").
            let amount = net_cash.or(proceeds);

            let item = match buy_sell.to_uppercase().as_str() {
                "BUY" | "SELL" => format!("{} {symbol}", if buy_sell.to_uppercase() == "BUY" { "Buy" } else { "Sell" }),
                _ => format!("Trade {symbol}"),
            };

            let mut extra = Map::new();
            extra.insert("buy_sell".into(), Value::String(buy_sell.clone()));
            extra.insert("asset_category".into(), Value::String(asset_cat));
            if let Some(q) = qty {
                extra.insert("quantity".into(), Value::from(q));
            }
            if let Some(p) = price {
                extra.insert("trade_price".into(), Value::from(p));
            }
            if let Some(c) = commission {
                extra.insert("commission".into(), Value::from(c));
            }
            if let Some(p) = proceeds {
                extra.insert("proceeds".into(), Value::from(p));
            }
            if let Some(tid) = rec.get("transactionID") {
                extra.insert("transaction_id".into(), Value::String(tid.to_string()));
            }
            if let Some(trid) = rec.get("tradeID") {
                extra.insert("trade_id".into(), Value::String(trid.to_string()));
            }
            if let Some(acct) = rec.get("accountId") {
                extra.insert("account_id".into(), Value::String(acct.to_string()));
            }
            if let Some(exch) = rec.get("exchange").filter(|s| !s.is_empty()) {
                extra.insert("exchange".into(), Value::String(exch.to_string()));
            }
            if let Some(desc) = rec.get("description").filter(|s| !s.is_empty()) {
                extra.insert("description".into(), Value::String(desc.to_string()));
            }
            if let Some(isin) = rec.get("isin").filter(|s| !s.is_empty()) {
                extra.insert("isin".into(), Value::String(isin.to_string()));
            }
            if let Some(cusip) = rec.get("cusip").filter(|s| !s.is_empty()) {
                extra.insert("cusip".into(), Value::String(cusip.to_string()));
            }

            let mut li = LineItem::new(SERVICE, guid, ts, "Interactive Brokers");
            li.item = item;
            li.amount = amount;
            li.currency = currency;
            li.extra = extra;
            Some(li)
        }
        RecordKind::CashTransaction => {
            let txn_type = rec.get("type").unwrap_or("").to_string();
            let amount: Option<f64> = rec.get("amount").and_then(|v| v.parse().ok());
            let currency = rec.get("currency").unwrap_or("USD").to_string();
            let symbol = rec.get("symbol").filter(|s| !s.is_empty()).unwrap_or("");
            let desc = rec.get("description").filter(|s| !s.is_empty()).unwrap_or("");

            // Human-readable item string.
            let item = match txn_type.as_str() {
                "Dividends" | "Payment In Lieu Of Dividends" => {
                    if !symbol.is_empty() {
                        format!("Dividend {symbol}")
                    } else {
                        "Dividend".to_string()
                    }
                }
                "Withholding Tax" => "Withholding Tax".to_string(),
                "Broker Interest Paid" | "Broker Interest Received" => "Interest".to_string(),
                "Commission Adjustments" => "Commission Adjustment".to_string(),
                "Other Fees" => "Fee".to_string(),
                _ if !txn_type.is_empty() => txn_type.clone(),
                _ => "Cash Transaction".to_string(),
            };

            let mut extra = Map::new();
            extra.insert("transaction_type".into(), Value::String(txn_type));
            if let Some(tid) = rec.get("transactionID") {
                extra.insert("transaction_id".into(), Value::String(tid.to_string()));
            }
            if let Some(acct) = rec.get("accountId") {
                extra.insert("account_id".into(), Value::String(acct.to_string()));
            }
            if !symbol.is_empty() {
                extra.insert("symbol".into(), Value::String(symbol.to_string()));
            }
            if !desc.is_empty() {
                extra.insert("description".into(), Value::String(desc.to_string()));
            }
            if let Some(isin) = rec.get("isin").filter(|s| !s.is_empty()) {
                extra.insert("isin".into(), Value::String(isin.to_string()));
            }

            let mut li = LineItem::new(SERVICE, guid, ts, "Interactive Brokers");
            li.item = item;
            li.amount = amount;
            li.currency = currency;
            li.extra = extra;
            Some(li)
        }
        RecordKind::CorporateAction => {
            let corp_type = rec.get("type").unwrap_or("").to_string();
            let symbol = rec.get("symbol").filter(|s| !s.is_empty()).unwrap_or("Unknown");
            let amount: Option<f64> = rec.get("amount").and_then(|v| v.parse().ok());
            let qty: Option<f64> = rec.get("quantity").and_then(|v| v.parse().ok());
            let currency = rec.get("currency").unwrap_or("USD").to_string();
            let desc = rec.get("description").filter(|s| !s.is_empty()).unwrap_or("");

            let item = if !corp_type.is_empty() {
                format!("{corp_type}: {symbol}")
            } else {
                format!("Corporate Action: {symbol}")
            };

            let mut extra = Map::new();
            extra.insert("action_type".into(), Value::String(corp_type));
            if let Some(aid) = rec.get("actionID") {
                extra.insert("action_id".into(), Value::String(aid.to_string()));
            }
            if let Some(acct) = rec.get("accountId") {
                extra.insert("account_id".into(), Value::String(acct.to_string()));
            }
            if let Some(q) = qty {
                extra.insert("quantity".into(), Value::from(q));
            }
            if !desc.is_empty() {
                extra.insert("description".into(), Value::String(desc.to_string()));
            }
            if let Some(isin) = rec.get("isin").filter(|s| !s.is_empty()) {
                extra.insert("isin".into(), Value::String(isin.to_string()));
            }

            let mut li = LineItem::new(SERVICE, guid, ts, "Interactive Brokers");
            li.item = item;
            li.amount = amount;
            li.currency = currency;
            li.extra = extra;
            Some(li)
        }
        RecordKind::OpenPosition => None, // positions → raw-only snapshot
    }
}

// ---------------------------------------------------------------------------
// Write layer.

/// Raw line: the JSON object tagged with `ts` for the month-partition writer.
/// The `ts` field is skipped (it's purely for partitioning).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}


/// Write a dated positions snapshot (raw-only, finance-holdings draft).
/// `finance/interactive-brokers/positions/YYYY-MM-DD.jsonl` — one line per position.
fn write_positions_snapshot(vault: &Vault, positions: &[FlexRecord]) -> Result<()> {
    if positions.is_empty() {
        return Ok(());
    }
    let today = Local::now().format("%Y-%m-%d").to_string();
    let path = vault.resolve(&format!("{POS_DIR}/{today}.jsonl"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .context("creating positions snapshot dir")?;
    }
    let mut lines = String::new();
    for pos in positions {
        let v = pos.to_json();
        lines.push_str(&serde_json::to_string(&v).context("serializing position")?);
        lines.push('\n');
    }
    std::fs::write(&path, &lines).context("writing positions snapshot")
}

// ---------------------------------------------------------------------------
// HTTP / Flex Web Service.

/// Performs the two-step Flex Web Service request+poll cycle.
/// Returns the full XML response body on success.
pub(crate) fn fetch_flex_statement(
    api: &impl FlexApi,
    token: &str,
    query_id: &str,
) -> Result<String> {
    // Step 1: request generation.
    let status_xml = api.send_request(token, query_id)?;
    let (ref_code, poll_url) = parse_send_response(&status_xml)?;

    // Step 2: poll until ready.
    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            std::thread::sleep(POLL_INTERVAL);
        }
        let body = api.get_statement(token, &ref_code, poll_url.as_deref())?;
        match poll_response_kind(&body) {
            PollResult::Ready(xml) => return Ok(xml),
            PollResult::NotReady => {} // retry
            PollResult::Error(msg) => bail!("Flex statement error: {msg}"),
        }
    }
    bail!("Flex statement not ready after {} attempts", MAX_RETRIES)
}

/// Parse the SendRequest response XML. Returns `(reference_code, optional_url)`.
fn parse_send_response(xml: &str) -> Result<(String, Option<String>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut status = String::new();
    let mut ref_code = String::new();
    let mut url = None::<String>;
    let mut error_code = String::new();
    let mut error_msg = String::new();
    let mut current_tag = String::new();

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => {
                current_tag = std::str::from_utf8(e.name().as_ref()).unwrap_or("").to_string();
            }
            Event::Text(ref e) => {
                let txt = text_decode(e);
                match current_tag.as_str() {
                    "Status" => status = txt,
                    "ReferenceCode" => ref_code = txt,
                    "Url" => url = Some(txt),
                    "ErrorCode" => error_code = txt,
                    "ErrorMessage" => error_msg = txt,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if status != "Success" {
        bail!("Flex SendRequest failed (code {error_code}): {error_msg}");
    }
    if ref_code.is_empty() {
        bail!("Flex SendRequest succeeded but returned no ReferenceCode");
    }
    Ok((ref_code, url))
}

/// The outcome of parsing a GetStatement poll response body.
enum PollResult {
    /// The statement is ready; the inner string is the full FlexQueryResponse XML.
    Ready(String),
    /// Not yet ready (codes 1019, 1018); retry.
    NotReady,
    /// A terminal error from the service.
    Error(String),
}

/// Decide whether a GetStatement response body is ready, pending, or an error.
fn poll_response_kind(body: &str) -> PollResult {
    // A ready response contains <FlexQueryResponse somewhere near the start
    // (after an optional XML declaration <?xml …?> and whitespace).
    if body.contains("<FlexQueryResponse") {
        return PollResult::Ready(body.to_string());
    }
    // Otherwise parse as FlexStatementResponse to extract error code / message.
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut error_code = String::new();
    let mut error_msg = String::new();
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                current_tag = std::str::from_utf8(e.name().as_ref()).unwrap_or("").to_string();
            }
            Ok(Event::Text(ref e)) => {
                let txt = text_decode(e);
                match current_tag.as_str() {
                    "ErrorCode" => error_code = txt,
                    "ErrorMessage" => error_msg = txt,
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    // 1009 = server busy (retry 5s); 1018 = throttled (retry 10s);
    // 1019 = generation in progress (retry 5s). All are transient retry signals.
    match error_code.as_str() {
        "1009" | "1018" | "1019" => PollResult::NotReady,
        _ if !error_msg.is_empty() => PollResult::Error(error_msg),
        _ if !error_code.is_empty() => PollResult::Error(format!("code {error_code}")),
        _ => PollResult::NotReady, // unknown shape → retry
    }
}

/// The HTTP actions this collector needs. A trait for testability.
pub(crate) trait FlexApi {
    fn send_request(&self, token: &str, query_id: &str) -> Result<String>;
    fn get_statement(&self, token: &str, ref_code: &str, url: Option<&str>) -> Result<String>;
}

/// Production HTTP client.
struct LiveClient;

impl FlexApi for LiveClient {
    fn send_request(&self, token: &str, query_id: &str) -> Result<String> {
        let url = format!("{SEND_REQUEST_URL}?t={token}&q={query_id}&v={FLEX_VERSION}");
        ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .map_err(|e| anyhow::anyhow!("Flex SendRequest HTTP error: {e}"))?
            .into_string()
            .context("reading SendRequest body")
    }

    fn get_statement(&self, token: &str, ref_code: &str, url: Option<&str>) -> Result<String> {
        let base = url.unwrap_or(GET_STATEMENT_URL);
        let full_url = format!("{base}?t={token}&q={ref_code}&v={FLEX_VERSION}");
        ureq::get(&full_url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .map_err(|e| anyhow::anyhow!("Flex GetStatement HTTP error: {e}"))?
            .into_string()
            .context("reading GetStatement body")
    }
}

// ---------------------------------------------------------------------------
// The pull.

/// Load and validate credentials from the vault.
fn load_credentials(vault: &Vault) -> Result<(String, String)> {
    let token_set = vault
        .load_sync_token(SERVICE)?
        .context("Interactive Brokers is not connected — paste your token|query_id in the Integrations tab")?;
    parse_credentials(&token_set.access_token)
        .context("Interactive Brokers credentials malformed — paste <token>|<query_id>")
}

/// Public pull entry point (called by the DEF hooks and tests).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let (token, query_id) = load_credentials(vault)?;
    let client = LiveClient;
    pull_with(vault, &client, &token, &query_id)
}

/// Testable pull body over an injected API.
pub(crate) fn pull_with(
    vault: &Vault,
    api: &impl FlexApi,
    token: &str,
    query_id: &str,
) -> Result<PullOutcome> {
    let xml = fetch_flex_statement(api, token, query_id)?;
    process_xml(vault, &xml)
}

/// Parse XML, write vault layers, return outcome. Separate from the HTTP step
/// so tests can drive it with fixture XML without touching the network.
pub(crate) fn process_xml(vault: &Vault, xml: &str) -> Result<PullOutcome> {
    let records = parse_flex_xml(xml)?;

    // Partition into ledger records and positions.
    let mut ledger_rows: Vec<(LineItem, Value)> = Vec::new();
    let mut positions: Vec<FlexRecord> = Vec::new();

    for rec in &records {
        match rec.kind {
            RecordKind::OpenPosition => positions.push(rec.clone()),
            _ => {
                if let Some(li) = to_line_item(rec) {
                    let raw = rec.to_json();
                    ledger_rows.push((li, raw));
                }
            }
        }
    }

    // Build the candidate raw records (all non-Position records with a valid ts).
    // Each record tagged with its guid (stored as "_guid" on the JSON) so the
    // dedup on re-pull reads the same key regardless of numeric vs. string type
    // or camelCase attribute naming.
    let all_raw: Vec<(String, String, Value)> = records
        .iter()
        .filter(|r| r.kind != RecordKind::OpenPosition)
        .filter_map(|r| {
            let ts = r.ts()?;
            let mut val = r.to_json();
            // Tag with _guid for raw-layer dedup. Records without a guid
            // (e.g. unguidable edge cases) get "_guid": null and are always
            // written (they cannot be deduped anyway).
            if let Value::Object(ref mut m) = val {
                match r.guid() {
                    Some(ref g) => { m.insert("_guid".into(), Value::String(g.clone())); }
                    None        => { m.insert("_guid".into(), Value::Null); }
                }
            }
            Some((ts, r.guid().unwrap_or_default(), val))
        })
        .collect();

    // Dedup raw against existing raw on disk: read back the _guid field written
    // by previous runs and skip anything already present.
    {
        let raw_stream = vault.stream(RAW_DIR, Partition::Month);
        let mut raw_seen: HashSet<String> = HashSet::new();
        for key in raw_stream.partitions()? {
            for v in raw_stream.read::<Value>(&key)? {
                if let Some(g) = v.get("_guid").and_then(Value::as_str) {
                    if !g.is_empty() {
                        raw_seen.insert(g.to_string());
                    }
                }
            }
        }
        let mut raw_new: Vec<RawLine> = Vec::new();
        for (ts, guid, val) in &all_raw {
            // Skip records whose guid we have already written to disk.
            if !guid.is_empty() && raw_seen.contains(guid.as_str()) {
                continue;
            }
            raw_new.push(RawLine { ts: ts.clone(), value: val.clone() });
        }
        if !raw_new.is_empty() {
            raw_stream.append(&raw_new, |r| &r.ts)?;
        }
    }

    // Write contract rows (deduped by guid).
    let contract = vault.stream(DIR, Partition::Month);
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
    for (row, _) in &ledger_rows {
        if !row.guid.is_empty() && seen.insert(row.guid.clone()) {
            new_rows.push(row.clone());
        }
    }
    if !new_rows.is_empty() {
        contract.append(&new_rows, |r| &r.ts)?;
    }
    let written = new_rows.len() as u64;

    // Positions snapshot (raw-only).
    write_positions_snapshot(vault, &positions)?;

    // Update sync state.
    let mut state = vault.read_ib_sync();
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_ib_sync(&state)?;

    let headline = if written == 0 {
        "Interactive Brokers is up to date — no new records".to_string()
    } else {
        format!("Interactive Brokers synced — {written} new records")
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("transactions", written)]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-ib-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — constructed from the confirmed ibflex Types.py XML attribute names.

    /// A minimal Flex XML with one Trade, one CashTransaction (dividend),
    /// one CorporateAction, and one OpenPosition.
    fn sample_flex_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FlexQueryResponse queryName="Trove" type="AF">
  <FlexStatements count="1">
    <FlexStatement accountId="U1234567" fromDate="20260101" toDate="20260615">
      <Trades>
        <Trade
          transactionID="123456789"
          tradeID="987654321"
          accountId="U1234567"
          currency="USD"
          symbol="AAPL"
          assetCategory="STK"
          description="APPLE INC"
          tradeDate="2026-05-15"
          tradeTime="14:23:00"
          buySell="BUY"
          quantity="10"
          tradePrice="190.50"
          tradeMoney="1905.00"
          proceeds="-1905.00"
          netCash="-1906.50"
          ibCommission="-1.50"
          ibCommissionCurrency="USD"
          exchange="NASDAQ"
          isin="US0378331005"
          cusip="037833100"
          notes=""
        />
      </Trades>
      <CashTransactions>
        <CashTransaction
          transactionID="222333444"
          accountId="U1234567"
          currency="USD"
          symbol="AAPL"
          description="APPLE INC(US0378331005) CASH DIVIDEND 0.25000000 USD PER SHARE (Ordinary Dividend)"
          type="Dividends"
          amount="12.50"
          dateTime="2026-05-28;12:00:00"
          reportDate="2026-05-28"
          isin="US0378331005"
        />
      </CashTransactions>
      <CorporateActions>
        <CorporateAction
          actionID="999888777"
          accountId="U1234567"
          currency="USD"
          symbol="AAPL"
          description="APPLE INC(US0378331005) SPLIT 4 FOR 1"
          type="FS"
          amount="0"
          quantity="30"
          dateTime="2026-06-01;08:00:00"
          reportDate="2026-06-01"
          isin="US0378331005"
        />
      </CorporateActions>
      <OpenPositions>
        <OpenPosition
          accountId="U1234567"
          currency="USD"
          symbol="AAPL"
          assetCategory="STK"
          description="APPLE INC"
          reportDate="2026-06-15"
          position="40"
          markPrice="195.00"
          positionValue="7800.00"
          costBasisPrice="190.50"
          costBasisMoney="7620.00"
          fifoPnlUnrealized="180.00"
          percentOfNAV="42.50"
          side="Long"
        />
      </OpenPositions>
    </FlexStatement>
  </FlexStatements>
</FlexQueryResponse>"#
    }

    // -----------------------------------------------------------------------
    // XML parsing tests.

    #[test]
    fn parses_all_record_kinds_from_xml() {
        let records = parse_flex_xml(sample_flex_xml()).unwrap();
        assert_eq!(records.len(), 4, "should parse 1 Trade + 1 Cash + 1 Corp + 1 Position");

        let trade = records.iter().find(|r| r.kind == RecordKind::Trade).unwrap();
        assert_eq!(trade.get("symbol"), Some("AAPL"));
        assert_eq!(trade.get("transactionID"), Some("123456789"));
        assert_eq!(trade.get("buySell"), Some("BUY"));
        assert_eq!(trade.get("tradeDate"), Some("2026-05-15"));

        let cash = records.iter().find(|r| r.kind == RecordKind::CashTransaction).unwrap();
        assert_eq!(cash.get("type"), Some("Dividends"));
        assert_eq!(cash.get("transactionID"), Some("222333444"));
        assert_eq!(cash.get("amount"), Some("12.50"));

        let corp = records.iter().find(|r| r.kind == RecordKind::CorporateAction).unwrap();
        assert_eq!(corp.get("actionID"), Some("999888777"));
        assert_eq!(corp.get("type"), Some("FS"));

        let pos = records.iter().find(|r| r.kind == RecordKind::OpenPosition).unwrap();
        assert_eq!(pos.get("position"), Some("40"));
        assert_eq!(pos.get("markPrice"), Some("195.00"));
    }

    // -----------------------------------------------------------------------
    // Date parsing tests.

    #[test]
    fn parse_ibkr_date_formats() {
        // YYYY-MM-DD
        let t = parse_ibkr_date_to_local("2026-05-15").unwrap();
        assert!(t.starts_with("2026-05-15"));

        // YYYYMMDD
        let t = parse_ibkr_date_to_local("20260515").unwrap();
        assert!(t.starts_with("2026-05-15"));

        // dateTime with semicolon separator: date part extracted
        let t = parse_ibkr_date_to_local("2026-05-28;12:00:00").unwrap();
        assert!(t.starts_with("2026-05-28"));

        // Empty / zero
        assert!(parse_ibkr_date_to_local("").is_none());
        assert!(parse_ibkr_date_to_local("0").is_none());
    }

    // -----------------------------------------------------------------------
    // Line item mapping tests.

    #[test]
    fn maps_trade_to_line_item() {
        let records = parse_flex_xml(sample_flex_xml()).unwrap();
        let trade = records.iter().find(|r| r.kind == RecordKind::Trade).unwrap();
        let li = to_line_item(trade).unwrap();

        assert_eq!(li.source, SERVICE);
        assert_eq!(li.guid, "trade:123456789");
        assert_eq!(li.item, "Buy AAPL");
        assert_eq!(li.currency, "USD");
        assert_eq!(li.amount, Some(-1906.50)); // netCash
        assert_eq!(li.extra.get("buy_sell"), Some(&json!("BUY")));
        assert_eq!(li.extra.get("quantity"), Some(&json!(10.0)));
        assert_eq!(li.extra.get("trade_price"), Some(&json!(190.50)));
        assert_eq!(li.extra.get("commission"), Some(&json!(-1.50)));
        assert_eq!(li.extra.get("isin"), Some(&json!("US0378331005")));
        assert_eq!(li.extra.get("exchange"), Some(&json!("NASDAQ")));
        // ts should be the tradeDate as a local RFC3339
        assert!(li.ts.starts_with("2026-05-15"));
    }

    #[test]
    fn maps_cash_transaction_dividend_to_line_item() {
        let records = parse_flex_xml(sample_flex_xml()).unwrap();
        let cash = records.iter().find(|r| r.kind == RecordKind::CashTransaction).unwrap();
        let li = to_line_item(cash).unwrap();

        assert_eq!(li.guid, "cash:222333444");
        assert_eq!(li.item, "Dividend AAPL");
        assert_eq!(li.amount, Some(12.50));
        assert_eq!(li.currency, "USD");
        assert_eq!(li.extra.get("transaction_type"), Some(&json!("Dividends")));
        assert_eq!(li.extra.get("symbol"), Some(&json!("AAPL")));
        assert!(li.ts.starts_with("2026-05-28"));
    }

    #[test]
    fn maps_corporate_action_to_line_item() {
        let records = parse_flex_xml(sample_flex_xml()).unwrap();
        let corp = records.iter().find(|r| r.kind == RecordKind::CorporateAction).unwrap();
        let li = to_line_item(corp).unwrap();

        assert_eq!(li.guid, "corp:999888777");
        assert!(li.item.contains("AAPL"));
        assert_eq!(li.extra.get("action_type"), Some(&json!("FS")));
        assert_eq!(li.extra.get("quantity"), Some(&json!(30.0)));
        assert!(li.ts.starts_with("2026-06-01"));
    }

    #[test]
    fn open_position_does_not_map_to_line_item() {
        let records = parse_flex_xml(sample_flex_xml()).unwrap();
        let pos = records.iter().find(|r| r.kind == RecordKind::OpenPosition).unwrap();
        assert!(to_line_item(pos).is_none(), "OpenPosition must not produce a LineItem");
    }

    // -----------------------------------------------------------------------
    // Credential parsing tests.

    #[test]
    fn parse_credentials_splits_token_and_query_id() {
        let (tok, qid) = parse_credentials("abc123|456789").unwrap();
        assert_eq!(tok, "abc123");
        assert_eq!(qid, "456789");

        // Whitespace tolerance.
        let (tok2, qid2) = parse_credentials("  abc123 | 456789  ").unwrap();
        assert_eq!(tok2, "abc123");
        assert_eq!(qid2, "456789");

        // Missing pipe → None.
        assert!(parse_credentials("abc123").is_none());
        // Empty token → None.
        assert!(parse_credentials("|456789").is_none());
        // Empty qid → None.
        assert!(parse_credentials("abc123|").is_none());
    }

    // -----------------------------------------------------------------------
    // Poll response parsing tests.

    #[test]
    fn poll_response_detects_ready() {
        let ready = r#"<?xml version="1.0" encoding="UTF-8"?>
<FlexQueryResponse queryName="Test" type="AF">
  <FlexStatements count="0">
  </FlexStatements>
</FlexQueryResponse>"#;
        matches!(poll_response_kind(ready), PollResult::Ready(_));
    }

    #[test]
    fn poll_response_detects_not_ready_code_1019() {
        let not_ready = r#"<FlexStatementResponse timestamp="01 June, 2026 09:00 AM EDT">
  <Status>Warn</Status>
  <ErrorCode>1019</ErrorCode>
  <ErrorMessage>Statement generation in progress. Please try again shortly.</ErrorMessage>
</FlexStatementResponse>"#;
        matches!(poll_response_kind(not_ready), PollResult::NotReady);
    }

    #[test]
    fn poll_response_detects_error() {
        let err = r#"<FlexStatementResponse timestamp="01 June, 2026 09:00 AM EDT">
  <Status>Fail</Status>
  <ErrorCode>1003</ErrorCode>
  <ErrorMessage>Account not authorized for Flex Web Service.</ErrorMessage>
</FlexStatementResponse>"#;
        matches!(poll_response_kind(err), PollResult::Error(_));
    }

    // -----------------------------------------------------------------------
    // Send response parsing tests.

    #[test]
    fn parse_send_response_extracts_ref_code() {
        let xml = r#"<FlexStatementResponse timestamp="15 May, 2026 02:30 PM EDT">
  <Status>Success</Status>
  <ReferenceCode>REF12345</ReferenceCode>
  <Url>https://gdcdyn.interactivebrokers.com/Universal/servlet/FlexStatementService.GetStatement</Url>
</FlexStatementResponse>"#;
        let (code, url) = parse_send_response(xml).unwrap();
        assert_eq!(code, "REF12345");
        assert!(url.is_some());
    }

    #[test]
    fn parse_send_response_errors_on_failure() {
        let xml = r#"<FlexStatementResponse timestamp="15 May, 2026 02:30 PM EDT">
  <Status>Fail</Status>
  <ErrorCode>1003</ErrorCode>
  <ErrorMessage>Account not authorized for Flex Web Service.</ErrorMessage>
</FlexStatementResponse>"#;
        let err = parse_send_response(xml).unwrap_err().to_string();
        assert!(err.contains("not authorized"), "clear error: {err}");
    }

    // -----------------------------------------------------------------------
    // Full integration tests via process_xml (bypasses HTTP; tests vault writes).
    // The HTTP layer (fetch_flex_statement) is tested via the poll_response_kind
    // and parse_send_response unit tests above. process_xml is the right seam
    // for testing the vault write logic — the same pattern as schwab.rs's
    // `pull_with` over a mock SchwabApi.

    #[test]
    fn full_process_xml_writes_both_layers_and_positions() {
        let v = temp_vault("fullpull");

        let out = process_xml(&v, sample_flex_xml()).unwrap();
        // 3 contract-eligible records (Trade + CashTransaction + CorporateAction)
        assert_eq!(
            out.counts.get("transactions"),
            Some(&3),
            "should write 3 contract rows"
        );

        // Contract rows exist.
        let contract = v.stream(DIR, Partition::Month);
        let total: usize = contract
            .partitions()
            .unwrap()
            .into_iter()
            .map(|k| contract.read::<Value>(&k).unwrap().len())
            .sum();
        assert_eq!(total, 3, "3 contract rows on disk");

        // The trade row has the expected fields.
        let mut all: Vec<Value> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<Value>(&key).unwrap());
        }
        let trade_row = all.iter().find(|r| r["guid"] == "trade:123456789").unwrap();
        assert_eq!(trade_row["source"], SERVICE);
        assert_eq!(trade_row["amount"], json!(-1906.50));
        assert_eq!(trade_row["currency"], "USD");

        // Raw layer exists with records.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let raw_total: usize = raw
            .partitions()
            .unwrap()
            .into_iter()
            .map(|k| raw.read::<Value>(&k).unwrap().len())
            .sum();
        assert!(raw_total >= 3, "raw layer has at least 3 records, got {raw_total}");

        // Positions snapshot written.
        let today = Local::now().format("%Y-%m-%d").to_string();
        let pos_path = v.root().join(format!("{POS_DIR}/{today}.jsonl"));
        assert!(pos_path.exists(), "positions snapshot written");
        let pos_content = std::fs::read_to_string(&pos_path).unwrap();
        assert!(pos_content.contains("AAPL"), "position has symbol");
        assert!(pos_content.contains("40"), "position has quantity");
    }

    #[test]
    fn re_process_xml_dedupes_by_guid() {
        let v = temp_vault("dedup");
        let out1 = process_xml(&v, sample_flex_xml()).unwrap();
        assert_eq!(out1.counts.get("transactions"), Some(&3));

        // Second run with the same XML → all deduped.
        let out2 = process_xml(&v, sample_flex_xml()).unwrap();
        assert_eq!(out2.counts.get("transactions"), Some(&0), "second run all deduped");
    }

    // -----------------------------------------------------------------------
    // fetch_flex_statement via a mock API (covers send→poll→ready cycle).

    struct MockFlexApi {
        send_response: String,
        poll_responses: std::cell::RefCell<std::collections::VecDeque<String>>,
    }

    impl MockFlexApi {
        fn new_ready(xml: &str) -> Self {
            // A minimal valid SendRequest success response.
            let send = concat!(
                "<FlexStatementResponse>",
                "<Status>Success</Status>",
                "<ReferenceCode>TESTREF001</ReferenceCode>",
                "</FlexStatementResponse>"
            );
            let mut q = std::collections::VecDeque::new();
            q.push_back(xml.to_string());
            MockFlexApi {
                send_response: send.to_string(),
                poll_responses: std::cell::RefCell::new(q),
            }
        }

        fn new_retry_then_ready(xml: &str) -> Self {
            let send = concat!(
                "<FlexStatementResponse>",
                "<Status>Success</Status>",
                "<ReferenceCode>TESTREF002</ReferenceCode>",
                "</FlexStatementResponse>"
            );
            // Code 1019 = not ready; poll once more.
            let not_ready = concat!(
                "<FlexStatementResponse>",
                "<Status>Warn</Status>",
                "<ErrorCode>1019</ErrorCode>",
                "<ErrorMessage>Statement generation in progress.</ErrorMessage>",
                "</FlexStatementResponse>"
            );
            let mut q = std::collections::VecDeque::new();
            q.push_back(not_ready.to_string());
            q.push_back(xml.to_string());
            MockFlexApi {
                send_response: send.to_string(),
                poll_responses: std::cell::RefCell::new(q),
            }
        }
    }

    impl FlexApi for MockFlexApi {
        fn send_request(&self, _token: &str, _query_id: &str) -> Result<String> {
            Ok(self.send_response.clone())
        }
        fn get_statement(
            &self,
            _token: &str,
            _ref_code: &str,
            _url: Option<&str>,
        ) -> Result<String> {
            self.poll_responses
                .borrow_mut()
                .pop_front()
                .context("mock: no more poll responses queued")
        }
    }

    #[test]
    fn fetch_flex_statement_returns_xml_immediately_when_ready() {
        // fetch_flex_statement must return the XML body on the first poll.
        let api = MockFlexApi::new_ready(sample_flex_xml());
        let xml = fetch_flex_statement(&api, "token", "12345").unwrap();
        // The XML may include a <?xml ...?> declaration before <FlexQueryResponse.
        assert!(
            xml.contains("<FlexQueryResponse"),
            "returned XML should contain <FlexQueryResponse, got: {}",
            &xml[..xml.len().min(200)]
        );
    }

    #[test]
    fn fetch_flex_statement_retries_on_1019() {
        // First poll returns 1019 (not ready), second returns the XML.
        let api = MockFlexApi::new_retry_then_ready(sample_flex_xml());
        let xml = fetch_flex_statement(&api, "token", "12345").unwrap();
        assert!(xml.contains("<FlexQueryResponse"));
    }

    #[test]
    fn pull_needs_connection() {
        let v = temp_vault("noconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn connection_status_shows_query_id() {
        let v = temp_vault("status");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "MYTOKEN|1000456".into(),
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
        assert_eq!(status.accounts[0].label, "Query 1000456");
    }

    #[test]
    fn connection_token_paste_method_linked() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, SERVICE);
        assert_eq!(DEF.connection, Some(SERVICE));
    }

    #[test]
    fn def_behavior_is_periodic() {
        matches!(DEF.behavior, Behavior::Periodic { .. });
    }

    #[test]
    fn sync_state_deserializes_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.updated.is_none());

        let partial: SyncState =
            serde_json::from_str(r#"{"updated":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert_eq!(partial.updated.as_deref(), Some("2026-06-01T00:00:00-07:00"));

        // Back-compat: old cursor files that contained `last_trade_date` still
        // deserialize successfully (serde ignores unknown fields by default).
        let old_format: SyncState = serde_json::from_str(
            r#"{"updated":"2026-06-01T00:00:00-07:00","last_trade_date":"2026-05-31"}"#
        ).unwrap();
        assert_eq!(old_format.updated.as_deref(), Some("2026-06-01T00:00:00-07:00"));
    }

    #[test]
    fn poll_response_detects_not_ready_code_1009_server_busy() {
        let server_busy = r#"<FlexStatementResponse timestamp="01 June, 2026 09:00 AM EDT">
  <Status>Warn</Status>
  <ErrorCode>1009</ErrorCode>
  <ErrorMessage>Server busy. Please try again in a moment.</ErrorMessage>
</FlexStatementResponse>"#;
        assert!(
            matches!(poll_response_kind(server_busy), PollResult::NotReady),
            "error code 1009 (server busy) must be treated as NotReady, not Error"
        );
    }

    #[test]
    fn corporate_action_guid_uses_transaction_id_when_present() {
        // A CorporateAction with transactionID set — guid should prefer it over actionID.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FlexQueryResponse queryName="Test" type="AF">
  <FlexStatements count="1">
    <FlexStatement accountId="U1234567">
      <CorporateActions>
        <CorporateAction
          transactionID="TXN555"
          actionID="ACT888"
          accountId="U1234567"
          currency="USD"
          symbol="XYZ"
          type="TC"
          amount="0"
          quantity="10"
          dateTime="2026-06-10;08:00:00"
        />
      </CorporateActions>
    </FlexStatement>
  </FlexStatements>
</FlexQueryResponse>"#;
        let records = parse_flex_xml(xml).unwrap();
        let corp = records.iter().find(|r| r.kind == RecordKind::CorporateAction).unwrap();
        // Should use transactionID first.
        assert_eq!(corp.guid(), Some("corp:TXN555".to_string()));
        let li = to_line_item(corp).unwrap();
        assert_eq!(li.guid, "corp:TXN555");
    }

    #[test]
    fn corporate_action_guid_falls_back_to_action_id_when_no_transaction_id() {
        // The sample_flex_xml fixture has actionID only — confirm fallback still works.
        let records = parse_flex_xml(sample_flex_xml()).unwrap();
        let corp = records.iter().find(|r| r.kind == RecordKind::CorporateAction).unwrap();
        assert_eq!(corp.guid(), Some("corp:999888777".to_string()));
    }

    #[test]
    fn raw_layer_deduped_on_second_pull() {
        // Verify that re-running process_xml does NOT grow the raw layer.
        let v = temp_vault("rawdedup");
        let out1 = process_xml(&v, sample_flex_xml()).unwrap();
        assert_eq!(out1.counts.get("transactions"), Some(&3));

        // Count raw records after first pull.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let count1: usize = raw
            .partitions()
            .unwrap()
            .into_iter()
            .map(|k| raw.read::<Value>(&k).unwrap().len())
            .sum();
        assert!(count1 >= 3, "raw has records after first pull");

        // Second pull with identical XML — raw must not grow.
        let out2 = process_xml(&v, sample_flex_xml()).unwrap();
        assert_eq!(out2.counts.get("transactions"), Some(&0), "contract deduped");

        let count2: usize = raw
            .partitions()
            .unwrap()
            .into_iter()
            .map(|k| raw.read::<Value>(&k).unwrap().len())
            .sum();
        assert_eq!(count2, count1, "raw layer must not grow on repeat pull: before={count1} after={count2}");
    }

    #[test]
    fn raw_records_carry_guid_tag() {
        // Every raw record (that has a guid) must have _guid set so dedup works.
        let v = temp_vault("rawguid");
        process_xml(&v, sample_flex_xml()).unwrap();

        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut all: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            all.extend(raw.read::<Value>(&key).unwrap());
        }
        for record in &all {
            let guid = record.get("_guid");
            assert!(
                guid.is_some(),
                "_guid tag must be present on every raw record; missing from: {record}"
            );
            // _guid must be a non-empty string (not null for guidable records).
            if let Some(v) = guid {
                if v.is_string() {
                    assert!(!v.as_str().unwrap().is_empty(), "_guid must not be empty string");
                }
            }
        }
    }

    #[test]
    fn sell_trade_has_correct_item_label() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FlexQueryResponse queryName="Test" type="AF">
  <FlexStatements count="1">
    <FlexStatement accountId="U1234567">
      <Trades>
        <Trade
          transactionID="999"
          accountId="U1234567"
          currency="USD"
          symbol="MSFT"
          assetCategory="STK"
          tradeDate="2026-06-10"
          buySell="SELL"
          quantity="-5"
          tradePrice="420.00"
          tradeMoney="-2100.00"
          proceeds="2100.00"
          netCash="2098.50"
          ibCommission="-1.50"
        />
      </Trades>
    </FlexStatement>
  </FlexStatements>
</FlexQueryResponse>"#;
        let records = parse_flex_xml(xml).unwrap();
        let trade = records.iter().find(|r| r.kind == RecordKind::Trade).unwrap();
        let li = to_line_item(trade).unwrap();
        assert_eq!(li.item, "Sell MSFT");
        assert_eq!(li.amount, Some(2098.50)); // netCash for a sell is positive
    }

    #[test]
    fn line_item_serializes_without_empty_optionals() {
        let records = parse_flex_xml(sample_flex_xml()).unwrap();
        let trade = records.iter().find(|r| r.kind == RecordKind::Trade).unwrap();
        let li = to_line_item(trade).unwrap();
        let v = serde_json::to_value(&li).unwrap();
        // Required fields present.
        assert!(v.get("ts").is_some());
        assert!(v.get("source").is_some());
        assert!(v.get("guid").is_some());
        assert!(v.get("merchant").is_some());
        // Empty-omit fields absent.
        assert!(v.get("order_id").is_none(), "empty order_id should be omitted");
    }
}

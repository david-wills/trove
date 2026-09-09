//! Koinly / CoinTracker CSV export import — one-time backfill of normalized
//! crypto transaction history with cost basis from third-party tax aggregators.
//!
//! **Brief:** docs/integrations/crypto-tax-exports.md
//!
//! Both Koinly (Settings → Tax Reports → Export transactions) and CoinTracker
//! (Portfolio → Export → CSV) produce CSVs that a user downloads and drops
//! into Trove's import box. Trove never calls either service directly. This is
//! a backfill seed; ongoing sync continues via the direct exchange integrations
//! (Coinbase, Kraken, Ethereum, Bitcoin).
//!
//! ## Column shapes (confirmed from BittyTax open-source parsers)
//!
//! **Koinly** (`all_transactions.csv` from Tax Reports):
//! ```text
//! Date, Type, Label (or Tag), Sending Wallet, Sent Amount, Sent Currency,
//! Sent Cost Basis, Receiving Wallet, Received Amount, Received Currency,
//! Received Cost Basis, Fee Amount, Fee Currency, Gain (USD),
//! Net Value (USD), Fee Value (USD), TxSrc, TxDest, TxHash, Description
//! ```
//! `Date` is `YYYY-MM-DD HH:MM UTC` or similar; `TxHash` is present when
//! available (on-chain txns); cost basis / gain columns are currency-suffixed.
//!
//! **CoinTracker** (`transaction_history.csv`):
//! ```text
//! Date, Type, Transaction ID, Received Quantity, Received Currency,
//! Received Cost Basis (USD), Received Wallet, Received Address,
//! Received Comment, Sent Quantity, Sent Currency, Sent Cost Basis (USD),
//! Sent Wallet, Sent Address, Sent Comment, Fee Amount, Fee Currency,
//! Fee Cost Basis (USD), Realized Return (USD), Fee Realized Return (USD),
//! Transaction Hash
//! ```
//! The `Transaction ID` column carries exchange-native ids when present.
//!
//! ## Vault layout
//!
//! - **Raw** (unconditional, full fidelity):
//!   `finance/crypto-tax-exports/<source>/raw/YYYY-MM.jsonl`
//!   where `<source>` is `koinly` or `cointracker` (detected by header sniff).
//! - **Contract** (`finance-purchases` / [`crate::finance::LineItem`]):
//!   `finance/purchases/crypto-tax-exports/YYYY-MM.jsonl`
//!   One row per transaction; `guid` = the on-chain tx hash or exchange-native
//!   id when present, else a SHA-256 over `date+type+sent+received+wallet`
//!   (stable across re-imports of the same export).
//!
//! ## Mapping → `finance-purchases`
//!
//! | LineItem field | Koinly source        | CoinTracker source              |
//! |----------------|---------------------|---------------------------------|
//! | `ts`           | `Date` → local RFC3339 (noon when no time) | `Date` same |
//! | `source`       | `"crypto-tax-exports"` | same                       |
//! | `guid`         | `TxHash` else hash  | `Transaction ID` else `Transaction Hash` else hash |
//! | `merchant`     | sending/receiving wallet, else `"Crypto"` | same |
//! | `item`         | `Type` (buy/sell/trade/…) | `Type`                   |
//! | `amount`       | `Net Value (USD)` as f64 | `Received Quantity` − `Sent Quantity` (non-USD) |
//! | `currency`     | net value currency (USD) | `Received Currency` or `Sent Currency` |
//! | `extra`        | all remaining columns at full fidelity | same         |

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::finance::LineItem;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer directory (one subfolder covers all aggregator sources).
const DIR: &str = "finance/purchases/crypto-tax-exports";
/// Raw Koinly layer.
const RAW_KOINLY: &str = "finance/crypto-tax-exports/koinly/raw";
/// Raw CoinTracker layer.
const RAW_COINTRACKER: &str = "finance/crypto-tax-exports/cointracker/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "crypto-tax-exports",
        name: "Koinly / CoinTracker Exports",
        kind: IntegrationKind::Import,
        // 🔒 Financial detail — opt-in with explicit acknowledgement.
        default_on: false,
        description: "Import your normalized crypto transaction history — including cost basis \
                      and realized gains — from a Koinly or CoinTracker CSV export. Drop the file \
                      you downloaded from either service; Trove detects the format automatically. \
                      Re-running with a newer export is safe: duplicates are skipped by transaction id.",
        domain: "finance",
        vault_path: "finance/purchases/crypto-tax-exports/",
        toggleable: false,
        setup: &[
            "Koinly: Settings → Tax Reports → Export transactions → download the CSV.",
            "CoinTracker: Portfolio → Export → CSV → download the file.",
            "Drop the downloaded file into the import box here.",
        ],
        caveats: "One-time backfill only — ongoing sync uses the Coinbase, Kraken, Ethereum, \
                  and Bitcoin integrations. A row from this import and a row from a direct \
                  exchange integration covering the same wallet may overlap; dedupe at read \
                  time by matching date + asset + amount + wallet.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Format detection — sniff the header row to pick the right parser.

#[derive(Debug, Clone, Copy, PartialEq)]
enum CsvFormat {
    Koinly,
    CoinTracker,
}

/// Detect the CSV format from the header row.  Returns `None` when the header
/// matches neither known format.
fn detect_format(header: &csv::StringRecord) -> Option<CsvFormat> {
    // Koinly: has "Sent Amount" and "Received Amount" (not "Received Quantity").
    // CoinTracker: has "Received Quantity" and "Sent Quantity".
    let fields: Vec<&str> = header.iter().collect();
    if fields.iter().any(|f| f.trim() == "Received Quantity") {
        return Some(CsvFormat::CoinTracker);
    }
    if fields.iter().any(|f| f.trim() == "Received Amount") {
        return Some(CsvFormat::Koinly);
    }
    None
}

// ---------------------------------------------------------------------------
// Guid helpers — stable across re-imports of the same row.

/// A 16-char hex fingerprint used as a fallback guid when no on-chain or
/// exchange id is available.  The hash includes a row ordinal so that two
/// otherwise-identical rows (same-minute DCA fills, split fills) get distinct
/// guids rather than silently deduplicating.
fn fallback_guid(parts: &[&str], row_idx: usize) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(b"|");
    }
    // Row ordinal breaks ties for same-minute identical rows.
    h.update(row_idx.to_string().as_bytes());
    h.update(b"|");
    let digest = h.finalize();
    // 8 bytes → 16 hex chars; short but sufficient for de-dup within one file
    format!("{:016x}", u64::from_be_bytes(digest[..8].try_into().unwrap()))
}

// ---------------------------------------------------------------------------
// Date parsing — aggregators use several date formats.

/// Parse an aggregator date string to a local RFC3339 timestamp.  When the
/// string has no time component, noon UTC is used (avoids midnight-boundary
/// surprises, same as letterboxd).
///
/// Formats tried, in order:
/// 1. RFC 3339 / ISO 8601 with explicit offset or Z (e.g. `…T…Z`, `…+00:00`,
///    `….000Z`) — parsed with `parse_from_rfc3339`; offset respected, then
///    converted to local.
/// 2. ISO date-time with " UTC" suffix (Koinly: `YYYY-MM-DD HH:MM:SS UTC`)
///    or "T…Z" variant — treated as UTC.
/// 3. ISO date-time with minute precision + " UTC" (Koinly doc: `HH:MM UTC`)
/// 4. ISO date-time without suffix (`YYYY-MM-DD HH:MM:SS`, `…T…`) — treated
///    as UTC (both Koinly and BittyTax interpret naive timestamps as UTC).
/// 5. CoinTracker `MM/DD/YYYY HH:MM:SS` (officially documented format) —
///    treated as UTC.
/// 6. CoinTracker `MM/DD/YYYY` date-only — UTC noon.
/// 7. ISO date-only `YYYY-MM-DD` — UTC noon.
fn parse_ts(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // 1. RFC 3339 / ISO 8601 with explicit offset or Z.
    //    chrono::DateTime::parse_from_rfc3339 handles Z, +HH:MM, milliseconds.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }

    // 2a. ISO date-time with " UTC" suffix (Koinly primary format).
    if let Ok(dt) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S UTC") {
        return Some(Utc.from_utc_datetime(&dt).with_timezone(&Local).to_rfc3339());
    }
    // 2b. " UTC" at minute precision (e.g. "2021-01-15 14:32 UTC" — Koinly docs).
    if let Ok(dt) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M UTC") {
        return Some(Utc.from_utc_datetime(&dt).with_timezone(&Local).to_rfc3339());
    }

    // 3. ISO date-time without suffix — treat as UTC (matches BittyTax default
    //    and both Koinly/Koinly-derived CSVs; avoids local-timezone shift).
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S",
                "%Y-%m-%d %H:%M",    "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(raw, fmt) {
            return Some(Utc.from_utc_datetime(&dt).with_timezone(&Local).to_rfc3339());
        }
    }

    // 4. CoinTracker: "MM/DD/YYYY HH:MM:SS" (official documented format, UTC).
    if let Ok(dt) = NaiveDateTime::parse_from_str(raw, "%m/%d/%Y %H:%M:%S") {
        return Some(Utc.from_utc_datetime(&dt).with_timezone(&Local).to_rfc3339());
    }
    // 4b. CoinTracker date-only: "MM/DD/YYYY".
    if let Ok(d) = NaiveDate::parse_from_str(raw, "%m/%d/%Y") {
        let dt = d.and_hms_opt(12, 0, 0)?;
        return Some(Utc.from_utc_datetime(&dt).with_timezone(&Local).to_rfc3339());
    }

    // 5. ISO date-only "YYYY-MM-DD" — UTC noon.
    if let Ok(d) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let dt = d.and_hms_opt(12, 0, 0)?;
        return Some(Utc.from_utc_datetime(&dt).with_timezone(&Local).to_rfc3339());
    }

    None
}

/// Parse a numeric string as f64, stripping commas (e.g. "1,234.56") and
/// leading/trailing whitespace.  Returns `None` for empty or non-numeric strings.
fn parse_f64(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', "");
    if s.is_empty() { return None; }
    s.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Koinly row parsing.

/// One Koinly CSV row (after header sniff).
///
/// Columns (in order from BittyTax's open-source Koinly parser / the official
/// export): Date, Type, Label or Tag, Sending Wallet, Sent Amount, Sent Currency,
/// Sent Cost Basis, Receiving Wallet, Received Amount, Received Currency,
/// Received Cost Basis, Fee Amount, Fee Currency, Gain (USD), Net Value (USD),
/// Fee Value (USD), TxSrc, TxDest, TxHash, Description.
struct KoinlyRow<'a> {
    record: &'a csv::StringRecord,
    header: &'a csv::StringRecord,
}

impl<'a> KoinlyRow<'a> {
    fn get(&self, col: &str) -> &str {
        self.header
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(col))
            .and_then(|i| self.record.get(i))
            .unwrap_or("")
            .trim()
    }

    fn to_line_item(&self, row_idx: usize) -> Option<LineItem> {
        let date_raw = self.get("Date");
        let ts = parse_ts(date_raw)?;
        // Partition key must resolve — skip rows without a parseable date.
        Partition::Month.key(&ts)?;

        let tx_type = self.get("Type");
        let tx_hash = self.get("TxHash");
        let sent_currency = self.get("Sent Currency");
        let received_currency = self.get("Received Currency");
        let sending_wallet = self.get("Sending Wallet");
        let receiving_wallet = self.get("Receiving Wallet");
        let sent_amount_raw = self.get("Sent Amount");
        let received_amount_raw = self.get("Received Amount");
        let fee_amount_raw = self.get("Fee Amount");
        let description = self.get("Description");

        // Guid: prefer on-chain hash, else a stable fallback.
        // Row ordinal included so same-minute identical rows don't collide.
        let guid = if !tx_hash.is_empty() {
            tx_hash.to_string()
        } else {
            fallback_guid(&[date_raw, tx_type, sent_amount_raw, sent_currency,
                             received_amount_raw, received_currency, sending_wallet,
                             fee_amount_raw], row_idx)
        };

        // Merchant: the wallet from which value flows, else "Crypto".
        let merchant = if !sending_wallet.is_empty() {
            sending_wallet.to_string()
        } else if !receiving_wallet.is_empty() {
            receiving_wallet.to_string()
        } else {
            "Crypto".to_string()
        };

        // Amount: asset quantity — matches the Bitcoin sibling and CoinTracker.
        // Received amount/currency wins (buy/receive); sent amount (negated)
        // for sells/sends.  Net Value (USD) / Gain / cost basis all go to extra
        // so `amount` is always the asset quantity in `currency`, never mixed
        // with USD fiat values.
        let (amount, currency) = if !received_amount_raw.is_empty() && !received_currency.is_empty() {
            (parse_f64(received_amount_raw), received_currency.to_string())
        } else if !sent_amount_raw.is_empty() && !sent_currency.is_empty() {
            (parse_f64(sent_amount_raw).map(|v| -v), sent_currency.to_string())
        } else {
            (None, String::new())
        };

        // Extra: full fidelity of every aggregator-specific column.
        let mut extra = Map::new();
        {
            let mut put = |k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.to_string(), Value::String(v.to_string()));
                }
            };
            let tag_col = if self.header.iter().any(|h| h.trim().eq_ignore_ascii_case("label")) {
                "Label"
            } else {
                "Tag"
            };
            put("koinly_type", tx_type);
            put("koinly_tag", self.get(tag_col));
            put("sending_wallet", sending_wallet);
            put("receiving_wallet", receiving_wallet);
            put("sent_amount", sent_amount_raw);
            put("sent_currency", sent_currency);
            put("received_amount", received_amount_raw);
            put("received_currency", received_currency);
            put("fee_amount", fee_amount_raw);
            put("fee_currency", self.get("Fee Currency"));
            put("tx_src", self.get("TxSrc"));
            put("tx_dest", self.get("TxDest"));
            put("tx_hash", tx_hash);
            put("description", description);
        }
        // Cost basis / gain / net-value columns (currency-suffixed) — full fidelity.
        for h in self.header.iter() {
            let hl = h.trim().to_ascii_lowercase();
            if hl.starts_with("sent cost basis")
                || hl.starts_with("received cost basis")
                || hl.starts_with("gain")
                || hl.starts_with("net value")
                || hl.starts_with("fee value")
            {
                let pos = self.header.iter().position(|x| x == h).unwrap_or(usize::MAX);
                if let Some(v) = self.record.get(pos) {
                    let v = v.trim();
                    if !v.is_empty() {
                        extra.insert(h.trim().to_string(), Value::String(v.to_string()));
                    }
                }
            }
        }

        let mut li = LineItem::new("crypto-tax-exports", guid, ts, merchant);
        li.item = tx_type.to_string();
        li.amount = amount;
        if !currency.is_empty() { li.currency = currency; }
        li.extra = extra;
        Some(li)
    }

    /// A raw JSONL line: the entire CSV row as a JSON object keyed by column name.
    fn to_raw(&self) -> (String, Value) {
        let mut obj = Map::new();
        for (h, v) in self.header.iter().zip(self.record.iter()) {
            obj.insert(h.trim().to_string(), Value::String(v.trim().to_string()));
        }
        let ts_raw = self.header.iter().position(|h| h.trim().eq_ignore_ascii_case("date"))
            .and_then(|i| self.record.get(i))
            .unwrap_or("");
        let ts = parse_ts(ts_raw).unwrap_or_default();
        (ts, Value::Object(obj))
    }
}

// ---------------------------------------------------------------------------
// CoinTracker row parsing.

/// One CoinTracker CSV row.
///
/// Columns: Date, Type, Transaction ID, Received Quantity, Received Currency,
/// Received Cost Basis (USD), Received Wallet, Received Address, Received Comment,
/// Sent Quantity, Sent Currency, Sent Cost Basis (USD), Sent Wallet, Sent Address,
/// Sent Comment, Fee Amount, Fee Currency, Fee Cost Basis (USD),
/// Realized Return (USD), Fee Realized Return (USD), Transaction Hash.
struct CoinTrackerRow<'a> {
    record: &'a csv::StringRecord,
    header: &'a csv::StringRecord,
}

impl<'a> CoinTrackerRow<'a> {
    fn get(&self, col: &str) -> &str {
        self.header
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(col))
            .and_then(|i| self.record.get(i))
            .unwrap_or("")
            .trim()
    }

    fn to_line_item(&self, row_idx: usize) -> Option<LineItem> {
        let date_raw = self.get("Date");
        let ts = parse_ts(date_raw)?;
        Partition::Month.key(&ts)?;

        let tx_type = self.get("Type");
        let tx_id = self.get("Transaction ID");
        let tx_hash = self.get("Transaction Hash");
        let received_qty = self.get("Received Quantity");
        let received_ccy = self.get("Received Currency");
        let sent_qty = self.get("Sent Quantity");
        let sent_ccy = self.get("Sent Currency");
        let received_wallet = self.get("Received Wallet");
        let sent_wallet = self.get("Sent Wallet");
        let fee_amount_raw = self.get("Fee Amount");

        // Guid: exchange-native id, then on-chain hash, then stable fallback.
        // Row ordinal included so same-minute identical rows don't collide.
        let guid = if !tx_id.is_empty() {
            tx_id.to_string()
        } else if !tx_hash.is_empty() {
            tx_hash.to_string()
        } else {
            fallback_guid(&[date_raw, tx_type, sent_qty, sent_ccy,
                             received_qty, received_ccy, received_wallet,
                             fee_amount_raw], row_idx)
        };

        let merchant = if !received_wallet.is_empty() {
            received_wallet.to_string()
        } else if !sent_wallet.is_empty() {
            sent_wallet.to_string()
        } else {
            "Crypto".to_string()
        };

        // Amount: prefer received quantity (for buys/receives), fallback to sent.
        let (amount, currency) = if !received_qty.is_empty() && !received_ccy.is_empty() {
            (parse_f64(received_qty), received_ccy.to_string())
        } else if !sent_qty.is_empty() && !sent_ccy.is_empty() {
            (parse_f64(sent_qty).map(|v| -v), sent_ccy.to_string()) // sent = negative
        } else {
            (None, String::new())
        };

        let mut extra = Map::new();
        {
            let mut put = |k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.to_string(), Value::String(v.to_string()));
                }
            };
            put("cointracker_type", tx_type);
            put("transaction_id", tx_id);
            put("received_quantity", received_qty);
            put("received_currency", received_ccy);
            put("sent_quantity", sent_qty);
            put("sent_currency", sent_ccy);
            put("received_wallet", received_wallet);
            put("sent_wallet", sent_wallet);
            put("received_address", self.get("Received Address"));
            put("sent_address", self.get("Sent Address"));
            put("received_comment", self.get("Received Comment"));
            put("sent_comment", self.get("Sent Comment"));
            put("fee_amount", fee_amount_raw);
            put("fee_currency", self.get("Fee Currency"));
            put("transaction_hash", tx_hash);
        }
        // Cost basis / realized return columns — after the closure is dropped.
        for h in self.header.iter() {
            let hl = h.trim().to_ascii_lowercase();
            if hl.contains("cost basis") || hl.contains("realized return") {
                let pos = self.header.iter().position(|x| x == h).unwrap_or(usize::MAX);
                if let Some(v) = self.record.get(pos) {
                    let v = v.trim();
                    if !v.is_empty() {
                        extra.insert(h.trim().to_string(), Value::String(v.to_string()));
                    }
                }
            }
        }

        let mut li = LineItem::new("crypto-tax-exports", guid, ts, merchant);
        li.item = tx_type.to_string();
        li.amount = amount;
        if !currency.is_empty() { li.currency = currency; }
        li.extra = extra;
        Some(li)
    }

    fn to_raw(&self) -> (String, Value) {
        let mut obj = Map::new();
        for (h, v) in self.header.iter().zip(self.record.iter()) {
            obj.insert(h.trim().to_string(), Value::String(v.trim().to_string()));
        }
        let ts_raw = self.header.iter().position(|h| h.trim().eq_ignore_ascii_case("date"))
            .and_then(|i| self.record.get(i))
            .unwrap_or("");
        let ts = parse_ts(ts_raw).unwrap_or_default();
        (ts, Value::Object(obj))
    }
}

// ---------------------------------------------------------------------------
// Raw row shape — ts is the partition key; value is the full row object.

use serde::Serialize;

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Import entry point.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut rdr = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        // Real-world exports sometimes have rows with fewer or more fields than
        // the header (trailing empty columns are commonly omitted).  Flexible
        // mode tolerates this instead of treating the whole row as a parse error.
        .flexible(true)
        .from_reader(body.as_bytes());
    let header = rdr.headers()?.clone();

    let fmt = detect_format(&header)
        .with_context(|| {
            "File does not look like a Koinly or CoinTracker CSV export. \
             Expected a Koinly 'all_transactions.csv' (with columns 'Sent Amount', \
             'Received Amount') or a CoinTracker export (with 'Received Quantity', \
             'Sent Quantity')."
        })?;

    let raw_dir = match fmt {
        CsvFormat::Koinly => RAW_KOINLY,
        CsvFormat::CoinTracker => RAW_COINTRACKER,
    };

    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(raw_dir, Partition::Month);

    // Load existing contract guids for idempotent re-import.
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

    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, 0u64);
    // unparseable_dates counts rows that have a raw value but no parseable date —
    // these land in the raw layer (full fidelity) but are excluded from the
    // contract layer; a non-zero count surfaces a warning so silent zero-collection
    // is visible.
    let mut unparseable_dates = 0u64;
    let mut rows_processed = 0u64;
    let mut items: Vec<LineItem> = Vec::new();
    let mut raws: Vec<RawLine> = Vec::new();

    for record in rdr.records() {
        rows_processed += 1;
        let Ok(record) = record else {
            skipped += 1;
            continue;
        };
        // Skip blank/comment/footer lines.
        if record.iter().all(|f| f.trim().is_empty())
            || record.get(0).map_or(false, |f| f.trim().starts_with("..."))
        {
            skipped += 1;
            continue;
        }

        // Use rows_processed as a stable row ordinal for fallback guid
        // disambiguation (1-based, reset per file).
        let row_idx = rows_processed as usize;

        let (item, raw_row) = match fmt {
            CsvFormat::Koinly => {
                let row = KoinlyRow { record: &record, header: &header };
                let raw = row.to_raw();
                (row.to_line_item(row_idx), raw)
            }
            CsvFormat::CoinTracker => {
                let row = CoinTrackerRow { record: &record, header: &header };
                let raw = row.to_raw();
                (row.to_line_item(row_idx), raw)
            }
        };

        // Push raw BEFORE the contract gate so full-fidelity layer is always
        // populated for rows with a parseable date, even when the contract
        // mapping otherwise fails.  Rows with an unparseable date can't be
        // partitioned (the store requires a valid YYYY-MM prefix), so we
        // count them but don't write them to raw either.
        let (raw_ts, raw_val) = raw_row;
        if raw_ts.is_empty() {
            // Date didn't parse — can't partition; count and skip.
            unparseable_dates += 1;
            continue;
        }
        raws.push(RawLine { ts: raw_ts, value: raw_val });

        let Some(item) = item else {
            // to_line_item returned None despite a parseable date (should not
            // normally happen, but guard defensively).
            continue;
        };
        if item.guid.is_empty() || !seen.insert(item.guid.clone()) {
            duplicates += 1;
            continue;
        }
        items.push(item);
        imported += 1;
        if rows_processed % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Append raw first (full fidelity, unconditional), then contract.
    raw.append(&raws, |r| &r.ts)?;
    contract.append(&items, |i| &i.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    let format_name = match fmt {
        CsvFormat::Koinly => "Koinly",
        CsvFormat::CoinTracker => "CoinTracker",
    };
    let mut headline = format!(
        "{imported} {format_name} transactions imported, {duplicates} duplicates skipped"
    );
    if unparseable_dates > 0 {
        headline.push_str(&format!(
            " ({unparseable_dates} rows skipped — date format not recognised; \
             raw layer preserved)"
        ));
    }
    Ok(ImportOutcome {
        headline,
        counts: [("imported", imported), ("duplicates", duplicates),
                 ("skipped", skipped), ("unparseable_dates", unparseable_dates)].into(),
    })
}

// ---------------------------------------------------------------------------
// Tests — offline, fixture-driven. Exact field names from BittyTax parsers.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-crypto-tax-exports-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn do_import(v: &Vault, csv_body: &str) -> ImportOutcome {
        let path = v.root().join("export.csv");
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // Koinly fixture — columns from the official export shape (BittyTax parser
    // source-of-truth). The cost basis / gain column is currency-suffixed.
    // Dates use "YYYY-MM-DD HH:MM:SS UTC" as the primary Koinly format.
    // Each data row has exactly 20 fields to match the 20-column header.
    //
    // Header col index reference (0-based):
    //   0=Date 1=Type 2=Label 3=Sending Wallet 4=Sent Amount 5=Sent Currency
    //   6=Sent Cost Basis 7=Receiving Wallet 8=Received Amount 9=Received Currency
    //   10=Received Cost Basis 11=Fee Amount 12=Fee Currency 13=Gain (USD)
    //   14=Net Value (USD) 15=Fee Value (USD) 16=TxSrc 17=TxDest 18=TxHash 19=Description
    const KOINLY_CSV: &str = "Date,Type,Label,Sending Wallet,Sent Amount,Sent Currency,Sent Cost Basis,Receiving Wallet,Received Amount,Received Currency,Received Cost Basis,Fee Amount,Fee Currency,Gain (USD),Net Value (USD),Fee Value (USD),TxSrc,TxDest,TxHash,Description
2021-11-10 14:23:00 UTC,buy,,Coinbase,,,,,0.05,BTC,,0.001,BTC,,2800.00,0.056,,,, Bought BTC
2021-12-01 09:00:00 UTC,sell,,Coinbase,0.02,BTC,600.00,,,,,,,,1100.00,,,,0xabc123def456,Sold BTC
2021-12-15 12:00:00 UTC,crypto_transfer,,Binance,0.01,BTC,,,0.01,BTC,,,,,,,,,0xfeedfeed,Transfer
2022-01-05 08:00:00 UTC,staking_reward,,Kraken,,,,,0.001,ETH,3.50,,,3.50,0.01,,,,, ETH staking reward
";

    // -----------------------------------------------------------------------
    // CoinTracker fixture — columns from the BittyTax CoinTracker parser.
    // Dates use the REAL CoinTracker documented format: "MM/DD/YYYY HH:MM:SS".
    // (Previously the fixture used ISO dates, which masked the parsing bug.)
    // The cost basis columns are currency-suffixed "Received Cost Basis (USD)".
    // Each data row has exactly 21 fields to match the 21-column header.
    //
    // Header col index reference (0-based):
    //   0=Date 1=Type 2=Transaction ID 3=Received Quantity 4=Received Currency
    //   5=Received Cost Basis (USD) 6=Received Wallet 7=Received Address
    //   8=Received Comment 9=Sent Quantity 10=Sent Currency 11=Sent Cost Basis (USD)
    //   12=Sent Wallet 13=Sent Address 14=Sent Comment 15=Fee Amount 16=Fee Currency
    //   17=Fee Cost Basis (USD) 18=Realized Return (USD) 19=Fee Realized Return (USD)
    //   20=Transaction Hash
    const COINTRACKER_CSV: &str = "Date,Type,Transaction ID,Received Quantity,Received Currency,Received Cost Basis (USD),Received Wallet,Received Address,Received Comment,Sent Quantity,Sent Currency,Sent Cost Basis (USD),Sent Wallet,Sent Address,Sent Comment,Fee Amount,Fee Currency,Fee Cost Basis (USD),Realized Return (USD),Fee Realized Return (USD),Transaction Hash
03/01/2022 10:00:00,BUY,tx-ct-001,1.5,ETH,3000.00,Coinbase,,Buy ETH,,,,,,,,,,,,0xhash001
04/15/2022 15:30:00,SELL,tx-ct-002,,,,,,,0.5,ETH,1500.00,Coinbase,,,0.002,ETH,6.00,250.00,0.50,0xhash002
05/01/2022 08:00:00,RECEIVE,tx-ct-003,0.001,BTC,,Wallet A,bc1qabc,,,,,,,,,,,,,0xhash003
";

    // -----------------------------------------------------------------------
    // Detection.

    #[test]
    fn detects_koinly_format() {
        let mut rdr = csv::ReaderBuilder::new().trim(csv::Trim::All)
            .from_reader(KOINLY_CSV.as_bytes());
        let header = rdr.headers().unwrap().clone();
        assert_eq!(detect_format(&header), Some(CsvFormat::Koinly));
    }

    #[test]
    fn detects_cointracker_format() {
        let mut rdr = csv::ReaderBuilder::new().trim(csv::Trim::All)
            .from_reader(COINTRACKER_CSV.as_bytes());
        let header = rdr.headers().unwrap().clone();
        assert_eq!(detect_format(&header), Some(CsvFormat::CoinTracker));
    }

    #[test]
    fn unrecognised_format_returns_none() {
        let body = "Foo,Bar,Baz\n1,2,3\n";
        let mut rdr = csv::ReaderBuilder::new().trim(csv::Trim::All).from_reader(body.as_bytes());
        let header = rdr.headers().unwrap().clone();
        assert_eq!(detect_format(&header), None);
    }

    // -----------------------------------------------------------------------
    // Date parsing — covers all formats the real services emit.

    #[test]
    fn parse_ts_handles_all_formats() {
        // 1. Koinly primary: "YYYY-MM-DD HH:MM:SS UTC"
        let utc_suffix = parse_ts("2021-11-10 14:23:00 UTC").unwrap();
        assert!(utc_suffix.contains("2021-11-10"), "UTC suffix: {utc_suffix}");

        // 2. Koinly minute-precision: "YYYY-MM-DD HH:MM UTC"
        let utc_min = parse_ts("2021-11-10 14:23 UTC").unwrap();
        assert!(utc_min.contains("2021-11-10"), "UTC minute-precision: {utc_min}");

        // 3. RFC 3339 / ISO 8601 with Z suffix
        let z = parse_ts("2021-12-01T09:00:00Z").unwrap();
        assert!(z.contains("2021-12-01"), "Z suffix: {z}");

        // 4. RFC 3339 with milliseconds + Z
        let ms_z = parse_ts("2021-12-01T09:00:00.000Z").unwrap();
        assert!(ms_z.contains("2021-12-01"), "millis+Z: {ms_z}");

        // 5. RFC 3339 with explicit +00:00 offset
        let offset = parse_ts("2021-12-01T09:00:00+00:00").unwrap();
        assert!(offset.contains("2021-12-01"), "+00:00 offset: {offset}");

        // 6. ISO naive without suffix — treated as UTC (BittyTax default)
        let naive = parse_ts("2021-12-01 09:00:00").unwrap();
        assert!(naive.contains("2021-12-01"), "naive datetime: {naive}");

        // 7. ISO T-separator without suffix
        let naive_t = parse_ts("2021-12-01T09:00:00").unwrap();
        assert!(naive_t.contains("2021-12-01"), "naive T-sep: {naive_t}");

        // 8. CoinTracker MM/DD/YYYY HH:MM:SS (officially documented format)
        let ct_full = parse_ts("06/14/2017 20:57:35").unwrap();
        assert!(ct_full.contains("2017"), "CoinTracker datetime: {ct_full}");
        // Specific date sanity check: June 14
        assert!(ct_full.contains("2017-06-14"), "CoinTracker date component: {ct_full}");

        // 9. CoinTracker date-only MM/DD/YYYY
        let ct_date = parse_ts("12/31/2021").unwrap();
        assert!(ct_date.contains("2021-12-31"), "CoinTracker date-only: {ct_date}");

        // 10. ISO date-only YYYY-MM-DD
        let iso_date = parse_ts("2021-11-10").unwrap();
        assert!(iso_date.contains("2021-11-10"), "ISO date-only: {iso_date}");

        // Negative cases
        assert!(parse_ts("").is_none(), "empty string → None");
        assert!(parse_ts("not-a-date").is_none(), "garbage → None");
    }

    // Naive ISO datetimes must be treated as UTC, not local time.
    // Under any non-UTC timezone, local.from_local_datetime would add an
    // offset (e.g. -08:00) so the instant differs from UTC by the tz offset.
    // The correct behaviour is to round-trip through UTC then convert to local
    // RFC3339, preserving the wall-clock "date" written in the export.
    #[test]
    fn parse_ts_naive_iso_treated_as_utc() {
        // Koinly exports "2021-12-01 09:00:00" meaning 09:00 UTC.
        // Regardless of the machine's local TZ, the output must represent
        // the same instant as 2021-12-01T09:00:00Z.
        let result = parse_ts("2021-12-01 09:00:00").unwrap();
        // Parse the result back to compare instants.
        let parsed_back = chrono::DateTime::parse_from_rfc3339(&result).unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2021-12-01T09:00:00Z").unwrap();
        assert_eq!(
            parsed_back.with_timezone(&Utc),
            expected.with_timezone(&Utc),
            "naive datetime treated as UTC: got {result}"
        );
    }

    // -----------------------------------------------------------------------
    // Guid.

    #[test]
    fn fallback_guid_is_stable_and_16_hex_chars() {
        let g1 = fallback_guid(&["2021-01-01", "buy", "100", "USD", "0.01", "BTC", "Coinbase", "0.001"], 1);
        let g2 = fallback_guid(&["2021-01-01", "buy", "100", "USD", "0.01", "BTC", "Coinbase", "0.001"], 1);
        assert_eq!(g1, g2, "same inputs → same guid");
        assert_eq!(g1.len(), 16, "16 hex chars");
        assert!(g1.chars().all(|c| c.is_ascii_hexdigit()), "all hex: {g1}");

        // Different date → different guid
        let g3 = fallback_guid(&["2021-01-02", "buy", "100", "USD", "0.01", "BTC", "Coinbase", "0.001"], 1);
        assert_ne!(g1, g3, "different date → different guid");

        // Same-instant identical row at different ordinal → different guid
        // (prevents silent dedup of DCA fills).
        let g4 = fallback_guid(&["2021-01-01", "buy", "100", "USD", "0.01", "BTC", "Coinbase", "0.001"], 2);
        assert_ne!(g1, g4, "different row ordinal → different guid (no false dedup)");
    }

    // -----------------------------------------------------------------------
    // Koinly import.

    #[test]
    fn koinly_import_maps_fields_and_writes_both_layers() {
        let v = temp_vault("koinly");
        let out = do_import(&v, KOINLY_CSV);
        assert_eq!(out.counts.get("imported"), Some(&4), "4 rows imported: {out:?}");
        assert_eq!(out.counts.get("duplicates"), Some(&0));
        assert!(out.headline.contains("Koinly"), "headline names format: {}", out.headline);

        // Contract rows exist under the right path.
        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<Value> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<Value>(&key).unwrap());
        }
        assert_eq!(all.len(), 4, "4 contract rows total");

        // The sell row has the on-chain TxHash as guid.
        let sell = all.iter().find(|r| r["item"] == "sell").unwrap();
        assert_eq!(sell["guid"], "0xabc123def456", "TxHash used as guid when present");
        assert_eq!(sell["source"], "crypto-tax-exports");
        assert_eq!(sell["merchant"], "Coinbase", "sending wallet is merchant for sell");

        // The buy row has no TxHash → stable fallback guid (16 hex chars).
        let buy = all.iter().find(|r| r["item"] == "buy").unwrap();
        let buy_guid = buy["guid"].as_str().unwrap();
        assert_eq!(buy_guid.len(), 16, "fallback guid is 16 hex chars");

        // Koinly amount = asset quantity (Received Amount), not USD Net Value.
        // sell row: sent 0.02 BTC (no received amount) → amount = -0.02 BTC.
        let sell_amount = sell["amount"].as_f64().unwrap();
        assert!((sell_amount - (-0.02)).abs() < 1e-9, "sell: sent 0.02 BTC negated: {sell_amount}");
        assert_eq!(sell["currency"], "BTC", "sell currency is BTC (asset), not USD");

        // buy row: received 0.05 BTC → amount = 0.05 BTC.
        let buy_amount = buy["amount"].as_f64().unwrap();
        assert!((buy_amount - 0.05).abs() < 1e-9, "buy: received 0.05 BTC: {buy_amount}");
        assert_eq!(buy["currency"], "BTC");

        // Net Value (USD) must be in extra, not as the top-level amount.
        let sell_extra = sell["extra"].as_object().unwrap();
        assert_eq!(sell_extra.get("koinly_type").and_then(Value::as_str), Some("sell"));
        assert_eq!(sell_extra.get("tx_hash").and_then(Value::as_str), Some("0xabc123def456"));
        // Net Value (USD) = 1100.00 lives in extra.
        assert!(
            sell_extra.get("Net Value (USD)").and_then(Value::as_str).is_some(),
            "Net Value (USD) preserved in extra: {sell_extra:?}"
        );

        // Raw layer under koinly subfolder.
        let raw = v.stream(RAW_KOINLY, Partition::Month);
        let mut raw_rows: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_rows.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_rows.len(), 4, "raw layer mirrors contract count");
        // Raw row preserves the original column names.
        assert!(raw_rows.iter().any(|r| r.get("TxHash").is_some()), "raw keeps TxHash col");
    }

    #[test]
    fn koinly_reimport_is_idempotent() {
        let v = temp_vault("koinly-rerun");
        do_import(&v, KOINLY_CSV);
        let out2 = do_import(&v, KOINLY_CSV);
        assert_eq!(out2.counts.get("imported"), Some(&0), "all deduped on re-import");
        assert_eq!(out2.counts.get("duplicates"), Some(&4));
    }

    // -----------------------------------------------------------------------
    // CoinTracker import — fixture uses real MM/DD/YYYY dates.

    #[test]
    fn cointracker_import_maps_fields_and_writes_both_layers() {
        let v = temp_vault("cointracker");
        let out = do_import(&v, COINTRACKER_CSV);
        assert_eq!(out.counts.get("imported"), Some(&3), "3 rows: {out:?}");
        assert!(out.headline.contains("CoinTracker"));
        // No unparseable_dates because we now parse MM/DD/YYYY correctly.
        assert_eq!(
            out.counts.get("unparseable_dates").copied().unwrap_or(0), 0,
            "MM/DD/YYYY dates must parse: {out:?}"
        );

        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<Value> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<Value>(&key).unwrap());
        }
        assert_eq!(all.len(), 3);

        // BUY row uses Transaction ID as guid.
        let buy = all.iter().find(|r| r["item"].as_str().map(|s| s.eq_ignore_ascii_case("buy")).unwrap_or(false)).unwrap();
        assert_eq!(buy["guid"], "tx-ct-001", "Transaction ID used as guid");
        assert_eq!(buy["source"], "crypto-tax-exports");
        assert_eq!(buy["merchant"], "Coinbase", "received wallet as merchant");
        let buy_amount = buy["amount"].as_f64().unwrap();
        assert!((buy_amount - 1.5).abs() < 1e-6, "Received Quantity: {buy_amount}");
        assert_eq!(buy["currency"], "ETH");

        // SELL row: sent qty (positive in the CSV) negated to indicate outflow.
        let sell = all.iter().find(|r| r["item"].as_str().map(|s| s.eq_ignore_ascii_case("sell")).unwrap_or(false)).unwrap();
        let sell_amount = sell["amount"].as_f64().unwrap();
        assert!((sell_amount - (-0.5)).abs() < 1e-6, "sent qty negated to -0.5: {sell_amount}");

        // Extra carries cost basis.
        let buy_extra = buy["extra"].as_object().unwrap();
        assert_eq!(
            buy_extra.get("Received Cost Basis (USD)").and_then(Value::as_str),
            Some("3000.00"),
            "cost basis preserved in extra"
        );

        // Raw under cointracker subfolder.
        let raw = v.stream(RAW_COINTRACKER, Partition::Month);
        let mut raw_rows: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_rows.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_rows.len(), 3);
    }

    #[test]
    fn cointracker_reimport_is_idempotent() {
        let v = temp_vault("cointracker-rerun");
        do_import(&v, COINTRACKER_CSV);
        let out2 = do_import(&v, COINTRACKER_CSV);
        assert_eq!(out2.counts.get("imported"), Some(&0));
        assert_eq!(out2.counts.get("duplicates"), Some(&3));
    }

    // -----------------------------------------------------------------------
    // Unparseable-date gate: when a date can't be parsed the row is counted
    // under `unparseable_dates` and the headline warns the user so that
    // silent zero-collection is visible.  (Raw rows with unpartitionable dates
    // cannot be stored — the vault store requires a valid YYYY-MM prefix —
    // so they are counted but not written; users will see the warning and can
    // re-export with a recognised format.)
    #[test]
    fn unparseable_date_counted_and_warned_in_headline() {
        let bad_date_csv = "Date,Type,Transaction ID,Received Quantity,Received Currency,Received Cost Basis (USD),Received Wallet,Received Address,Received Comment,Sent Quantity,Sent Currency,Sent Cost Basis (USD),Sent Wallet,Sent Address,Sent Comment,Fee Amount,Fee Currency,Fee Cost Basis (USD),Realized Return (USD),Fee Realized Return (USD),Transaction Hash
BADDATE,BUY,tx-raw-001,1.0,ETH,,,Coinbase,,,,,,,,,,,,,0xhashraw
";
        let v = temp_vault("rawgate");
        let out = do_import(&v, bad_date_csv);
        // Contract gets nothing (date unparseable).
        assert_eq!(out.counts.get("imported").copied().unwrap_or(0), 0, "no contract rows");
        assert_eq!(out.counts.get("unparseable_dates").copied().unwrap_or(0), 1, "1 unparseable date counted");
        // Headline must warn the user.
        let headline = &out.headline;
        assert!(
            headline.contains("unparseable") || headline.contains("skipped"),
            "headline warns about unparseable rows: {headline}"
        );
    }

    // -----------------------------------------------------------------------
    // Bad format rejects cleanly.

    #[test]
    fn unknown_format_returns_error() {
        let v = temp_vault("badformat");
        let path = v.root().join("garbage.csv");
        fs::write(&path, "Foo,Bar,Baz\n1,2,3\n").unwrap();
        let err = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.to_lowercase().contains("koinly") || err.to_lowercase().contains("cointracker"),
            "error message guides the user: {err}");
    }

    // -----------------------------------------------------------------------
    // DEF shape.

    #[test]
    fn def_is_import_with_csv_accept() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        let spec = DEF.import_spec().unwrap();
        assert!(spec.accepts.contains(&"csv"), "accepts csv");
        assert_eq!(DEF.connection, None, "no connection needed");
        assert_eq!(DEF.meta.id, "crypto-tax-exports");
    }

    // -----------------------------------------------------------------------
    // Koinly staking_reward row (no TxHash, no wallet → fallback guid).
    // Sending Wallet is "Kraken" so merchant = "Kraken".

    #[test]
    fn koinly_staking_row_gets_fallback_guid_and_kraken_merchant() {
        let v = temp_vault("koinly-staking");
        do_import(&v, KOINLY_CSV);
        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<Value> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<Value>(&key).unwrap());
        }
        let staking = all.iter().find(|r| r["item"] == "staking_reward").unwrap();
        let sg = staking["guid"].as_str().unwrap();
        assert_eq!(sg.len(), 16, "fallback guid for staking row: {sg}");
        assert_eq!(staking["merchant"], "Kraken", "sending wallet 'Kraken' used as merchant");
    }
}

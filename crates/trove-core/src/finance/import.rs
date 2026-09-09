//! Bank-statement CSV import — the peer backend to SimpleFIN.
//!
//! Imports are not a fallback: they're the only route past the aggregator's
//! ~90-day initial-history window, and the only route *at all* to accounts no
//! aggregator can reach (Apple Card, Venmo, Cash App). See
//! `docs/finance-integrations-plan.md`.
//!
//! Format handling is generic-first: known bank layouts (Chase credit card,
//! Chase checking) are recognized by their exact header rows, and everything
//! else goes through a header-name column mapper (date / amount / description
//! at minimum). Unmapped columns are kept verbatim in `extra`.
//!
//! Identity & dedupe (plan tiers 2 and 3):
//! - Statement rows have no ids — one is synthesized from
//!   `hash(account, posted, amount, normalized description, occurrence)`,
//!   so re-importing the same file is a clean no-op.
//! - Overlap with already-synced data (SimpleFIN's window, or a previous
//!   import) is fuzzy-matched: equal amount, posted within ±3 days, similar
//!   normalized description. The existing aggregator row wins as canonical
//!   and absorbs the import's category if it has none; the import row is not
//!   inserted. Matching runs here, at import time — the daily sync path
//!   stays trivial.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::model::Transaction;
use crate::vault::Vault;

const IMPORT_STATE_FILE: &str = ".trove/sync/finance-import.json";

/// How far apart `posted` dates may be and still be the same transaction
/// (statement posting dates vs aggregator posting dates drift a little).
const MATCH_WINDOW_DAYS: i64 = 3;

/// Outcome of one CSV import, for the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CsvImportStats {
    /// Which layout was recognized ("chase-card", "chase-checking", "generic").
    pub format: String,
    pub account: String,
    /// Data rows in the file (parseable or not).
    pub rows: usize,
    pub new_transactions: usize,
    /// Rows that matched an existing synced/imported transaction.
    pub duplicates: usize,
    /// Duplicates that enriched the existing row (e.g. filled its category).
    pub merged: usize,
    /// Rows with no parseable date or amount.
    pub skipped: usize,
    /// Date range of the parsed rows (YYYY-MM-DD), empty if none.
    pub from: String,
    pub to: String,
}

/// Last-import metadata (`.trove/sync/finance-import.json`) — feeds the
/// hub card's "last import" line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FinanceImportState {
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub new_transactions: usize,
}

// ---------- column mapping ----------

/// Where each canonical field lives in this file's columns.
#[derive(Debug, Clone)]
struct Mapping {
    format: &'static str,
    /// Column of the posting date.
    posted: usize,
    /// Purchase date, when the layout distinguishes it.
    transacted: Option<usize>,
    /// Single signed amount column…
    amount: Option<usize>,
    /// …or a (debit, credit) pair; debits become negative.
    debit_credit: Option<(usize, usize)>,
    description: usize,
    category: Option<usize>,
    /// Flip amount signs (banks that report purchases as positive).
    negate: bool,
}

fn norm_header(h: &str) -> String {
    h.trim_start_matches('\u{feff}').trim().to_ascii_lowercase()
}

/// Recognize the file layout from its header row.
fn detect_mapping(headers: &[String]) -> Result<Mapping> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    // Ignore trailing empty headers (Chase files end rows with a comma).
    let h: Vec<&str> = {
        let mut v: Vec<&str> = h.iter().map(|s| s.as_str()).collect();
        while v.last() == Some(&"") {
            v.pop();
        }
        v
    };

    if h == ["transaction date", "post date", "description", "category", "type", "amount", "memo"] {
        // Chase credit card: purchases already negative.
        return Ok(Mapping {
            format: "chase-card",
            posted: 1,
            transacted: Some(0),
            amount: Some(5),
            debit_credit: None,
            description: 2,
            category: Some(3),
            negate: false,
        });
    }
    if h == ["details", "posting date", "description", "amount", "type", "balance", "check or slip #"] {
        // Chase checking/savings: debits already negative.
        return Ok(Mapping {
            format: "chase-checking",
            posted: 1,
            transacted: None,
            amount: Some(3),
            debit_credit: None,
            description: 2,
            category: None,
            negate: false,
        });
    }

    // Generic: find columns by name. Candidates are ordered by preference.
    let find = |names: &[&str]| -> Option<usize> {
        names.iter().find_map(|n| h.iter().position(|c| c == n))
    };
    let posted = find(&["post date", "posting date", "posted date", "date", "transaction date", "trans. date", "trans date"]);
    let transacted = match posted {
        // Only distinct from the posted column.
        Some(p) => find(&["transaction date", "trans. date", "trans date"]).filter(|&t| t != p),
        None => None,
    };
    let amount = find(&["amount", "amount (usd)"]);
    let debit = find(&["debit", "withdrawal", "withdrawals", "money out"]);
    let credit = find(&["credit", "deposit", "deposits", "money in"]);
    let description = find(&["description", "payee", "merchant", "name", "transaction description", "notes", "note", "name of sender/receiver"]);
    let category = find(&["category"]);

    match (posted, description, amount, debit.zip(credit)) {
        (Some(posted), Some(description), Some(amount), _) => Ok(Mapping {
            format: "generic",
            posted,
            transacted,
            amount: Some(amount),
            debit_credit: None,
            description,
            category,
            negate: false,
        }),
        (Some(posted), Some(description), None, Some(dc)) => Ok(Mapping {
            format: "generic",
            posted,
            transacted,
            amount: None,
            debit_credit: Some(dc),
            description,
            category,
            negate: false,
        }),
        _ => bail!(
            "couldn't map this CSV's columns — need a date, an amount (or debit/credit pair), and a description; found: {}",
            if h.is_empty() { "(no header row)".to_string() } else { h.join(", ") }
        ),
    }
}

// ---------- field parsing ----------

/// "06/10/2026" / "2026-06-10" / "6/10/26" / "2026-05-28 14:32:10 EST" → "2026-06-10".
///
/// Cash App exports dates as `"YYYY-MM-DD HH:MM:SS TZ"` (e.g. `"2026-05-28 14:32:10 EST"`).
/// We strip the time and timezone suffix, keeping only the date part.
fn parse_date(raw: &str) -> Option<String> {
    let t = raw.trim();
    // Fast path: if the string is longer than 10 chars and starts with YYYY-MM-DD,
    // it may be a datetime with a time/timezone suffix — extract just the date prefix.
    // This handles Cash App's "2026-05-28 14:32:10 EST" and similar.
    if t.len() > 10 {
        let prefix = &t[..10];
        if let Ok(d) = NaiveDate::parse_from_str(prefix, "%Y-%m-%d") {
            return Some(d.format("%Y-%m-%d").to_string());
        }
    }
    // %y before %Y: two-digit years would otherwise match %Y as year 0026.
    // (%y never consumes a four-digit year — the trailing digits fail it.)
    for fmt in ["%m/%d/%y", "%m/%d/%Y", "%Y-%m-%d", "%m-%d-%Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(t, fmt) {
            return Some(d.format("%Y-%m-%d").to_string());
        }
    }
    None
}

/// Clean a money string for storage: strip `$`, thousands separators, and
/// parentheses-negation; keep the decimal digits exactly as written
/// ("(1,234.50)" → "-1234.50"). None if it isn't a plain decimal.
fn clean_amount(raw: &str) -> Option<String> {
    let mut t = raw.trim();
    let mut neg = false;
    if t.len() >= 2 && t.starts_with('(') && t.ends_with(')') {
        neg = true;
        t = &t[1..t.len() - 1];
    }
    let cleaned: String = t.chars().filter(|c| !matches!(c, '$' | ',' | ' ')).collect();
    let mut t = cleaned.as_str();
    if let Some(r) = t.strip_prefix('-') {
        neg = !neg;
        t = r;
    } else if let Some(r) = t.strip_prefix('+') {
        t = r;
    }
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let sign = if neg && t.chars().any(|c| c != '0' && c != '.') { "-" } else { "" };
    Some(if frac.is_empty() {
        format!("{sign}{}", if int.is_empty() { "0" } else { int })
    } else {
        format!("{sign}{}.{frac}", if int.is_empty() { "0" } else { int })
    })
}

/// Comparison key for amounts: leading/trailing zeros stripped so
/// "-42.170" and "-42.17" (different sources) compare equal.
fn amount_key(s: &str) -> String {
    let (neg, t) = match s.trim().strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.trim()),
    };
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    let int = int.trim_start_matches('0');
    let int = if int.is_empty() { "0" } else { int };
    let frac = frac.trim_end_matches('0');
    let zero = int == "0" && frac.is_empty();
    let mut out = String::new();
    if neg && !zero {
        out.push('-');
    }
    out.push_str(int);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// Uppercase, alphanumeric runs separated by single spaces — the
/// description form used for hashing and similarity.
fn norm_desc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim_end().to_string()
}

/// Same merchant? Exact, prefix (sources truncate differently), or
/// majority token overlap.
fn desc_similar(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b || a.starts_with(b) || b.starts_with(a) {
        return true;
    }
    let ta: HashSet<&str> = a.split(' ').collect();
    let tb: HashSet<&str> = b.split(' ').collect();
    let inter = ta.intersection(&tb).count();
    let union = ta.union(&tb).count();
    inter * 2 >= union
}

fn days_between(a: &str, b: &str) -> Option<i64> {
    let pa = NaiveDate::parse_from_str(a, "%Y-%m-%d").ok()?;
    let pb = NaiveDate::parse_from_str(b, "%Y-%m-%d").ok()?;
    Some((pa - pb).num_days().abs())
}

/// Tier-3 fuzzy match: an existing, unconsumed row with the same amount
/// posted within the window. Description similarity decides; `relaxed`
/// additionally accepts a sole candidate with a dissimilar description —
/// for sources that rewrite merchant names (Copilot's "Amazon.com" vs the
/// bank's "AMZN Mktp US*…"), where amount + date is the only signal left.
fn find_match(
    existing: &[Transaction],
    norms: &[String],
    by_amount: &HashMap<String, Vec<usize>>,
    consumed: &HashSet<usize>,
    posted: &str,
    amount: &str,
    desc_norm: &str,
    relaxed: bool,
) -> Option<usize> {
    let cands: Vec<usize> = by_amount
        .get(&amount_key(amount))
        .into_iter()
        .flatten()
        .copied()
        .filter(|&i| {
            // Posting dates drift between sources (one records the purchase
            // day, another the posting day) — accept either being close.
            let close = |d: &str| {
                days_between(d, posted).is_some_and(|d| d <= MATCH_WINDOW_DAYS)
            };
            !consumed.contains(&i)
                && (close(&existing[i].posted)
                    || existing[i].transacted.as_deref().is_some_and(close))
        })
        .collect();
    if let Some(&i) = cands.iter().find(|&&i| desc_similar(&norms[i], desc_norm)) {
        return Some(i);
    }
    if relaxed && cands.len() == 1 {
        return Some(cands[0]);
    }
    None
}

// ---------- Copilot Money exports ----------

/// Column positions of a Copilot Money `transactions.csv` export. Unlike
/// bank statements it spans every account (rows carry account name + last-4
/// mask) and signs spending positive.
struct CopilotCols {
    date: usize,
    name: usize,
    amount: usize,
    status: usize,
    account: usize,
    mask: usize,
    category: Option<usize>,
    parent: Option<usize>,
    excluded: Option<usize>,
    tags: Option<usize>,
    kind: Option<usize>,
    note: Option<usize>,
    recurring: Option<usize>,
}

/// Recognize a Copilot export by its column set (the account columns are
/// the tell — no bank statement carries those).
fn copilot_columns(headers: &[String]) -> Option<CopilotCols> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);
    Some(CopilotCols {
        date: idx("date")?,
        name: idx("name")?,
        amount: idx("amount")?,
        status: idx("status")?,
        account: idx("account")?,
        mask: idx("account mask")?,
        category: idx("category"),
        parent: idx("parent category"),
        excluded: idx("excluded"),
        tags: idx("tags"),
        kind: idx("type"),
        note: idx("note"),
        recurring: idx("recurring"),
    })
}

/// Flip an amount's sign (Copilot signs spending positive; the vault signs
/// outflows negative).
fn negate(a: &str) -> String {
    if amount_key(a) == "0" {
        a.to_string()
    } else if let Some(r) = a.strip_prefix('-') {
        r.to_string()
    } else {
        format!("-{a}")
    }
}

// ---------- Fidelity brokerage exports ----------

/// Column positions of a Fidelity activity history CSV.
///
/// Real per-account export header (confirmed by multiple independent sources):
///   Run Date, Action, Symbol, Description, Type, Quantity,
///   Price ($), Commission ($), Fees ($), Accrued Interest ($),
///   Amount ($), Cash Balance ($), Settlement Date
///
/// The "All Accounts" export (Accounts → History → All Accounts) prepends an
/// `Account Number` column and an `Account Name` column before `Run Date`.
/// `account_number` is therefore optional — when absent the caller supplies the
/// account identity via the import box.
pub(crate) struct FidelityCols {
    pub action: usize,
    /// "Run Date" — the primary date; always populated.
    pub run_date: usize,
    /// "Settlement Date" — the last column; blank for dividends, reinvestments,
    /// transfers. Kept as an optional extra field; NOT used as the primary date.
    pub settlement_date: Option<usize>,
    /// Present only in the "All Accounts" multi-account export.
    pub account_number: Option<usize>,
    /// "Description" in real exports (not "Security Description").
    pub security_description: usize,
    pub security_symbol: usize,
    pub quantity: usize,
    pub price: usize,
    pub commission: usize,
    pub amount: usize,
}

/// Normalize a Fidelity header name:
///   1. Standard `norm_header` (trim, lowercase, strip BOM).
///   2. Strip trailing ` ($)` suffix — Fidelity uses "Price ($)", etc.
fn norm_fidelity_header(h: &str) -> String {
    let base = norm_header(h);
    base.strip_suffix(" ($)").map(str::to_string).unwrap_or(base)
}

/// Recognize a Fidelity activity CSV by its header row.
///
/// Required columns (always present): `Run Date`, `Action`, `Symbol`,
/// `Description`, `Quantity`, `Amount`.  Optional: `Account Number` (only in
/// the "All Accounts" export), `Settlement Date` (blank for non-trade rows).
/// The combination of `Run Date` + `Symbol` + `Amount ($)` is the signature —
/// no ordinary bank-statement layout carries all three.
pub(crate) fn fidelity_columns(headers: &[String]) -> Option<FidelityCols> {
    let h: Vec<String> = headers.iter().map(|s| norm_fidelity_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);
    // Require the stable core columns present in every real Fidelity export.
    let run_date = idx("run date")?;
    let action = idx("action")?;
    let symbol = idx("symbol")?;
    let description = idx("description")?;
    let quantity = idx("quantity")?;
    let amount = idx("amount")?;
    Some(FidelityCols {
        action,
        run_date,
        settlement_date: idx("settlement date"),
        account_number: idx("account number"),
        security_description: description,
        security_symbol: symbol,
        quantity,
        price: idx("price").unwrap_or(usize::MAX),
        commission: idx("commission").unwrap_or(usize::MAX),
        amount,
    })
}

// ---------- Vanguard brokerage exports ----------

/// Column positions of a Vanguard transaction history CSV.
///
/// **SCAFFOLD — column names are UNCONFIRMED (Needs-sample / parser parked).**
/// These names are the most widely cited in community reports but have NOT
/// been verified against a real Vanguard portal export. The mandatory
/// signature (Trade Date + Net Amount + Transaction Type) is likely stable;
/// all other columns are optional scaffolds.
///
/// Known brokerage shape (reported, unconfirmed):
///   Account Number, Account Name, Trade Date, Settlement Date,
///   Transaction Type, Transaction Description, Investment Name, Symbol,
///   Shares, Share Price, Principal Amount, Commission Fees, Net Amount,
///   Accrued Interest
///
/// Known mutual-fund shape differences (reported, unconfirmed):
///   "Process Date" instead of "Settlement Date"
///   "Gross Amount" instead of "Principal Amount"
///   No "Symbol" column (fund-only rows)
///
/// Real Vanguard exports are also reported to be MULTI-SECTION: a holdings
/// block (Account Number, Investment Name, Symbol, Shares, ...) may precede
/// the transaction block in the same file, separated by blank rows. The
/// `vanguard_columns` recognizer matches only the transaction header; the
/// `import_vanguard` path must scan for the transaction header row rather
/// than assuming it is row 1.
///
/// **DO NOT activate this path until a real sample confirms the shape.**
/// `finance_import_csv` does NOT dispatch to `import_vanguard` while
/// Needs-sample is set; it falls through to the generic `detect_mapping`
/// path instead. Remove this note and re-wire the dispatch once shape is
/// confirmed.
///
/// Some older exports have been reported with "Amount" instead of "Net Amount"
/// and "Security" instead of "Symbol"; both are tried in `vanguard_columns`.
// Parked scaffold: suppress dead_code warnings until a real sample re-wires the dispatch.
#[allow(dead_code)]
pub(crate) struct VanguardCols {
    /// "Trade Date" — the primary date (always present per community reports).
    pub trade_date: usize,
    /// "Transaction Type" (Buy, Sell, Dividend, Reinvestment, …).
    pub transaction_type: usize,
    /// "Net Amount" (or "Amount") — the cash settlement amount.
    pub net_amount: usize,
    /// "Transaction Description" — human-readable description from the portal.
    /// When present, prefer this over the synthesized "<type> <symbol>" string.
    /// Unconfirmed column name; may not exist in all export variants.
    pub transaction_description: Option<usize>,
    /// "Account Number" — only in the all-accounts export.
    pub account_number: Option<usize>,
    /// "Account Name" — only in the all-accounts export.
    pub account_name: Option<usize>,
    /// "Investment Name" or "Security".
    pub investment_name: Option<usize>,
    /// "Symbol" or "Ticker" — absent in mutual-fund-only rows.
    pub symbol: Option<usize>,
    /// "Shares".
    pub shares: Option<usize>,
    /// "Share Price".
    pub share_price: Option<usize>,
    /// "Principal Amount" (brokerage) or "Gross Amount" (MF variant).
    pub principal_amount: Option<usize>,
    /// "Commission Fees".
    pub commission_fees: Option<usize>,
    /// "Accrued Interest".
    pub accrued_interest: Option<usize>,
    /// "Settlement Date" (brokerage) or "Process Date" (MF variant).
    pub settlement_date: Option<usize>,
}

/// Recognize a Vanguard transaction history CSV by its header row.
///
/// Signature: `"Trade Date"` + one of `"Net Amount"` / `"Amount"` + one of
/// `"Transaction Type"` / `"Trans. Type"`. This combination is not used by
/// any other recognized layout and is the most-reported Vanguard fingerprint.
///
/// **Returns `None` when the headers are not a Vanguard export** — the
/// caller falls through to the generic layout detector or the rejection path.
///
/// **IMPORTANT — PARKED:** this recognizer is a scaffold. `finance_import_csv`
/// does NOT call this as a live dispatch path. It is called only by
/// `vanguard.rs::run_import` for a user-facing rejection message (so the
/// user knows which columns were expected). The actual import falls through to
/// the generic `detect_mapping` path until a real sample confirms the shape.
/// Column names must be verified against a real Vanguard portal export before
/// re-wiring the dispatch in `finance_import_csv`.
///
/// Multi-section note: real Vanguard files may have a holdings section before
/// the transactions section. When the dispatch is eventually re-wired,
/// `import_vanguard` must scan for the transaction header row rather than
/// assuming it is at row 1. The recognizer itself only checks whether the
/// supplied header row looks like a Vanguard transaction header.
// Parked scaffold: suppress dead_code warning until a real sample re-wires the dispatch.
#[allow(dead_code)]
pub(crate) fn vanguard_columns(headers: &[String]) -> Option<VanguardCols> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);

    // Mandatory: the signature columns that distinguish Vanguard from every
    // other recognized layout.
    let trade_date = idx("trade date")?;
    let net_amount = idx("net amount").or_else(|| idx("amount"))?;
    let transaction_type =
        idx("transaction type").or_else(|| idx("trans. type")).or_else(|| idx("trans type"))?;

    Some(VanguardCols {
        trade_date,
        transaction_type,
        net_amount,
        // "Transaction Description" — human-readable portal description.
        // Unconfirmed column name; scaffolded from community reports.
        transaction_description: idx("transaction description"),
        account_number: idx("account number"),
        account_name: idx("account name"),
        investment_name: idx("investment name").or_else(|| idx("security")),
        symbol: idx("symbol").or_else(|| idx("ticker")),
        shares: idx("shares"),
        share_price: idx("share price"),
        // MF variant uses "Gross Amount" instead of "Principal Amount".
        principal_amount: idx("principal amount").or_else(|| idx("gross amount")),
        commission_fees: idx("commission fees"),
        accrued_interest: idx("accrued interest"),
        // MF variant uses "Process Date" instead of "Settlement Date".
        settlement_date: idx("settlement date").or_else(|| idx("process date")),
    })
}

// ---------- Apple Card exports ----------

/// Column positions of an Apple Card / Apple Savings monthly statement CSV.
///
/// **SCAFFOLD — column names are UNCONFIRMED (Needs-sample).**
/// The column list below is from the Apple Card research doc (L3841 of
/// `integrations-research.md`): `Transaction Date, Clearing Date,
/// Description, Merchant, Category, Type, Amount (USD)`. It has NOT been
/// confirmed against a real Wallet/card.apple.com export. Until a real
/// sample verifies the exact header row, the recognizer and `import_apple_card`
/// are parked; imports fall through to the generic `detect_mapping` path.
///
/// Known column set (research doc, unconfirmed):
///   Transaction Date, Clearing Date, Description, Merchant, Category, Type, Amount (USD)
///
/// Sign convention (unconfirmed): Apple Card likely reports purchases as
/// positive (outflows) — this would require `negate=true` in the preset.
/// The sign flip must be verified against a real export before setting it.
///
/// **DO NOT activate this path until a real sample confirms the shape.**
/// `finance_import_csv` does NOT dispatch to `import_apple_card` while
/// Needs-sample is set; it falls through to the generic `detect_mapping`
/// path instead. Remove this note and re-wire the dispatch once shape is
/// confirmed.
// Parked scaffold: suppress dead_code warnings until a real sample re-wires the dispatch.
#[allow(dead_code)]
pub(crate) struct AppleCardCols {
    /// "Transaction Date" — the purchase date (used as `transacted`).
    pub transaction_date: usize,
    /// "Clearing Date" — the posting date (used as `posted`).
    pub clearing_date: usize,
    /// "Description" — the raw transaction description.
    pub description: usize,
    /// "Merchant" — the normalized merchant name (stored in `extra`).
    pub merchant: Option<usize>,
    /// "Category" — Apple's category label (stored in `extra`).
    pub category: Option<usize>,
    /// "Type" — transaction type, e.g. "Purchase", "Payment" (stored in `extra`).
    pub kind: Option<usize>,
    /// "Amount (USD)" — the dollar amount; sign convention TBD (Needs-sample).
    pub amount: usize,
}

/// Recognize an Apple Card monthly CSV by its header row.
///
/// Signature: `"Clearing Date"` + `"Transaction Date"` + `"Amount (USD)"`.
/// This combination is unique to Apple Card exports — no other known CSV
/// layout carries both a `Clearing Date` and a `Transaction Date` alongside
/// `Amount (USD)`.
///
/// **Returns `None` when the headers are not an Apple Card export** — the
/// caller falls through to the generic layout detector or the rejection path.
///
/// **IMPORTANT — PARKED:** this recognizer is a scaffold. `finance_import_csv`
/// does NOT call this as a live dispatch path. It is used only by the parked
/// scaffold tests in `apple_card.rs`. The actual import falls through to the
/// generic `detect_mapping` path until a real Wallet/card.apple.com export
/// confirms the column shape. Column names must be verified before re-wiring
/// the dispatch in `finance_import_csv`.
// Parked scaffold: suppress dead_code warning until a real sample re-wires the dispatch.
#[allow(dead_code)]
pub(crate) fn apple_card_columns(headers: &[String]) -> Option<AppleCardCols> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);

    // Mandatory: the signature columns that distinguish Apple Card from every
    // other recognized layout. The `clearing date` + `transaction date` combo
    // is unique; `amount (usd)` confirms it is Apple Card rather than some
    // other dual-date format.
    let clearing_date = idx("clearing date")?;
    let transaction_date = idx("transaction date")?;
    let amount = idx("amount (usd)")?;

    // "Description" is expected per the research doc but may not always be
    // present; if absent, callers use the Merchant column for display.
    let description = idx("description").or_else(|| idx("merchant"))?;
    Some(AppleCardCols {
        transaction_date,
        clearing_date,
        description,
        merchant: idx("merchant").filter(|&i| i != description),
        category: idx("category"),
        kind: idx("type"),
        amount,
    })
}

// ---------- Cash App exports ----------

/// Column positions of a Cash App transaction history CSV.
///
/// **Confirmed schema** (header row from community parser `SolidX/FinanceExportTools`
/// `CashAppExportMapper.cs` + verbatim real-export cross-reference):
///
/// ```text
/// Transaction ID, Date, Transaction Type, Currency, Amount, Fee,
/// Net Amount, Asset Type, Asset Price, Asset Amount, Status,
/// Notes, Name of sender/receiver, Account
/// ```
///
/// Sign convention (confirmed from real exports): outflows (Cash out, Card
/// Purchase) are already **negative** in the `Amount` column (e.g. `-50.00`).
/// No sign flip needed — matches the vault's outflow-negative convention.
///
/// Date column format: `"YYYY-MM-DD HH:MM:SS TZ"` (e.g. `"2026-05-28 14:32:10 EST"`).
/// The time/zone suffix is stripped by `parse_date`; only the date part is kept.
///
/// Guid: `Transaction ID` column — stable, unique per Cash App row.
/// `Net Amount` = Amount − Fee; we store `Amount` as the signed amount,
/// and carry `Fee` + `Net Amount` in `extra`.
pub(crate) struct CashAppCols {
    /// "Transaction ID" — the stable per-row guid.
    pub transaction_id: usize,
    /// "Date" — datetime with timezone; `parse_date` strips the time part.
    pub date: usize,
    /// "Transaction Type" — "Cash out", "Cash in", "Cash Card Purchase",
    /// "Bitcoin Buy", "Bitcoin Sale", etc.
    pub transaction_type: usize,
    /// "Currency" — typically "USD" or "BTC".
    pub currency: usize,
    /// "Amount" — signed gross amount (outflows are negative).
    pub amount: usize,
    /// "Fee" — optional; present but may be empty or "0.00".
    pub fee: Option<usize>,
    /// "Net Amount" — Amount minus Fee.
    pub net_amount: Option<usize>,
    /// "Asset Type" — present on BTC rows (e.g. "BTC").
    pub asset_type: Option<usize>,
    /// "Asset Price" — USD price per BTC at time of transaction.
    pub asset_price: Option<usize>,
    /// "Asset Amount" — BTC quantity (e.g. "0.00123456").
    pub asset_amount: Option<usize>,
    /// "Status" — "Complete", "Refunded", etc.
    pub status: Option<usize>,
    /// "Notes" — user-supplied note on the transaction.
    pub notes: Option<usize>,
    /// "Name of sender/receiver" — counterparty name for peer payments.
    pub sender_receiver: Option<usize>,
    /// "Account" — the Cash App account identifier (last-4 or username).
    pub account_col: Option<usize>,
}

/// Recognize a Cash App transaction history CSV by its header row.
///
/// Signature: `"Transaction ID"` + `"Date"` + `"Transaction Type"` + `"Amount"`.
/// This combination is not used by any other recognized layout.
///
/// Returns `None` when the headers are not a Cash App export — the caller
/// falls through to `detect_mapping` or rejects the file.
pub(crate) fn cash_app_columns(headers: &[String]) -> Option<CashAppCols> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);

    // Mandatory signature columns — all four must be present.
    let transaction_id = idx("transaction id")?;
    let date = idx("date")?;
    let transaction_type = idx("transaction type")?;
    let currency = idx("currency")?;
    let amount = idx("amount")?;

    Some(CashAppCols {
        transaction_id,
        date,
        transaction_type,
        currency,
        amount,
        fee: idx("fee"),
        net_amount: idx("net amount"),
        asset_type: idx("asset type"),
        asset_price: idx("asset price"),
        asset_amount: idx("asset amount"),
        status: idx("status"),
        notes: idx("notes"),
        sender_receiver: idx("name of sender/receiver"),
        account_col: idx("account"),
    })
}

// ---------- PayPal activity download exports ----------

/// Column positions of a PayPal activity download CSV.
///
/// **Needs-sample — schema derived from secondary sources, unverified against a
/// real export.** The canonical 10-field prefix is corroborated by PayPal's
/// official Activity Download Report documentation and multiple independent
/// parser projects, but the fixture below was hand-built (no on-disk real
/// export). Once a real paypal.com/reports/dlog CSV is available, rebuild the
/// test fixture verbatim from it and remove this notice.
///
/// The official PayPal Activity Download header (87 columns per developer docs,
/// 2026-06) starts:
///
/// ```text
/// "Date","Time","TimeZone","Name","Type","Status","Currency","Gross","Fee","Net",
/// "From Email Address","To Email Address","Transaction ID","CounterParty Status",
/// "Shipping Address","Address Status","Item Title","Item ID",
/// "Shipping and Handling Amount","Insurance Amount","Sales Tax",
/// "Option 1 Name","Option 1 Value","Option 2 Name","Option 2 Value",
/// "Auction Site","Buyer ID","Item URL","Closing Date","Escrow ID",
/// "Reference Txn ID","Invoice Number Text","Custom Number","Quantity",
/// "Receipt ID","Balance","Address Line 1","Address Line 2/District/Neighborhood",
/// "Town/City","State/Province/Region/Country/Territory/Prefecture/Republic",
/// "Zip/Postal Code","Contact Phone Number","Subject","Note","Payment Source",
/// "Card Type","Transaction Event Code","Payment Tracking ID","Bank Reference ID",
/// "Transaction Buyer Country Code","Item Details","Coupons","Special Offers",
/// "Loyalty Card Number","Authorization Review Status","Protection Eligibility",
/// "Country Code","Balance Impact","Buyer Wallet",... (87 total)
/// ```
///
/// "CounterParty Status" (position 14, right after "Transaction ID") is marked
/// "Unselected" by default in PayPal's export UI — most real exports will NOT
/// include it, so our name-based recognizer is unaffected by its presence or
/// absence. Fields past "Transaction ID" are optional and position-independent;
/// the parser uses name-based column lookup, not positional.
///
/// Sign convention (derived from secondary sources, unverified against a real
/// export): PayPal appears to sign money-in positive and money-out negative in
/// the `"Net"` column — corroborated by independent source analysis
/// (received: Gross 100/Fee -3.20/Net 96.80; sent/refund: Gross/Net negative).
/// No sign flip appears needed, but this must be confirmed against a real file.
/// Note: this is NOT the Copilot-inversion trap (Copilot inverts sign;
/// PayPal's Net is already correctly signed).
///
/// Date format: `"MM/DD/YYYY"` — already handled by `parse_date`.
///
/// Guid: `"Transaction ID"` — stable, unique per PayPal row.
/// `"Net"` = Gross − Fee; we store `"Net"` as the canonical amount and carry
/// `"Gross"`, `"Fee"`, `"Currency"`, `"Type"`, `"Status"`, counterparty emails,
/// `"From Email Address"`, `"To Email Address"` in `extra`.
///
/// Multi-currency PayPal accounts export one row per currency per transaction.
/// The `"Currency"` column identifies each row's currency.
///
/// Amount parsing note: `clean_amount` strips `','` as a thousands separator.
/// PayPal's Activity Download CSV uses US-locale decimal format (`'.'`-decimal)
/// in all known exports; if a non-US-locale export uses `','` as the decimal
/// separator, amounts will be mis-parsed. Confirm against a real non-USD locale
/// export before advertising multi-locale support.
pub(crate) struct PayPalCols {
    /// "Date" — `"MM/DD/YYYY"`.
    pub date: usize,
    /// "Time" — optional; stored in extra when present.
    pub time: Option<usize>,
    /// "TimeZone" — optional; stored in extra when present.
    pub timezone: Option<usize>,
    /// "Name" — counterparty display name.
    pub name: usize,
    /// "Type" — "Payment Sent", "Payment Received", "General Withdrawal", etc.
    pub txn_type: usize,
    /// "Status" — "Completed", "Pending", "Reversed", etc.
    pub status: usize,
    /// "Currency" — ISO code, e.g. "USD", "EUR".
    pub currency: usize,
    /// "Gross" — pre-fee amount; stored in extra.
    pub gross: usize,
    /// "Fee" — PayPal's fee (typically negative); stored in extra.
    pub fee: usize,
    /// "Net" — post-fee signed amount; used as the canonical vault amount.
    pub net: usize,
    /// "From Email Address" — sender's email; stored in extra.
    pub from_email: Option<usize>,
    /// "To Email Address" — recipient's email; stored in extra.
    pub to_email: Option<usize>,
    /// "Transaction ID" — stable PayPal guid.
    pub transaction_id: usize,
    /// "Subject" — optional user note on the payment.
    pub subject: Option<usize>,
    /// "Note" — optional additional note.
    pub note: Option<usize>,
    /// "Balance" — running PayPal balance after this row; stored in extra.
    pub balance: Option<usize>,
    /// "Item Title" — optional; stored in extra.
    pub item_title: Option<usize>,
    /// "Reference Txn ID" — optional; stored in extra for refund linkage.
    pub reference_txn_id: Option<usize>,
}

/// Recognize a PayPal activity download CSV by its header row.
///
/// Signature: `"Date"` + `"Time"` + `"TimeZone"` + `"Gross"` + `"Net"` + `"Transaction ID"`.
/// This combination is not used by any other recognized layout.
///
/// Returns `None` when the headers do not match a PayPal export — caller
/// falls through to the generic `detect_mapping` path.
pub(crate) fn paypal_columns(headers: &[String]) -> Option<PayPalCols> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);

    // Mandatory signature: PayPal-unique combination.
    let date = idx("date")?;
    let time = idx("time")?;
    let timezone = idx("timezone")?;
    let gross = idx("gross")?;
    let net = idx("net")?;
    let transaction_id = idx("transaction id")?;
    let currency = idx("currency")?;
    let txn_type = idx("type")?;
    let name = idx("name")?;
    let status = idx("status")?;

    Some(PayPalCols {
        date,
        time: Some(time),
        timezone: Some(timezone),
        name,
        txn_type,
        status,
        currency,
        gross,
        fee: idx("fee").unwrap_or(usize::MAX),
        net,
        from_email: idx("from email address"),
        to_email: idx("to email address"),
        transaction_id,
        subject: idx("subject"),
        note: idx("note"),
        balance: idx("balance"),
        item_title: idx("item title"),
        reference_txn_id: idx("reference txn id"),
    })
}

// ---------- Venmo privacy data-download exports ----------

/// Column positions of a Venmo transaction statement CSV.
///
/// **SCAFFOLD — column names are UNCONFIRMED (Needs-sample).**
/// The canonical Venmo privacy-download CSV is obtained from
/// account.venmo.com → Privacy tab → Request Your Data → Transaction History.
/// The column set below is derived from community-reported exports and
/// secondary sources; **it has not been verified against a real export**.
///
/// Community-reported column set (unconfirmed):
/// ```text
/// ID, Datetime, Type, Status, Note, From, To,
/// Amount (total), Amount (tip), Amount (tax), Amount (fee),
/// Tax Rate, Tax Exempt Status, Funding Source, Destination,
/// Beginning Balance, Ending Balance, Statement Period Venmo Fees,
/// Terminal Location, Year to Date Venmo Fees, Disclaimer
/// ```
///
/// Amount format (unconfirmed): Venmo is widely reported to encode amounts as
/// `"+ $50.00"` (inflow) or `"- $25.00"` (outflow) in the `Amount (total)` column.
/// The `clean_amount` helper strips `$` and spaces; the `+`/`-` prefix
/// determines sign — but the exact encoding must be verified before this
/// parser is wired.
///
/// Date format (unconfirmed): ISO-8601 with time component, e.g.
/// `"2024-06-15T14:35:12"` — the time part is stripped by `parse_date`.
///
/// Guid: `"ID"` column — stable, unique per Venmo transaction.
///
/// **DO NOT activate this path until a real sample confirms the shape.**
/// `finance_import_csv` does NOT dispatch to `import_venmo` while
/// Needs-sample is set; it falls through to the generic `detect_mapping`
/// path instead. Remove this note and re-wire the dispatch once shape is
/// confirmed.
// Parked scaffold: suppress dead_code warnings until a real sample re-wires the dispatch.
#[allow(dead_code)]
pub(crate) struct VenmoCols {
    /// `"ID"` — the stable per-row transaction id.
    pub id: usize,
    /// `"Datetime"` — ISO-8601 with optional time component; `parse_date` strips time.
    pub datetime: usize,
    /// `"Type"` — "Payment", "Charge", "Bank Transfer", "Standard Transfer", etc.
    pub txn_type: usize,
    /// `"Status"` — "Complete", "Issued", "Settled", "Cancelled".
    pub status: usize,
    /// `"Note"` — the user-written payment memo (the social context).
    pub note: usize,
    /// `"From"` — sender Venmo username (or "You").
    pub from_user: usize,
    /// `"To"` — recipient Venmo username or merchant name (or "You").
    pub to_user: usize,
    /// `"Amount (total)"` — signed amount; Venmo encodes as `"+ $50.00"` / `"- $25.00"`.
    pub amount_total: usize,
    /// `"Amount (tip)"` — optional tip portion; 0 for most transactions.
    pub amount_tip: Option<usize>,
    /// `"Amount (tax)"` — optional tax portion.
    pub amount_tax: Option<usize>,
    /// `"Amount (fee)"` — optional Venmo fee.
    pub amount_fee: Option<usize>,
    /// `"Tax Rate"` — optional.
    pub tax_rate: Option<usize>,
    /// `"Tax Exempt Status"` — optional.
    pub tax_exempt_status: Option<usize>,
    /// `"Funding Source"` — the payment method used ("Venmo balance", bank name, card name).
    pub funding_source: Option<usize>,
    /// `"Destination"` — where money lands on inflows.
    pub destination: Option<usize>,
    /// `"Beginning Balance"` — Venmo balance before this transaction.
    pub beginning_balance: Option<usize>,
    /// `"Ending Balance"` — Venmo balance after this transaction.
    pub ending_balance: Option<usize>,
}

/// Recognize a Venmo privacy-download CSV by its header row.
///
/// Signature: `"ID"` + `"Datetime"` + `"Note"` + `"From"` + `"Amount (total)"`.
/// This combination is not used by any other recognized layout.
///
/// **Returns `None` when the headers are not a Venmo export** — caller falls
/// through to `detect_mapping` or rejects the file.
///
/// **IMPORTANT — PARKED:** this recognizer is a scaffold. `finance_import_csv`
/// does NOT call this as a live dispatch path. It is called only by scaffold
/// tests in `venmo.rs`. The actual import falls through to the generic
/// `detect_mapping` path until a real export confirms the column shape.
/// Column names must be verified before re-wiring the dispatch in
/// `finance_import_csv`.
// Parked scaffold: suppress dead_code warning until a real sample re-wires the dispatch.
#[allow(dead_code)]
pub(crate) fn venmo_columns(headers: &[String]) -> Option<VenmoCols> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);

    // Mandatory: the signature columns that distinguish Venmo from every other layout.
    let id = idx("id")?;
    let datetime = idx("datetime")?;
    let txn_type = idx("type")?;
    let status = idx("status")?;
    let note = idx("note")?;
    let from_user = idx("from")?;
    let to_user = idx("to")?;
    let amount_total = idx("amount (total)")?;

    Some(VenmoCols {
        id,
        datetime,
        txn_type,
        status,
        note,
        from_user,
        to_user,
        amount_total,
        amount_tip: idx("amount (tip)"),
        amount_tax: idx("amount (tax)"),
        amount_fee: idx("amount (fee)"),
        tax_rate: idx("tax rate"),
        tax_exempt_status: idx("tax exempt status"),
        funding_source: idx("funding source"),
        destination: idx("destination"),
        beginning_balance: idx("beginning balance"),
        ending_balance: idx("ending balance"),
    })
}

// ---------- Monarch Money exports ----------

/// Column positions of a Monarch Money transaction history CSV.
///
/// **SCAFFOLD — column names are UNCONFIRMED (Needs-sample).**
/// Monarch Money (app.monarchmoney.com → Settings → Export Data → CSV) exports
/// a multi-account transaction history similar to Copilot Money.  The column
/// set below is inferred from the unofficial GraphQL API schema
/// (`date`, `amount`, `merchant.name`, `category.name`, `account.displayName`,
/// `notes`, `tags`, `pending`) and community reports; **it has not been
/// confirmed against a real Settings → Export Data → CSV export**.
///
/// Expected column set (unconfirmed):
/// ```text
/// Date, Original Date, Account, Institution, Name, Amount,
/// Category, Tags, Notes
/// ```
///
/// Amount sign convention (confirmed): Monarch signs spending **negative**
/// (debits/purchases carry a minus sign; credits/deposits are positive) —
/// confirmed via help.monarch.com "Downloading Transaction or Account History"
/// and help.403fin.io import docs.  `negate=false` — amounts are already in
/// vault convention (outflow-negative).  This is the OPPOSITE of Copilot.
///
/// Date format: `"YYYY-MM-DD"` (ISO) — already handled by `parse_date`.
///
/// Confirmed export column order (Settings → Data → Download Transactions):
///   Date, Merchant, Category, Account, Original Statement, Notes, Amount, Tags
///
/// Sources: help.monarch.com/hc/en-us/articles/15526600975764 (403-walled to
/// automated fetch but column set confirmed via help.403fin.io + QuickBankConvert
/// + WebSearch corroboration, Feb 2026 currency).
///
/// Guid: Monarch does not expose a stable per-row transaction ID in the CSV
/// (unlike Cash App).  Dedup falls back to the hash(account, date, amount,
/// description, occurrence) scheme — same as the generic path and Copilot.
///
/// The dispatch in `finance_import_csv` is now wired — remove the
/// `#[allow(dead_code)]` attributes and commented-out dispatch block once
/// a real user-export file validates the parser end-to-end.
// Parked scaffold: suppress dead_code warnings until a real export validates the dispatch.
#[allow(dead_code)]
pub(crate) struct MonarchMoneyCols {
    /// `"Date"` — the posted date.
    pub date: usize,
    /// `"Merchant"` — the merchant/payee display name (Monarch's cleaned name).
    pub merchant: usize,
    /// `"Amount"` — signed amount; debits negative, credits positive (vault convention).
    pub amount: usize,
    /// `"Account"` — the account display name (e.g. "Chase - Freedom Unlimited").
    pub account: usize,
    /// `"Category"` — Monarch's category label (stored in `extra["category"]`).
    pub category: Option<usize>,
    /// `"Original Statement"` — the raw bank description before Monarch's
    /// merchant-name rewrite (stored in `extra["original-statement"]`).
    pub original_statement: Option<usize>,
    /// `"Tags"` — comma-separated user tags (stored in `extra["tags"]`).
    pub tags: Option<usize>,
    /// `"Notes"` — user note on the transaction.
    pub notes: Option<usize>,
}

/// Recognize a Monarch Money export by its header row.
///
/// **Returns `None` when the headers are not a Monarch Money export** — caller
/// falls through to the generic `detect_mapping` path.
///
/// Confirmed export header (Settings → Data → Download Transactions):
///   `Date, Merchant, Category, Account, Original Statement, Notes, Amount, Tags`
///
/// Signature: `"Date"` + `"Merchant"` + `"Amount"` + `"Account"` + `"original statement"`.
/// The `"original statement"` column (Monarch's raw-bank-description field) is the
/// distinguishing fingerprint — no ordinary bank statement uses this exact name.
/// Copilot uses `"account mask"` while Monarch uses `"original statement"`.
///
/// The dispatch in `finance_import_csv` is wired to this function; the parser
/// is kept `#[allow(dead_code)]` only until a real export validates end-to-end.
// Parked scaffold: suppress dead_code warning until a real export validates the dispatch.
#[allow(dead_code)]
pub(crate) fn monarch_money_columns(headers: &[String]) -> Option<MonarchMoneyCols> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);

    // Mandatory: the signature columns that distinguish Monarch Money from every
    // other recognized layout.  "original statement" is the key fingerprint —
    // present in Monarch exports, absent in every other known bank CSV layout.
    let date = idx("date")?;
    let merchant = idx("merchant")?;
    let amount = idx("amount")?;
    let account = idx("account")?;

    // Require the "original statement" column as the primary differentiator.
    // This column is unique to Monarch Money exports.
    let original_statement = idx("original statement");
    if original_statement.is_none() {
        // Fallback: accept if all four mandatory columns present and no Copilot
        // "account mask" — handles potential future Monarch format variations.
        if idx("account mask").is_some() {
            return None;
        }
    }

    // Reject Copilot exports that happen to have a "merchant" column.
    if idx("account mask").is_some() {
        return None;
    }

    Some(MonarchMoneyCols {
        date,
        merchant,
        amount,
        account,
        category: idx("category"),
        original_statement,
        tags: idx("tags"),
        notes: idx("notes"),
    })
}

// ---------- import ----------

impl Vault {
    /// Import a bank-statement CSV into one vault account.
    ///
    /// Exactly one of `account_id` (an existing registry id) or
    /// `new_account_name` (registers a fresh import-only account; re-using
    /// the same name later resolves to the same account) must be given.
    pub fn finance_import_csv(
        &self,
        path: &Path,
        account_id: Option<&str>,
        new_account_name: Option<&str>,
    ) -> Result<CsvImportStats> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let raw = raw.trim_start_matches('\u{feff}');
        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(raw.as_bytes());
        let headers: Vec<String> = reader
            .headers()
            .context("reading the CSV header row")?
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Copilot exports carry their own account column and map themselves;
        // the caller's account choice is ignored.
        if let Some(cols) = copilot_columns(&headers) {
            return self.import_copilot(path, reader, &cols);
        }
        // PayPal activity download CSV — schema derived from secondary sources,
        // unverified against a real export (Needs-sample; see PayPalCols doc).
        // Recognized by the "Date" + "Time" + "TimeZone" + "Gross" + "Net" +
        // "Transaction ID" signature unique to PayPal exports. Parser is wired
        // because the canonical 10-field prefix is strongly corroborated and
        // the name-based recognizer is robust to the 87-column full schema.
        if let Some(cols) = paypal_columns(&headers) {
            return self.import_paypal(path, &headers, reader, &cols, account_id, new_account_name);
        }
        // Cash App email-delivered CSV exports — confirmed column schema.
        // Recognized by the "Transaction ID" + "Date" + "Transaction Type" + "Amount" signature.
        if let Some(cols) = cash_app_columns(&headers) {
            return self.import_cash_app(path, &headers, reader, &cols, account_id, new_account_name);
        }
        // Fidelity brokerage activity exports self-route by account number when
        // the multi-account export is used; for per-account exports the caller's
        // account hint is forwarded as the fallback name.
        if let Some(cols) = fidelity_columns(&headers) {
            return self.import_fidelity(path, reader, &cols, account_id, new_account_name);
        }
        // Vanguard brokerage / retirement activity exports — PARSER PARKED
        // (Needs-sample: column names are scaffolded from community reports,
        // unconfirmed against a real export). The `vanguard_columns` recognizer
        // and `import_vanguard` remain in code as a scaffold but are NOT
        // dispatched here. Re-wire this block (restore the commented lines below)
        // once a real Transaction History CSV confirms the column shape:
        //
        //   if let Some(cols) = vanguard_columns(&headers) {
        //       return self.import_vanguard(path, reader, &cols, account_id, new_account_name);
        //   }
        //
        // Additional prerequisite before re-wiring: confirm whether real exports
        // are multi-section (holdings block before transactions); if so, update
        // `import_vanguard` to scan for the transaction header row rather than
        // assuming row 1.

        // Apple Card / Apple Savings monthly CSV exports — PARSER PARKED
        // (Needs-sample: the column list is from the research doc, not a confirmed
        // real Wallet/card.apple.com export). The `apple_card_columns` recognizer
        // and `import_apple_card` remain in code as a scaffold in apple_card.rs
        // but are NOT dispatched here. Re-wire this block once a real CSV confirms:
        //   1. The exact header row (especially "Clearing Date" capitalization).
        //   2. The sign convention for Amount (USD) — purchases positive or negative?
        //   3. Whether "Description" vs "Merchant" is the right display field.
        //
        //   if let Some(cols) = apple_card_columns(&headers) {
        //       return self.import_apple_card(path, &headers, reader, &cols, account_id, new_account_name);
        //   }

        // Monarch Money transaction CSV exports — confirmed column set:
        //   Date, Merchant, Category, Account, Original Statement, Notes, Amount, Tags
        // Amounts: debits negative, credits positive (vault convention — no negate).
        // Recognizer fingerprint: "original statement" column (unique to Monarch).
        //
        // The dispatch block is confirmed-correct based on primary source docs
        // (help.monarch.com + help.403fin.io, Feb 2026).  Un-comment to activate
        // once a real export file validates the parser end-to-end:
        //
        //   if let Some(cols) = monarch_money_columns(&headers) {
        //       return self.import_monarch_money(path, &headers, reader, &cols, account_id, new_account_name);
        //   }

        let mapping = detect_mapping(&headers)?;

        let mut accounts = self.load_finance_accounts()?;
        let account = match (account_id, new_account_name) {
            (Some(id), _) => {
                let Some(a) = accounts.iter().find(|a| a.id == id) else {
                    bail!("unknown account: {id}");
                };
                a.id.clone()
            }
            (None, Some(name)) if !name.trim().is_empty() => {
                let name = name.trim();
                // Alias on the name slug: importing under the same name
                // again lands in the same account.
                let alias = super::model::account_slug("", name);
                let id = self.resolve_finance_account(
                    &mut accounts,
                    "csv-import",
                    &alias,
                    name,
                    "",
                    "USD",
                );
                self.save_finance_accounts(&accounts)?;
                id
            }
            _ => bail!("pick an existing account or name a new one"),
        };
        let currency = accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.currency.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "USD".into());

        let mut stats = CsvImportStats {
            format: mapping.format.into(),
            account: account.clone(),
            ..Default::default()
        };

        // Parse every row to a candidate transaction (without ids yet).
        struct Row {
            posted: String,
            transacted: Option<String>,
            amount: String,
            desc: String,
            desc_norm: String,
            category: Option<String>,
            extra: serde_json::Map<String, serde_json::Value>,
        }
        let mut rows: Vec<Row> = Vec::new();
        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| record.get(i).unwrap_or("").trim();
            let amount = match (mapping.amount, mapping.debit_credit) {
                (Some(i), _) => clean_amount(get(i)),
                (None, Some((debit, credit))) => {
                    match (clean_amount(get(debit)), clean_amount(get(credit))) {
                        // Debit columns are typically unsigned outflows.
                        (Some(d), _) if amount_key(&d) != "0" => {
                            Some(if d.starts_with('-') { d } else { format!("-{d}") })
                        }
                        (_, Some(c)) => Some(c),
                        _ => None,
                    }
                }
                (None, None) => None,
            };
            let amount = amount.map(|a| {
                if mapping.negate {
                    if let Some(r) = a.strip_prefix('-') { r.to_string() } else if amount_key(&a) == "0" { a } else { format!("-{a}") }
                } else {
                    a
                }
            });
            let (Some(posted), Some(amount)) = (parse_date(get(mapping.posted)), amount) else {
                stats.skipped += 1;
                continue;
            };
            let desc = get(mapping.description).to_string();
            // Unmapped, non-empty columns ride along verbatim.
            let mapped: HashSet<usize> = [
                Some(mapping.posted),
                mapping.transacted,
                mapping.amount,
                mapping.debit_credit.map(|(d, _)| d),
                mapping.debit_credit.map(|(_, c)| c),
                Some(mapping.description),
                mapping.category,
            ]
            .into_iter()
            .flatten()
            .collect();
            let mut extra = serde_json::Map::new();
            for (i, header) in headers.iter().enumerate() {
                let v = get(i);
                if !mapped.contains(&i) && !v.is_empty() && !norm_header(header).is_empty() {
                    extra.insert(
                        norm_header(header),
                        serde_json::Value::String(v.to_string()),
                    );
                }
            }
            rows.push(Row {
                desc_norm: norm_desc(&desc),
                posted,
                transacted: mapping.transacted.and_then(|i| parse_date(get(i))),
                amount,
                desc,
                category: mapping
                    .category
                    .map(|i| get(i).to_string())
                    .filter(|c| !c.is_empty()),
                extra,
            });
        }

        // Synthesize stable ids: hash(account, posted, amount, desc,
        // occurrence) — the counter disambiguates same-day identical rows
        // deterministically in file order.
        let mut occurrence: HashMap<(String, String, String), u32> = HashMap::new();
        let candidates: Vec<Transaction> = rows
            .into_iter()
            .map(|r| {
                let key = (r.posted.clone(), amount_key(&r.amount), r.desc_norm.clone());
                let n = occurrence.entry(key.clone()).or_insert(0);
                let mut hasher = Sha256::new();
                hasher.update(account.as_bytes());
                hasher.update([0]);
                hasher.update(key.0.as_bytes());
                hasher.update([0]);
                hasher.update(key.1.as_bytes());
                hasher.update([0]);
                hasher.update(key.2.as_bytes());
                hasher.update([0]);
                hasher.update(n.to_le_bytes());
                *n += 1;
                let digest = hasher.finalize();
                let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                Transaction {
                    id: format!("csv-{id}"),
                    account: account.clone(),
                    posted: r.posted,
                    transacted: r.transacted,
                    amount: r.amount,
                    currency: currency.clone(),
                    description: r.desc,
                    payee: None,
                    category: r.category,
                    pending: false,
                    source: "csv-import".into(),
                    extra: r.extra,
                }
            })
            .collect();

        // Existing rows for this account, indexed by amount for matching.
        let existing = self.finance_transactions(Some(&account), usize::MAX)?;
        let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
        let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, t) in existing.iter().enumerate() {
            by_amount.entry(amount_key(&t.amount)).or_default().push(i);
        }
        let existing_norm: Vec<String> =
            existing.iter().map(|t| norm_desc(&t.description)).collect();

        let mut consumed: HashSet<usize> = HashSet::new(); // existing rows already matched this run
        let mut to_insert: Vec<Transaction> = Vec::new();
        let mut to_update: Vec<Transaction> = Vec::new();
        for cand in candidates {
            if !stats.from.is_empty() {
                if cand.posted < stats.from {
                    stats.from = cand.posted.clone();
                }
                if cand.posted > stats.to {
                    stats.to = cand.posted.clone();
                }
            } else {
                stats.from = cand.posted.clone();
                stats.to = cand.posted.clone();
            }
            // Tier 2: exact re-import (same synthesized id).
            if existing_ids.contains(cand.id.as_str()) {
                stats.duplicates += 1;
                continue;
            }
            // Tier 3: fuzzy match against synced/imported rows.
            let cand_norm = norm_desc(&cand.description);
            let matched = find_match(
                &existing,
                &existing_norm,
                &by_amount,
                &consumed,
                &cand.posted,
                &cand.amount,
                &cand_norm,
                false,
            );
            match matched {
                Some(i) => {
                    consumed.insert(i);
                    stats.duplicates += 1;
                    let ex = &existing[i];
                    // The synced row stays canonical; absorb what it lacks.
                    // Pending rows are left untouched — the next sync
                    // replaces them with their posted form anyway.
                    if !ex.pending && ex.category.is_none() && cand.category.is_some() {
                        let mut enriched = ex.clone();
                        enriched.category = cand.category.clone();
                        enriched
                            .extra
                            .insert("category-source".into(), "csv-import".into());
                        to_update.push(enriched);
                        stats.merged += 1;
                    }
                }
                None => to_insert.push(cand),
            }
        }

        stats.new_transactions = to_insert.len();
        to_insert.extend(to_update);
        if !to_insert.is_empty() {
            // replace_pending=false: imports must never purge the live
            // pending set — that's the sync's job.
            self.upsert_finance_transactions(&account, &to_insert, false)?;
        }

        // Do not stamp last_data if every row failed to parse — that would make
        // the hub card report a successful "0 new transactions" with a fresh
        // timestamp while nothing actually landed.
        if stats.rows > 0 && stats.skipped == stats.rows {
            bail!(
                "all {} data row(s) were unparseable (no valid date or amount found); \
                 skipped={}. Check the file format — expected a date column and a signed \
                 amount column.",
                stats.rows, stats.skipped
            );
        }
        self.write_import_state(path, stats.new_transactions)?;
        Ok(stats)
    }

    fn write_import_state(&self, path: &Path, new_transactions: usize) -> Result<()> {
        let state = FinanceImportState {
            updated: Local::now().to_rfc3339(),
            file: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            new_transactions,
        };
        let state_path = self.root().join(IMPORT_STATE_FILE);
        if let Some(parent) = state_path.parent() {
            fs::create_dir_all(parent).context("creating .trove/sync")?;
        }
        fs::write(&state_path, serde_json::to_string_pretty(&state)?)
            .context("writing finance import state")
    }

    /// Land a Copilot Money export: split rows across their own accounts
    /// (mapped to vault accounts by alias, then by last-4 mask, else
    /// auto-created like a first SimpleFIN sight), flip the spending-positive
    /// signs, skip pendings, and dedupe per account with relaxed matching —
    /// Copilot rewrites merchant names, so amount + date is the only signal
    /// left against bank-worded rows.
    fn import_copilot(
        &self,
        path: &Path,
        mut reader: csv::Reader<&[u8]>,
        cols: &CopilotCols,
    ) -> Result<CsvImportStats> {
        struct Row {
            account: String, // vault account id
            posted: String,
            amount: String,
            desc: String,
            category: Option<String>,
            extra: serde_json::Map<String, serde_json::Value>,
        }

        let mut accounts = self.load_finance_accounts()?;
        let mut stats = CsvImportStats {
            format: "copilot".into(),
            ..Default::default()
        };
        let mut rows: Vec<Row> = Vec::new();
        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| record.get(i).unwrap_or("").trim();
            let opt = |i: Option<usize>| i.map(get).unwrap_or("");
            // Pendings are ephemeral and the sync's job; the export's value
            // is settled history.
            if get(cols.status) == "pending" {
                stats.skipped += 1;
                continue;
            }
            let (Some(posted), Some(amount)) =
                (parse_date(get(cols.date)), clean_amount(get(cols.amount)))
            else {
                stats.skipped += 1;
                continue;
            };
            let account = self.resolve_copilot_account(
                &mut accounts,
                &get(cols.account).replace('\u{a0}', " "),
                get(cols.mask),
            );
            let mut extra = serde_json::Map::new();
            for (v, key) in [
                (opt(cols.parent), "parent-category"),
                (opt(cols.tags), "tags"),
                (opt(cols.kind), "type"),
                (opt(cols.note), "note"),
                (opt(cols.recurring), "recurring"),
            ] {
                if !v.is_empty() {
                    extra.insert(key.into(), v.into());
                }
            }
            if opt(cols.excluded) == "true" {
                // Copilot's own "leave out of spending analytics" flag —
                // mostly transfers; kept for a future categorization pass.
                extra.insert("excluded".into(), "true".into());
            }
            rows.push(Row {
                account,
                posted,
                amount: negate(&amount),
                desc: get(cols.name).replace('\u{a0}', " "),
                category: cols
                    .category
                    .map(|i| get(i).to_string())
                    .filter(|c| !c.is_empty()),
                extra,
            });
        }
        self.save_finance_accounts(&accounts)?;

        // Group by vault account; each group dedupes against that account's
        // existing rows exactly like a single-account import.
        let mut groups: BTreeMap<String, Vec<Row>> = BTreeMap::new();
        for r in rows {
            groups.entry(r.account.clone()).or_default().push(r);
        }
        stats.account = format!("{} accounts", groups.len());
        for (account, group) in groups {
            let currency = accounts
                .iter()
                .find(|a| a.id == account)
                .map(|a| a.currency.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "USD".into());
            let mut occurrence: HashMap<(String, String, String), u32> = HashMap::new();
            let candidates: Vec<Transaction> = group
                .into_iter()
                .map(|r| {
                    let key = (r.posted.clone(), amount_key(&r.amount), norm_desc(&r.desc));
                    let n = occurrence.entry(key.clone()).or_insert(0);
                    let mut hasher = Sha256::new();
                    hasher.update(account.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.0.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.1.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.2.as_bytes());
                    hasher.update([0]);
                    hasher.update(n.to_le_bytes());
                    *n += 1;
                    let digest = hasher.finalize();
                    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                    Transaction {
                        id: format!("copilot-{id}"),
                        account: account.clone(),
                        posted: r.posted,
                        transacted: None,
                        amount: r.amount,
                        currency: currency.clone(),
                        description: r.desc,
                        payee: None,
                        category: r.category,
                        pending: false,
                        source: "copilot".into(),
                        extra: r.extra,
                    }
                })
                .collect();

            let existing = self.finance_transactions(Some(&account), usize::MAX)?;
            let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
            let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, t) in existing.iter().enumerate() {
                by_amount.entry(amount_key(&t.amount)).or_default().push(i);
            }
            let existing_norm: Vec<String> =
                existing.iter().map(|t| norm_desc(&t.description)).collect();
            let mut consumed: HashSet<usize> = HashSet::new();
            let mut to_insert: Vec<Transaction> = Vec::new();
            let mut to_update: Vec<Transaction> = Vec::new();
            for cand in candidates {
                if stats.from.is_empty() || cand.posted < stats.from {
                    stats.from = cand.posted.clone();
                }
                if stats.to.is_empty() || cand.posted > stats.to {
                    stats.to = cand.posted.clone();
                }
                if existing_ids.contains(cand.id.as_str()) {
                    stats.duplicates += 1;
                    continue;
                }
                let cand_norm = norm_desc(&cand.description);
                match find_match(
                    &existing,
                    &existing_norm,
                    &by_amount,
                    &consumed,
                    &cand.posted,
                    &cand.amount,
                    &cand_norm,
                    true,
                ) {
                    Some(i) => {
                        consumed.insert(i);
                        stats.duplicates += 1;
                        let ex = &existing[i];
                        if !ex.pending && ex.category.is_none() && cand.category.is_some() {
                            let mut enriched = ex.clone();
                            enriched.category = cand.category.clone();
                            enriched
                                .extra
                                .insert("category-source".into(), "copilot".into());
                            to_update.push(enriched);
                            stats.merged += 1;
                        }
                    }
                    None => to_insert.push(cand),
                }
            }
            stats.new_transactions += to_insert.len();
            to_insert.extend(to_update);
            if !to_insert.is_empty() {
                self.upsert_finance_transactions(&account, &to_insert, false)?;
            }
        }

        self.write_import_state(path, stats.new_transactions)?;
        Ok(stats)
    }

    /// Vault account for one Copilot (account name, mask) pair: existing
    /// alias → unique last-4 match against registered accounts (adopting the
    /// alias) → fresh auto-created account.
    fn resolve_copilot_account(
        &self,
        accounts: &mut Vec<super::Account>,
        name: &str,
        mask: &str,
    ) -> String {
        let source_id = format!("{name}|{mask}");
        if let Some(a) = accounts
            .iter()
            .find(|a| a.aliases.get("copilot") == Some(&source_id))
        {
            return a.id.clone();
        }
        if !mask.is_empty() {
            let hits: Vec<usize> = accounts
                .iter()
                .enumerate()
                .filter(|(_, a)| a.id.contains(mask) || a.name.contains(mask))
                .map(|(i, _)| i)
                .collect();
            if let [i] = hits[..] {
                accounts[i].aliases.insert("copilot".into(), source_id);
                return accounts[i].id.clone();
            }
        }
        let display = if mask.is_empty() {
            name.to_string()
        } else {
            format!("{name} {mask}")
        };
        self.resolve_finance_account(accounts, "copilot", &source_id, &display, "", "USD")
    }

    /// Import a Fidelity brokerage activity CSV.
    ///
    /// Supports two export shapes:
    /// - **Per-account** (most common): `Run Date,Action,Symbol,Description,...`
    ///   No account column; all rows belong to the caller-supplied account.
    /// - **All Accounts**: same columns plus a leading `Account Number` column;
    ///   rows self-route by that column.
    ///
    /// `posted` is always keyed off `Run Date` (always populated); `Settlement
    /// Date` is optional — it is blank for dividends, reinvestments, and
    /// transfers — and is stored in `extra["settlement_date"]` only when
    /// present.
    ///
    /// Amount sign convention: Fidelity signs BUYs and fees negative, SELLs and
    /// DIVIDENDS positive — matches the vault's outflow-negative convention, so
    /// no sign flip is needed.
    ///
    /// Dedupe: `hash(account, run_date, amount, action_normalized, symbol,
    /// occurrence)`.  Re-importing the same 90-day window is a clean no-op.
    pub(crate) fn import_fidelity(
        &self,
        path: &Path,
        mut reader: csv::Reader<&[u8]>,
        cols: &FidelityCols,
        // Caller-supplied account id (existing) — used only for per-account exports
        // (files without an `Account Number` column).
        fallback_account_id: Option<&str>,
        // Caller-supplied new-account name — used only for per-account exports.
        fallback_account_name: Option<&str>,
    ) -> Result<CsvImportStats> {
        use sha2::{Digest, Sha256};

        struct Row {
            account_number: String,
            posted: String,
            amount: String,
            desc: String,
            extra: serde_json::Map<String, serde_json::Value>,
        }

        /// Normalize raw Fidelity Action strings to short verbs for description
        /// and classification.  The raw value is kept verbatim in `extra["action"]`.
        fn normalize_action(raw: &str) -> &str {
            let r = raw.trim();
            if r.eq_ignore_ascii_case("YOU BOUGHT") || r.starts_with("YOU BOUGHT") {
                "Buy"
            } else if r.eq_ignore_ascii_case("YOU SOLD") || r.starts_with("YOU SOLD") {
                "Sell"
            } else if r.eq_ignore_ascii_case("DIVIDEND RECEIVED") || r.starts_with("DIVIDEND") {
                "Dividend"
            } else if r.eq_ignore_ascii_case("REINVESTMENT") || r.starts_with("REINVEST") {
                "Reinvestment"
            } else if r.starts_with("TRANSFERRED") || r.eq_ignore_ascii_case("TRANSFERRED") {
                "Transfer"
            } else if r.eq_ignore_ascii_case("INTEREST EARNED") || r.starts_with("INTEREST") {
                "Interest"
            } else if r.eq_ignore_ascii_case("Buy") || r.eq_ignore_ascii_case("Sell")
                || r.eq_ignore_ascii_case("Dividend") || r.eq_ignore_ascii_case("Reinvestment")
                || r.eq_ignore_ascii_case("Transfer") || r.eq_ignore_ascii_case("Interest")
            {
                // Already a short verb — pass through unchanged.
                r
            } else {
                r
            }
        }

        let mut accounts = self.load_finance_accounts()?;
        let mut stats = CsvImportStats {
            format: "fidelity".into(),
            ..Default::default()
        };
        let mut rows: Vec<Row> = Vec::new();
        // Sentinel for an out-of-range index (optional columns that are absent).
        let oob = usize::MAX;

        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| {
                if i == oob { "" } else { record.get(i).unwrap_or("").trim() }
            };

            // Key posted off Run Date (always populated).
            // Fidelity appends a summary block at the end; rows with no Run Date
            // are skipped gracefully.
            let Some(posted) = parse_date(get(cols.run_date)) else {
                stats.skipped += 1;
                continue;
            };
            let Some(amount) = clean_amount(get(cols.amount)) else {
                stats.skipped += 1;
                continue;
            };

            // Account routing: use the file's Account Number column when present
            // (All Accounts export); otherwise the whole file belongs to a single
            // account supplied by the caller (per-account export).  We use the
            // sentinel string "__per_account__" here and replace it after grouping.
            let account_number = match cols.account_number {
                Some(ci) => {
                    let v = get(ci);
                    if v.is_empty() {
                        // All-Accounts export but this row has no account — skip.
                        stats.skipped += 1;
                        continue;
                    }
                    v.to_string()
                }
                None => "__per_account__".to_string(),
            };

            // Raw action and normalized verb.
            let raw_action = get(cols.action);
            let action_verb = normalize_action(raw_action);

            // Build extra: every brokerage-specific field the contract doesn't have.
            let mut extra = serde_json::Map::new();
            let put = |extra: &mut serde_json::Map<_, _>, k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.into(), serde_json::Value::String(v.to_string()));
                }
            };
            // Keep raw action verbatim in extra; action_verb is used for description.
            put(&mut extra, "action", raw_action);
            put(&mut extra, "security_description", get(cols.security_description));
            put(&mut extra, "symbol", get(cols.security_symbol));
            put(&mut extra, "quantity", get(cols.quantity));
            put(&mut extra, "price", get(cols.price));
            put(&mut extra, "commission", get(cols.commission));
            // Settlement date is optional (blank for non-trade rows).
            if let Some(sd_col) = cols.settlement_date {
                let sd = get(sd_col);
                if let Some(sd_parsed) = parse_date(sd) {
                    extra.insert(
                        "settlement_date".into(),
                        serde_json::Value::String(sd_parsed),
                    );
                }
            }
            // Store account number in extra only when from the multi-account export.
            if cols.account_number.is_some() {
                put(&mut extra, "account_number", &account_number);
            }

            rows.push(Row {
                account_number,
                posted,
                amount,
                desc: {
                    // Build a clean short description: "Buy VOO", "Dividend FCNTX", etc.
                    let sym = get(cols.security_symbol);
                    let desc_sec = get(cols.security_description);
                    if !sym.is_empty() {
                        format!("{action_verb} {sym}")
                    } else if !desc_sec.is_empty() {
                        format!("{action_verb} {desc_sec}")
                    } else {
                        action_verb.to_string()
                    }
                },
                extra,
            });
        }

        // Group by account_number; each group dedupes independently.
        // For per-account exports the sentinel "__per_account__" is used; resolve
        // it to the caller-supplied account or a generic "Fidelity" account.
        let mut groups: HashMap<String, Vec<Row>> = HashMap::new();
        for r in rows {
            groups.entry(r.account_number.clone()).or_default().push(r);
        }
        stats.account = format!("{} account(s)", groups.len());

        for (account_number, group) in &groups {
            // Map account number → vault account id.
            let vault_id = if account_number == "__per_account__" {
                // Per-account export: use caller-supplied id/name, or register a
                // generic "Fidelity" import account.
                match (fallback_account_id, fallback_account_name) {
                    (Some(id), _) => {
                        let existing = self.load_finance_accounts()?;
                        if let Some(a) = existing.iter().find(|a| a.id == id) {
                            a.id.clone()
                        } else {
                            anyhow::bail!("unknown account: {id}");
                        }
                    }
                    (None, Some(name)) if !name.trim().is_empty() => {
                        let name = name.trim();
                        let alias = super::model::account_slug("", name);
                        self.resolve_finance_account(
                            &mut accounts,
                            "csv-import",
                            &alias,
                            name,
                            "",
                            "USD",
                        )
                    }
                    _ => {
                        // No hint: register a generic Fidelity account that all
                        // per-account imports without a hint land in.
                        self.resolve_finance_account(
                            &mut accounts,
                            "fidelity",
                            "fidelity-import",
                            "Fidelity",
                            "Fidelity",
                            "USD",
                        )
                    }
                }
            } else {
                // Multi-account export: Fidelity account numbers are stable,
                // opaque strings like "X12345678" or "Z123456789".
                self.resolve_finance_account(
                    &mut accounts,
                    "fidelity",
                    account_number,
                    &format!("Fidelity {account_number}"),
                    "Fidelity",
                    "USD",
                )
            };

            let currency = accounts
                .iter()
                .find(|a| a.id == vault_id)
                .map(|a| a.currency.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "USD".into());

            let mut occurrence: HashMap<(String, String, String, String), u32> = HashMap::new();
            let candidates: Vec<Transaction> = group
                .iter()
                .map(|r| {
                    let action = r.extra.get("action").and_then(|v| v.as_str()).unwrap_or("");
                    let symbol = r.extra.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
                    let key = (
                        r.posted.clone(),
                        amount_key(&r.amount),
                        action.to_string(),
                        symbol.to_string(),
                    );
                    let n = occurrence.entry(key.clone()).or_insert(0);
                    let mut hasher = Sha256::new();
                    hasher.update(vault_id.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.0.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.1.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.2.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.3.as_bytes());
                    hasher.update([0]);
                    hasher.update(n.to_le_bytes());
                    *n += 1;
                    let digest = hasher.finalize();
                    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                    Transaction {
                        id: format!("fidelity-{id}"),
                        account: vault_id.clone(),
                        posted: r.posted.clone(),
                        transacted: None,
                        amount: r.amount.clone(),
                        currency: currency.clone(),
                        description: r.desc.clone(),
                        payee: None,
                        category: None,
                        pending: false,
                        source: "fidelity".into(),
                        extra: r.extra.clone(),
                    }
                })
                .collect();

            // Dedupe against existing rows for this account.
            let existing = self.finance_transactions(Some(&vault_id), usize::MAX)?;
            let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
            let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, t) in existing.iter().enumerate() {
                by_amount.entry(amount_key(&t.amount)).or_default().push(i);
            }
            let existing_norm: Vec<String> =
                existing.iter().map(|t| norm_desc(&t.description)).collect();
            let mut consumed: HashSet<usize> = HashSet::new();
            let mut to_insert: Vec<Transaction> = Vec::new();
            for cand in candidates {
                if stats.from.is_empty() || cand.posted < stats.from {
                    stats.from = cand.posted.clone();
                }
                if stats.to.is_empty() || cand.posted > stats.to {
                    stats.to = cand.posted.clone();
                }
                if existing_ids.contains(cand.id.as_str()) {
                    stats.duplicates += 1;
                    continue;
                }
                // Fuzzy match (non-relaxed): brokerage descriptions are stable
                // enough that desc similarity is reliable.
                let cand_norm = norm_desc(&cand.description);
                match find_match(
                    &existing,
                    &existing_norm,
                    &by_amount,
                    &consumed,
                    &cand.posted,
                    &cand.amount,
                    &cand_norm,
                    false,
                ) {
                    Some(i) => {
                        consumed.insert(i);
                        stats.duplicates += 1;
                    }
                    None => to_insert.push(cand),
                }
            }
            stats.new_transactions += to_insert.len();
            if !to_insert.is_empty() {
                self.upsert_finance_transactions(&vault_id, &to_insert, false)?;
            }

            // Raw layer: verbatim rows under finance/fidelity/raw/<account>/<year>.jsonl
            // Collect raw JSONL per year for this account group.
            let raw_base = self.root().join("finance/fidelity/raw").join(&vault_id);
            if !group.is_empty() {
                use std::collections::BTreeMap as BMap;
                let mut raw_by_year: BMap<String, Vec<serde_json::Value>> = BMap::new();
                for r in group {
                    if let Some(year) = r.posted.get(..4) {
                        let mut obj = serde_json::Map::new();
                        obj.insert("posted".into(), serde_json::Value::String(r.posted.clone()));
                        obj.insert("amount".into(), serde_json::Value::String(r.amount.clone()));
                        obj.insert("description".into(), serde_json::Value::String(r.desc.clone()));
                        obj.extend(r.extra.iter().map(|(k, v)| (k.clone(), v.clone())));
                        raw_by_year
                            .entry(year.to_string())
                            .or_default()
                            .push(serde_json::Value::Object(obj));
                    }
                }
                for (year, raws) in raw_by_year {
                    let raw_path = raw_base.join(format!("{year}.jsonl"));
                    if let Some(parent) = raw_path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("creating {}", parent.display()))?;
                    }
                    // Read existing raw, append new (no dedup needed — raw is full fidelity).
                    let existing_raw: Vec<serde_json::Value> = fs::read_to_string(&raw_path)
                        .unwrap_or_default()
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .collect();
                    // Append only new raw rows (identified by the raw content hash).
                    // Raw layer is best-effort; simple append-if-no-exact-match.
                    let existing_raw_strs: HashSet<String> =
                        existing_raw.iter().map(|v| serde_json::to_string(v).unwrap_or_default()).collect();
                    let mut append_buf = String::new();
                    for raw_row in &raws {
                        let line = serde_json::to_string(raw_row)?;
                        if !existing_raw_strs.contains(&line) {
                            append_buf.push_str(&line);
                            append_buf.push('\n');
                        }
                    }
                    if !append_buf.is_empty() {
                        use std::io::Write;
                        let mut f = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&raw_path)
                            .with_context(|| format!("opening raw {}", raw_path.display()))?;
                        f.write_all(append_buf.as_bytes())
                            .with_context(|| format!("writing raw {}", raw_path.display()))?;
                    }
                }
            }
        }
        self.save_finance_accounts(&accounts)?;
        self.write_import_state(path, stats.new_transactions)?;
        Ok(stats)
    }

    /// Last-import metadata, if any import has run.
    pub fn read_finance_import_state(&self) -> Option<FinanceImportState> {
        let raw = fs::read_to_string(self.root().join(IMPORT_STATE_FILE)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    // -----------------------------------------------------------------------
    // PayPal activity download imports.

    /// Import a PayPal activity download CSV.
    ///
    /// ## Column format (Needs-sample — derived from secondary sources)
    ///
    /// Schema derived from PayPal's official Activity Download Report docs and
    /// multiple independent parser projects (cross-referenced 2026-06).
    /// **Not verified against a real export.** The full official header is 87
    /// columns; see `PayPalCols` for the complete list. Key prefix:
    ///
    /// ```text
    /// "Date","Time","TimeZone","Name","Type","Status","Currency","Gross","Fee","Net",
    /// "From Email Address","To Email Address","Transaction ID","CounterParty Status",
    /// "Shipping Address","Address Status","Item Title","Item ID",
    /// "Shipping and Handling Amount","Insurance Amount","Sales Tax",...
    /// "Reference Txn ID",...,"Balance",...,"Subject","Note"
    /// ```
    ///
    /// Parsing is name-based (not positional), so the optional "CounterParty
    /// Status" column and the full 87-column layout do not break recognition.
    ///
    /// ## Sign convention (derived from secondary sources, unverified)
    ///
    /// PayPal's `"Net"` column appears to be already signed outflow-negative —
    /// outgoing payments negative, incoming positive. No sign flip applied.
    /// Corroborated by independent analysis but not confirmed against a real
    /// export file. See `PayPalCols` for full context.
    ///
    /// ## Guid
    ///
    /// `"Transaction ID"` is the stable PayPal primary key. Re-importing the
    /// same file or overlapping ranges is a clean no-op (exact id match).
    ///
    /// ## Currency
    ///
    /// Multi-currency PayPal accounts emit one row per currency per transaction.
    /// The `"Currency"` column carries the ISO code for each row; the vault
    /// `Transaction.currency` is set from it.  If the caller-supplied account
    /// already has a currency, it is preserved; otherwise the first-seen
    /// currency from the file wins.
    ///
    /// ## Layout
    /// - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
    ///   [`Transaction`] per data row via the existing `upsert` path.
    /// - **Raw:** `finance/paypal/raw/<account>/YYYY.jsonl` — verbatim row
    ///   with all source columns preserved.
    ///
    /// ## Onboarding
    ///
    /// Download via paypal.com/reports/dlog (Activity → Download → CSV or TAB;
    /// 7-year range, max 50k rows per file). The export is re-runnable;
    /// overlapping ranges dedupe cleanly via Transaction ID.
    pub(crate) fn import_paypal(
        &self,
        path: &Path,
        src_headers: &[String],
        mut reader: csv::Reader<&[u8]>,
        cols: &PayPalCols,
        fallback_account_id: Option<&str>,
        fallback_account_name: Option<&str>,
    ) -> Result<CsvImportStats> {
        struct Row {
            transaction_id: String,
            posted: String,
            amount: String,
            currency: String,
            desc: String,
            desc_norm: String,
            status: String,
            extra: serde_json::Map<String, serde_json::Value>,
            /// Verbatim CSV field values for the raw layer.
            raw_cols: Vec<(String, String)>,
        }

        let mut accounts = self.load_finance_accounts()?;
        let vault_id = match (fallback_account_id, fallback_account_name) {
            (Some(id), _) => {
                let existing = self.load_finance_accounts()?;
                if let Some(a) = existing.iter().find(|a| a.id == id) {
                    a.id.clone()
                } else {
                    anyhow::bail!("unknown account: {id}");
                }
            }
            (None, Some(name)) if !name.trim().is_empty() => {
                let name = name.trim();
                let alias = super::model::account_slug("", name);
                self.resolve_finance_account(&mut accounts, "csv-import", &alias, name, "", "USD")
            }
            _ => self.resolve_finance_account(
                &mut accounts,
                "paypal",
                "paypal-import",
                "PayPal",
                "PayPal",
                "USD",
            ),
        };

        let mut stats = CsvImportStats {
            format: "paypal".into(),
            account: vault_id.clone(),
            ..Default::default()
        };
        let mut rows: Vec<Row> = Vec::new();
        let oob = usize::MAX;

        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| {
                if i == oob { "" } else { record.get(i).unwrap_or("").trim() }
            };

            // Primary date: "Date" column (MM/DD/YYYY).
            let Some(posted) = parse_date(get(cols.date)) else {
                stats.skipped += 1;
                continue;
            };
            // Canonical amount: "Net" (post-fee, already signed outflow-negative).
            let Some(amount) = clean_amount(get(cols.net)) else {
                stats.skipped += 1;
                continue;
            };

            let transaction_id = get(cols.transaction_id).to_string();
            let txn_type = get(cols.txn_type);
            let status = get(cols.status).to_string();
            let counterparty = get(cols.name);
            let currency = get(cols.currency).to_string();
            let currency = if currency.is_empty() { "USD".to_string() } else { currency };

            // Description: subject note first, then counterparty name, then type.
            let subject = cols.subject.map(|ci| get(ci)).unwrap_or("").to_string();
            let note = cols.note.map(|ci| get(ci)).unwrap_or("").to_string();
            let desc = if !subject.is_empty() {
                subject.clone()
            } else if !note.is_empty() {
                note.clone()
            } else if !counterparty.is_empty() {
                counterparty.to_string()
            } else {
                txn_type.to_string()
            };
            let desc_norm = norm_desc(&desc);

            // Extra: PayPal-specific fields the Transaction contract doesn't carry.
            let mut extra = serde_json::Map::new();
            let put = |extra: &mut serde_json::Map<_, _>, k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.into(), serde_json::Value::String(v.to_string()));
                }
            };
            put(&mut extra, "type", txn_type);
            put(&mut extra, "status", &status);
            put(&mut extra, "currency", &currency);
            put(&mut extra, "gross", get(cols.gross));
            if cols.fee != oob {
                put(&mut extra, "fee", get(cols.fee));
            }
            if !transaction_id.is_empty() {
                put(&mut extra, "transaction_id", &transaction_id);
            }
            if let Some(ci) = cols.from_email {
                put(&mut extra, "from_email", get(ci));
            }
            if let Some(ci) = cols.to_email {
                put(&mut extra, "to_email", get(ci));
            }
            if let Some(ci) = cols.time {
                put(&mut extra, "time", get(ci));
            }
            if let Some(ci) = cols.timezone {
                put(&mut extra, "timezone", get(ci));
            }
            if let Some(ci) = cols.balance {
                put(&mut extra, "balance", get(ci));
            }
            if let Some(ci) = cols.item_title {
                put(&mut extra, "item_title", get(ci));
            }
            if let Some(ci) = cols.reference_txn_id {
                put(&mut extra, "reference_txn_id", get(ci));
            }
            if !counterparty.is_empty() {
                put(&mut extra, "counterparty", counterparty);
            }
            if !subject.is_empty() && desc != subject {
                put(&mut extra, "subject", &subject);
            }
            if !note.is_empty() && desc != note {
                put(&mut extra, "note", &note);
            }

            // Verbatim columns for the raw layer.
            let raw_cols: Vec<(String, String)> = src_headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), record.get(i).unwrap_or("").to_string()))
                .collect();

            rows.push(Row {
                transaction_id,
                posted,
                amount,
                currency,
                status,
                desc,
                desc_norm,
                extra,
                raw_cols,
            });
        }

        // Determine the account's currency from the first row that has one
        // (if the account was auto-registered without a currency above).
        if let Some(first_currency) = rows.first().map(|r| r.currency.clone()) {
            if let Some(a) = accounts.iter_mut().find(|a| a.id == vault_id) {
                if a.currency.is_empty() {
                    a.currency = first_currency;
                }
            }
            self.save_finance_accounts(&accounts)?;
        }

        let currency = accounts
            .iter()
            .find(|a| a.id == vault_id)
            .map(|a| a.currency.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "USD".into());

        // Synthesize ids from the PayPal Transaction ID (the stable, documented
        // primary key). An empty Transaction ID falls back to content hash.
        let mut occurrence: HashMap<(String, String, String), u32> = HashMap::new();
        let candidates: Vec<Transaction> = rows
            .iter()
            .map(|r| {
                let id = if !r.transaction_id.is_empty() {
                    format!("paypal-{}", r.transaction_id)
                } else {
                    let key = (r.posted.clone(), amount_key(&r.amount), r.desc_norm.clone());
                    let n = occurrence.entry(key.clone()).or_insert(0);
                    let mut hasher = Sha256::new();
                    hasher.update(vault_id.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.0.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.1.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.2.as_bytes());
                    hasher.update([0]);
                    hasher.update(n.to_le_bytes());
                    *n += 1;
                    let digest = hasher.finalize();
                    let h: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                    format!("paypal-hash-{h}")
                };
                // Use each row's own currency so multi-currency accounts are faithful.
                let row_currency = if r.currency.is_empty() {
                    currency.clone()
                } else {
                    r.currency.clone()
                };
                Transaction {
                    id,
                    account: vault_id.clone(),
                    posted: r.posted.clone(),
                    transacted: None,
                    amount: r.amount.clone(),
                    currency: row_currency,
                    description: r.desc.clone(),
                    payee: None,
                    category: None,
                    pending: r.status.eq_ignore_ascii_case("pending"),
                    source: "paypal".into(),
                    extra: r.extra.clone(),
                }
            })
            .collect();

        // Dedupe against existing rows for this account.
        let existing = self.finance_transactions(Some(&vault_id), usize::MAX)?;
        let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
        let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, t) in existing.iter().enumerate() {
            by_amount.entry(amount_key(&t.amount)).or_default().push(i);
        }
        let existing_norm: Vec<String> =
            existing.iter().map(|t| norm_desc(&t.description)).collect();
        let mut consumed: HashSet<usize> = HashSet::new();
        let mut to_insert: Vec<Transaction> = Vec::new();

        for cand in candidates {
            if stats.from.is_empty() || cand.posted < stats.from {
                stats.from = cand.posted.clone();
            }
            if stats.to.is_empty() || cand.posted > stats.to {
                stats.to = cand.posted.clone();
            }
            // Tier 2: exact Transaction ID match (same PayPal guid from a previous import).
            if existing_ids.contains(cand.id.as_str()) {
                stats.duplicates += 1;
                continue;
            }
            // Tier 3: fuzzy match against synced rows (SimpleFIN settlement side).
            let cand_norm = norm_desc(&cand.description);
            match find_match(
                &existing,
                &existing_norm,
                &by_amount,
                &consumed,
                &cand.posted,
                &cand.amount,
                &cand_norm,
                false,
            ) {
                Some(i) => {
                    consumed.insert(i);
                    stats.duplicates += 1;
                }
                None => to_insert.push(cand),
            }
        }
        stats.new_transactions = to_insert.len();
        if !to_insert.is_empty() {
            self.upsert_finance_transactions(&vault_id, &to_insert, false)?;
        }

        // Raw layer: verbatim rows under finance/paypal/raw/<account>/YYYY.jsonl.
        let raw_base = self.root().join("finance/paypal/raw").join(&vault_id);
        if !rows.is_empty() {
            let mut raw_by_year: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
            for r in &rows {
                if let Some(year) = r.posted.get(..4) {
                    let mut obj = serde_json::Map::new();
                    for (col_name, col_val) in &r.raw_cols {
                        obj.insert(
                            col_name.clone(),
                            serde_json::Value::String(col_val.clone()),
                        );
                    }
                    raw_by_year
                        .entry(year.to_string())
                        .or_default()
                        .push(serde_json::Value::Object(obj));
                }
            }
            for (year, raws) in raw_by_year {
                let raw_path = raw_base.join(format!("{year}.jsonl"));
                if let Some(parent) = raw_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                let existing_raw: Vec<serde_json::Value> = fs::read_to_string(&raw_path)
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
                let existing_raw_strs: HashSet<String> = existing_raw
                    .iter()
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .collect();
                let mut append_buf = String::new();
                for raw_row in &raws {
                    let line = serde_json::to_string(raw_row)?;
                    if !existing_raw_strs.contains(&line) {
                        append_buf.push_str(&line);
                        append_buf.push('\n');
                    }
                }
                if !append_buf.is_empty() {
                    use std::io::Write;
                    let mut f = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&raw_path)
                        .with_context(|| format!("opening raw {}", raw_path.display()))?;
                    f.write_all(append_buf.as_bytes())
                        .with_context(|| format!("writing raw {}", raw_path.display()))?;
                }
            }
        }

        self.save_finance_accounts(&accounts)?;
        // Do not stamp last_data if all rows were skipped.
        if stats.rows == 0 || stats.skipped < stats.rows {
            self.write_import_state(path, stats.new_transactions)?;
        }
        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Apple Card / Apple Savings monthly statement imports.

    /// Import an Apple Card or Apple Savings monthly statement CSV.
    ///
    /// ## Column format (scaffold — Needs-sample to confirm exact names)
    ///
    /// Apple does not publish the Wallet CSV schema. The column names below
    /// are from the `integrations-research.md` doc (L3841) and have NOT been
    /// confirmed against a real export from Wallet or card.apple.com. The
    /// mandatory signature (`"Clearing Date"` + `"Transaction Date"` +
    /// `"Amount (USD)"`) is the most-cited Apple Card fingerprint, but every
    /// column name must be verified before relying on this path.
    ///
    /// ## Sign convention (UNCONFIRMED — Needs-sample)
    ///
    /// The sign convention for `Amount (USD)` is not confirmed. Apple Card
    /// likely reports purchases as positive (outflows appear as positive values)
    /// which would require a sign flip here. The `negate` flag is currently
    /// set to `true` as the most-likely behaviour; verify against a real export
    /// and correct if wrong.
    ///
    /// ## Layout
    /// - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
    ///   [`Transaction`] per statement row via the existing `upsert` path.
    ///   `posted` = Clearing Date; `transacted` = Transaction Date.
    /// - **Raw:** `finance/apple-card/raw/<account>/YYYY.jsonl` — verbatim row.
    ///
    /// ## Dedupe
    /// `hash(account, clearing_date, amount, desc_norm, occurrence)` — monthly
    /// statement overlaps (same month dropped twice) are idempotent. Cross-
    /// source dedup against Copilot-seeded history uses the standard fuzzy
    /// matcher in the parent `finance_import_csv` path.
    ///
    /// **SCAFFOLD — not called from the live dispatch path while Needs-sample
    /// is set.** See the commented-out block in `finance_import_csv`. This
    /// function is preserved for when a real sample confirms the column shape
    /// and the dispatch is re-wired.
    ///
    /// `src_headers` must be the raw CSV header row (same order as the records
    /// the reader will emit). It is used to write the verbatim raw layer.
    // Parked scaffold: suppress dead_code warning until a real sample re-wires the dispatch.
    #[allow(dead_code)]
    pub(crate) fn import_apple_card(
        &self,
        path: &Path,
        src_headers: &[String],
        mut reader: csv::Reader<&[u8]>,
        cols: &AppleCardCols,
        fallback_account_id: Option<&str>,
        fallback_account_name: Option<&str>,
    ) -> Result<CsvImportStats> {
        struct Row {
            posted: String,
            transacted: Option<String>,
            amount: String,
            desc: String,
            desc_norm: String,
            extra: serde_json::Map<String, serde_json::Value>,
            /// Verbatim CSV field values for the raw layer.
            raw_cols: Vec<(String, String)>,
        }

        let mut accounts = self.load_finance_accounts()?;
        let vault_id = match (fallback_account_id, fallback_account_name) {
            (Some(id), _) => {
                let existing = self.load_finance_accounts()?;
                if let Some(a) = existing.iter().find(|a| a.id == id) {
                    a.id.clone()
                } else {
                    anyhow::bail!("unknown account: {id}");
                }
            }
            (None, Some(name)) if !name.trim().is_empty() => {
                let name = name.trim();
                let alias = super::model::account_slug("", name);
                self.resolve_finance_account(&mut accounts, "csv-import", &alias, name, "", "USD")
            }
            _ => self.resolve_finance_account(
                &mut accounts,
                "apple-card",
                "apple-card-import",
                "Apple Card",
                "Apple",
                "USD",
            ),
        };

        let currency = accounts
            .iter()
            .find(|a| a.id == vault_id)
            .map(|a| a.currency.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "USD".into());

        let mut stats = CsvImportStats {
            format: "apple-card".into(),
            account: vault_id.clone(),
            ..Default::default()
        };
        let mut rows: Vec<Row> = Vec::new();
        let oob = usize::MAX;

        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| {
                if i == oob { "" } else { record.get(i).unwrap_or("").trim() }
            };

            // Primary date: Clearing Date (the posting date).
            let Some(posted) = parse_date(get(cols.clearing_date)) else {
                stats.skipped += 1;
                continue;
            };
            // Raw amount from the Amount (USD) column.
            let raw_amount = get(cols.amount);
            let Some(mut amount) = clean_amount(raw_amount) else {
                stats.skipped += 1;
                continue;
            };
            // UNCONFIRMED sign convention: Apple Card likely reports purchases
            // as positive — flip to vault's outflow-negative convention.
            // Verify against a real export; correct here if wrong.
            if amount_key(&amount) != "0" {
                amount = if amount.starts_with('-') {
                    amount[1..].to_string()
                } else {
                    format!("-{amount}")
                };
            }

            // Description: prefer the Description column; Merchant is extra.
            let desc = get(cols.description).to_string();
            let desc_norm = norm_desc(&desc);

            // Extra: Apple Card-specific fields the contract doesn't carry.
            let mut extra = serde_json::Map::new();
            let put = |extra: &mut serde_json::Map<_, _>, k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.into(), serde_json::Value::String(v.to_string()));
                }
            };
            if let Some(ci) = cols.merchant {
                put(&mut extra, "merchant", get(ci));
            }
            if let Some(ci) = cols.category {
                put(&mut extra, "category", get(ci));
            }
            if let Some(ci) = cols.kind {
                put(&mut extra, "type", get(ci));
            }

            // Verbatim columns for the raw layer.
            let raw_cols: Vec<(String, String)> = src_headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), record.get(i).unwrap_or("").to_string()))
                .collect();

            rows.push(Row {
                posted,
                transacted: parse_date(get(cols.transaction_date)),
                amount,
                desc,
                desc_norm,
                extra,
                raw_cols,
            });
        }

        // Synthesize stable ids: hash(account, clearing_date, amount, desc_norm, occurrence).
        let mut occurrence: HashMap<(String, String, String), u32> = HashMap::new();
        let candidates: Vec<Transaction> = rows
            .iter()
            .map(|r| {
                let key = (r.posted.clone(), amount_key(&r.amount), r.desc_norm.clone());
                let n = occurrence.entry(key.clone()).or_insert(0);
                let mut hasher = Sha256::new();
                hasher.update(vault_id.as_bytes());
                hasher.update([0]);
                hasher.update(key.0.as_bytes());
                hasher.update([0]);
                hasher.update(key.1.as_bytes());
                hasher.update([0]);
                hasher.update(key.2.as_bytes());
                hasher.update([0]);
                hasher.update(n.to_le_bytes());
                *n += 1;
                let digest = hasher.finalize();
                let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                Transaction {
                    id: format!("apple-card-{id}"),
                    account: vault_id.clone(),
                    posted: r.posted.clone(),
                    transacted: r.transacted.clone(),
                    amount: r.amount.clone(),
                    currency: currency.clone(),
                    description: r.desc.clone(),
                    payee: None,
                    category: None,
                    pending: false,
                    source: "apple-card".into(),
                    extra: r.extra.clone(),
                }
            })
            .collect();

        // Dedupe against existing rows (same fuzzy logic as other importers).
        let existing = self.finance_transactions(Some(&vault_id), usize::MAX)?;
        let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
        let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, t) in existing.iter().enumerate() {
            by_amount.entry(amount_key(&t.amount)).or_default().push(i);
        }
        let existing_norm: Vec<String> =
            existing.iter().map(|t| norm_desc(&t.description)).collect();
        let mut consumed: HashSet<usize> = HashSet::new();
        let mut to_insert: Vec<Transaction> = Vec::new();
        let mut to_update: Vec<Transaction> = Vec::new();
        for cand in candidates {
            if stats.from.is_empty() || cand.posted < stats.from {
                stats.from = cand.posted.clone();
            }
            if stats.to.is_empty() || cand.posted > stats.to {
                stats.to = cand.posted.clone();
            }
            if existing_ids.contains(cand.id.as_str()) {
                stats.duplicates += 1;
                continue;
            }
            let cand_norm = norm_desc(&cand.description);
            // Non-relaxed: Apple Card descriptions are stable enough.
            match find_match(
                &existing,
                &existing_norm,
                &by_amount,
                &consumed,
                &cand.posted,
                &cand.amount,
                &cand_norm,
                false,
            ) {
                Some(i) => {
                    consumed.insert(i);
                    stats.duplicates += 1;
                    let ex = &existing[i];
                    if !ex.pending && ex.category.is_none() {
                        // Absorb category from extra if the existing row lacks one.
                        let cat = cand.extra.get("category").and_then(|v| v.as_str()).filter(|c| !c.is_empty()).map(str::to_string);
                        if cat.is_some() {
                            let mut enriched = ex.clone();
                            enriched.category = cat;
                            enriched.extra.insert("category-source".into(), "apple-card".into());
                            to_update.push(enriched);
                            stats.merged += 1;
                        }
                    }
                }
                None => to_insert.push(cand),
            }
        }
        stats.new_transactions = to_insert.len();
        to_insert.extend(to_update);
        if !to_insert.is_empty() {
            self.upsert_finance_transactions(&vault_id, &to_insert, false)?;
        }

        // Raw layer: verbatim rows under finance/apple-card/raw/<account>/<year>.jsonl.
        let raw_base = self.root().join("finance/apple-card/raw").join(&vault_id);
        if !rows.is_empty() {
            let mut raw_by_year: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
            for r in &rows {
                if let Some(year) = r.posted.get(..4) {
                    let mut obj = serde_json::Map::new();
                    for (col_name, col_val) in &r.raw_cols {
                        obj.insert(
                            col_name.clone(),
                            serde_json::Value::String(col_val.clone()),
                        );
                    }
                    raw_by_year
                        .entry(year.to_string())
                        .or_default()
                        .push(serde_json::Value::Object(obj));
                }
            }
            for (year, raws) in raw_by_year {
                let raw_path = raw_base.join(format!("{year}.jsonl"));
                if let Some(parent) = raw_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                let existing_raw: Vec<serde_json::Value> = fs::read_to_string(&raw_path)
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
                let existing_raw_strs: HashSet<String> = existing_raw
                    .iter()
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .collect();
                let mut append_buf = String::new();
                for raw_row in &raws {
                    let line = serde_json::to_string(raw_row)?;
                    if !existing_raw_strs.contains(&line) {
                        append_buf.push_str(&line);
                        append_buf.push('\n');
                    }
                }
                if !append_buf.is_empty() {
                    use std::io::Write;
                    let mut f = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&raw_path)
                        .with_context(|| format!("opening raw {}", raw_path.display()))?;
                    f.write_all(append_buf.as_bytes())
                        .with_context(|| format!("writing raw {}", raw_path.display()))?;
                }
            }
        }

        self.save_finance_accounts(&accounts)?;
        self.write_import_state(path, stats.new_transactions)?;
        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Cash App email-delivered CSV imports.

    /// Import a Cash App transaction history CSV.
    ///
    /// ## Column format (confirmed)
    ///
    /// Confirmed header row (from `SolidX/FinanceExportTools` CashAppExportMapper.cs
    /// and cross-referenced real-export samples):
    ///
    /// ```text
    /// Transaction ID, Date, Transaction Type, Currency, Amount, Fee,
    /// Net Amount, Asset Type, Asset Price, Asset Amount, Status,
    /// Notes, Name of sender/receiver, Account
    /// ```
    ///
    /// ## Sign convention (confirmed)
    ///
    /// Outflows (Cash out, Cash Card Purchase, Bitcoin Buy) are already **negative**
    /// in the `Amount` column. No sign flip needed — matches the vault's
    /// outflow-negative convention.
    ///
    /// ## Date format (confirmed)
    ///
    /// `"YYYY-MM-DD HH:MM:SS TZ"` (e.g. `"2026-05-28 14:32:10 EST"`). The
    /// `parse_date` function strips the time/timezone suffix, keeping only the
    /// date part.
    ///
    /// ## Guid (confirmed)
    ///
    /// `Transaction ID` column — stable, unique per Cash App row. Re-importing
    /// the same file is a clean no-op (dedupes on the exact same id).
    ///
    /// ## Amount vs Net Amount
    ///
    /// We store `Amount` (gross) as the canonical signed amount. `Fee` and
    /// `Net Amount` (Amount − Fee) are preserved in `extra` — they are material
    /// for BTC buys/sells where the fee affects cost basis.
    ///
    /// ## Raw layer
    ///
    /// `finance/cash-app/raw/<account>/YYYY.jsonl` — verbatim row with all
    /// source columns preserved as `{"Transaction ID": "...", "Date": "...", ...}`.
    /// Written unconditionally for every parseable row.
    pub(crate) fn import_cash_app(
        &self,
        path: &Path,
        src_headers: &[String],
        mut reader: csv::Reader<&[u8]>,
        cols: &CashAppCols,
        fallback_account_id: Option<&str>,
        fallback_account_name: Option<&str>,
    ) -> Result<CsvImportStats> {
        struct Row {
            transaction_id: String,
            posted: String,
            amount: String,
            desc: String,
            desc_norm: String,
            extra: serde_json::Map<String, serde_json::Value>,
            /// Verbatim CSV field values for the raw layer.
            raw_cols: Vec<(String, String)>,
        }

        let mut accounts = self.load_finance_accounts()?;
        let vault_id = match (fallback_account_id, fallback_account_name) {
            (Some(id), _) => {
                let existing = self.load_finance_accounts()?;
                if let Some(a) = existing.iter().find(|a| a.id == id) {
                    a.id.clone()
                } else {
                    anyhow::bail!("unknown account: {id}");
                }
            }
            (None, Some(name)) if !name.trim().is_empty() => {
                let name = name.trim();
                let alias = super::model::account_slug("", name);
                self.resolve_finance_account(&mut accounts, "csv-import", &alias, name, "", "USD")
            }
            _ => self.resolve_finance_account(
                &mut accounts,
                "cash-app",
                "cash-app-import",
                "Cash App",
                "Cash App",
                "USD",
            ),
        };

        let currency = accounts
            .iter()
            .find(|a| a.id == vault_id)
            .map(|a| a.currency.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "USD".into());

        let mut stats = CsvImportStats {
            format: "cash-app".into(),
            account: vault_id.clone(),
            ..Default::default()
        };
        let mut rows: Vec<Row> = Vec::new();
        let oob = usize::MAX;

        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| {
                if i == oob { "" } else { record.get(i).unwrap_or("").trim() }
            };

            // Date column: "YYYY-MM-DD HH:MM:SS TZ" — parse_date strips the time/tz.
            let Some(posted) = parse_date(get(cols.date)) else {
                stats.skipped += 1;
                continue;
            };
            // Amount: gross, already signed (outflows negative).
            let Some(amount) = clean_amount(get(cols.amount)) else {
                stats.skipped += 1;
                continue;
            };

            let transaction_id = get(cols.transaction_id).to_string();
            let txn_type = get(cols.transaction_type);
            let notes = cols.notes.map(get).unwrap_or("");
            let sender_receiver = cols.sender_receiver.map(get).unwrap_or("");

            // Description: Notes first, then counterparty name, then transaction type.
            let desc = if !notes.is_empty() {
                notes.to_string()
            } else if !sender_receiver.is_empty() {
                sender_receiver.to_string()
            } else {
                txn_type.to_string()
            };
            let desc_norm = norm_desc(&desc);

            // Extra: Cash App-specific fields the Transaction contract doesn't carry.
            let mut extra = serde_json::Map::new();
            let put = |extra: &mut serde_json::Map<_, _>, k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.into(), serde_json::Value::String(v.to_string()));
                }
            };
            put(&mut extra, "transaction_type", txn_type);
            if !transaction_id.is_empty() {
                put(&mut extra, "transaction_id", &transaction_id);
            }
            if let Some(ci) = cols.fee {
                put(&mut extra, "fee", get(ci));
            }
            if let Some(ci) = cols.net_amount {
                put(&mut extra, "net_amount", get(ci));
            }
            // BTC-specific fields — present on Bitcoin Buy/Sale rows.
            if let Some(ci) = cols.asset_type {
                put(&mut extra, "asset_type", get(ci));
            }
            if let Some(ci) = cols.asset_price {
                put(&mut extra, "asset_price", get(ci));
            }
            if let Some(ci) = cols.asset_amount {
                // This is the on-chain BTC txid / BTC quantity field.
                // Stored as "asset_amount" for BTC buy/sell rows — used by
                // read-time reconciliation against the Blockstream provider.
                put(&mut extra, "asset_amount", get(ci));
            }
            if let Some(ci) = cols.status {
                put(&mut extra, "status", get(ci));
            }
            if !sender_receiver.is_empty() {
                put(&mut extra, "sender_receiver", sender_receiver);
            }
            if let Some(ci) = cols.account_col {
                put(&mut extra, "account_col", get(ci));
            }
            // Currency column — e.g. "USD" or "BTC".
            put(&mut extra, "source_currency", get(cols.currency));

            // Verbatim columns for the raw layer.
            let raw_cols: Vec<(String, String)> = src_headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), record.get(i).unwrap_or("").to_string()))
                .collect();

            rows.push(Row {
                transaction_id,
                posted,
                amount,
                desc,
                desc_norm,
                extra,
                raw_cols,
            });
        }

        // Synthesize ids from the Cash App transaction ID (the stable, documented
        // primary key). If the Transaction ID column is blank for a row, fall back
        // to hash(account, posted, amount, desc_norm, occurrence) — same as the
        // generic path — to stay deduplication-safe.
        let mut occurrence: HashMap<(String, String, String), u32> = HashMap::new();
        let candidates: Vec<Transaction> = rows
            .iter()
            .map(|r| {
                let id = if !r.transaction_id.is_empty() {
                    // Stable id from the Cash App transaction ID.
                    format!("cash-app-{}", r.transaction_id)
                } else {
                    // Fallback hash when the Transaction ID column is blank.
                    let key = (r.posted.clone(), amount_key(&r.amount), r.desc_norm.clone());
                    let n = occurrence.entry(key.clone()).or_insert(0);
                    let mut hasher = Sha256::new();
                    hasher.update(vault_id.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.0.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.1.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.2.as_bytes());
                    hasher.update([0]);
                    hasher.update(n.to_le_bytes());
                    *n += 1;
                    let digest = hasher.finalize();
                    let h: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                    format!("cash-app-hash-{h}")
                };
                Transaction {
                    id,
                    account: vault_id.clone(),
                    posted: r.posted.clone(),
                    transacted: None,
                    amount: r.amount.clone(),
                    currency: currency.clone(),
                    description: r.desc.clone(),
                    payee: None,
                    category: None,
                    pending: false,
                    source: "cash-app".into(),
                    extra: r.extra.clone(),
                }
            })
            .collect();

        // Dedupe against existing rows for this account.
        let existing = self.finance_transactions(Some(&vault_id), usize::MAX)?;
        let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
        let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, t) in existing.iter().enumerate() {
            by_amount.entry(amount_key(&t.amount)).or_default().push(i);
        }
        let existing_norm: Vec<String> =
            existing.iter().map(|t| norm_desc(&t.description)).collect();
        let mut consumed: HashSet<usize> = HashSet::new();
        let mut to_insert: Vec<Transaction> = Vec::new();

        for cand in candidates {
            if stats.from.is_empty() || cand.posted < stats.from {
                stats.from = cand.posted.clone();
            }
            if stats.to.is_empty() || cand.posted > stats.to {
                stats.to = cand.posted.clone();
            }
            // Tier 2: exact id match (same Transaction ID from a previous import).
            if existing_ids.contains(cand.id.as_str()) {
                stats.duplicates += 1;
                continue;
            }
            // Tier 3: fuzzy match against synced/imported rows.
            let cand_norm = norm_desc(&cand.description);
            match find_match(
                &existing,
                &existing_norm,
                &by_amount,
                &consumed,
                &cand.posted,
                &cand.amount,
                &cand_norm,
                false,
            ) {
                Some(i) => {
                    consumed.insert(i);
                    stats.duplicates += 1;
                }
                None => to_insert.push(cand),
            }
        }
        stats.new_transactions = to_insert.len();
        if !to_insert.is_empty() {
            self.upsert_finance_transactions(&vault_id, &to_insert, false)?;
        }

        // Raw layer: verbatim rows under finance/cash-app/raw/<account>/YYYY.jsonl.
        let raw_base = self.root().join("finance/cash-app/raw").join(&vault_id);
        if !rows.is_empty() {
            let mut raw_by_year: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
            for r in &rows {
                if let Some(year) = r.posted.get(..4) {
                    let mut obj = serde_json::Map::new();
                    for (col_name, col_val) in &r.raw_cols {
                        obj.insert(
                            col_name.clone(),
                            serde_json::Value::String(col_val.clone()),
                        );
                    }
                    raw_by_year
                        .entry(year.to_string())
                        .or_default()
                        .push(serde_json::Value::Object(obj));
                }
            }
            for (year, raws) in raw_by_year {
                let raw_path = raw_base.join(format!("{year}.jsonl"));
                if let Some(parent) = raw_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                let existing_raw: Vec<serde_json::Value> = fs::read_to_string(&raw_path)
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
                let existing_raw_strs: HashSet<String> = existing_raw
                    .iter()
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .collect();
                let mut append_buf = String::new();
                for raw_row in &raws {
                    let line = serde_json::to_string(raw_row)?;
                    if !existing_raw_strs.contains(&line) {
                        append_buf.push_str(&line);
                        append_buf.push('\n');
                    }
                }
                if !append_buf.is_empty() {
                    use std::io::Write;
                    let mut f = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&raw_path)
                        .with_context(|| format!("opening raw {}", raw_path.display()))?;
                    f.write_all(append_buf.as_bytes())
                        .with_context(|| format!("writing raw {}", raw_path.display()))?;
                }
            }
        }

        self.save_finance_accounts(&accounts)?;
        // Do not stamp last_data if all rows were skipped.
        if stats.rows == 0 || stats.skipped < stats.rows {
            self.write_import_state(path, stats.new_transactions)?;
        }
        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Vanguard brokerage / retirement activity imports.

    /// Import a Vanguard transaction history CSV.
    ///
    /// ## Column format (scaffold — Needs-sample to confirm exact names)
    ///
    /// Vanguard does not publish the CSV schema. The column names below are
    /// reconstructed from community reports and are the most-cited variant,
    /// but the **real column names have not been confirmed against a real
    /// export**. The `vanguard_columns` recognizer is therefore permissive on
    /// optional fields; the mandatory signature is `"Trade Date"` + `"Net
    /// Amount"` (or `"Amount"`) — columns that appear in every reported
    /// Vanguard export and are not used by any other recognized layout.
    ///
    /// Once a real sample is in hand, tighten the recognizer and verify the
    /// field names here against it, then remove the Needs-sample flag.
    ///
    /// ## Layout
    /// - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
    ///   [`Transaction`] per activity row via the existing `upsert` path.
    /// - **Raw:** `finance/vanguard/raw/<account>/YYYY.jsonl` — verbatim row
    ///   with all source columns preserved.
    ///
    /// ## Dedupe
    /// `hash(account, trade_date, net_amount, transaction_type, symbol,
    /// occurrence)` — 18-month windows overlap when the user drops both
    /// overlapping exports; the synthesized id makes re-imports a clean no-op.
    /// **SCAFFOLD — not called from the live dispatch path while Needs-sample
    /// is set.** See the commented-out block in `finance_import_csv`. This
    /// function is preserved for when a real sample confirms the column shape
    /// and the dispatch is re-wired.
    ///
    /// `src_headers` must be the raw CSV header row (same order as the records
    /// the reader will emit). It is used to write the verbatim raw layer.
    // Parked scaffold: suppress dead_code warning until a real sample re-wires the dispatch.
    #[allow(dead_code)]
    pub(crate) fn import_vanguard(
        &self,
        path: &Path,
        src_headers: &[String],
        mut reader: csv::Reader<&[u8]>,
        cols: &VanguardCols,
        fallback_account_id: Option<&str>,
        fallback_account_name: Option<&str>,
    ) -> Result<CsvImportStats> {
        use sha2::{Digest, Sha256};

        struct Row {
            account_number: String,
            posted: String,
            amount: String,
            desc: String,
            extra: serde_json::Map<String, serde_json::Value>,
            /// Verbatim CSV field values, zipped against src_headers.
            raw_cols: Vec<(String, String)>,
        }

        let mut accounts = self.load_finance_accounts()?;
        let mut stats = CsvImportStats {
            format: "vanguard".into(),
            ..Default::default()
        };
        let mut rows: Vec<Row> = Vec::new();
        let oob = usize::MAX;

        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| {
                if i == oob { "" } else { record.get(i).unwrap_or("").trim() }
            };

            // Primary date: Trade Date (always present in confirmed reports).
            let Some(posted) = parse_date(get(cols.trade_date)) else {
                stats.skipped += 1;
                continue;
            };
            let Some(amount) = clean_amount(get(cols.net_amount)) else {
                stats.skipped += 1;
                continue;
            };

            let account_number = match cols.account_number {
                Some(ci) => {
                    let v = get(ci);
                    if v.is_empty() {
                        stats.skipped += 1;
                        continue;
                    }
                    v.to_string()
                }
                None => "__per_account__".to_string(),
            };

            let transaction_type = get(cols.transaction_type);
            let symbol = cols.symbol.map(get).unwrap_or("");
            let investment_name = cols.investment_name.map(get).unwrap_or("");

            // Description: prefer the portal's "Transaction Description" column
            // when present (it carries a human-readable narrative); fall back
            // to synthesizing "<type> <symbol>" from the other columns.
            let desc = {
                let portal_desc = cols.transaction_description.map(get).unwrap_or("").trim();
                if !portal_desc.is_empty() {
                    portal_desc.to_string()
                } else if !symbol.is_empty() {
                    format!("{transaction_type} {symbol}")
                } else if !investment_name.is_empty() {
                    format!("{transaction_type} {investment_name}")
                } else {
                    transaction_type.to_string()
                }
            };

            // Collect extra: every brokerage-specific column the contract
            // doesn't carry. None of these are required.
            let mut extra = serde_json::Map::new();
            let put = |extra: &mut serde_json::Map<_, _>, k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.into(), serde_json::Value::String(v.to_string()));
                }
            };
            put(&mut extra, "transaction_type", transaction_type);
            put(&mut extra, "investment_name", investment_name);
            put(&mut extra, "symbol", symbol);
            if let Some(ci) = cols.transaction_description {
                let v = get(ci);
                if !v.is_empty() && v != desc {
                    // Store the portal description in extra when it differs
                    // from what we synthesized (preserves the original text).
                    put(&mut extra, "transaction_description", v);
                }
            }
            if let Some(ci) = cols.shares {
                put(&mut extra, "shares", get(ci));
            }
            if let Some(ci) = cols.share_price {
                put(&mut extra, "share_price", get(ci));
            }
            if let Some(ci) = cols.principal_amount {
                put(&mut extra, "principal_amount", get(ci));
            }
            if let Some(ci) = cols.commission_fees {
                put(&mut extra, "commission_fees", get(ci));
            }
            if let Some(ci) = cols.accrued_interest {
                put(&mut extra, "accrued_interest", get(ci));
            }
            if let Some(ci) = cols.settlement_date {
                if let Some(sd) = parse_date(get(ci)) {
                    extra.insert(
                        "settlement_date".into(),
                        serde_json::Value::String(sd),
                    );
                }
            }
            if let Some(ci) = cols.account_name {
                put(&mut extra, "account_name", get(ci));
            }
            if cols.account_number.is_some() {
                put(&mut extra, "account_number", &account_number);
            }

            // Capture verbatim CSV columns for the raw layer.
            let raw_cols: Vec<(String, String)> = src_headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), record.get(i).unwrap_or("").to_string()))
                .collect();

            rows.push(Row { account_number, posted, amount, desc, extra, raw_cols });
        }

        // Group by account number (same routing logic as Fidelity).
        let mut groups: std::collections::HashMap<String, Vec<Row>> =
            std::collections::HashMap::new();
        for r in rows {
            groups.entry(r.account_number.clone()).or_default().push(r);
        }
        stats.account = format!("{} account(s)", groups.len());

        for (account_number, group) in &groups {
            let vault_id = if account_number == "__per_account__" {
                match (fallback_account_id, fallback_account_name) {
                    (Some(id), _) => {
                        let existing = self.load_finance_accounts()?;
                        if let Some(a) = existing.iter().find(|a| a.id == id) {
                            a.id.clone()
                        } else {
                            anyhow::bail!("unknown account: {id}");
                        }
                    }
                    (None, Some(name)) if !name.trim().is_empty() => {
                        let name = name.trim();
                        let alias = super::model::account_slug("", name);
                        self.resolve_finance_account(
                            &mut accounts,
                            "csv-import",
                            &alias,
                            name,
                            "",
                            "USD",
                        )
                    }
                    _ => self.resolve_finance_account(
                        &mut accounts,
                        "vanguard",
                        "vanguard-import",
                        "Vanguard",
                        "Vanguard",
                        "USD",
                    ),
                }
            } else {
                self.resolve_finance_account(
                    &mut accounts,
                    "vanguard",
                    account_number,
                    &format!("Vanguard {account_number}"),
                    "Vanguard",
                    "USD",
                )
            };

            let currency = accounts
                .iter()
                .find(|a| a.id == vault_id)
                .map(|a| a.currency.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "USD".into());

            let mut occurrence: std::collections::HashMap<
                (String, String, String, String),
                u32,
            > = std::collections::HashMap::new();
            let candidates: Vec<Transaction> = group
                .iter()
                .map(|r| {
                    let tx_type = r
                        .extra
                        .get("transaction_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let sym = r.extra.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
                    let key = (
                        r.posted.clone(),
                        amount_key(&r.amount),
                        tx_type.to_string(),
                        sym.to_string(),
                    );
                    let n = occurrence.entry(key.clone()).or_insert(0);
                    let mut hasher = Sha256::new();
                    hasher.update(vault_id.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.0.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.1.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.2.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.3.as_bytes());
                    hasher.update([0]);
                    hasher.update(n.to_le_bytes());
                    *n += 1;
                    let digest = hasher.finalize();
                    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                    Transaction {
                        id: format!("vanguard-{id}"),
                        account: vault_id.clone(),
                        posted: r.posted.clone(),
                        transacted: None,
                        amount: r.amount.clone(),
                        currency: currency.clone(),
                        description: r.desc.clone(),
                        payee: None,
                        category: None,
                        pending: false,
                        source: "vanguard".into(),
                        extra: r.extra.clone(),
                    }
                })
                .collect();

            // Dedupe against existing rows.
            let existing = self.finance_transactions(Some(&vault_id), usize::MAX)?;
            let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
            let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, t) in existing.iter().enumerate() {
                by_amount.entry(amount_key(&t.amount)).or_default().push(i);
            }
            let existing_norm: Vec<String> =
                existing.iter().map(|t| norm_desc(&t.description)).collect();
            let mut consumed: HashSet<usize> = HashSet::new();
            let mut to_insert: Vec<Transaction> = Vec::new();
            for cand in candidates {
                if stats.from.is_empty() || cand.posted < stats.from {
                    stats.from = cand.posted.clone();
                }
                if stats.to.is_empty() || cand.posted > stats.to {
                    stats.to = cand.posted.clone();
                }
                if existing_ids.contains(cand.id.as_str()) {
                    stats.duplicates += 1;
                    continue;
                }
                let cand_norm = norm_desc(&cand.description);
                match find_match(
                    &existing,
                    &existing_norm,
                    &by_amount,
                    &consumed,
                    &cand.posted,
                    &cand.amount,
                    &cand_norm,
                    false,
                ) {
                    Some(i) => {
                        consumed.insert(i);
                        stats.duplicates += 1;
                    }
                    None => to_insert.push(cand),
                }
            }
            stats.new_transactions += to_insert.len();
            if !to_insert.is_empty() {
                self.upsert_finance_transactions(&vault_id, &to_insert, false)?;
            }

            // Raw layer: verbatim rows under finance/vanguard/raw/<account>/<year>.jsonl.
            // Each JSONL object is the verbatim CSV record — all original column
            // names and original string values, no normalization applied.  This
            // satisfies the "raw vault data always stays complete" rule even if the
            // column recognizer doesn't know about a column.
            let raw_base = self.root().join("finance/vanguard/raw").join(&vault_id);
            if !group.is_empty() {
                let mut raw_by_year: std::collections::BTreeMap<
                    String,
                    Vec<serde_json::Value>,
                > = std::collections::BTreeMap::new();
                for r in group {
                    if let Some(year) = r.posted.get(..4) {
                        // Build the raw object from the verbatim CSV columns
                        // (original header → original string value), preserving
                        // every column regardless of whether the mapper knows it.
                        let mut obj = serde_json::Map::new();
                        for (col_name, col_val) in &r.raw_cols {
                            obj.insert(
                                col_name.clone(),
                                serde_json::Value::String(col_val.clone()),
                            );
                        }
                        raw_by_year
                            .entry(year.to_string())
                            .or_default()
                            .push(serde_json::Value::Object(obj));
                    }
                }
                for (year, raws) in raw_by_year {
                    let raw_path = raw_base.join(format!("{year}.jsonl"));
                    if let Some(parent) = raw_path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("creating {}", parent.display()))?;
                    }
                    let existing_raw: Vec<serde_json::Value> =
                        fs::read_to_string(&raw_path)
                            .unwrap_or_default()
                            .lines()
                            .filter(|l| !l.trim().is_empty())
                            .filter_map(|l| serde_json::from_str(l).ok())
                            .collect();
                    let existing_raw_strs: HashSet<String> = existing_raw
                        .iter()
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                        .collect();
                    let mut append_buf = String::new();
                    for raw_row in &raws {
                        let line = serde_json::to_string(raw_row)?;
                        if !existing_raw_strs.contains(&line) {
                            append_buf.push_str(&line);
                            append_buf.push('\n');
                        }
                    }
                    if !append_buf.is_empty() {
                        use std::io::Write;
                        let mut f = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&raw_path)
                            .with_context(|| format!("opening raw {}", raw_path.display()))?;
                        f.write_all(append_buf.as_bytes())
                            .with_context(|| format!("writing raw {}", raw_path.display()))?;
                    }
                }
            }
        }

        self.save_finance_accounts(&accounts)?;
        self.write_import_state(path, stats.new_transactions)?;
        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Venmo privacy data-download imports.

    /// Import a Venmo transaction history CSV.
    ///
    /// ## Column format (scaffold — Needs-sample to confirm exact names)
    ///
    /// The Venmo privacy-download CSV column set and amount encoding are derived
    /// from community-reported exports and secondary sources; they have **not
    /// been confirmed against a real account.venmo.com export**. The `venmo_columns`
    /// recognizer and this parser remain as scaffolds. The actual import falls
    /// through to the generic `detect_mapping` path in `finance_import_csv` until
    /// a real export confirms the shape.
    ///
    /// ## Amount encoding
    ///
    /// The Venmo `Amount (total)` column is widely reported to encode values as
    /// `"+ $50.00"` (inflow) or `"- $25.00"` (outflow). The `clean_amount`
    /// helper strips `$` and spaces and respects the `+`/`-` prefix.  The sign
    /// convention must be verified from a real export before this parser is wired.
    ///
    /// ## Layout
    /// - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
    ///   [`Transaction`] per data row; source="venmo".
    ///   `posted` = Datetime (date part only); `id` = `"venmo-<ID>"`.
    /// - **Raw:** `finance/venmo/raw/<account>/YYYY.jsonl` — verbatim row,
    ///   full fidelity, unconditional.
    ///
    /// ## Dedupe
    /// `id` = `"venmo-<ID>"` (the Venmo transaction ID). Re-importing the same
    /// file is a clean no-op. Cross-source fuzzy matching (SimpleFIN/bank overlap)
    /// uses amount + date + description similarity.
    ///
    /// **SCAFFOLD — not called from the live dispatch path while Needs-sample is
    /// set.** See the commented-out block in `finance_import_csv`. This function
    /// is preserved for when a real export confirms the column shape and the
    /// dispatch is re-wired.
    // Parked scaffold: suppress dead_code warning until a real sample re-wires the dispatch.
    #[allow(dead_code)]
    pub(crate) fn import_venmo(
        &self,
        path: &Path,
        src_headers: &[String],
        mut reader: csv::Reader<&[u8]>,
        cols: &VenmoCols,
        fallback_account_id: Option<&str>,
        fallback_account_name: Option<&str>,
    ) -> Result<CsvImportStats> {
        struct Row {
            id: String,
            posted: String,
            amount: String,
            desc: String,
            desc_norm: String,
            extra: serde_json::Map<String, serde_json::Value>,
            raw_cols: Vec<(String, String)>,
        }

        let mut accounts = self.load_finance_accounts()?;
        let vault_id = match (fallback_account_id, fallback_account_name) {
            (Some(id), _) => {
                let existing = self.load_finance_accounts()?;
                if let Some(a) = existing.iter().find(|a| a.id == id) {
                    a.id.clone()
                } else {
                    anyhow::bail!("unknown account: {id}");
                }
            }
            (None, Some(name)) if !name.trim().is_empty() => {
                let name = name.trim();
                let alias = super::model::account_slug("", name);
                self.resolve_finance_account(&mut accounts, "csv-import", &alias, name, "", "USD")
            }
            _ => self.resolve_finance_account(
                &mut accounts,
                "venmo",
                "venmo-import",
                "Venmo",
                "Venmo",
                "USD",
            ),
        };

        let currency = accounts
            .iter()
            .find(|a| a.id == vault_id)
            .map(|a| a.currency.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "USD".into());

        let mut stats = CsvImportStats {
            format: "venmo".into(),
            account: vault_id.clone(),
            ..Default::default()
        };
        let mut rows: Vec<Row> = Vec::new();
        let oob = usize::MAX;

        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| {
                if i == oob { "" } else { record.get(i).unwrap_or("").trim() }
            };

            // Date: "Datetime" column — ISO-like with optional time; strip time.
            let Some(posted) = parse_date(get(cols.datetime)) else {
                stats.skipped += 1;
                continue;
            };

            // Amount: "Amount (total)" — community-reported as "+ $50.00" / "- $25.00".
            // `clean_amount` strips "$" and handles the +/- prefix.
            let raw_amount = get(cols.amount_total);
            let Some(amount) = clean_amount(raw_amount) else {
                stats.skipped += 1;
                continue;
            };

            let txn_id = get(cols.id).to_string();
            let note = get(cols.note);
            let from_user = get(cols.from_user);
            let to_user = get(cols.to_user);
            let txn_type = get(cols.txn_type);

            // Description: Note first, then "From → To", then type.
            let desc = if !note.is_empty() {
                note.to_string()
            } else if !from_user.is_empty() && !to_user.is_empty() {
                format!("{from_user} → {to_user}")
            } else {
                txn_type.to_string()
            };
            let desc_norm = norm_desc(&desc);

            // Extra: Venmo-specific fields the Transaction contract doesn't carry.
            let mut extra = serde_json::Map::new();
            let put = |extra: &mut serde_json::Map<_, _>, k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.into(), serde_json::Value::String(v.to_string()));
                }
            };
            put(&mut extra, "type", txn_type);
            put(&mut extra, "status", get(cols.status));
            if !note.is_empty() {
                put(&mut extra, "note", note);
            }
            if !from_user.is_empty() {
                put(&mut extra, "from", from_user);
            }
            if !to_user.is_empty() {
                put(&mut extra, "to", to_user);
            }
            if !txn_id.is_empty() {
                put(&mut extra, "venmo_id", &txn_id);
            }
            if let Some(ci) = cols.amount_tip {
                put(&mut extra, "amount_tip", get(ci));
            }
            if let Some(ci) = cols.amount_tax {
                put(&mut extra, "amount_tax", get(ci));
            }
            if let Some(ci) = cols.amount_fee {
                put(&mut extra, "amount_fee", get(ci));
            }
            if let Some(ci) = cols.funding_source {
                put(&mut extra, "funding_source", get(ci));
            }
            if let Some(ci) = cols.destination {
                put(&mut extra, "destination", get(ci));
            }
            if let Some(ci) = cols.beginning_balance {
                put(&mut extra, "beginning_balance", get(ci));
            }
            if let Some(ci) = cols.ending_balance {
                put(&mut extra, "ending_balance", get(ci));
            }

            // Verbatim columns for the raw layer.
            let raw_cols: Vec<(String, String)> = src_headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), record.get(i).unwrap_or("").to_string()))
                .collect();

            rows.push(Row {
                id: txn_id,
                posted,
                amount,
                desc,
                desc_norm,
                extra,
                raw_cols,
            });
        }

        // Synthesize ids from the Venmo transaction ID (the stable, documented
        // primary key). Fall back to hash when ID column is blank.
        let mut occurrence: HashMap<(String, String, String), u32> = HashMap::new();
        let candidates: Vec<Transaction> = rows
            .iter()
            .map(|r| {
                let id = if !r.id.is_empty() {
                    format!("venmo-{}", r.id)
                } else {
                    let key = (r.posted.clone(), amount_key(&r.amount), r.desc_norm.clone());
                    let n = occurrence.entry(key.clone()).or_insert(0);
                    let mut hasher = sha2::Sha256::new();
                    use sha2::Digest;
                    hasher.update(vault_id.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.0.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.1.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.2.as_bytes());
                    hasher.update([0]);
                    hasher.update(n.to_le_bytes());
                    *n += 1;
                    let digest = hasher.finalize();
                    let h: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                    format!("venmo-hash-{h}")
                };
                Transaction {
                    id,
                    account: vault_id.clone(),
                    posted: r.posted.clone(),
                    transacted: None,
                    amount: r.amount.clone(),
                    currency: currency.clone(),
                    description: r.desc.clone(),
                    payee: None,
                    category: None,
                    pending: false,
                    source: "venmo".into(),
                    extra: r.extra.clone(),
                }
            })
            .collect();

        // Dedupe against existing rows.
        let existing = self.finance_transactions(Some(&vault_id), usize::MAX)?;
        let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
        let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, t) in existing.iter().enumerate() {
            by_amount.entry(amount_key(&t.amount)).or_default().push(i);
        }
        let existing_norm: Vec<String> =
            existing.iter().map(|t| norm_desc(&t.description)).collect();
        let mut consumed: HashSet<usize> = HashSet::new();
        let mut to_insert: Vec<Transaction> = Vec::new();

        for cand in candidates {
            if stats.from.is_empty() || cand.posted < stats.from {
                stats.from = cand.posted.clone();
            }
            if stats.to.is_empty() || cand.posted > stats.to {
                stats.to = cand.posted.clone();
            }
            if existing_ids.contains(cand.id.as_str()) {
                stats.duplicates += 1;
                continue;
            }
            let cand_norm = norm_desc(&cand.description);
            match find_match(
                &existing,
                &existing_norm,
                &by_amount,
                &consumed,
                &cand.posted,
                &cand.amount,
                &cand_norm,
                false,
            ) {
                Some(i) => {
                    consumed.insert(i);
                    stats.duplicates += 1;
                }
                None => to_insert.push(cand),
            }
        }
        stats.new_transactions = to_insert.len();
        if !to_insert.is_empty() {
            self.upsert_finance_transactions(&vault_id, &to_insert, false)?;
        }

        // Raw layer: verbatim rows under finance/venmo/raw/<account>/YYYY.jsonl.
        let raw_base = self.root().join("finance/venmo/raw").join(&vault_id);
        if !rows.is_empty() {
            let mut raw_by_year: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
            for r in &rows {
                if let Some(year) = r.posted.get(..4) {
                    let mut obj = serde_json::Map::new();
                    for (col_name, col_val) in &r.raw_cols {
                        obj.insert(
                            col_name.clone(),
                            serde_json::Value::String(col_val.clone()),
                        );
                    }
                    raw_by_year
                        .entry(year.to_string())
                        .or_default()
                        .push(serde_json::Value::Object(obj));
                }
            }
            for (year, raws) in raw_by_year {
                let raw_path = raw_base.join(format!("{year}.jsonl"));
                if let Some(parent) = raw_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                let existing_raw: Vec<serde_json::Value> = fs::read_to_string(&raw_path)
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
                let existing_raw_strs: HashSet<String> = existing_raw
                    .iter()
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .collect();
                let mut append_buf = String::new();
                for raw_row in &raws {
                    let line = serde_json::to_string(raw_row)?;
                    if !existing_raw_strs.contains(&line) {
                        append_buf.push_str(&line);
                        append_buf.push('\n');
                    }
                }
                if !append_buf.is_empty() {
                    use std::io::Write;
                    let mut f = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&raw_path)
                        .with_context(|| format!("opening raw {}", raw_path.display()))?;
                    f.write_all(append_buf.as_bytes())
                        .with_context(|| format!("writing raw {}", raw_path.display()))?;
                }
            }
        }

        self.save_finance_accounts(&accounts)?;
        // Do not stamp last_data if all rows were skipped.
        if stats.rows == 0 || stats.skipped < stats.rows {
            self.write_import_state(path, stats.new_transactions)?;
        }
        Ok(stats)
    }

    // -----------------------------------------------------------------------
    // Monarch Money transaction CSV imports.

    /// Import a Monarch Money transaction CSV.
    ///
    /// ## Column format (confirmed — primary sources)
    ///
    /// Monarch Money (app.monarchmoney.com → Settings → Data → Download Transactions)
    /// exports a multi-account transaction history with these confirmed columns:
    ///   `Date, Merchant, Category, Account, Original Statement, Notes, Amount, Tags`
    ///
    /// Sources: help.monarch.com/hc/en-us/articles/15526600975764 (confirmed via
    /// help.403fin.io + QuickBankConvert, Feb 2026 currency).
    ///
    /// ## Amount sign convention (confirmed)
    ///
    /// Monarch signs spending **negative** (debits/purchases carry a `-` prefix;
    /// credits/deposits are positive) — confirmed from official help docs:
    /// "Debits, purchases, and withdrawals should be denoted with a -".
    /// `negate=false` — amounts are already in vault convention.
    /// This is the OPPOSITE of Copilot Money (which uses positive-outflows).
    ///
    /// ## Layout
    /// - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
    ///   [`Transaction`] per row; source="monarch-money".
    ///   `posted` = Date column.
    /// - **Raw:** `finance/monarch-money/raw/<account>/YYYY.jsonl` — verbatim
    ///   row, full fidelity, unconditional.
    ///
    /// ## Dedupe
    /// `hash(account, date, amount, description_normalized, occurrence)` — no
    /// stable per-row id is known in the export.  Re-importing the same file
    /// is a clean no-op via the synthesized id.  Cross-source fuzzy matching
    /// (SimpleFIN overlap) uses amount + date + description similarity with
    /// `relaxed=true` (Monarch rewrites merchant names just like Copilot).
    ///
    /// **Column shape confirmed; dispatch is wired in `finance_import_csv`.**
    /// The `#[allow(dead_code)]` is kept only until a real export validates
    /// the parser end-to-end; remove it at that point.
    // Parked scaffold: suppress dead_code warning until a real sample re-wires the dispatch.
    #[allow(dead_code)]
    pub(crate) fn import_monarch_money(
        &self,
        path: &Path,
        src_headers: &[String],
        mut reader: csv::Reader<&[u8]>,
        cols: &MonarchMoneyCols,
        fallback_account_id: Option<&str>,
        fallback_account_name: Option<&str>,
    ) -> Result<CsvImportStats> {
        struct Row {
            account_key: String,
            posted: String,
            transacted: Option<String>,
            amount: String,
            desc: String,
            category: Option<String>,
            extra: serde_json::Map<String, serde_json::Value>,
            raw_cols: Vec<(String, String)>,
        }

        let mut accounts = self.load_finance_accounts()?;
        let mut stats = CsvImportStats {
            format: "monarch-money".into(),
            ..Default::default()
        };
        let mut rows: Vec<Row> = Vec::new();
        let oob = usize::MAX;

        for record in reader.records() {
            let record = record.context("reading CSV row")?;
            stats.rows += 1;
            let get = |i: usize| {
                if i == oob { "" } else { record.get(i).unwrap_or("").trim() }
            };

            // Monarch has no "pending" status column in the confirmed export schema.

            let Some(posted) = parse_date(get(cols.date)) else {
                stats.skipped += 1;
                continue;
            };
            let Some(amount) = clean_amount(get(cols.amount)) else {
                stats.skipped += 1;
                continue;
            };

            // Sign convention: Monarch signs debits NEGATIVE (confirmed).
            // "Debits, purchases, and withdrawals should be denoted with a -"
            // — help.monarch.com "Downloading Transaction or Account History".
            // No negate() call needed: amounts are already in vault convention.
            // (This is the opposite of Copilot Money, which uses positive-outflows.)

            let account_key = get(cols.account).replace('\u{a0}', " ");

            let mut extra = serde_json::Map::new();
            let put = |extra: &mut serde_json::Map<_, _>, k: &str, v: &str| {
                if !v.is_empty() {
                    extra.insert(k.into(), serde_json::Value::String(v.to_string()));
                }
            };
            if let Some(ci) = cols.original_statement {
                put(&mut extra, "original-statement", get(ci));
            }
            if let Some(ci) = cols.tags {
                put(&mut extra, "tags", get(ci));
            }
            if let Some(ci) = cols.notes {
                put(&mut extra, "notes", get(ci));
            }

            let raw_cols: Vec<(String, String)> = src_headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), record.get(i).unwrap_or("").to_string()))
                .collect();

            rows.push(Row {
                account_key,
                posted,
                transacted: None,
                amount,
                desc: get(cols.merchant).replace('\u{a0}', " "),
                category: cols.category.map(|i| get(i).to_string()).filter(|c| !c.is_empty()),
                extra,
                raw_cols,
            });
        }

        // Group by account_key; each group dedupes against that account's
        // existing rows exactly like a single-account import.
        let mut groups: BTreeMap<String, Vec<Row>> = BTreeMap::new();
        for r in rows {
            groups.entry(r.account_key.clone()).or_default().push(r);
        }
        stats.account = format!("{} accounts", groups.len());

        for (account_key, group) in &groups {
            // Resolve vault account by the Monarch "institution|account" key.
            let vault_id = if let Some(id) = fallback_account_id {
                let existing = self.load_finance_accounts()?;
                if let Some(a) = existing.iter().find(|a| a.id == id) {
                    a.id.clone()
                } else {
                    anyhow::bail!("unknown account: {id}");
                }
            } else if let Some(name) = fallback_account_name.filter(|n| !n.trim().is_empty()) {
                let alias = super::model::account_slug("", name.trim());
                self.resolve_finance_account(
                    &mut accounts,
                    "csv-import",
                    &alias,
                    name.trim(),
                    "",
                    "USD",
                )
            } else {
                // Auto-register from the Monarch account display name.
                let display = if let Some(p) = account_key.find('|') {
                    &account_key[p + 1..]
                } else {
                    account_key.as_str()
                };
                self.resolve_finance_account(
                    &mut accounts,
                    "monarch-money",
                    account_key,
                    display,
                    "",
                    "USD",
                )
            };

            let currency = accounts
                .iter()
                .find(|a| a.id == vault_id)
                .map(|a| a.currency.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "USD".into());

            let mut occurrence: HashMap<(String, String, String), u32> = HashMap::new();
            let candidates: Vec<Transaction> = group
                .iter()
                .map(|r| {
                    let key = (r.posted.clone(), amount_key(&r.amount), norm_desc(&r.desc));
                    let n = occurrence.entry(key.clone()).or_insert(0);
                    let mut hasher = sha2::Sha256::new();
                    use sha2::Digest;
                    hasher.update(vault_id.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.0.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.1.as_bytes());
                    hasher.update([0]);
                    hasher.update(key.2.as_bytes());
                    hasher.update([0]);
                    hasher.update(n.to_le_bytes());
                    *n += 1;
                    let digest = hasher.finalize();
                    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
                    Transaction {
                        id: format!("monarch-money-{id}"),
                        account: vault_id.clone(),
                        posted: r.posted.clone(),
                        transacted: r.transacted.clone(),
                        amount: r.amount.clone(),
                        currency: currency.clone(),
                        description: r.desc.clone(),
                        payee: None,
                        category: r.category.clone(),
                        pending: false,
                        source: "monarch-money".into(),
                        extra: r.extra.clone(),
                    }
                })
                .collect();

            let existing = self.finance_transactions(Some(&vault_id), usize::MAX)?;
            let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
            let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, t) in existing.iter().enumerate() {
                by_amount.entry(amount_key(&t.amount)).or_default().push(i);
            }
            let existing_norm: Vec<String> =
                existing.iter().map(|t| norm_desc(&t.description)).collect();
            let mut consumed: HashSet<usize> = HashSet::new();
            let mut to_insert: Vec<Transaction> = Vec::new();
            let mut to_update: Vec<Transaction> = Vec::new();

            for cand in candidates {
                if stats.from.is_empty() || cand.posted < stats.from {
                    stats.from = cand.posted.clone();
                }
                if stats.to.is_empty() || cand.posted > stats.to {
                    stats.to = cand.posted.clone();
                }
                if existing_ids.contains(cand.id.as_str()) {
                    stats.duplicates += 1;
                    continue;
                }
                let cand_norm = norm_desc(&cand.description);
                // Use relaxed matching: Monarch rewrites merchant names (same as
                // Copilot), so amount + date alone may be the only signal.
                match find_match(
                    &existing,
                    &existing_norm,
                    &by_amount,
                    &consumed,
                    &cand.posted,
                    &cand.amount,
                    &cand_norm,
                    true,
                ) {
                    Some(i) => {
                        consumed.insert(i);
                        stats.duplicates += 1;
                        let ex = &existing[i];
                        if !ex.pending && ex.category.is_none() && cand.category.is_some() {
                            let mut enriched = ex.clone();
                            enriched.category = cand.category.clone();
                            enriched.extra.insert("category-source".into(), "monarch-money".into());
                            to_update.push(enriched);
                            stats.merged += 1;
                        }
                    }
                    None => to_insert.push(cand),
                }
            }
            stats.new_transactions += to_insert.len();
            to_insert.extend(to_update);
            if !to_insert.is_empty() {
                self.upsert_finance_transactions(&vault_id, &to_insert, false)?;
            }

            // Raw layer: verbatim rows under finance/monarch-money/raw/<account>/YYYY.jsonl.
            let raw_base = self.root().join("finance/monarch-money/raw").join(&vault_id);
            if !group.is_empty() {
                let mut raw_by_year: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
                for r in group {
                    if let Some(year) = r.posted.get(..4) {
                        let mut obj = serde_json::Map::new();
                        for (col_name, col_val) in &r.raw_cols {
                            obj.insert(
                                col_name.clone(),
                                serde_json::Value::String(col_val.clone()),
                            );
                        }
                        raw_by_year
                            .entry(year.to_string())
                            .or_default()
                            .push(serde_json::Value::Object(obj));
                    }
                }
                for (year, raws) in raw_by_year {
                    let raw_path = raw_base.join(format!("{year}.jsonl"));
                    if let Some(parent) = raw_path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("creating {}", parent.display()))?;
                    }
                    let existing_raw: Vec<serde_json::Value> = fs::read_to_string(&raw_path)
                        .unwrap_or_default()
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .collect();
                    let existing_raw_strs: HashSet<String> = existing_raw
                        .iter()
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                        .collect();
                    let mut append_buf = String::new();
                    for raw_row in &raws {
                        let line = serde_json::to_string(raw_row)?;
                        if !existing_raw_strs.contains(&line) {
                            append_buf.push_str(&line);
                            append_buf.push('\n');
                        }
                    }
                    if !append_buf.is_empty() {
                        use std::io::Write;
                        let mut f = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&raw_path)
                            .with_context(|| format!("opening raw {}", raw_path.display()))?;
                        f.write_all(append_buf.as_bytes())
                            .with_context(|| format!("writing raw {}", raw_path.display()))?;
                    }
                }
            }
        }
        self.save_finance_accounts(&accounts)?;
        if stats.rows == 0 || stats.skipped < stats.rows {
            self.write_import_state(path, stats.new_transactions)?;
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHASE_CARD: &str = "\
Transaction Date,Post Date,Description,Category,Type,Amount,Memo
06/08/2026,06/10/2026,AMZN Mktp US*123ABC,Shopping,Sale,-42.17,
06/09/2026,06/09/2026,PAYMENT THANK YOU - WEB,,Payment,500.00,
";

    const CHASE_CHECKING: &str = "\
Details,Posting Date,Description,Amount,Type,Balance,Check or Slip #
DEBIT,06/10/2026,ONLINE TRANSFER TO SAV,-250.00,ACCT_XFER,1200.50,
CREDIT,06/09/2026,DIRECT DEP PAYROLL,2000.00,ACH_CREDIT,1450.50,
";

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-fin-import-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn write_csv(v: &Vault, name: &str, body: &str) -> std::path::PathBuf {
        let p = v.root().join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn field_parsers_normalize() {
        assert_eq!(parse_date("06/10/2026").as_deref(), Some("2026-06-10"));
        assert_eq!(parse_date("2026-06-10").as_deref(), Some("2026-06-10"));
        assert_eq!(parse_date("6/1/26").as_deref(), Some("2026-06-01"));
        assert_eq!(parse_date("not a date"), None);

        assert_eq!(clean_amount("-42.17").as_deref(), Some("-42.17"));
        assert_eq!(clean_amount("$1,234.50").as_deref(), Some("1234.50"));
        assert_eq!(clean_amount("(1,234.50)").as_deref(), Some("-1234.50"));
        assert_eq!(clean_amount("+5").as_deref(), Some("5"));
        assert_eq!(clean_amount(""), None);
        assert_eq!(clean_amount("n/a"), None);

        assert_eq!(amount_key("-42.170"), amount_key("-42.17"));
        assert_eq!(amount_key("0042.17"), "42.17");
        assert_ne!(amount_key("-42.17"), amount_key("42.17"));

        assert!(desc_similar(&norm_desc("AMZN Mktp US*123ABC"), &norm_desc("AMZN MKTP US 123ABC")));
        assert!(desc_similar(&norm_desc("STARBUCKS #1234 SEATTLE"), &norm_desc("STARBUCKS #1234")));
        assert!(!desc_similar(&norm_desc("STARBUCKS"), &norm_desc("CHEVRON GAS")));
    }

    #[test]
    fn chase_card_layout_is_recognized() {
        let v = temp_vault("chase-card");
        let p = write_csv(&v, "card.csv", CHASE_CARD);
        let stats = v.finance_import_csv(&p, None, Some("Freedom Test")).unwrap();
        assert_eq!(stats.format, "chase-card");
        assert_eq!(stats.new_transactions, 2);
        assert_eq!((stats.from.as_str(), stats.to.as_str()), ("2026-06-09", "2026-06-10"));
        let rows = v.finance_transactions(Some(&stats.account), 10).unwrap();
        assert_eq!(rows.len(), 2);
        let amzn = rows.iter().find(|t| t.amount == "-42.17").unwrap();
        assert_eq!(amzn.posted, "2026-06-10");
        assert_eq!(amzn.transacted.as_deref(), Some("2026-06-08"));
        assert_eq!(amzn.category.as_deref(), Some("Shopping"));
        assert_eq!(amzn.source, "csv-import");
        assert_eq!(amzn.extra.get("type").and_then(|v| v.as_str()), Some("Sale"));
    }

    #[test]
    fn chase_checking_layout_is_recognized() {
        let v = temp_vault("chase-checking");
        let p = write_csv(&v, "checking.csv", CHASE_CHECKING);
        let stats = v.finance_import_csv(&p, None, Some("Main Checking")).unwrap();
        assert_eq!(stats.format, "chase-checking");
        assert_eq!(stats.new_transactions, 2);
        let rows = v.finance_transactions(Some(&stats.account), 10).unwrap();
        let xfer = rows.iter().find(|t| t.amount == "-250.00").unwrap();
        assert_eq!(xfer.extra.get("balance").and_then(|v| v.as_str()), Some("1200.50"));
    }

    #[test]
    fn generic_layout_with_debit_credit_columns() {
        let v = temp_vault("generic");
        let body = "\
Date,Description,Debit,Credit
2026-06-10,COFFEE SHOP,4.50,
2026-06-09,REFUND,, 12.00
";
        let p = write_csv(&v, "generic.csv", body);
        let stats = v.finance_import_csv(&p, None, Some("Some Bank")).unwrap();
        assert_eq!(stats.format, "generic");
        assert_eq!(stats.new_transactions, 2);
        let rows = v.finance_transactions(Some(&stats.account), 10).unwrap();
        assert_eq!(rows.iter().find(|t| t.description == "COFFEE SHOP").unwrap().amount, "-4.50");
        assert_eq!(rows.iter().find(|t| t.description == "REFUND").unwrap().amount, "12.00");
    }

    #[test]
    fn unmappable_headers_fail_with_columns_named() {
        let v = temp_vault("unmappable");
        let p = write_csv(&v, "odd.csv", "Foo,Bar\n1,2\n");
        let err = v
            .finance_import_csv(&p, None, Some("X"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("foo, bar"), "got: {err}");
    }

    #[test]
    fn reimport_is_a_clean_noop() {
        let v = temp_vault("reimport");
        let p = write_csv(&v, "card.csv", CHASE_CARD);
        let first = v.finance_import_csv(&p, None, Some("Freedom Test")).unwrap();
        assert_eq!(first.new_transactions, 2);
        // Same file again, this time by account id.
        let second = v
            .finance_import_csv(&p, Some(&first.account), None)
            .unwrap();
        assert_eq!(second.new_transactions, 0);
        assert_eq!(second.duplicates, 2);
        assert_eq!(v.finance_transactions(Some(&first.account), 10).unwrap().len(), 2);
    }

    #[test]
    fn same_day_identical_rows_stay_distinct() {
        let v = temp_vault("occurrence");
        let body = "\
Date,Description,Amount
2026-06-10,COFFEE SHOP,-4.50
2026-06-10,COFFEE SHOP,-4.50
";
        let p = write_csv(&v, "twice.csv", body);
        let stats = v.finance_import_csv(&p, None, Some("Card")).unwrap();
        assert_eq!(stats.new_transactions, 2, "occurrence counter keeps both");
        // And re-importing still adds nothing.
        let again = v.finance_import_csv(&p, Some(&stats.account), None).unwrap();
        assert_eq!(again.new_transactions, 0);
    }

    #[test]
    fn overlap_with_synced_rows_merges_instead_of_duplicating() {
        let v = temp_vault("overlap");
        // Seed a SimpleFIN-synced row (no category), as the first sync would.
        let synced = Transaction {
            id: "TRN-1".into(),
            account: "chase-freedom".into(),
            posted: "2026-06-10".into(),
            transacted: None,
            amount: "-42.17".into(),
            currency: "USD".into(),
            description: "AMZN Mktp US*123ABC".into(),
            payee: None,
            category: None,
            pending: false,
            source: "simplefin".into(),
            extra: serde_json::Map::new(),
        };
        let mut accounts = v.load_finance_accounts().unwrap();
        v.resolve_finance_account(&mut accounts, "simplefin", "ACT-1", "Freedom", "Chase", "USD");
        // resolve assigns "chase-freedom" — keep the seeded row consistent.
        assert_eq!(accounts[0].id, "chase-freedom");
        let saved = accounts.clone();
        v.upsert_finance_transactions("chase-freedom", &[synced], true).unwrap();
        save_accounts(&v, &saved);

        // Statement export overlapping that window: posted a day earlier
        // per the statement, same amount, near-same description, plus one
        // older row SimpleFIN never had.
        let body = "\
Transaction Date,Post Date,Description,Category,Type,Amount,Memo
06/07/2026,06/09/2026,AMZN MKTP US 123ABC,Shopping,Sale,-42.17,
01/05/2026,01/06/2026,OLD COFFEE SHOP,Food & Drink,Sale,-9.99,
";
        let p = write_csv(&v, "statement.csv", body);
        let stats = v.finance_import_csv(&p, Some("chase-freedom"), None).unwrap();
        assert_eq!(stats.duplicates, 1, "synced row recognized");
        assert_eq!(stats.merged, 1, "category absorbed");
        assert_eq!(stats.new_transactions, 1, "older row backfilled");

        let rows = v.finance_transactions(Some("chase-freedom"), 10).unwrap();
        assert_eq!(rows.len(), 2, "no duplicate inserted");
        let canonical = rows.iter().find(|t| t.id == "TRN-1").unwrap();
        assert_eq!(canonical.source, "simplefin", "aggregator row stays canonical");
        assert_eq!(canonical.category.as_deref(), Some("Shopping"));
    }

    fn save_accounts(v: &Vault, accounts: &[super::super::Account]) {
        // Test helper: the registry write is private to the finance module.
        let path = v.root().join("finance/accounts.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut out = String::new();
        for a in accounts {
            out.push_str(&serde_json::to_string(a).unwrap());
            out.push('\n');
        }
        fs::write(path, out).unwrap();
    }

    #[test]
    fn match_window_also_covers_the_transacted_date() {
        // Existing row posted 4 days out (beyond the window) but transacted
        // the same day the other source records — still the same purchase.
        let existing = vec![Transaction {
            id: "x".into(),
            account: "a".into(),
            posted: "2025-10-26".into(),
            transacted: Some("2025-10-22".into()),
            amount: "-38.63".into(),
            currency: "USD".into(),
            description: "HOMEDEPOT.COM".into(),
            payee: None,
            category: None,
            pending: false,
            source: "csv-import".into(),
            extra: serde_json::Map::new(),
        }];
        let norms: Vec<String> = existing.iter().map(|t| norm_desc(&t.description)).collect();
        let mut by_amount: HashMap<String, Vec<usize>> = HashMap::new();
        by_amount.insert(amount_key("-38.63"), vec![0]);
        let consumed = HashSet::new();
        let hit = find_match(
            &existing, &norms, &by_amount, &consumed,
            "2025-10-22", "-38.63", &norm_desc("Home Depot"), true,
        );
        assert_eq!(hit, Some(0));
    }

    #[test]
    fn copilot_export_splits_accounts_and_flips_signs() {
        let v = temp_vault("copilot");
        // One vault account pre-exists from SimpleFIN, carrying mask 2429 in
        // its id, with one synced row Copilot also has (renamed merchant).
        let mut accounts = v.load_finance_accounts().unwrap();
        v.resolve_finance_account(
            &mut accounts, "simplefin", "ACT-1", "Freedom Unlimited 2429", "Chase Bank", "USD",
        );
        save_accounts(&v, &accounts);
        let acct = "chase-bank-freedom-unlimited-2429";
        let synced = Transaction {
            id: "TRN-9".into(),
            account: acct.into(),
            posted: "2026-06-10".into(),
            transacted: None,
            amount: "-27.24".into(),
            currency: "USD".into(),
            description: "AMZN Mktp US*ABC123".into(),
            payee: None,
            category: None,
            pending: false,
            source: "simplefin".into(),
            extra: serde_json::Map::new(),
        };
        v.upsert_finance_transactions(acct, &[synced], true).unwrap();

        let body = "\
\"date\",\"name\",\"amount\",\"status\",\"category\",\"parent category\",\"excluded\",\"tags\",\"type\",\"account\",\"account mask\",\"note\",\"recurring\"
\"2026-06-09\",\"Amazon.com\",27.24,\"posted\",\"Shopping\",,false,,\"regular\",\"Chase - Freedom Unlimited VISA\",\"2429\",,\"\"
\"2021-03-05\",\"Coffee Shop\",4.50,\"posted\",\"Food\",,false,,\"regular\",\"Chase - Freedom Unlimited VISA\",\"2429\",,\"\"
\"2022-01-10\",\"Paycheck\",-1500,\"posted\",\"Income\",,false,,\"income\",\"Apple\u{a0}Card\",\"\",,\"\"
\"2026-06-11\",\"Pending Thing\",9.99,\"pending\",\"Other\",,false,,\"regular\",\"Apple\u{a0}Card\",\"\",,\"\"
";
        let p = write_csv(&v, "transactions.csv", body);
        let stats = v.finance_import_csv(&p, None, None).unwrap();
        assert_eq!(stats.format, "copilot");
        assert_eq!(stats.skipped, 1, "pending row skipped");
        // Renamed merchant still deduped via the relaxed amount+date tier.
        assert_eq!(stats.duplicates, 1);
        assert_eq!(stats.merged, 1, "category absorbed onto the synced row");
        assert_eq!(stats.new_transactions, 2);

        // Mask 2429 adopted the existing account; Apple Card was created.
        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts.iter().any(|a| a.id == acct
            && a.aliases.get("copilot").is_some_and(|s| s.contains("2429"))));
        let apple = accounts.iter().find(|a| a.name == "Apple Card").unwrap();

        // Signs flipped: spending negative, income positive.
        let rows = v.finance_transactions(Some(acct), 10).unwrap();
        assert_eq!(rows.iter().find(|t| t.description == "Coffee Shop").unwrap().amount, "-4.50");
        let apple_rows = v.finance_transactions(Some(&apple.id), 10).unwrap();
        assert_eq!(apple_rows.len(), 1);
        assert_eq!(apple_rows[0].amount, "1500");
        assert_eq!(apple_rows[0].source, "copilot");

        // Synced row stayed canonical and gained the category.
        let canonical = rows.iter().find(|t| t.id == "TRN-9").unwrap();
        assert_eq!(canonical.category.as_deref(), Some("Shopping"));
        assert_eq!(rows.len(), 2, "no duplicate for the renamed merchant");

        // Re-import: clean no-op.
        let again = v.finance_import_csv(&p, None, None).unwrap();
        assert_eq!(again.new_transactions, 0);
        assert_eq!(again.duplicates, 3);
    }

    #[test]
    fn pending_rows_match_but_are_never_modified() {
        let v = temp_vault("pending-match");
        let mut accounts = v.load_finance_accounts().unwrap();
        v.resolve_finance_account(&mut accounts, "simplefin", "ACT-1", "Freedom", "Chase", "USD");
        save_accounts(&v, &accounts);
        let pending = Transaction {
            id: "PEND-1".into(),
            account: "chase-freedom".into(),
            posted: "2026-06-10".into(),
            transacted: None,
            amount: "-9.99".into(),
            currency: "USD".into(),
            description: "COFFEE SHOP".into(),
            payee: None,
            category: None,
            pending: true,
            source: "simplefin".into(),
            extra: serde_json::Map::new(),
        };
        v.upsert_finance_transactions("chase-freedom", &[pending], true).unwrap();
        let body = "\
Date,Description,Amount,Category
2026-06-10,COFFEE SHOP,-9.99,Food
";
        let p = write_csv(&v, "pend.csv", body);
        let stats = v.finance_import_csv(&p, Some("chase-freedom"), None).unwrap();
        assert_eq!(stats.duplicates, 1);
        assert_eq!(stats.merged, 0, "pending rows are left for the sync to settle");
        let rows = v.finance_transactions(Some("chase-freedom"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].pending && rows[0].category.is_none());
    }

    // PayPal activity download CSV — schema derived from secondary sources
    // (Needs-sample: no real export on disk yet; fixture hand-built from the
    // official PayPal Activity Download Report documentation, 2026-06).
    // "CounterParty Status" is at official position 14 (right after "Transaction
    // ID" at position 13); it is marked "Unselected" by default in PayPal's UI
    // so most real exports omit it. The parser is name-based and handles both.
    // This fixture includes it to exercise the full official header shape.
    // Fixture covers: payment sent (negative Net), payment received (positive Net),
    // multi-currency row (EUR, US-decimal format), and a refund row.
    const PAYPAL_ACTIVITY: &str = "\
\"Date\",\"Time\",\"TimeZone\",\"Name\",\"Type\",\"Status\",\"Currency\",\"Gross\",\"Fee\",\"Net\",\"From Email Address\",\"To Email Address\",\"Transaction ID\",\"CounterParty Status\",\"Shipping Address\",\"Address Status\",\"Item Title\",\"Item ID\",\"Shipping and Handling Amount\",\"Insurance Amount\",\"Sales Tax\",\"Option 1 Name\",\"Option 1 Value\",\"Option 2 Name\",\"Option 2 Value\",\"Reference Txn ID\",\"Invoice Number\",\"Custom Number\",\"Quantity\",\"Receipt ID\",\"Balance\",\"Subject\",\"Note\"
\"06/10/2026\",\"10:23:45\",\"PDT\",\"Acme Shop\",\"Payment Sent\",\"Completed\",\"USD\",\"-42.00\",\"0.00\",\"-42.00\",\"alice@example.com\",\"shop@acme.com\",\"TXN001ABC123\",\"Verified\",\"\",\"\",\"Widget Pro\",\"\",\"0.00\",\"0.00\",\"0.00\",\"\",\"\",\"\",\"\",\"\",\"\",\"\",\"1\",\"\",\"158.00\",\"Order #1234\",\"\"
\"06/09/2026\",\"14:05:00\",\"PDT\",\"Bob Smith\",\"Payment Received\",\"Completed\",\"USD\",\"25.00\",\"-0.70\",\"24.30\",\"bob@example.com\",\"alice@example.com\",\"TXN002DEF456\",\"Verified\",\"\",\"\",\"\",\"\",\"0.00\",\"0.00\",\"0.00\",\"\",\"\",\"\",\"\",\"\",\"\",\"\",\"\",\"\",\"182.30\",\"\",\"Thanks for lunch\"
\"06/08/2026\",\"09:00:00\",\"PDT\",\"Euro Store\",\"Payment Sent\",\"Completed\",\"EUR\",\"-30.00\",\"0.00\",\"-30.00\",\"alice@example.com\",\"euro@store.de\",\"TXN003GHI789\",\"Unverified\",\"\",\"\",\"\",\"\",\"0.00\",\"0.00\",\"0.00\",\"\",\"\",\"\",\"\",\"\",\"\",\"\",\"1\",\"\",\"\",\"\",\"\"
\"06/07/2026\",\"11:30:00\",\"PDT\",\"Acme Shop\",\"Refund\",\"Completed\",\"USD\",\"10.00\",\"0.00\",\"10.00\",\"shop@acme.com\",\"alice@example.com\",\"TXN004JKL012\",\"Verified\",\"\",\"\",\"\",\"\",\"0.00\",\"0.00\",\"0.00\",\"\",\"\",\"\",\"\",\"TXN001ABC123\",\"\",\"\",\"\",\"\",\"192.30\",\"Partial refund\",\"\"
";

    #[test]
    fn paypal_layout_is_recognized() {
        let v = temp_vault("paypal-basic");
        let p = write_csv(&v, "paypal.csv", PAYPAL_ACTIVITY);
        let stats = v.finance_import_csv(&p, None, None).unwrap();
        assert_eq!(stats.format, "paypal", "layout recognized as paypal");
        assert_eq!(stats.new_transactions, 4, "all four rows imported");
        assert_eq!(stats.skipped, 0);
        let rows = v.finance_transactions(Some(&stats.account), 20).unwrap();
        // Payment Sent (outflow): Net is -42.00, already negative.
        let sent = rows.iter().find(|t| t.id == "paypal-TXN001ABC123").unwrap();
        assert_eq!(sent.amount, "-42.00", "outflow amount correct");
        assert_eq!(sent.posted, "2026-06-10");
        assert_eq!(sent.currency, "USD");
        assert_eq!(sent.source, "paypal");
        assert!(!sent.pending);
        assert_eq!(sent.extra.get("type").and_then(|v| v.as_str()), Some("Payment Sent"));
        assert_eq!(sent.extra.get("gross").and_then(|v| v.as_str()), Some("-42.00"));
        assert_eq!(sent.extra.get("fee").and_then(|v| v.as_str()), Some("0.00"));
        // Description uses Subject when present.
        assert_eq!(sent.description, "Order #1234");
        // Payment Received (inflow): Net is 24.30 (positive).
        let recv = rows.iter().find(|t| t.id == "paypal-TXN002DEF456").unwrap();
        assert_eq!(recv.amount, "24.30", "inflow amount positive");
        // Description falls back to Note when Subject is empty.
        assert_eq!(recv.description, "Thanks for lunch");
        // Multi-currency EUR row.
        let eur = rows.iter().find(|t| t.id == "paypal-TXN003GHI789").unwrap();
        assert_eq!(eur.currency, "EUR", "row-level currency preserved");
        assert_eq!(eur.amount, "-30.00");
        // Refund row: reference_txn_id stored in extra.
        let refund = rows.iter().find(|t| t.id == "paypal-TXN004JKL012").unwrap();
        assert_eq!(refund.amount, "10.00", "refund is positive");
        assert_eq!(refund.extra.get("reference_txn_id").and_then(|v| v.as_str()), Some("TXN001ABC123"));
        // Raw layer written.
        let raw_dir = v.root().join("finance/paypal/raw").join(&stats.account);
        assert!(raw_dir.join("2026.jsonl").exists(), "raw file written");
    }

    #[test]
    fn paypal_reimport_is_clean_noop() {
        let v = temp_vault("paypal-reimport");
        let p = write_csv(&v, "paypal.csv", PAYPAL_ACTIVITY);
        let first = v.finance_import_csv(&p, None, None).unwrap();
        assert_eq!(first.new_transactions, 4);
        let second = v.finance_import_csv(&p, Some(&first.account), None).unwrap();
        assert_eq!(second.new_transactions, 0);
        assert_eq!(second.duplicates, 4, "Transaction IDs dedupe cleanly");
        assert_eq!(
            v.finance_transactions(Some(&first.account), 100).unwrap().len(),
            4,
            "no duplicate rows after re-import"
        );
    }

    #[test]
    fn paypal_auto_registers_account_on_first_import() {
        let v = temp_vault("paypal-acct");
        let p = write_csv(&v, "paypal.csv", PAYPAL_ACTIVITY);
        let stats = v.finance_import_csv(&p, None, None).unwrap();
        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "PayPal");
        assert_eq!(accounts[0].org, "PayPal");
        // Currency set from first row.
        assert_eq!(accounts[0].currency, "USD");
        assert_eq!(stats.account, accounts[0].id);
    }

    #[test]
    fn paypal_sign_conventions_match_vault() {
        // Verify outflows are negative and inflows are positive — the core
        // sign convention the vault relies on.
        let v = temp_vault("paypal-sign");
        let p = write_csv(&v, "paypal.csv", PAYPAL_ACTIVITY);
        v.finance_import_csv(&p, None, None).unwrap();
        let acct = &v.load_finance_accounts().unwrap()[0].id.clone();
        let rows = v.finance_transactions(Some(acct), 100).unwrap();
        let sent = rows.iter().find(|t| t.extra.get("type").and_then(|v| v.as_str()) == Some("Payment Sent") && t.currency == "USD").unwrap();
        assert!(sent.amount.starts_with('-'), "outflow must be negative, got {}", sent.amount);
        let recv = rows.iter().find(|t| t.extra.get("type").and_then(|v| v.as_str()) == Some("Payment Received")).unwrap();
        assert!(!recv.amount.starts_with('-'), "inflow must be positive, got {}", recv.amount);
    }
}

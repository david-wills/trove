//! Actual Budget — local-first, open-source envelope-budgeting app.
//!
//! Reads the local SQLite database written by the Electron desktop app
//! (copy-then-read, the iMessage / browser pattern — never opens the live DB).
//! Also accepts a hand-fed `db.sqlite` or exported ZIP for self-hosted users.
//!
//! ## Vault layout
//!
//! - **Contract layer:** `finance/transactions/<account-id>/<year>.jsonl` — one
//!   [`crate::finance::Transaction`] per transaction, merged into the canonical
//!   finance ledger alongside SimpleFIN and CSV-import rows. `source = "actual-budget"`;
//!   `id` = Actual's transaction UUID (stable, the dedupe key).
//! - **Raw layer:** `finance/actual-budget/raw/YYYY-MM.jsonl` — full-fidelity
//!   joined rows (transaction + payee name + category name + account name),
//!   unconditional, partitioned by the transaction's local month.
//!
//! ## Path discovery (macOS Electron app)
//!
//! Actual's Electron app stores budgets under:
//! `~/Library/Application Support/Actual/{budget-id}/db.sqlite`
//!
//! We glob for `db.sqlite` files one level deep. If none are found, the import
//! fallback accepts a hand-fed file. No FDA required — `~/Library/Application
//! Support/Actual/` is a normal user-domain directory.
//!
//! ## Privacy
//!
//! Financial detail is privacy-sensitive; this integration ships opt-in
//! (default-off) with explicit toggle acknowledgement, same as Bitcoin.
//!
//! ## Sync strategy
//!
//! Watermark: `rowid` in the transactions table, persisted in
//! `.trove/actual-budget-sync.json`. All new rows (`rowid > cursor`) are
//! drained per budget file; the cursor advances only after a successful write.
//! `tombstone = 1` rows are skipped (soft-deleted in Actual). The schema is
//! read defensively with PRAGMA table_info to survive migrations.
//!
//! ## Physical schema (Actual Budget raw db.sqlite)
//!
//! Physical column names (camelCase where noted) differ from the AQL logical names:
//!
//! ```sql
//! transactions:
//!   id TEXT, isParent INTEGER, isChild INTEGER, parent_id TEXT,
//!   acct TEXT (FK → accounts.id),
//!   category TEXT (FK → categories.id),
//!   amount INTEGER (cents; 1 dollar = 100 units; negative = outflow),
//!   description TEXT (FK → payees.id — NOT free text),
//!   notes TEXT (user notes),
//!   date INTEGER (YYYYMMDD),
//!   imported_description TEXT (pre-rule payee string),
//!   financial_id TEXT (bank import dedup id),
//!   cleared INTEGER, reconciled INTEGER,
//!   tombstone INTEGER
//! accounts:  id TEXT, name TEXT, offbudget INTEGER, closed INTEGER, tombstone INTEGER
//! categories: id TEXT, name TEXT, is_income INTEGER, tombstone INTEGER
//! payees:    id TEXT, name TEXT, tombstone INTEGER
//! ```
//!
//! The AQL layer renames these for API consumers:
//!   acct → account, description → payee, isParent → is_parent,
//!   isChild → is_child, financial_id → imported_id,
//!   imported_description → imported_payee
//!
//! This module queries the physical layer directly, using the raw column names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::finance::Transaction;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::vault::Vault;

/// Source tag written into every `Transaction::source` and raw row.
const SOURCE: &str = "actual-budget";
/// Raw layer directory (vault-relative). Not the canonical ledger — a
/// full-fidelity copy of the joined Actual rows before normalization.
const RAW_DIR: &str = "finance/actual-budget/raw";
/// Non-secret rebuildable cursor — `.trove/` but NOT `sync/` (no secrets here).
const SYNC_FILE: &str = ".trove/actual-budget-sync.json";

// ---------------------------------------------------------------------------
// Registry hooks.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_actual_budget_sync()
        .and_then(|s| if s.updated.is_empty() { None } else { Some(s.updated) })
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let txs = out.counts.get("transactions").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(txs > 0, || {
                format!("actual-budget synced — {txs} new transactions")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "actual-budget sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let txs = out.counts.get("transactions").copied().unwrap_or(0);
    let headline = if txs == 0 {
        "Actual Budget is up to date — no new transactions".to_string()
    } else {
        format!("Actual Budget synced — {txs} new transactions")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

fn def_permission() -> crate::integrations::PermissionInfo {
    let found = !find_actual_dbs().is_empty();
    crate::integrations::PermissionInfo {
        kind: "actual-budget-db",
        granted: Some(found),
        required: false, // import fallback always works; the auto-path is optional
    }
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "actual-budget",
        name: "Actual Budget",
        kind: IntegrationKind::LocalSync,
        // 🔒 financial detail — ships opt-in with explicit acknowledgement.
        default_on: false,
        description: "Reads your Actual Budget local database to pull in transactions with \
                      full category and payee context. No network or login required — the \
                      app's own SQLite file is read directly on a schedule. Also accepts the \
                      Settings → Export Data zip for self-hosted users or manual imports.",
        domain: "finance",
        vault_path: "finance/",
        toggleable: true,
        setup: &[
            "Enable this card to start syncing. Trove will find Actual's database automatically \
             (~/Library/Application Support/Actual/).",
            "Self-hosted users: copy your db.sqlite to a local path and import it via the import \
             button (Settings → Export Data zip is also accepted).",
        ],
        caveats: "Transaction amounts are in the currency of each Actual account (no cross-currency \
                  conversion). Split transactions (parent rows) are stored as individual child rows; \
                  the parent row is skipped. Off-budget accounts are included — filter by the \
                  account name if you want to exclude them.",
    },
    // Periodic for auto-discovery; Import for the zip/manual path.
    behavior: Behavior::Periodic {
        cadence: Cadence::every(ACTUAL_BUDGET_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

/// Seconds between syncs. Actual is a local app; hourly is frequent enough.
pub const ACTUAL_BUDGET_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Path discovery.

/// Returns all `db.sqlite` paths one level under the Actual app-support dir.
/// On macOS Electron: `~/Library/Application Support/Actual/<id>/db.sqlite`.
fn find_actual_dbs() -> Vec<PathBuf> {
    let mut found = Vec::new();
    let base = match dirs::data_local_dir() {
        // macOS: ~/Library/Application Support
        Some(d) => d.join("Actual"),
        None => return found,
    };
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return found,
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join("db.sqlite");
        if candidate.is_file() {
            found.push(candidate);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Sync cursor.

/// Per-budget-file watermark — `rowid` of the last transaction imported.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct BudgetCursor {
    /// Highest `rowid` successfully written (0 = no rows yet).
    #[serde(default)]
    last_rowid: i64,
}

/// Sync state persisted at `.trove/actual-budget-sync.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// `db_path (display)` → per-file cursor.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    cursors: BTreeMap<String, BudgetCursor>,
    /// RFC3339 local time of last successful sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    updated: String,
    /// How many transactions were imported across all budget files, lifetime.
    #[serde(default)]
    total_transactions: u64,
}

impl Vault {
    fn read_actual_budget_sync(&self) -> Option<SyncState> {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    fn write_actual_budget_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape.

fn bool_is_false(b: &bool) -> bool { !b }

/// Full-fidelity joined row written to `finance/actual-budget/raw/YYYY-MM.jsonl`.
/// The `ts` field (local midnight on the transaction date) drives partitioning.
#[derive(Serialize)]
struct RawRow {
    /// Vault partition key — local month of the transaction.
    #[serde(skip)]
    ts: String,
    /// Actual's transaction UUID.
    id: String,
    /// Actual's account UUID (physical: `acct`).
    account_id: String,
    /// Account display name (joined from accounts table).
    account_name: String,
    /// `true` when the account is off-budget.
    #[serde(skip_serializing_if = "bool_is_false")]
    account_offbudget: bool,
    /// Payee UUID (physical: `description` FK → payees.id; empty for starting-balance rows).
    #[serde(skip_serializing_if = "String::is_empty")]
    payee_id: String,
    /// Payee display name (joined).
    #[serde(skip_serializing_if = "String::is_empty")]
    payee_name: String,
    /// Category UUID (empty for income / off-budget).
    #[serde(skip_serializing_if = "String::is_empty")]
    category_id: String,
    /// Category display name (joined).
    #[serde(skip_serializing_if = "String::is_empty")]
    category_name: String,
    /// `true` when the category is an income category.
    #[serde(skip_serializing_if = "bool_is_false")]
    is_income: bool,
    /// Transaction date in YYYYMMDD format (Actual native).
    date: String,
    /// Amount in cents (signed integer; 1 dollar = 100 units; negative = outflow).
    amount_cents: i64,
    /// Amount as a decimal string (signed; e.g. "-42.50").
    amount: String,
    /// User notes on the transaction (physical: `notes`).
    #[serde(skip_serializing_if = "String::is_empty")]
    notes: String,
    /// `true` when this is a child of a split transaction (physical: `isChild`).
    #[serde(skip_serializing_if = "bool_is_false")]
    is_child: bool,
    /// Parent transaction UUID for split children.
    #[serde(skip_serializing_if = "String::is_empty")]
    parent_id: String,
    /// Whether the transaction was cleared in Actual.
    #[serde(skip_serializing_if = "bool_is_false")]
    cleared: bool,
    /// Whether the transaction was reconciled in Actual.
    #[serde(skip_serializing_if = "bool_is_false")]
    reconciled: bool,
    /// Actual's bank import dedup id (physical: `financial_id`; AQL: `imported_id`).
    #[serde(skip_serializing_if = "String::is_empty")]
    imported_id: String,
    /// Actual's pre-rule payee string (physical: `imported_description`; AQL: `imported_payee`).
    #[serde(skip_serializing_if = "String::is_empty")]
    imported_payee: String,
}

// ---------------------------------------------------------------------------
// Date parsing.

/// Actual stores dates as `YYYYMMDD` integers (or text like "20260528").
/// Convert to `YYYY-MM-DD` and then to local midnight RFC3339.
fn actual_date_to_ts(date_val: i64) -> Option<String> {
    // e.g. 20260528 → "2026-05-28"
    let s = format!("{date_val:08}");
    if s.len() < 8 {
        return None;
    }
    let (y, rest) = s.split_at(4);
    let (m, d) = rest.split_at(2);
    let date = NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)?;
    // Local midnight.
    let dt = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .earliest()?;
    Some(dt.to_rfc3339())
}

/// Convert cents (signed integer) to a signed decimal string.
/// Actual stores amounts as integer cents (1 dollar = 100 units). Negative = outflow.
/// Examples: 5500 → "55.00", -4250 → "-42.50", 0 → "0.00".
fn cents_to_decimal(cents: i64) -> String {
    let abs = cents.unsigned_abs();
    let whole = abs / 100;
    let frac = abs % 100;
    if cents < 0 {
        format!("-{whole}.{frac:02}")
    } else {
        format!("{whole}.{frac:02}")
    }
}

// ---------------------------------------------------------------------------
// Core DB reader (works on a copy of db.sqlite).

/// Read transactions with `rowid > cursor` from an Actual `db.sqlite` copy.
/// Returns (normalized Transaction rows, raw rows, new_cursor, account_set).
fn read_actual_db(
    db: &Path,
    cursor: i64,
) -> Result<(Vec<(Transaction, RawRow, String)>, i64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening Actual db copy {}", db.display()))?;

    // Check which columns exist (schema evolves with migrations).
    // NOTE: all column names are physical (raw db.sqlite), NOT AQL logical names.
    let cols = table_columns(&conn, "transactions")?;
    // Required core columns (guaranteed since the initial migration).
    // Physical: `acct` (AQL: account), `description` (AQL: payee FK)
    let has_category = cols.contains("category");
    // `description` is the payee FK (physical name); AQL renames it to `payee`.
    let has_description = cols.contains("description");
    let has_notes = cols.contains("notes");
    // Optional columns added in later migrations.
    let has_cleared = cols.contains("cleared");
    let has_reconciled = cols.contains("reconciled");
    // Physical: `imported_description` (AQL: imported_payee)
    let has_imported_description = cols.contains("imported_description");
    // Physical: `financial_id` (AQL: imported_id)
    let has_financial_id = cols.contains("financial_id");
    // Physical camelCase: `isChild` (AQL: is_child), `isParent` (AQL: is_parent)
    let has_is_child = cols.contains("isChild");
    let has_parent_id = cols.contains("parent_id");
    let has_is_parent = cols.contains("isParent");

    // Build lookup maps for account/category/payee names.
    // Physical account FK column: `acct` → joins accounts table on `acct = accounts.id`
    let accounts = load_lookup(&conn, "accounts", &["id", "name", "offbudget"])?;
    let categories = if has_category {
        load_lookup(&conn, "categories", &["id", "name", "is_income"])?
    } else {
        BTreeMap::new()
    };
    // Payees are looked up via the `description` column (payee FK).
    let payees = if has_description {
        load_lookup(&conn, "payees", &["id", "name"])?
    } else {
        BTreeMap::new()
    };

    // Select only new non-tombstoned rows.
    // Physical column names throughout — NOT AQL logical names:
    //   `acct`               = account FK (AQL: account)
    //   `description`        = payee FK   (AQL: payee)
    //   `isParent`           = split parent flag (AQL: is_parent)
    //   `isChild`            = split child flag  (AQL: is_child)
    //   `financial_id`       = bank import dedup (AQL: imported_id)
    //   `imported_description` = pre-rule payee  (AQL: imported_payee)
    //
    // Parent split rows (isParent=1) are skipped — they carry the sum of all
    // children and would double-count. Child rows carry the real amounts.
    let sql = format!(
        "SELECT ROWID, id, acct, {category}, amount, {description}, {notes}, date,
                {cleared}, {reconciled}, {is_child}, {parent_id},
                {financial_id}, {imported_description}
         FROM transactions
         WHERE ROWID > ?1
           AND tombstone = 0
           AND ({is_parent_filter})
         ORDER BY ROWID ASC",
        is_parent_filter = if has_is_parent {
            "isParent IS NULL OR isParent = 0"
        } else {
            "1=1"
        },
        category = if has_category { "category" } else { "NULL" },
        description = if has_description { "description" } else { "NULL" },
        notes = if has_notes { "notes" } else { "NULL" },
        cleared = if has_cleared { "cleared" } else { "0" },
        reconciled = if has_reconciled { "reconciled" } else { "0" },
        is_child = if has_is_child { "isChild" } else { "0" },
        parent_id = if has_parent_id { "parent_id" } else { "NULL" },
        financial_id = if has_financial_id { "financial_id" } else { "NULL" },
        imported_description = if has_imported_description { "imported_description" } else { "NULL" },
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows_out: Vec<(Transaction, RawRow, String)> = Vec::new();
    let mut new_cursor = cursor;

    let mut rows = stmt.query([cursor])?;
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        let id: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        // Col 2: `acct` (physical account FK)
        let account_id: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
        let category_id: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
        // Col 4: `amount` in cents (1 dollar = 100 units)
        let amount_cents: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
        // Col 5: `description` (physical payee FK → payees.id)
        let payee_id: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();
        let notes: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
        let date_int: i64 = row.get::<_, Option<i64>>(7)?.unwrap_or(0);
        let cleared: bool = row.get::<_, Option<i64>>(8)?.unwrap_or(0) != 0;
        let reconciled: bool = row.get::<_, Option<i64>>(9)?.unwrap_or(0) != 0;
        let is_child: bool = row.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0;
        let parent_id: String = row.get::<_, Option<String>>(11)?.unwrap_or_default();
        // Col 12: `financial_id` (AQL: imported_id)
        let imported_id: String = row.get::<_, Option<String>>(12)?.unwrap_or_default();
        // Col 13: `imported_description` (AQL: imported_payee)
        let imported_payee: String = row.get::<_, Option<String>>(13)?.unwrap_or_default();

        if id.is_empty() || date_int == 0 {
            new_cursor = new_cursor.max(rowid);
            continue;
        }

        let ts = match actual_date_to_ts(date_int) {
            Some(t) => t,
            None => {
                new_cursor = new_cursor.max(rowid);
                continue;
            }
        };

        // Derive YYYY-MM-DD for the Transaction's `posted` field.
        let posted = ts.get(..10).unwrap_or("").to_string();
        let amount_str = cents_to_decimal(amount_cents);

        // Resolve display names from lookup maps.
        let (account_name, account_offbudget) =
            accounts.get(&account_id).map_or(("Unknown Account".to_string(), false), |row| {
                (row.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                 row.get("offbudget").and_then(Value::as_i64).unwrap_or(0) != 0)
            });
        let (category_name, is_income) =
            categories.get(&category_id).map_or(("".to_string(), false), |row| {
                (row.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                 row.get("is_income").and_then(Value::as_i64).unwrap_or(0) != 0)
            });
        // Join payees via `description` FK (payee_id = transactions.description).
        let payee_name = payees
            .get(&payee_id)
            .and_then(|r| r.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Vault account id: slug from account name.
        let vault_account_id = crate::finance::model::account_slug("actual-budget", &account_name);

        // Build the contract Transaction.
        // description = payee/merchant line (consistent with SimpleFIN convention).
        // notes stay in extra to avoid polluting the description field.
        let mut extra: Map<String, Value> = Map::new();
        if !category_name.is_empty() {
            extra.insert("actual_category".into(), Value::String(category_name.clone()));
        }
        if !account_name.is_empty() {
            extra.insert("actual_account".into(), Value::String(account_name.clone()));
        }
        if !payee_name.is_empty() {
            extra.insert("actual_payee".into(), Value::String(payee_name.clone()));
        }
        if !imported_id.is_empty() {
            extra.insert("imported_id".into(), Value::String(imported_id.clone()));
        }
        if !notes.is_empty() {
            extra.insert("notes".into(), Value::String(notes.clone()));
        }

        let txn = Transaction {
            id: id.clone(),
            account: vault_account_id.clone(),
            posted: posted.clone(),
            transacted: None,
            amount: amount_str.clone(),
            currency: String::new(), // Actual doesn't expose currency per-transaction
            // description = payee name (merchant/payee line; consistent with SimpleFIN).
            // User notes go in extra["notes"], not here.
            description: payee_name.clone(),
            payee: if payee_name.is_empty() { None } else { Some(payee_name.clone()) },
            category: if category_name.is_empty() { None } else { Some(category_name.clone()) },
            pending: false,
            source: SOURCE.to_string(),
            extra,
        };

        let raw = RawRow {
            ts: ts.clone(),
            id: id.clone(),
            account_id: account_id.clone(),
            account_name: account_name.clone(),
            account_offbudget,
            payee_id: payee_id.clone(),
            payee_name: payee_name.clone(),
            category_id: category_id.clone(),
            category_name: category_name.clone(),
            is_income,
            date: format!("{date_int:08}"),
            amount_cents,
            amount: amount_str,
            notes,
            is_child,
            parent_id,
            cleared,
            reconciled,
            imported_id,
            imported_payee,
        };

        rows_out.push((txn, raw, vault_account_id));
        new_cursor = new_cursor.max(rowid);
    }

    Ok((rows_out, new_cursor))
}

/// Returns the set of column names for a table (schema-adaptive).
fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut cols = std::collections::HashSet::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        cols.insert(name);
    }
    Ok(cols)
}

/// Load a lookup table's non-tombstoned rows into a `id → {field → Value}` map.
fn load_lookup(
    conn: &rusqlite::Connection,
    table: &str,
    cols: &[&str],
) -> Result<BTreeMap<String, Map<String, Value>>> {
    // Check which of the requested columns actually exist.
    let existing = table_columns(conn, table)?;
    let sel_cols: Vec<&str> = cols.iter().copied().filter(|c| existing.contains(*c)).collect();
    if sel_cols.len() < 2 {
        // Table might not have enough columns (unusual migration).
        return Ok(BTreeMap::new());
    }
    let col_list = sel_cols.join(", ");
    let sql = format!("SELECT {col_list} FROM {table} WHERE tombstone = 0 OR tombstone IS NULL");
    let mut stmt = conn.prepare(&sql)?;
    let col_names = sel_cols.clone();
    let mut map = BTreeMap::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let mut entry = Map::new();
        for (i, name) in col_names.iter().enumerate().skip(1) {
            let v: Value = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(n) => Value::from(n),
                rusqlite::types::ValueRef::Real(f) => Value::from(f),
                rusqlite::types::ValueRef::Text(t) => {
                    Value::String(String::from_utf8_lossy(t).into_owned())
                }
                rusqlite::types::ValueRef::Blob(b) => {
                    Value::String(format!("<blob {} bytes>", b.len()))
                }
            };
            entry.insert(name.to_string(), v);
        }
        map.insert(id, entry);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Write contract rows into the canonical finance ledger + raw rows.
/// Returns the count of new contract rows written.
fn write_rows(
    vault: &Vault,
    rows: Vec<(Transaction, RawRow, String)>,
) -> Result<u64> {
    // Group by vault_account_id for batch upsert.
    let mut by_account: BTreeMap<String, Vec<Transaction>> = BTreeMap::new();
    let mut raw_rows: Vec<RawRow> = Vec::new();

    for (txn, raw, acct_id) in rows {
        by_account.entry(acct_id).or_default().push(txn);
        raw_rows.push(raw);
    }

    // Ensure accounts exist in the vault registry before upserting transactions.
    // We need the account names — derive from the first transaction in each group.
    {
        let mut accounts = vault.load_finance_accounts()?;
        let mut changed = false;
        for (acct_slug, txns) in &by_account {
            // Check if this account already exists (by alias or by id).
            if accounts.iter().any(|a| &a.id == acct_slug) {
                continue;
            }
            // The slug was derived from "actual-budget" + account_name.
            // Reconstruct account_name from the slug is lossy; use the raw
            // row's account_name instead.
            let account_name = txns
                .first()
                .and_then(|t| t.extra.get("actual_account"))
                .and_then(Value::as_str)
                .unwrap_or(acct_slug.as_str());
            vault.resolve_finance_account(
                &mut accounts,
                SOURCE,
                acct_slug, // source_id = the vault slug itself (stable)
                account_name,
                "Actual Budget",
                "", // currency unknown at schema level
            );
            changed = true;
        }
        if changed {
            vault.save_finance_accounts(&accounts)?;
        }
    }

    let mut total_new: usize = 0;
    for (acct_id, txns) in by_account {
        let (new, _updated) = vault.upsert_finance_transactions(&acct_id, &txns, false)?;
        total_new += new;
    }

    // Raw layer: append unconditionally (month partitioned).
    if !raw_rows.is_empty() {
        let raw_stream = vault.stream(RAW_DIR, Partition::Month);
        raw_stream.append(&raw_rows, |r| &r.ts)?;
    }

    Ok(total_new as u64)
}

// ---------------------------------------------------------------------------
// The pull (auto-discovery path).

/// One sync pass over all discovered Actual Budget databases.
pub fn pull(vault: &Vault) -> Result<crate::registry::PullOutcome> {
    let dbs = find_actual_dbs();
    if dbs.is_empty() {
        return Ok(crate::registry::PullOutcome {
            headline: "No Actual Budget database found".to_string(),
            counts: BTreeMap::from([("transactions", 0)]),
        });
    }
    let mut state = vault.read_actual_budget_sync().unwrap_or_default();
    let mut total: u64 = 0;

    for db_path in &dbs {
        let key = db_path.display().to_string();
        let cursor = state.cursors.entry(key.clone()).or_default().last_rowid;

        // Use a stem that is unique per (process, db-path) to avoid races when
        // multiple budget files are processed in parallel.
        let path_hash: u64 = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            db_path.hash(&mut h);
            h.finish()
        };
        let stem = format!("trove-actual-budget-{}-{path_hash:x}", std::process::id());
        let (rows, new_cursor) = import_via_copy(db_path, &stem, |tmp| {
            read_actual_db(tmp, cursor)
        })
        .with_context(|| format!("reading Actual db {key}"))?;

        let written = write_rows(vault, rows)?;
        total += written;
        state.cursors.entry(key).and_modify(|c| c.last_rowid = new_cursor);
    }

    state.updated = Local::now().to_rfc3339();
    state.total_transactions += total;
    vault.write_actual_budget_sync(&state)?;

    Ok(crate::registry::PullOutcome {
        headline: format!("{total} transactions"),
        counts: BTreeMap::from([("transactions", total)]),
    })
}

/// Import a single db.sqlite (or unzipped path) directly, no path discovery.
/// Used by the import box and tests.
pub fn import_one_db(vault: &Vault, db_path: &Path) -> Result<crate::registry::PullOutcome> {
    let mut state = vault.read_actual_budget_sync().unwrap_or_default();
    let key = db_path.display().to_string();
    let cursor = state.cursors.get(&key).map_or(0, |c| c.last_rowid);

    // Always copy before reading (WAL-safe; db may be held open by Actual).
    // Stem is unique per (process, db-path) to avoid test-parallel collisions.
    let path_hash: u64 = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        db_path.hash(&mut h);
        h.finish()
    };
    let stem = format!("trove-actual-budget-import-{}-{path_hash:x}", std::process::id());
    let (rows, new_cursor) = import_via_copy(db_path, &stem, |tmp| read_actual_db(tmp, cursor))?;

    let written = write_rows(vault, rows)?;
    state.cursors.insert(key, BudgetCursor { last_rowid: new_cursor });
    state.updated = Local::now().to_rfc3339();
    state.total_transactions += written;
    vault.write_actual_budget_sync(&state)?;

    Ok(crate::registry::PullOutcome {
        headline: format!("{written} transactions"),
        counts: BTreeMap::from([("transactions", written)]),
    })
}


// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-actual-budget-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Helpers: construct a minimal Actual db.sqlite using the REAL physical schema.
    //
    // Physical column names (raw db.sqlite, NOT AQL logical names):
    //   acct             = account FK   (AQL: account)
    //   description      = payee FK     (AQL: payee)
    //   isParent         = split parent (AQL: is_parent)  [camelCase]
    //   isChild          = split child  (AQL: is_child)   [camelCase]
    //   financial_id     = bank import dedup (AQL: imported_id)
    //   imported_description = pre-rule payee (AQL: imported_payee)
    //   amount           = cents (1 dollar = 100 units)

    fn create_test_db(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE transactions (
                ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT,
                isParent INTEGER DEFAULT 0,
                isChild INTEGER DEFAULT 0,
                parent_id TEXT,
                acct TEXT,
                category TEXT,
                amount INTEGER DEFAULT 0,
                description TEXT,
                notes TEXT,
                date INTEGER,
                financial_id TEXT,
                imported_description TEXT,
                cleared INTEGER DEFAULT 1,
                reconciled INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0
            );
            CREATE TABLE accounts (
                id TEXT PRIMARY KEY,
                name TEXT,
                offbudget INTEGER DEFAULT 0,
                closed INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0
            );
            CREATE TABLE categories (
                id TEXT PRIMARY KEY,
                name TEXT,
                is_income INTEGER DEFAULT 0,
                tombstone INTEGER DEFAULT 0
            );
            CREATE TABLE payees (
                id TEXT PRIMARY KEY,
                name TEXT,
                tombstone INTEGER DEFAULT 0
            );",
        )
        .unwrap();
        conn
    }

    fn insert_account(conn: &rusqlite::Connection, id: &str, name: &str, offbudget: i64) {
        conn.execute(
            "INSERT INTO accounts (id, name, offbudget) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, name, offbudget],
        )
        .unwrap();
    }

    fn insert_category(conn: &rusqlite::Connection, id: &str, name: &str, is_income: i64) {
        conn.execute(
            "INSERT INTO categories (id, name, is_income) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, name, is_income],
        )
        .unwrap();
    }

    fn insert_payee(conn: &rusqlite::Connection, id: &str, name: &str) {
        conn.execute(
            "INSERT INTO payees (id, name) VALUES (?1, ?2)",
            rusqlite::params![id, name],
        )
        .unwrap();
    }

    /// Insert a transaction using the real physical column names.
    /// `acct_id` = FK → accounts.id (physical: `acct`)
    /// `payee_id` = FK → payees.id (physical: `description`)
    /// `amount_cents` = integer cents (1 dollar = 100)
    fn insert_txn(
        conn: &rusqlite::Connection,
        id: &str,
        acct_id: &str,
        category: &str,
        amount_cents: i64,
        payee_id: &str,
        notes: &str,
        date: i64,
        tombstone: i64,
        is_parent: i64,
    ) {
        conn.execute(
            "INSERT INTO transactions (id, acct, category, amount, description, notes, date, tombstone, isParent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![id, acct_id, category, amount_cents, payee_id, notes, date, tombstone, is_parent],
        )
        .unwrap();
    }

    // ---------------------------------------------------------------------------
    // Pure helpers.

    #[test]
    fn date_conversion_is_correct() {
        // 20260528 → local midnight RFC3339 starting with "2026-05-28"
        let ts = actual_date_to_ts(20260528).unwrap();
        assert!(ts.starts_with("2026-05-28"), "got: {ts}");
        assert!(actual_date_to_ts(0).is_none());
        assert!(actual_date_to_ts(99991399).is_none(), "month 13 must fail");
    }

    #[test]
    fn cents_to_decimal_is_correct() {
        // $55.00 is stored as 5500 cents
        assert_eq!(cents_to_decimal(-5500), "-55.00");
        // $42.50 outflow: stored as -4250
        assert_eq!(cents_to_decimal(-4250), "-42.50");
        assert_eq!(cents_to_decimal(100), "1.00");
        assert_eq!(cents_to_decimal(-1), "-0.01");
        assert_eq!(cents_to_decimal(0), "0.00");
        // $12345.67 income: stored as 1234567
        assert_eq!(cents_to_decimal(1234567), "12345.67");
        assert_eq!(cents_to_decimal(-100), "-1.00");
        // $120.30 income: stored as 12030 (per Actual API reference)
        assert_eq!(cents_to_decimal(12030), "120.30");
    }

    // ---------------------------------------------------------------------------
    // DB reader fixtures using real physical schema.

    #[test]
    fn reads_transactions_with_full_join() {
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-read-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "acct-1", "Checking", 0);
        insert_category(&conn, "cat-1", "Groceries", 0);
        insert_payee(&conn, "pay-1", "Kroger");
        // $55.00 outflow = -5500 cents; payee FK stored in `description` column
        insert_txn(&conn, "txn-abc", "acct-1", "cat-1", -5500, "pay-1", "weekly shop", 20260601, 0, 0);
        drop(conn);

        let (rows, cursor) = read_actual_db(&tmp, 0).unwrap();
        assert_eq!(rows.len(), 1, "one transaction row");
        let (txn, raw, acct_id) = &rows[0];

        assert_eq!(txn.id, "txn-abc");
        // $55.00 outflow: stored as -5500 cents → "-55.00"
        assert_eq!(txn.amount, "-55.00");
        assert_eq!(txn.payee.as_deref(), Some("Kroger"));
        assert_eq!(txn.category.as_deref(), Some("Groceries"));
        // description = payee name (not notes); notes go to extra["notes"]
        assert_eq!(txn.description, "Kroger");
        assert_eq!(txn.extra.get("notes").and_then(Value::as_str), Some("weekly shop"));
        assert_eq!(txn.posted, "2026-06-01");
        assert_eq!(txn.source, SOURCE);

        assert_eq!(raw.account_name, "Checking");
        assert_eq!(raw.category_name, "Groceries");
        assert_eq!(raw.payee_name, "Kroger");
        // Raw layer stores cents
        assert_eq!(raw.amount_cents, -5500);
        assert!(!raw.account_offbudget);
        assert!(!raw.is_income);
        assert!(raw.date.starts_with("20260601"));

        assert!(acct_id.starts_with("actual-budget-"), "slug prefixed: {acct_id}");
        assert!(cursor >= 1, "cursor advanced");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn tombstoned_and_parent_rows_skipped() {
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-tomb-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "acct-1", "Checking", 0);
        // Tombstoned — must be skipped.
        insert_txn(&conn, "txn-dead", "acct-1", "", -100, "", "", 20260601, 1, 0);
        // Parent split (isParent=1) — must be skipped to avoid double-counting.
        insert_txn(&conn, "txn-parent", "acct-1", "", -500, "", "", 20260601, 0, 1);
        // Normal row — must be returned.
        insert_txn(&conn, "txn-good", "acct-1", "", -200, "", "", 20260601, 0, 0);
        drop(conn);

        let (rows, _) = read_actual_db(&tmp, 0).unwrap();
        assert_eq!(rows.len(), 1, "only the good row (tombstoned + isParent skipped)");
        assert_eq!(rows[0].0.id, "txn-good");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn incremental_cursor_resumes_from_rowid() {
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-incr-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "acct-1", "Checking", 0);
        insert_txn(&conn, "txn-1", "acct-1", "", -100, "", "", 20260601, 0, 0);
        insert_txn(&conn, "txn-2", "acct-1", "", -200, "", "", 20260602, 0, 0);
        drop(conn);

        // First pass: both rows.
        let (rows1, cursor1) = read_actual_db(&tmp, 0).unwrap();
        assert_eq!(rows1.len(), 2);
        assert!(cursor1 >= 2);

        // Second pass from the saved cursor: no new rows.
        let (rows2, cursor2) = read_actual_db(&tmp, cursor1).unwrap();
        assert_eq!(rows2.len(), 0, "nothing new");
        assert_eq!(cursor2, cursor1, "cursor unchanged with no new rows");
        let _ = std::fs::remove_file(&tmp);
    }

    // ---------------------------------------------------------------------------
    // Full vault integration.

    #[test]
    fn import_writes_contract_and_raw_layers() {
        let v = temp_vault("import_full");
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-full-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "acct-1", "Checking", 0);
        insert_category(&conn, "cat-food", "Groceries", 0);
        insert_payee(&conn, "pay-kroger", "Kroger");
        // $42.00 outflow = -4200 cents
        insert_txn(&conn, "txn-a", "acct-1", "cat-food", -4200, "pay-kroger", "", 20260528, 0, 0);
        // $50000.00 income = 5000000 cents
        insert_txn(&conn, "txn-b", "acct-1", "", 5000000, "", "Paycheck", 20260601, 0, 0);
        drop(conn);

        let out = import_one_db(&v, &tmp).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&2));

        // Contract layer: finance/transactions/<acct>/<year>.jsonl
        let txns = v.finance_transactions(None, 100).unwrap();
        assert_eq!(txns.len(), 2, "two contract rows");
        let paycheck = txns.iter().find(|t| t.id == "txn-b").unwrap();
        // $50000.00 = 5000000 cents → "50000.00"
        assert_eq!(paycheck.amount, "50000.00");
        // No payee for paycheck; notes stored in extra
        assert_eq!(paycheck.extra.get("notes").and_then(Value::as_str), Some("Paycheck"));
        let grocery = txns.iter().find(|t| t.id == "txn-a").unwrap();
        // $42.00 outflow = -4200 cents → "-42.00"
        assert_eq!(grocery.amount, "-42.00");
        assert_eq!(grocery.payee.as_deref(), Some("Kroger"));
        assert_eq!(grocery.category.as_deref(), Some("Groceries"));
        // description = payee name (not notes)
        assert_eq!(grocery.description, "Kroger");

        // Raw layer: finance/actual-budget/raw/YYYY-MM.jsonl
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let mut raw_rows: Vec<serde_json::Value> = Vec::new();
        for key in raw_stream.partitions().unwrap() {
            raw_rows.extend(raw_stream.read::<serde_json::Value>(&key).unwrap());
        }
        assert_eq!(raw_rows.len(), 2, "two raw rows");
        let raw_grocery = raw_rows.iter().find(|r| r["id"] == "txn-a").unwrap();
        // Raw layer stores cents
        assert_eq!(raw_grocery["amount_cents"], serde_json::json!(-4200));
        assert_eq!(raw_grocery["account_name"], "Checking");
        assert_eq!(raw_grocery["category_name"], "Groceries");
        assert_eq!(raw_grocery["payee_name"], "Kroger");

        // Cursor advanced in sync state.
        let state = v.read_actual_budget_sync().unwrap();
        let key = tmp.display().to_string();
        assert!(state.cursors.get(&key).map_or(0, |c| c.last_rowid) >= 2);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn re_import_is_idempotent() {
        let v = temp_vault("idempotent");
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-idem-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "acct-1", "Checking", 0);
        insert_txn(&conn, "txn-x", "acct-1", "", -100, "", "", 20260601, 0, 0);
        drop(conn);

        let out1 = import_one_db(&v, &tmp).unwrap();
        assert_eq!(out1.counts.get("transactions"), Some(&1));
        // Second import: cursor is advanced, so same DB yields 0 new rows.
        let out2 = import_one_db(&v, &tmp).unwrap();
        assert_eq!(out2.counts.get("transactions"), Some(&0));
        // Contract file is byte-identical.
        let txns = v.finance_transactions(None, 100).unwrap();
        assert_eq!(txns.len(), 1, "no duplicate");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn off_budget_account_is_included_with_flag() {
        let v = temp_vault("offbudget");
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-offb-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "savings", "Savings (off-budget)", 1); // offbudget=1
        insert_txn(&conn, "txn-s", "savings", "", 10000, "", "", 20260601, 0, 0);
        drop(conn);

        let out = import_one_db(&v, &tmp).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&1));

        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let mut raws: Vec<serde_json::Value> = Vec::new();
        for key in raw_stream.partitions().unwrap() {
            raws.extend(raw_stream.read::<serde_json::Value>(&key).unwrap());
        }
        let r = &raws[0];
        assert_eq!(r["account_offbudget"], serde_json::json!(true), "off-budget flagged in raw");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("syncstate");
        let state = SyncState {
            cursors: BTreeMap::from([("some/path/db.sqlite".into(), BudgetCursor { last_rowid: 42 })]),
            updated: "2026-06-01T08:00:00-07:00".into(),
            total_transactions: 100,
        };
        v.write_actual_budget_sync(&state).unwrap();
        let loaded = v.read_actual_budget_sync().unwrap();
        assert_eq!(loaded.cursors.get("some/path/db.sqlite").unwrap().last_rowid, 42);
        assert_eq!(loaded.total_transactions, 100);
    }

    #[test]
    fn pull_returns_not_found_when_no_dbs() {
        // find_actual_dbs will return empty on this machine (Actual not installed).
        // The auto-discovery pull should return 0 gracefully.
        let v = temp_vault("nodbs");
        let out = pull(&v).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&0));
    }

    #[test]
    fn schema_adaptive_reads_without_some_columns() {
        // A DB without `cleared`, `reconciled`, `imported_description` columns —
        // simulate an older migration state using physical column names. Reader must not error.
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-oldschema-{}.sqlite",
            std::process::id()
        ));
        let conn = rusqlite::Connection::open(&tmp).unwrap();
        conn.execute_batch(
            "CREATE TABLE transactions (
                id TEXT, acct TEXT, amount INTEGER DEFAULT 0,
                date INTEGER, tombstone INTEGER DEFAULT 0
            );
            CREATE TABLE accounts (id TEXT, name TEXT, tombstone INTEGER DEFAULT 0);
            CREATE TABLE categories (id TEXT, name TEXT, tombstone INTEGER DEFAULT 0);
            CREATE TABLE payees (id TEXT, name TEXT, tombstone INTEGER DEFAULT 0);",
        ).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, name) VALUES ('a1', 'OldChecking')",
            [],
        ).unwrap();
        // $20.00 outflow = -2000 cents
        conn.execute(
            "INSERT INTO transactions (id, acct, amount, date) VALUES ('t1', 'a1', -2000, 20260601)",
            [],
        ).unwrap();
        drop(conn);

        let (rows, _) = read_actual_db(&tmp, 0).unwrap();
        assert_eq!(rows.len(), 1, "reads minimal schema");
        assert_eq!(rows[0].0.id, "t1");
        // -2000 cents = -$20.00
        assert_eq!(rows[0].0.amount, "-20.00");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn amount_in_cents_not_milliunits() {
        // Key regression: Actual stores cents (÷100), NOT milliunits (÷1000).
        // $120.30 = 12030 cents; $55.00 outflow = -5500 cents.
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-cents-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "a1", "Checking", 0);
        // $120.30 income: 12030 cents (per Actual API reference: $120.30 = 12030)
        insert_txn(&conn, "t1", "a1", "", 12030, "", "", 20260601, 0, 0);
        // $55.00 outflow: -5500 cents
        insert_txn(&conn, "t2", "a1", "", -5500, "", "", 20260601, 0, 0);
        drop(conn);

        let (rows, _) = read_actual_db(&tmp, 0).unwrap();
        assert_eq!(rows.len(), 2);
        let t1 = rows.iter().find(|(t, _, _)| t.id == "t1").unwrap();
        let t2 = rows.iter().find(|(t, _, _)| t.id == "t2").unwrap();
        assert_eq!(t1.0.amount, "120.30", "$120.30 = 12030 cents");
        assert_eq!(t2.0.amount, "-55.00", "$55.00 outflow = -5500 cents");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn payee_resolved_via_description_column() {
        // Regression: payee FK is stored in physical column `description` (not `payee`).
        // Joining payees ON payees.id = transactions.description must resolve the name.
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-payeefk-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "a1", "Checking", 0);
        insert_payee(&conn, "p-amazon", "Amazon");
        // `description` column = payee FK (physical name)
        insert_txn(&conn, "t1", "a1", "", -2999, "p-amazon", "", 20260601, 0, 0);
        drop(conn);

        let (rows, _) = read_actual_db(&tmp, 0).unwrap();
        assert_eq!(rows.len(), 1);
        let (txn, raw, _) = &rows[0];
        assert_eq!(txn.payee.as_deref(), Some("Amazon"), "payee resolved from description FK");
        assert_eq!(raw.payee_name, "Amazon");
        // description field = payee name, not the FK uuid
        assert_eq!(txn.description, "Amazon");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn imported_fields_use_physical_column_names() {
        // Regression: physical `financial_id` (AQL: imported_id) and
        // `imported_description` (AQL: imported_payee) must be read correctly.
        let tmp = std::env::temp_dir().join(format!(
            "trove-ab-importedfk-{}.sqlite",
            std::process::id()
        ));
        let conn = create_test_db(&tmp);
        insert_account(&conn, "a1", "Checking", 0);
        insert_txn(&conn, "t1", "a1", "", -1000, "", "", 20260601, 0, 0);
        // Manually set financial_id and imported_description (physical column names)
        conn.execute(
            "UPDATE transactions SET financial_id='bank-ref-123', imported_description='RAW PAYEE NAME' WHERE id='t1'",
            [],
        ).unwrap();
        drop(conn);

        let (rows, _) = read_actual_db(&tmp, 0).unwrap();
        assert_eq!(rows.len(), 1);
        let (txn, raw, _) = &rows[0];
        assert_eq!(
            txn.extra.get("imported_id").and_then(Value::as_str),
            Some("bank-ref-123"),
            "financial_id → extra.imported_id"
        );
        assert_eq!(raw.imported_payee, "RAW PAYEE NAME", "imported_description → raw.imported_payee");
        let _ = std::fs::remove_file(&tmp);
    }
}

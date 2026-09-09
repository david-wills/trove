//! Cash App transaction export — CSV delivered by email from cash.app.
//!
//! ## Export path
//!
//! Web only: cash.app → Activity → ⋯ menu → Export Transactions → All Time → CSV.
//! The file is **emailed** to the registered address within minutes — it is NOT
//! downloaded directly. There is no mobile export and no public API.
//! Standalone-clean: no network calls at import time.
//!
//! ## Column format (CONFIRMED)
//!
//! Schema confirmed from community parser `SolidX/FinanceExportTools`
//! `CashAppExportMapper.cs` and cross-referenced real-export samples:
//!
//! ```text
//! Transaction ID, Date, Transaction Type, Currency, Amount, Fee,
//! Net Amount, Asset Type, Asset Price, Asset Amount, Status,
//! Notes, Name of sender/receiver, Account
//! ```
//!
//! Sign convention: outflows (Cash out, Cash Card Purchase, Bitcoin Buy) are
//! already **negative** in the `Amount` column. No sign flip needed.
//!
//! Date format: `"YYYY-MM-DD HH:MM:SS TZ"` (e.g. `"2026-05-28 14:32:10 EST"`).
//! The time/timezone suffix is stripped by `parse_date`.
//!
//! ## Vault layout
//!
//! - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
//!   [`crate::finance::Transaction`] per row; source="cash-app".
//!   `posted` = date column (date part only); `transacted` = same (no separate
//!   transaction date).
//!   `id` = `"cash-app-<Transaction ID>"` — the stable Cash App transaction ID.
//! - **Raw:** `finance/cash-app/raw/<account>/YYYY.jsonl` — verbatim row,
//!   all columns, full fidelity, unconditional.
//!
//! ## Dedupe
//!
//! `id` = `"cash-app-<Transaction ID>"` (the real Cash App transaction ID).
//! Re-importing the same file is a clean no-op. Cross-source fuzzy matching
//! (SimpleFIN overlap) uses amount + date + description similarity.
//!
//! ## Bitcoin overlap
//!
//! Cash App BTC buy/sell rows and on-chain Blockstream rows coexist in their
//! own accounts — reconciliation by txid is a read-time concern, never write-time.
//! `asset_type`, `asset_price`, and `asset_amount` are preserved in `extra`.
//!
//! ## Amount vs Net Amount
//!
//! `Amount` (gross) is stored as the canonical signed amount.
//! `Fee` and `Net Amount` (= Amount − Fee) are preserved in `extra`.
//! For BTC buys/sells the fee is material for cost-basis calculations.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/cash-app.md

use std::path::Path;

use anyhow::Result;

use crate::finance::import::CsvImportStats;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Import.
//
// The Cash App preset is WIRED: `crate::finance::import::cash_app_columns`
// recognizes the confirmed header row, and `finance_import_csv` dispatches to
// `import_cash_app` for all Cash App exports.

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let get = |k: &str| params.get(k).map(String::as_str).map(str::trim).filter(|v| !v.is_empty());
    let s = vault.finance_import_csv(path, get("account"), get("new_account"))?;
    outcome_from(s)
}

static IMPORT_SPEC: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[
        crate::registry::ImportParam {
            key: "account",
            label: "Existing account id",
            placeholder: "pick or leave empty when naming a new account",
            required: false,
        },
        crate::registry::ImportParam {
            key: "new_account",
            label: "New account name",
            placeholder: "e.g. Cash App",
            required: false,
        },
    ],
    run: run_import,
};

fn last_data(vault: &Vault) -> Option<String> {
    vault.read_finance_import_state().map(|s| s.updated).filter(|u| !u.is_empty())
}

// ---------------------------------------------------------------------------
// DEF.

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "cash-app",
        name: "Cash App",
        kind: IntegrationKind::Import,
        // Financial detail — opt-in; users must explicitly enable.
        default_on: false,
        description: "Import Cash App transactions from the account CSV export. \
                      The file is requested on the web (cash.app → Activity → ⋯ → \
                      Export Transactions → All Time → CSV) and delivered to your \
                      registered email within minutes. No API is available — this \
                      export is the only complete record of peer payments, Cash App \
                      Card purchases, and Bitcoin buys/sells.",
        domain: "finance",
        vault_path: "finance/cash-app/",
        toggleable: false,
        setup: &[
            "On the web at cash.app, go to Activity → the three-dot menu → \
             Export Transactions → All Time → CSV.",
            "The file will be emailed to your registered address within minutes \
             (it is not downloaded directly — check your inbox).",
            "Drag the emailed CSV here. Supply an account name in the import box \
             (e.g. \"Cash App\") if this is your first import.",
            "Re-imports and overlaps dedupe cleanly — you can re-run the same file \
             without creating duplicates.",
        ],
        caveats: "Cash App has no public API and no bank-aggregator connection — \
                  the email-delivered CSV export is the only route. Export is \
                  web-only; the mobile app has no export. Bitcoin buy/sell rows in \
                  the export coexist with on-chain Blockstream data (different \
                  accounts); read-time reconciliation by transaction ID handles any \
                  overlap.",
    },
    behavior: Behavior::Import(&IMPORT_SPEC),
    permission: None,
    last_data: Some(last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Shared outcome formatter.

fn outcome_from(s: CsvImportStats) -> Result<ImportOutcome> {
    Ok(ImportOutcome {
        headline: format!(
            "{} new transactions into {} (cash-app), {} duplicates skipped",
            s.new_transactions, s.account, s.duplicates
        ),
        counts: [
            ("rows", s.rows as u64),
            ("new_transactions", s.new_transactions as u64),
            ("duplicates", s.duplicates as u64),
            ("skipped", s.skipped as u64),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-cash-app-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Confirmed real Cash App CSV shape.
    //
    // Header confirmed from `SolidX/FinanceExportTools` CashAppExportMapper.cs
    // and real-export samples.
    // Date format: "YYYY-MM-DD HH:MM:SS TZ" (e.g. "2026-05-28 14:32:10 EST").
    // Sign convention: outflows are negative (e.g. "-50.00"), inflows positive.

    /// Fixture with the confirmed Cash App real-export column shape.
    /// Covers: peer payment out, peer payment in, card purchase, BTC buy.
    const REAL_SHAPE_CSV: &str = "\
Transaction ID,Date,Transaction Type,Currency,Amount,Fee,Net Amount,Asset Type,Asset Price,Asset Amount,Status,Notes,Name of sender/receiver,Account
CAXXXXYYYYZZZZ01,2026-05-28 14:32:10 EST,Cash out,USD,-50.00,0.00,-50.00,,,,,Dinner split,Alice Smith,Cash App
CABBBBBCCCCDDDD,2026-05-15 09:10:05 EDT,Cash in,USD,100.00,0.00,100.00,,,,,May rent split,Bob Jones,Cash App
CAEEEEEFFFGGGG0,2026-05-10 18:45:22 CDT,Cash Card Purchase,USD,-12.50,0.00,-12.50,,,,,STARBUCKS SEATTLE WA,,Cash App
CAHHHHIIIIJJJJJ,2026-05-05 11:20:00 EST,Bitcoin Buy,USD,-500.00,1.25,-501.25,BTC,50000.00,0.00999750,Complete,BTC purchase,,Cash App
";

    // -----------------------------------------------------------------------
    // Hub card tests — always valid.

    #[test]
    fn hub_card_shows_import_box_and_is_opt_in() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "cash-app").unwrap();
        let import_info = card.import.as_ref().expect("import box must be present");
        assert_eq!(import_info.accepts, &["csv"], "accepts CSV files");
        assert!(!card.enabled, "default_on=false: financial data is opt-in");
    }

    #[test]
    fn hub_card_params_include_account_and_new_account() {
        let v = temp_vault("params");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "cash-app").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        let keys: Vec<&str> = import_info.params.iter().map(|p| p.key).collect();
        assert!(keys.contains(&"account"), "account param present");
        assert!(keys.contains(&"new_account"), "new_account param present");
    }

    #[test]
    fn last_data_is_none_when_no_import_has_run() {
        let v = temp_vault("lastdata");
        assert!(last_data(&v).is_none(), "no import state yet");
    }

    // -----------------------------------------------------------------------
    // Real-shape parser tests.

    #[test]
    fn imports_real_shape_csv_correct_count() {
        let v = temp_vault("realshape");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("new_account".to_string(), "Cash App".to_string());

        let outcome = run_import(&v, &csv_path, &params, &mut |_| {})
            .expect("real-shape CSV must import without error");

        let new_txns = outcome.counts["new_transactions"];
        assert_eq!(new_txns, 4, "all 4 rows should import: got {:?}", outcome.counts);

        let rows_count = outcome.counts["rows"];
        assert_eq!(rows_count, 4, "4 data rows in the fixture");

        let skipped = outcome.counts["skipped"];
        assert_eq!(skipped, 0, "no rows should be skipped with the real date format");
    }

    #[test]
    fn guid_uses_real_transaction_id() {
        // The Transaction.id must be "cash-app-<Transaction ID>", not a hash.
        let v = temp_vault("guid");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("new_account".to_string(), "Cash App".to_string());

        run_import(&v, &csv_path, &params, &mut |_| {}).expect("import OK");

        // Read the stored transactions and verify the ids.
        let txns = v.finance_transactions(None, 100).expect("read transactions");
        assert!(!txns.is_empty(), "should have transactions");

        // Check that the first transaction uses the real Transaction ID.
        let outflow = txns.iter().find(|t| t.id == "cash-app-CAXXXXYYYYZZZZ01");
        assert!(
            outflow.is_some(),
            "must have a transaction with id 'cash-app-CAXXXXYYYYZZZZ01'; \
             actual ids: {:?}",
            txns.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn amounts_and_signs_correct() {
        // Outflows must be negative; inflows positive.
        let v = temp_vault("amounts");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("new_account".to_string(), "Cash App".to_string());

        run_import(&v, &csv_path, &params, &mut |_| {}).expect("import OK");

        let txns = v.finance_transactions(None, 100).expect("read");

        // Peer payment out: Amount = -50.00
        let alice = txns.iter().find(|t| t.id == "cash-app-CAXXXXYYYYZZZZ01")
            .expect("alice txn");
        assert_eq!(alice.amount, "-50.00", "Cash out should be negative");

        // Peer payment in: Amount = 100.00
        let bob = txns.iter().find(|t| t.id == "cash-app-CABBBBBCCCCDDDD")
            .expect("bob txn");
        assert_eq!(bob.amount, "100.00", "Cash in should be positive");

        // Card purchase: Amount = -12.50
        let starbucks = txns.iter().find(|t| t.id == "cash-app-CAEEEEEFFFGGGG0")
            .expect("starbucks txn");
        assert_eq!(starbucks.amount, "-12.50", "Card purchase should be negative");

        // BTC buy: Amount = -500.00
        let btc = txns.iter().find(|t| t.id == "cash-app-CAHHHHIIIIJJJJJ")
            .expect("btc txn");
        assert_eq!(btc.amount, "-500.00", "BTC buy should be negative");
    }

    #[test]
    fn btc_fields_preserved_in_extra() {
        // BTC buy rows must have asset_type, asset_price, asset_amount in extra.
        let v = temp_vault("btcextra");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("new_account".to_string(), "Cash App".to_string());

        run_import(&v, &csv_path, &params, &mut |_| {}).expect("import OK");

        let txns = v.finance_transactions(None, 100).expect("read");
        let btc = txns.iter().find(|t| t.id == "cash-app-CAHHHHIIIIJJJJJ")
            .expect("btc txn");

        assert_eq!(
            btc.extra.get("asset_type").and_then(|v| v.as_str()),
            Some("BTC"),
            "asset_type must be 'BTC'"
        );
        assert_eq!(
            btc.extra.get("asset_price").and_then(|v| v.as_str()),
            Some("50000.00"),
            "asset_price must be preserved"
        );
        assert_eq!(
            btc.extra.get("asset_amount").and_then(|v| v.as_str()),
            Some("0.00999750"),
            "asset_amount must be preserved"
        );
        // Fee also in extra.
        assert_eq!(
            btc.extra.get("fee").and_then(|v| v.as_str()),
            Some("1.25"),
            "fee must be in extra"
        );
    }

    #[test]
    fn date_format_strips_time_and_timezone() {
        // The real Cash App date format "2026-05-28 14:32:10 EST" must parse to "2026-05-28".
        let v = temp_vault("datefmt");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("new_account".to_string(), "Cash App".to_string());

        run_import(&v, &csv_path, &params, &mut |_| {}).expect("import OK");

        let txns = v.finance_transactions(None, 100).expect("read");
        let alice = txns.iter().find(|t| t.id == "cash-app-CAXXXXYYYYZZZZ01")
            .expect("alice txn");
        assert_eq!(alice.posted, "2026-05-28", "date must be YYYY-MM-DD only");
    }

    #[test]
    fn reimport_is_idempotent() {
        // Second import of the same file must add 0 new transactions.
        let v = temp_vault("reimport");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("new_account".to_string(), "Cash App".to_string());

        let first = run_import(&v, &csv_path, &params, &mut |_| {})
            .expect("first import OK");
        let second = run_import(&v, &csv_path, &params, &mut |_| {})
            .expect("second import OK");

        let new1 = first.counts["new_transactions"];
        let new2 = second.counts["new_transactions"];
        assert_eq!(new1, 4, "first import adds 4 transactions");
        assert_eq!(new2, 0, "re-import adds nothing (idempotent)");
    }

    #[test]
    fn raw_layer_written() {
        // The verbatim raw JSONL file must be written.
        let v = temp_vault("rawlayer");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("new_account".to_string(), "Cash App".to_string());

        run_import(&v, &csv_path, &params, &mut |_| {}).expect("import OK");

        // The raw directory should exist and have at least one JSONL file.
        let raw_dir = v.root().join("finance/cash-app/raw");
        assert!(raw_dir.exists(), "raw dir must exist: {:?}", raw_dir);
        let entries: Vec<_> = fs::read_dir(&raw_dir)
            .expect("read raw dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(!entries.is_empty(), "raw dir should contain subdirs");

        // Find any JSONL file and verify it has content.
        fn find_jsonl(dir: &std::path::Path) -> Option<std::path::PathBuf> {
            fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).find_map(|e| {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    Some(p)
                } else if p.is_dir() {
                    find_jsonl(&p)
                } else {
                    None
                }
            })
        }
        let jsonl = find_jsonl(&raw_dir).expect("a .jsonl file must exist under raw/");
        let content = fs::read_to_string(&jsonl).expect("read jsonl");
        assert!(!content.trim().is_empty(), "raw JSONL must not be empty");

        // Each line should be valid JSON and contain the Transaction ID column.
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let obj: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("raw line must be valid JSON: {e}\nLine: {line}"));
            assert!(
                obj.get("Transaction ID").is_some(),
                "raw row must have 'Transaction ID' field: {obj}"
            );
        }
    }

    #[test]
    fn requires_no_account_defaults_to_cash_app_account() {
        // Without an account param, the importer auto-creates a "Cash App" account.
        let v = temp_vault("noaccount");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let params = std::collections::BTreeMap::new(); // empty

        // Should succeed (not error) — Cash App importer creates a default account.
        let outcome = run_import(&v, &csv_path, &params, &mut |_| {});
        assert!(outcome.is_ok(), "empty params should work (auto-create Cash App account)");
    }

    #[test]
    fn last_data_set_after_import() {
        let v = temp_vault("lastdata2");
        let csv_path = v.root().join("cash-app.csv");
        fs::write(&csv_path, REAL_SHAPE_CSV).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("new_account".to_string(), "Cash App".to_string());

        assert!(last_data(&v).is_none(), "no import yet");
        run_import(&v, &csv_path, &params, &mut |_| {}).expect("import OK");
        assert!(last_data(&v).is_some(), "last_data must be set after import");
    }
}

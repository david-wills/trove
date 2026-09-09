//! Vanguard CSV export — brokerage, retirement, and mutual-fund transaction
//! history imported from the Vanguard investor portal.
//!
//! ## Export path
//!
//! investor.vanguard.com → My Accounts → Transaction History → Download →
//! CSV (one 18-month window per download; drop multiple files to cover a
//! longer period — overlapping windows dedupe cleanly).
//!
//! Holdings: Portfolio → Export (a separate CSV preset; currently raw-only
//! pending the Phase 3 finance-holdings contract).
//!
//! No consumer API; no OAuth; no network calls at import time. Standalone-
//! clean.
//!
//! ## Column format (SCAFFOLD — Needs-sample)
//!
//! Vanguard does not publish the CSV schema. The column recognizer in
//! [`crate::finance::import::vanguard_columns`] is built from the most-cited
//! community reports but **has not been verified against a real export**. The
//! signature columns (`Trade Date` + `Net Amount` + `Transaction Type`) are
//! likely stable; the optional brokerage-detail columns (Symbol, Shares,
//! Share Price, …) should be confirmed against the actual export before
//! relying on them. See: docs/integrations/vanguard.md.
//!
//! ## Vault layout
//! - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
//!   [`crate::finance::Transaction`] per activity row; source="vanguard".
//! - **Raw:** `finance/vanguard/raw/<account>/YYYY.jsonl` — verbatim row,
//!   full fidelity, unconditional.
//!
//! ## Dedupe
//! Synthesized id: `hash(account, trade_date, net_amount, transaction_type,
//! symbol, occurrence)` — 18-month window overlaps are idempotent.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/vanguard.md.

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
// PARSER PARKED (Needs-sample): the Vanguard-specific column recognizer
// (`vanguard_columns`) and parser (`import_vanguard`) exist as a scaffold in
// `finance/import.rs` but are NOT wired into the live dispatch path until a
// real Transaction History CSV confirms the column shape.
//
// Until then, imports fall through to the generic `detect_mapping` path in
// `finance_import_csv`, which handles simple flat CSV files.  Files that the
// generic mapper can't understand will be rejected with a helpful message.
// The user must also supply an account name or ID (via the import box params)
// since there is no Vanguard-specific account routing in the generic path.
//
// Re-wiring checklist (when a real sample is in hand):
//   1. Confirm actual column names against `VanguardCols` in import.rs.
//   2. Confirm whether real exports are multi-section (holdings before
//      transactions); if so, update `import_vanguard` to scan for the
//      transaction header row rather than assuming it is at row 1.
//   3. Re-enable the dispatch block in `finance_import_csv` (see the
//      commented-out block there).
//   4. Remove the Needs-sample flag from the brief and this file.
//   5. Replace the generic `finance_import_csv` call below with a
//      Vanguard-specific pre-check and dispatch.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Parser is parked: call the generic CSV import path.  The generic mapper
    // will handle simple flat Vanguard CSVs if the column names are standard
    // enough; for multi-section exports or unusual layouts it will return an
    // error asking for a date/amount column name, which is the correct
    // Needs-sample behaviour (no silent data loss).
    let s = vault.finance_import_csv(path, None, None)?;
    outcome_from(s)
}

static IMPORT_SPEC: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
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
        id: "vanguard",
        name: "Vanguard",
        kind: IntegrationKind::Import,
        // Financial detail (retirement / brokerage) — ships opt-in.
        default_on: false,
        description: "Import your Vanguard transaction and portfolio history from CSV files \
                      downloaded from the Vanguard investor portal. Covers 401(k), IRA, \
                      brokerage, and mutual-fund accounts. Each download covers an 18-month \
                      window — drop multiple overlapping files to cover a longer period; \
                      re-imports dedupe cleanly.",
        domain: "finance",
        vault_path: "finance/vanguard/",
        toggleable: false,
        setup: &[
            "investor.vanguard.com → My Accounts → Transaction History → Download → CSV.",
            "Each download covers an 18-month window — repeat for each period you want to \
             import. Drop all files here; overlapping windows dedupe automatically.",
            "For holdings (positions) snapshots: Portfolio → Export (a separate CSV). \
             Drop it here too — raw positions are preserved in finance/vanguard/raw/.",
        ],
        caveats: "The portal export covers an 18-month window per download; full history \
                  requires two or more separate downloads. The CSV column format is not \
                  publicly documented — if your export is not recognized, file a Needs-sample \
                  report so the parser can be confirmed.",
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
            "{} new transactions into {} (vanguard), {} duplicates skipped",
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
            .join(format!("trove-vanguard-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // SCAFFOLD FIXTURES — column names UNCONFIRMED (Needs-sample).
    //
    // These fixtures encode the community-reported column shape. They are used
    // by scaffold tests that call `import_vanguard` directly (bypassing the
    // parked dispatch in `finance_import_csv`). When a real sample confirms or
    // corrects the column names, update these fixtures and the `vanguard_columns`
    // recognizer in finance/import.rs, then re-wire the dispatch.
    //
    // Brokerage shape (reported, unconfirmed):
    //   Trade Date, Settlement Date, Transaction Type, Investment Name, Symbol,
    //   Shares, Share Price, Principal Amount, Commission Fees, Net Amount,
    //   Accrued Interest
    //
    // Multi-account shape: prepends Account Number and Account Name.
    //
    // Sign convention (reported): Net Amount is negative for purchases (cash
    // outflow) and positive for dividends/sells.

    const SCAFFOLD_BROKERAGE_CSV: &str = "\
Trade Date,Settlement Date,Transaction Type,Investment Name,Symbol,Shares,Share Price,Principal Amount,Commission Fees,Net Amount,Accrued Interest
03/15/2026,03/17/2026,Buy,Vanguard Total Stock Market Index Fund ETF Shares,VTI,5,248.50,-1242.50,0.00,-1242.50,0.00
03/10/2026,,Dividend,Vanguard Total Stock Market Index Fund ETF Shares,VTI,,,0.00,0.00,18.75,0.00
03/01/2026,03/03/2026,Sell,Vanguard 500 Index Fund ETF Shares,VOO,2,520.00,1040.00,0.00,1040.00,0.00
03/05/2026,03/07/2026,Reinvestment,Vanguard Total Bond Market Index Fund,VBTLX,0.432,10.45,-4.51,0.00,-4.51,0.00
";

    const SCAFFOLD_MULTI_ACCOUNT_CSV: &str = "\
Account Number,Account Name,Trade Date,Settlement Date,Transaction Type,Investment Name,Symbol,Shares,Share Price,Principal Amount,Commission Fees,Net Amount,Accrued Interest
12345678901,Individual Brokerage,03/15/2026,03/17/2026,Buy,Vanguard Total Stock Market Index Fund ETF Shares,VTI,5,248.50,-1242.50,0.00,-1242.50,0.00
12345678901,Individual Brokerage,03/10/2026,,Dividend,Vanguard Total Stock Market Index Fund ETF Shares,VTI,,,0.00,0.00,18.75,0.00
98765432100,Roth IRA,03/01/2026,03/03/2026,Sell,Vanguard 500 Index Fund ETF Shares,VOO,2,520.00,1040.00,0.00,1040.00,0.00
";

    /// Call `import_vanguard` directly, bypassing the parked dispatch.
    /// Used only by scaffold tests; do NOT use `IMPORT_SPEC.run` here —
    /// that path goes through the generic mapper (parked behaviour).
    fn scaffold_import(v: &Vault, csv_body: &str) -> crate::finance::import::CsvImportStats {
        use crate::finance::import::vanguard_columns;
        let raw = csv_body.trim_start_matches('\u{feff}');
        let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(raw.as_bytes());
        let headers: Vec<String> = reader
            .headers()
            .unwrap()
            .iter()
            .map(String::from)
            .collect();
        // Re-open because the reader already consumed the header.
        let mut reader2 = csv::ReaderBuilder::new().flexible(true).from_reader(raw.as_bytes());
        let _ = reader2.headers().unwrap(); // consume header row
        let cols = vanguard_columns(&headers).expect("scaffold fixture must be recognized");
        v.import_vanguard(
            std::path::Path::new("scaffold.csv"),
            &headers,
            reader2,
            &cols,
            None,
            None,
        )
        .unwrap()
    }

    // -----------------------------------------------------------------------
    // Hub card tests — always valid (not affected by parked dispatch).

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "vanguard").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        assert_eq!(import_info.accepts, &["csv"]);
        assert!(!card.enabled, "default_on=false: opt-in");
    }

    // -----------------------------------------------------------------------
    // Column recognizer tests — test the scaffold recognizer directly.
    // These are independent of the parked dispatch.

    #[test]
    fn vanguard_columns_recognizes_per_account_header() {
        use crate::finance::import::vanguard_columns;
        let headers: Vec<String> = vec![
            "Trade Date", "Settlement Date", "Transaction Type", "Investment Name",
            "Symbol", "Shares", "Share Price", "Principal Amount", "Commission Fees",
            "Net Amount", "Accrued Interest",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let cols = vanguard_columns(&headers).expect("must recognize per-account header");
        // Mandatory indices.
        assert_eq!(cols.trade_date, 0, "Trade Date is first column");
        assert_eq!(cols.transaction_type, 2, "Transaction Type is third column");
        assert!(cols.net_amount < headers.len(), "Net Amount found");
        // Optional indices.
        assert!(cols.account_number.is_none(), "no account column in per-account export");
        assert!(cols.symbol.is_some());
        assert!(cols.shares.is_some());
    }

    #[test]
    fn vanguard_columns_recognizes_multi_account_header() {
        use crate::finance::import::vanguard_columns;
        let headers: Vec<String> = vec![
            "Account Number", "Account Name", "Trade Date", "Settlement Date",
            "Transaction Type", "Investment Name", "Symbol", "Shares", "Share Price",
            "Principal Amount", "Commission Fees", "Net Amount", "Accrued Interest",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let cols = vanguard_columns(&headers).expect("must recognize multi-account header");
        assert!(cols.account_number.is_some(), "account number present");
        assert!(cols.account_name.is_some(), "account name present");
    }

    #[test]
    fn vanguard_columns_recognizes_transaction_description() {
        use crate::finance::import::vanguard_columns;
        // Scaffold with optional "Transaction Description" column (reported but unconfirmed).
        let headers: Vec<String> = vec![
            "Trade Date", "Transaction Type", "Transaction Description",
            "Investment Name", "Symbol", "Net Amount",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let cols = vanguard_columns(&headers).expect("must recognize with Transaction Description");
        assert!(cols.transaction_description.is_some(), "Transaction Description mapped");
        assert_eq!(cols.transaction_description.unwrap(), 2, "at position 2");
    }

    #[test]
    fn vanguard_columns_mf_variant_aliases() {
        use crate::finance::import::vanguard_columns;
        // MF variant: "Process Date" instead of "Settlement Date",
        // "Gross Amount" instead of "Principal Amount".
        let headers: Vec<String> = vec![
            "Trade Date", "Process Date", "Transaction Type", "Investment Name",
            "Gross Amount", "Net Amount",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let cols = vanguard_columns(&headers).expect("must recognize MF variant");
        assert!(cols.settlement_date.is_some(), "Process Date -> settlement_date");
        assert!(cols.principal_amount.is_some(), "Gross Amount -> principal_amount");
    }

    #[test]
    fn vanguard_columns_rejects_non_vanguard_header() {
        use crate::finance::import::vanguard_columns;
        // Fidelity header — must not be confused with Vanguard.
        let fidelity: Vec<String> = vec![
            "Run Date", "Action", "Symbol", "Description", "Type", "Quantity",
            "Price ($)", "Commission ($)", "Fees ($)", "Accrued Interest ($)", "Amount ($)",
            "Cash Balance ($)", "Settlement Date",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert!(vanguard_columns(&fidelity).is_none(), "Fidelity header must not match");

        // Generic bank statement — must not match either.
        let generic: Vec<String> = vec!["Date", "Description", "Amount"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(vanguard_columns(&generic).is_none(), "generic header must not match");
    }

    // -----------------------------------------------------------------------
    // Scaffold parser tests — call import_vanguard directly.
    // These verify the parked parser logic is internally consistent.
    // They will be promoted to integration tests when the parser is confirmed
    // and the dispatch is re-wired.

    #[test]
    fn scaffold_per_account_import_all_rows_including_dividend() {
        let v = temp_vault("scaffold-per-account");
        let stats = scaffold_import(&v, SCAFFOLD_BROKERAGE_CSV);
        assert_eq!(stats.rows, 4, "4 data rows");
        assert_eq!(stats.new_transactions, 4, "all 4 imported");
        assert_eq!(stats.duplicates, 0);
        assert_eq!(stats.skipped, 0, "dividend row not skipped");

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 1, "one account for a per-account file");

        let acct_id = accounts[0].id.clone();
        let rows = v.finance_transactions(Some(&acct_id), 100).unwrap();
        assert_eq!(rows.len(), 4, "4 transactions on disk");

        // Buy row: posted = Trade Date; negative amount.
        let buy = rows.iter().find(|t| t.description.contains("Buy VTI")).unwrap();
        assert_eq!(buy.posted, "2026-03-15", "posted from Trade Date");
        assert_eq!(buy.amount, "-1242.50", "buys are negative");
        assert_eq!(buy.source, "vanguard");
        assert_eq!(
            buy.extra.get("transaction_type").and_then(|v| v.as_str()),
            Some("Buy")
        );
        assert_eq!(buy.extra.get("symbol").and_then(|v| v.as_str()), Some("VTI"));
        assert_eq!(buy.extra.get("shares").and_then(|v| v.as_str()), Some("5"));
        assert_eq!(buy.extra.get("settlement_date").and_then(|v| v.as_str()), Some("2026-03-17"));
        assert!(buy.extra.get("account_number").is_none(), "no account_number for per-account");

        // Dividend row: blank settlement date must not skip.
        let div = rows.iter().find(|t| t.description.contains("Dividend VTI")).unwrap();
        assert_eq!(div.posted, "2026-03-10");
        assert_eq!(div.amount, "18.75", "dividends are positive");
        assert!(div.extra.get("settlement_date").is_none(), "blank settlement not stored");

        // Sell row: positive amount.
        let sell = rows.iter().find(|t| t.description.contains("Sell VOO")).unwrap();
        assert_eq!(sell.amount, "1040.00", "sells are positive");

        // Reinvestment row.
        let reinv = rows.iter().find(|t| t.description.contains("Reinvestment VBTLX")).unwrap();
        assert_eq!(reinv.amount, "-4.51");
    }

    #[test]
    fn scaffold_multi_account_export_routes_by_account_number() {
        let v = temp_vault("scaffold-multi");
        let stats = scaffold_import(&v, SCAFFOLD_MULTI_ACCOUNT_CSV);
        assert_eq!(stats.rows, 3);
        assert_eq!(stats.new_transactions, 3);

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 2, "two Vanguard account numbers → two vault accounts");
        assert!(accounts.iter().any(|a| a.aliases.get("vanguard") == Some(&"12345678901".to_string())));
        assert!(accounts.iter().any(|a| a.aliases.get("vanguard") == Some(&"98765432100".to_string())));

        let brokerage = accounts
            .iter()
            .find(|a| a.aliases.get("vanguard") == Some(&"12345678901".to_string()))
            .unwrap();
        let b_rows = v.finance_transactions(Some(&brokerage.id), 100).unwrap();
        assert_eq!(b_rows.len(), 2, "2 transactions in brokerage account");
        let buy = b_rows.iter().find(|t| t.description.contains("Buy")).unwrap();
        assert_eq!(
            buy.extra.get("account_number").and_then(|v| v.as_str()),
            Some("12345678901"),
            "account number in extra for multi-account exports"
        );
    }

    #[test]
    fn scaffold_reimport_is_idempotent() {
        let v = temp_vault("scaffold-reimport");
        let first = scaffold_import(&v, SCAFFOLD_BROKERAGE_CSV);
        assert_eq!(first.new_transactions, 4);

        let second = scaffold_import(&v, SCAFFOLD_BROKERAGE_CSV);
        assert_eq!(second.new_transactions, 0, "re-import adds nothing");
        assert_eq!(second.duplicates, 4, "all are duplicates");

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(v.finance_transactions(Some(&accounts[0].id), 100).unwrap().len(), 4);
    }

    #[test]
    fn scaffold_raw_layer_is_verbatim() {
        // Raw layer must be the verbatim CSV columns — not the parsed projection.
        // Specifically: original column names (e.g. "Trade Date", not "posted"),
        // original string values (e.g. "03/15/2026", not "2026-03-15"), and
        // ALL columns including any not known to the mapper.
        let v = temp_vault("scaffold-raw");
        scaffold_import(&v, SCAFFOLD_BROKERAGE_CSV);

        let accounts = v.load_finance_accounts().unwrap();
        let acct_id = &accounts[0].id;
        let raw_2026 = v.root()
            .join("finance/vanguard/raw")
            .join(acct_id)
            .join("2026.jsonl");
        assert!(raw_2026.exists(), "raw file written for 2026");
        let content = fs::read_to_string(&raw_2026).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 4, "all 4 rows in raw");

        // Verbatim: original column names, original date string.
        let row0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert!(row0.get("Trade Date").is_some(), "original 'Trade Date' key in raw");
        assert!(row0.get("Net Amount").is_some(), "original 'Net Amount' key in raw");
        // Original date string (not normalized).
        assert!(
            row0.get("Trade Date").and_then(|v| v.as_str()).map(|s| s.contains("/")).unwrap_or(false),
            "Trade Date is original MM/DD/YYYY string in raw"
        );
        // Not a parsed/synthetic key.
        assert!(row0.get("posted").is_none(), "no synthetic 'posted' key in raw");
        assert!(row0.get("amount").is_none(), "no synthetic 'amount' key in raw");
        assert!(row0.get("description").is_none(), "no synthetic 'description' key in raw");

        // Symbol present in raw for the right rows.
        assert!(lines.iter().any(|l| l.contains("\"VTI\"")), "symbol in raw");
        assert!(lines.iter().any(|l| l.contains("\"Dividend\"")), "dividend in raw");
    }

    #[test]
    fn scaffold_transaction_description_used_when_present() {
        // When a "Transaction Description" column is present, its value should
        // be used as the contract description rather than synthesized "<type> <symbol>".
        let csv = "\
Trade Date,Transaction Type,Transaction Description,Investment Name,Symbol,Net Amount
03/15/2026,Buy,Purchase of VTI ETF,Vanguard Total Stock Market,VTI,-1242.50
";
        let v = temp_vault("scaffold-txdesc");
        let stats = scaffold_import(&v, csv);
        assert_eq!(stats.new_transactions, 1);
        let accounts = v.load_finance_accounts().unwrap();
        let rows = v.finance_transactions(Some(&accounts[0].id), 100).unwrap();
        let tx = &rows[0];
        assert_eq!(
            tx.description, "Purchase of VTI ETF",
            "portal Transaction Description used as contract description"
        );
    }

    #[test]
    fn scaffold_summary_footer_rows_are_skipped_gracefully() {
        // Vanguard may append a text block after the data rows; rows with no
        // Trade Date must be skipped without panicking.
        let csv_with_footer = format!(
            "{SCAFFOLD_BROKERAGE_CSV}\
\"Total\",,,,,,,,,,\n\
\"\",,,,,,,,,,\n"
        );
        let v = temp_vault("scaffold-footer");
        let stats = scaffold_import(&v, &csv_with_footer);
        assert_eq!(stats.new_transactions, 4, "4 data rows only");
        assert!(stats.skipped >= 2, "footer rows skipped");
    }
}

//! Apple Card, Cash & Savings — monthly statement export from Wallet/card.apple.com.
//!
//! ## Export path
//!
//! **Apple Card / Apple Savings:**
//! iPhone: Wallet → Apple Card → Card Balance → Statements → Export Transactions
//! → CSV/OFX/QFX/QBO (one file per month).
//! Web: card.apple.com → Statements → Export Transactions.
//! Apple Savings uses the same Wallet interface.
//!
//! **Apple Cash:** PDF statement only (Wallet → Apple Cash → Request Statement,
//! last 12 months to Apple ID email) — not parsed in v1.
//!
//! No API. No OAuth. No network calls at import time. Standalone-clean.
//!
//! ## Column format (SCAFFOLD — Needs-sample)
//!
//! The column list is from `integrations-research.md` L3841 (research doc),
//! not a verified real export:
//!   Transaction Date, Clearing Date, Description, Merchant, Category, Type,
//!   Amount (USD)
//!
//! The recognizer in [`crate::finance::import::apple_card_columns`] is built
//! from that documented list but **has not been confirmed against a real Wallet
//! or card.apple.com CSV export**. Until confirmation arrives, imports fall
//! through to the generic `detect_mapping` path in `finance_import_csv`. See
//! docs/integrations/apple-card.md.
//!
//! ## Vault layout
//! - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
//!   [`crate::finance::Transaction`] per statement row; source="apple-card".
//!   `posted` = Clearing Date; `transacted` = Transaction Date.
//! - **Raw:** `finance/apple-card/raw/<account>/YYYY.jsonl` — verbatim row,
//!   full fidelity, unconditional.
//!
//! ## Dedupe
//! Synthesized id: `hash(account, clearing_date, amount, desc_norm, occurrence)`
//! — overlapping monthly re-imports are idempotent. Cross-source dedup against
//! Copilot-seeded history uses the standard fuzzy matcher.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/apple-card.md

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
// PARSER PARKED (Needs-sample): the Apple Card-specific column recognizer
// (`apple_card_columns`) and parser (`import_apple_card`) exist as a scaffold
// in `finance/import.rs` but are NOT wired into the live dispatch path until a
// real Wallet/card.apple.com CSV confirms the column shape and sign convention.
//
// Until then, imports fall through to the generic `detect_mapping` path in
// `finance_import_csv`. For an Apple Card export the generic path WILL pick up
// `Amount (USD)` and `Transaction Date` (or `Description`/`Merchant`) and
// produce output — but the sign convention may be wrong (purchases as positive
// instead of negative) and `Clearing Date` will not be used as the primary date.
// The user must also supply an account name or ID via the import box params.
//
// Re-wiring checklist (when a real export is in hand):
//   1. Confirm exact column headers (capitalization, spacing in "Clearing Date").
//   2. Confirm sign convention: are purchases positive or negative in Amount (USD)?
//      Adjust the sign flip in `import_apple_card` accordingly.
//   3. Confirm whether "Description" and "Merchant" are both present, or only one.
//   4. Re-enable the dispatch block in `finance_import_csv` (see commented block).
//   5. Remove the Needs-sample flag from the brief and this file.

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Parser is parked: call the generic CSV import path. The generic mapper
    // will pick up `Amount (USD)` and `Transaction Date` (or Description/Merchant)
    // and produce transactions, but `Clearing Date` won't be used as the
    // posted date and signs may need verification. This is the correct
    // Needs-sample behaviour — no silent data loss while the specific preset
    // awaits confirmation.
    //
    // NOTE: an account id or account name MUST be supplied via params, or the
    // generic path will error ("pick an existing account or name a new one").
    // The `new_account` param marked required:false is intentionally lenient so
    // the user can choose either route, but at least one must be non-empty.
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
            placeholder: "e.g. Apple Card",
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
        id: "apple-card",
        name: "Apple Card, Cash & Savings",
        kind: IntegrationKind::Import,
        // Financial detail — opt-in; users must explicitly enable.
        default_on: false,
        description: "Import Apple Card and Apple Savings transactions from the monthly \
                      statement CSV exported from your iPhone's Wallet app or \
                      card.apple.com. Covers the complete spend history for accounts \
                      no aggregator can reach — FinanceKit (live sync) is iPhone-only \
                      and Apple-entitlement-gated, so this monthly export is the only \
                      desktop route.",
        domain: "finance",
        vault_path: "finance/apple-card/",
        toggleable: false,
        setup: &[
            "iPhone: Wallet → Apple Card → Card Balance → Statements → pick a month → \
             Export Transactions → CSV. Or: card.apple.com → Statements → Export \
             Transactions.",
            "Apple Savings uses the same Wallet interface — pick the Savings card and \
             follow the same steps.",
            "Import one file per month. Overlapping re-imports dedupe cleanly — drop \
             the same file twice and no duplicates are added.",
            "Apple Cash statements are PDF-only (Wallet → Apple Cash → Request Statement \
             → emailed to your Apple ID). PDF extraction is not yet supported.",
        ],
        caveats: "Apple Card, Apple Cash, and Apple Savings cannot be reached by any \
                  bank aggregator — FinanceKit (the live-sync API used by Copilot, \
                  Monarch, and YNAB on iPhone) is iOS-only and requires an Apple \
                  entitlement not available to macOS desktop apps. Monthly CSV export \
                  from Wallet or card.apple.com is the only desktop route. Apple Cash \
                  is PDF-only; PDF extraction is a later capability. The CSV preset is \
                  currently a scaffold pending a real export sample — if your file is \
                  not recognized perfectly, the generic column reader will still handle \
                  it (supply an account name in the import box).",
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
            "{} new transactions into {} (apple-card), {} duplicates skipped",
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
            .join(format!("trove-apple-card-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // SCAFFOLD FIXTURES — column names UNCONFIRMED (Needs-sample).
    //
    // These fixtures encode the research-doc column shape (L3841 of
    // integrations-research.md). They are used by scaffold tests that call
    // `import_apple_card` directly (bypassing the parked dispatch in
    // `finance_import_csv`). When a real Wallet/card.apple.com CSV confirms or
    // corrects the column names, update these fixtures and `apple_card_columns`
    // in finance/import.rs, then re-wire the dispatch.
    //
    // Documented column set (research doc, unconfirmed):
    //   Transaction Date, Clearing Date, Description, Merchant, Category, Type,
    //   Amount (USD)
    //
    // Sign convention assumed: purchases positive in source → flipped to negative.
    // Payment/credit rows are negative in the source → flipped to positive.
    // Verify against a real export.

    const SCAFFOLD_CSV: &str = "\
Transaction Date,Clearing Date,Description,Merchant,Category,Type,Amount (USD)
06/08/2026,06/10/2026,AMAZON.COM,Amazon,Shopping,Purchase,42.17
06/09/2026,06/09/2026,STARBUCKS #1234 SEATTLE WA,Starbucks,Food & Drink,Purchase,5.80
06/01/2026,06/03/2026,PAYMENT THANK YOU,Apple Card,Payments,Payment,-500.00
06/07/2026,06/08/2026,NETFLIX.COM,Netflix,Entertainment,Purchase,15.99
";

    /// Call `import_apple_card` directly, bypassing the parked dispatch.
    /// Used only by scaffold tests; do NOT use `IMPORT_SPEC.run` here.
    fn scaffold_import(v: &Vault, csv_body: &str) -> CsvImportStats {
        use crate::finance::import::apple_card_columns;
        let raw = csv_body.trim_start_matches('\u{feff}');
        let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(raw.as_bytes());
        let headers: Vec<String> = reader.headers().unwrap().iter().map(String::from).collect();
        // Re-open for the actual import (first reader already consumed the header).
        let mut reader2 = csv::ReaderBuilder::new().flexible(true).from_reader(raw.as_bytes());
        let _ = reader2.headers().unwrap();
        let cols = apple_card_columns(&headers).expect("scaffold fixture must be recognized");
        v.import_apple_card(
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
        let card = status.iter().find(|s| s.id == "apple-card").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        assert_eq!(import_info.accepts, &["csv"]);
        assert!(!card.enabled, "default_on=false: opt-in");
    }

    // -----------------------------------------------------------------------
    // Column recognizer tests — test the scaffold recognizer directly.

    #[test]
    fn apple_card_columns_recognizes_documented_header() {
        use crate::finance::import::apple_card_columns;
        let headers: Vec<String> = vec![
            "Transaction Date", "Clearing Date", "Description",
            "Merchant", "Category", "Type", "Amount (USD)",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let cols = apple_card_columns(&headers).expect("must recognize documented header");
        assert_eq!(cols.clearing_date, 1, "Clearing Date is second column");
        assert_eq!(cols.transaction_date, 0, "Transaction Date is first column");
        assert_eq!(cols.amount, 6, "Amount (USD) is seventh column");
        assert_eq!(cols.description, 2, "Description at position 2");
        assert!(cols.merchant.is_some(), "Merchant present");
        assert!(cols.category.is_some(), "Category present");
        assert!(cols.kind.is_some(), "Type present");
    }

    #[test]
    fn apple_card_columns_without_description_falls_back_to_merchant() {
        use crate::finance::import::apple_card_columns;
        // If Description is absent, Merchant is used as the description column.
        let headers: Vec<String> = vec![
            "Transaction Date", "Clearing Date", "Merchant",
            "Category", "Type", "Amount (USD)",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let cols = apple_card_columns(&headers).expect("must recognize without Description");
        // description falls back to Merchant's index.
        assert_eq!(cols.description, 2, "falls back to Merchant column");
        // Merchant should NOT also appear as a separate extra field (same col).
        assert!(cols.merchant.is_none(), "Merchant not duplicated in extra when used as description");
    }

    #[test]
    fn apple_card_columns_rejects_non_apple_header() {
        use crate::finance::import::apple_card_columns;
        // Chase card header — must not match.
        let chase: Vec<String> = vec![
            "Transaction Date", "Post Date", "Description", "Category",
            "Type", "Amount", "Memo",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert!(
            apple_card_columns(&chase).is_none(),
            "Chase header must not match (no Clearing Date + no Amount (USD))"
        );

        // Generic bank — must not match.
        let generic: Vec<String> = vec!["Date", "Description", "Amount"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(apple_card_columns(&generic).is_none(), "generic header must not match");
    }

    // -----------------------------------------------------------------------
    // Scaffold parser tests — call import_apple_card directly.
    // These verify the parked parser is internally consistent.

    #[test]
    fn scaffold_import_parses_all_rows_with_sign_flip() {
        let v = temp_vault("scaffold-basic");
        let stats = scaffold_import(&v, SCAFFOLD_CSV);
        assert_eq!(stats.rows, 4, "4 data rows");
        assert_eq!(stats.new_transactions, 4, "all 4 imported");
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.format, "apple-card");

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 1, "one account created");

        let acct_id = &accounts[0].id;
        let rows = v.finance_transactions(Some(acct_id), 100).unwrap();
        assert_eq!(rows.len(), 4);

        // Purchase row: positive in CSV → flipped to negative.
        let amazon = rows.iter().find(|t| t.description.contains("AMAZON")).unwrap();
        assert_eq!(amazon.amount, "-42.17", "purchase flipped to negative");
        assert_eq!(amazon.posted, "2026-06-10", "Clearing Date used as posted");
        assert_eq!(amazon.transacted.as_deref(), Some("2026-06-08"), "Transaction Date in transacted");
        assert_eq!(amazon.source, "apple-card");
        assert_eq!(
            amazon.extra.get("merchant").and_then(|v| v.as_str()),
            Some("Amazon")
        );
        assert_eq!(
            amazon.extra.get("category").and_then(|v| v.as_str()),
            Some("Shopping")
        );
        assert_eq!(
            amazon.extra.get("type").and_then(|v| v.as_str()),
            Some("Purchase")
        );

        // Payment row: negative in CSV (credit) → flipped to positive.
        let payment = rows.iter().find(|t| t.description.contains("PAYMENT")).unwrap();
        assert_eq!(payment.amount, "500.00", "payment (credit) flipped to positive");

        // Starbucks: same-day transaction and clearing.
        let starbucks = rows.iter().find(|t| t.description.contains("STARBUCKS")).unwrap();
        assert_eq!(starbucks.posted, "2026-06-09");
        assert_eq!(starbucks.transacted.as_deref(), Some("2026-06-09"));
    }

    #[test]
    fn scaffold_reimport_is_idempotent() {
        let v = temp_vault("scaffold-reimport");
        let first = scaffold_import(&v, SCAFFOLD_CSV);
        assert_eq!(first.new_transactions, 4);

        let second = scaffold_import(&v, SCAFFOLD_CSV);
        assert_eq!(second.new_transactions, 0, "re-import adds nothing");
        assert_eq!(second.duplicates, 4, "all are duplicates");

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(v.finance_transactions(Some(&accounts[0].id), 100).unwrap().len(), 4);
    }

    #[test]
    fn scaffold_raw_layer_is_verbatim() {
        let v = temp_vault("scaffold-raw");
        scaffold_import(&v, SCAFFOLD_CSV);

        let accounts = v.load_finance_accounts().unwrap();
        let acct_id = &accounts[0].id;
        let raw_2026 = v.root()
            .join("finance/apple-card/raw")
            .join(acct_id)
            .join("2026.jsonl");
        assert!(raw_2026.exists(), "raw file written for 2026");
        let content = fs::read_to_string(&raw_2026).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 4, "all 4 rows in raw");

        // Verbatim: original column names and original string values.
        let row0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert!(row0.get("Transaction Date").is_some(), "original 'Transaction Date' key in raw");
        assert!(row0.get("Clearing Date").is_some(), "original 'Clearing Date' key in raw");
        assert!(row0.get("Amount (USD)").is_some(), "original 'Amount (USD)' key in raw");
        // Date still in original MM/DD/YYYY form (not normalized).
        assert!(
            row0.get("Clearing Date")
                .and_then(|v| v.as_str())
                .map(|s| s.contains('/'))
                .unwrap_or(false),
            "Clearing Date is original MM/DD/YYYY string in raw"
        );
        // No synthetic/normalized keys.
        assert!(row0.get("posted").is_none(), "no synthetic 'posted' key in raw");
        assert!(row0.get("amount").is_none(), "no synthetic 'amount' key in raw");
    }

    #[test]
    fn scaffold_same_day_identical_rows_stay_distinct() {
        // Two identical purchases on the same day must both be imported
        // (occurrence counter disambiguates them).
        let csv = "\
Transaction Date,Clearing Date,Description,Merchant,Category,Type,Amount (USD)
06/10/2026,06/10/2026,STARBUCKS,Starbucks,Food & Drink,Purchase,5.80
06/10/2026,06/10/2026,STARBUCKS,Starbucks,Food & Drink,Purchase,5.80
";
        let v = temp_vault("scaffold-occurrence");
        let stats = scaffold_import(&v, csv);
        assert_eq!(stats.new_transactions, 2, "occurrence counter keeps both");
        let again = scaffold_import(&v, csv);
        assert_eq!(again.new_transactions, 0, "re-import is idempotent");
        assert_eq!(again.duplicates, 2);
    }

    #[test]
    fn scaffold_bad_rows_are_skipped_gracefully() {
        // Rows with no clearing date or no amount must be skipped without panicking.
        let csv = "\
Transaction Date,Clearing Date,Description,Merchant,Category,Type,Amount (USD)
06/08/2026,06/10/2026,AMAZON,Amazon,Shopping,Purchase,42.17
,,MISSING DATE,,,Purchase,9.99
06/09/2026,06/09/2026,NO AMOUNT,Starbucks,Food & Drink,Purchase,
";
        let v = temp_vault("scaffold-skip");
        let stats = scaffold_import(&v, csv);
        assert_eq!(stats.rows, 3);
        assert_eq!(stats.new_transactions, 1, "only valid row imported");
        assert_eq!(stats.skipped, 2, "two bad rows skipped");
    }
}

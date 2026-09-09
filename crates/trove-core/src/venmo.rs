//! Venmo peer-payment history import — CSV from the Venmo privacy data download.
//!
//! ## Export path
//!
//! account.venmo.com → Privacy tab → Request Your Data → Transaction History
//! → CSV (or JSON). Also available as a direct download URL while logged in:
//! `https://account.venmo.com/api/statement/download?startDate=YYYY-MM-DD&endDate=YYYY-MM-DD&csv=true`.
//!
//! No API exists for individuals. No OAuth. No network calls at import time.
//! Standalone-clean by construction.
//!
//! ## Column format (SCAFFOLD — Needs-sample)
//!
//! The column layout is from community-reported exports and secondary sources;
//! **it has not been confirmed against a real account.venmo.com export**:
//!
//! ```text
//! ID, Datetime, Type, Status, Note, From, To,
//! Amount (total), Amount (tip), Amount (tax), Amount (fee),
//! Tax Rate, Tax Exempt Status, Funding Source, Destination,
//! Beginning Balance, Ending Balance, Statement Period Venmo Fees,
//! Terminal Location, Year to Date Venmo Fees, Disclaimer
//! ```
//!
//! Amount encoding (unconfirmed): `"+ $50.00"` (inflow) / `"- $25.00"` (outflow).
//! Date encoding (unconfirmed): ISO-8601 with time component, e.g. `"2024-06-15T14:35:12"`.
//!
//! The parser lives in `finance/import.rs` (same family as Cash App / PayPal);
//! this module provides the hub card DEF and the import box.
//!
//! The specific Venmo recognizer (`venmo_columns`) and `import_venmo` are
//! parked as a scaffold in `finance/import.rs`. Until a real export confirms the
//! column shape, imports fall through to the generic `detect_mapping` path in
//! `finance_import_csv`. The generic path will handle the Note/From/To fields
//! as the description column (if recognized) and the `Amount (total)` column as
//! the amount — the Note text and sign convention must be verified before the
//! dedicated parser is wired.
//!
//! ## Vault layout
//! - **Contract:** `finance/transactions/<account>/YYYY.jsonl` — one
//!   [`crate::finance::Transaction`] per row; source="venmo".
//!   `id` = `"venmo-<ID>"` — the stable Venmo transaction ID.
//!   Payment `Note`, `From`, `To`, `Type`, `Status`, and `Funding Source`
//!   go in `extra`.
//! - **Raw:** `finance/venmo/raw/<account>/YYYY.jsonl` — verbatim row,
//!   all columns, full fidelity, unconditional.
//!
//! ## Dedupe
//! `id` = `"venmo-<ID>"` (stable Venmo transaction ID). Re-importing the same
//! file is a clean no-op. Bank-side settlement rows (Venmo cash-outs to a linked
//! bank) coexist in the bank account; reconciliation is a read-time concern.
//!
//! ## Privacy note
//! Payment `Note` fields are message-like social content. The integration is
//! opt-in only (`default_on: false`).
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/venmo.md

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
// PARSER PARKED (Needs-sample): the Venmo-specific column recognizer
// (`venmo_columns`) and parser (`import_venmo`) exist as a scaffold in
// `finance/import.rs` but are NOT wired into the live dispatch path until a
// real account.venmo.com CSV confirms the column shape and sign convention.
//
// Until then, imports fall through to the generic `detect_mapping` path in
// `finance_import_csv`. For a Venmo export the generic path WILL attempt to
// pick up a date column and an amount column — but:
//   a. The "Amount (total)" column's "+ $50.00" / "- $25.00" encoding may
//      not parse correctly through the generic clean_amount path.
//   b. The Note/From/To columns may not be recognized as description.
//   c. The sign convention (inflow vs outflow direction) is unverified.
// The user must also supply an account name via the import box params.
//
// Re-wiring checklist (when a real export is in hand):
//   1. Confirm exact column headers (capitalization, spacing).
//   2. Confirm amount encoding: exactly "+ $50.00" / "- $25.00"?  Or bare
//      "50.00" / "-25.00"? Adjust import_venmo sign handling accordingly.
//   3. Confirm date format: ISO-8601 datetime? Does parse_date strip it?
//   4. Confirm which column to use as the vault `description` (Note, From/To, Type).
//   5. Re-enable the dispatch block in `finance_import_csv` (see commented block).
//   6. Remove the Needs-sample flag from the brief and this file.

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Parser is parked: call the generic CSV import path. The generic mapper
    // will attempt to parse what it can, but amount encoding and description
    // mapping may not be perfect until the specific preset is wired.
    //
    // NOTE: an account id or account name MUST be supplied via params, or the
    // generic path will error ("pick an existing account or name a new one").
    let get =
        |k: &str| params.get(k).map(String::as_str).map(str::trim).filter(|v| !v.is_empty());
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
            placeholder: "e.g. Venmo",
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
        id: "venmo",
        name: "Venmo",
        kind: IntegrationKind::Import,
        // Financial detail + payment notes (social/private) — opt-in only.
        default_on: false,
        description: "Import your full Venmo transaction history — peer payments, charges, \
                      merchant payments, bank transfers, and the payment notes attached to \
                      each — from the official privacy data export. Venmo has no public API \
                      for individuals, so the export is the only complete source. Payment \
                      notes can be socially rich and are treated as private.",
        domain: "finance",
        vault_path: "finance/venmo/",
        toggleable: false,
        setup: &[
            "Log in at account.venmo.com and go to Settings → Privacy → Request Your Data.",
            "Select \"Transaction History\" and choose CSV format (or the direct URL: \
             account.venmo.com/api/statement/download?csv=true while logged in).",
            "Download the CSV. If your history is long, Venmo may split it into date ranges \
             — import each file; overlapping re-imports dedupe cleanly.",
            "Drag the CSV file here. Supply a Venmo account name in the import box.",
        ],
        caveats: "Venmo has no public API for individuals and cannot be reached by any bank \
                  aggregator (SimpleFIN, Plaid, etc.) — the privacy export is the only \
                  complete source. Bank-side settlement rows (Venmo cash-outs to a linked \
                  bank) may also appear in your bank account rows; reconciliation is handled \
                  at read time. Payment notes are included and treated as private content. \
                  The specific Venmo column preset is currently a scaffold pending a real \
                  export sample — supply an account name in the import box.",
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
            "{} new transactions into {} (venmo), {} duplicates skipped",
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
            .join(format!("trove-venmo-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // SCAFFOLD FIXTURES — column names UNCONFIRMED (Needs-sample).
    //
    // These fixtures encode the community-reported Venmo CSV shape.
    // They are used by scaffold tests that call `import_venmo` directly
    // (bypassing the parked dispatch in `finance_import_csv`). When a real
    // account.venmo.com CSV confirms or corrects the column names, update
    // these fixtures and `venmo_columns` in finance/import.rs, then
    // re-wire the dispatch.
    //
    // Column set (community-reported, unconfirmed):
    //   ID, Datetime, Type, Status, Note, From, To,
    //   Amount (total), Amount (tip), Amount (tax), Amount (fee),
    //   Tax Rate, Tax Exempt Status, Funding Source, Destination,
    //   Beginning Balance, Ending Balance, Statement Period Venmo Fees,
    //   Terminal Location, Year to Date Venmo Fees, Disclaimer
    //
    // Amount encoding assumed: "+ $50.00" inflow, "- $25.00" outflow.
    // Date assumed: ISO-8601 datetime "YYYY-MM-DDTHH:MM:SS" — parse_date strips time.

    const SCAFFOLD_CSV: &str = "\
ID,Datetime,Type,Status,Note,From,To,Amount (total),Amount (tip),Amount (tax),Amount (fee),Tax Rate,Tax Exempt Status,Funding Source,Destination,Beginning Balance,Ending Balance,Statement Period Venmo Fees,Terminal Location,Year to Date Venmo Fees,Disclaimer
3141592653589793,2026-06-10T14:35:12,Payment,Complete,Dinner split,Alice Smith,You,+ $50.00,,,, , ,Venmo balance,Venmo balance,$150.00,$200.00, , ,$0.00,
2718281828459045,2026-06-09T09:20:00,Payment,Complete,Coffee,You,Bob Jones,- $5.80,,,, , ,Venmo balance,Venmo balance,$155.80,$150.00, , ,$0.00,
1414213562373095,2026-06-08T18:00:00,Bank Transfer,Settled,,You,Chase Bank,- $100.00,,,, , ,Venmo balance,Bank,$255.80,$155.80, , ,$0.00,
";

    /// Call `import_venmo` directly, bypassing the parked dispatch.
    /// Used only by scaffold tests; do NOT use `IMPORT_SPEC.run` here.
    fn scaffold_import(v: &Vault, csv_body: &str) -> CsvImportStats {
        use crate::finance::import::venmo_columns;
        let raw = csv_body.trim_start_matches('\u{feff}');
        let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(raw.as_bytes());
        let headers: Vec<String> = reader.headers().unwrap().iter().map(String::from).collect();
        let mut reader2 = csv::ReaderBuilder::new().flexible(true).from_reader(raw.as_bytes());
        let _ = reader2.headers().unwrap();
        let cols = venmo_columns(&headers).expect("scaffold fixture must be recognized");
        v.import_venmo(
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
        let card = status.iter().find(|s| s.id == "venmo").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        assert_eq!(import_info.accepts, &["csv"]);
        assert!(!card.enabled, "default_on=false: opt-in");
    }

    #[test]
    fn hub_card_params_include_account_fields() {
        let v = temp_vault("params");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "venmo").unwrap();
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
    // Column recognizer tests.

    #[test]
    fn venmo_columns_recognizes_scaffold_header() {
        use crate::finance::import::venmo_columns;
        let headers: Vec<String> = vec![
            "ID", "Datetime", "Type", "Status", "Note", "From", "To",
            "Amount (total)", "Amount (tip)", "Amount (tax)", "Amount (fee)",
            "Tax Rate", "Tax Exempt Status", "Funding Source", "Destination",
            "Beginning Balance", "Ending Balance", "Statement Period Venmo Fees",
            "Terminal Location", "Year to Date Venmo Fees", "Disclaimer",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let cols = venmo_columns(&headers).expect("must recognize scaffold header");
        assert_eq!(cols.id, 0, "ID at position 0");
        assert_eq!(cols.datetime, 1, "Datetime at position 1");
        assert_eq!(cols.note, 4, "Note at position 4");
        assert_eq!(cols.from_user, 5, "From at position 5");
        assert_eq!(cols.to_user, 6, "To at position 6");
        assert_eq!(cols.amount_total, 7, "Amount (total) at position 7");
        assert!(cols.funding_source.is_some(), "Funding Source present");
        assert!(cols.beginning_balance.is_some(), "Beginning Balance present");
        assert!(cols.ending_balance.is_some(), "Ending Balance present");
    }

    #[test]
    fn venmo_columns_rejects_non_venmo_header() {
        use crate::finance::import::venmo_columns;
        // Chase card header — must not match (no "Datetime", "Note", "Amount (total)").
        let chase: Vec<String> = vec![
            "Transaction Date", "Post Date", "Description", "Category",
            "Type", "Amount", "Memo",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert!(venmo_columns(&chase).is_none(), "Chase header must not match");

        // Cash App header — must not match.
        let cash_app: Vec<String> = vec![
            "Transaction ID", "Date", "Transaction Type", "Currency", "Amount",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert!(venmo_columns(&cash_app).is_none(), "Cash App header must not match");

        // Generic bank — must not match.
        let generic: Vec<String> = vec!["Date", "Description", "Amount"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(venmo_columns(&generic).is_none(), "generic header must not match");
    }

    // -----------------------------------------------------------------------
    // Scaffold parser tests — call import_venmo directly.
    // These verify the parked parser is internally consistent.

    #[test]
    fn scaffold_import_parses_all_rows() {
        let v = temp_vault("scaffold-basic");
        let stats = scaffold_import(&v, SCAFFOLD_CSV);
        assert_eq!(stats.rows, 3, "3 data rows");
        assert_eq!(stats.new_transactions, 3, "all 3 imported");
        assert_eq!(stats.skipped, 0, "no rows skipped");
        assert_eq!(stats.format, "venmo");

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 1, "one Venmo account created");
    }

    #[test]
    fn scaffold_guids_use_venmo_id() {
        let v = temp_vault("scaffold-guid");
        scaffold_import(&v, SCAFFOLD_CSV);
        let txns = v.finance_transactions(None, 100).unwrap();
        let alice = txns.iter().find(|t| t.id == "venmo-3141592653589793");
        assert!(
            alice.is_some(),
            "transaction id must be 'venmo-<ID>'; found: {:?}",
            txns.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn scaffold_inflow_is_positive_outflow_is_negative() {
        // "+ $50.00" should be positive (inflow); "- $5.80" / "- $100.00" negative (outflow).
        let v = temp_vault("scaffold-sign");
        scaffold_import(&v, SCAFFOLD_CSV);
        let txns = v.finance_transactions(None, 100).unwrap();

        let alice = txns.iter().find(|t| t.id == "venmo-3141592653589793").unwrap();
        assert!(
            !alice.amount.starts_with('-'),
            "inflow ('+ $50.00') must be positive, got {}",
            alice.amount
        );

        let bob = txns.iter().find(|t| t.id == "venmo-2718281828459045").unwrap();
        assert!(
            bob.amount.starts_with('-'),
            "outflow ('- $5.80') must be negative, got {}",
            bob.amount
        );
    }

    #[test]
    fn scaffold_note_preserved_in_extra_and_as_description() {
        let v = temp_vault("scaffold-note");
        scaffold_import(&v, SCAFFOLD_CSV);
        let txns = v.finance_transactions(None, 100).unwrap();
        let alice = txns.iter().find(|t| t.id == "venmo-3141592653589793").unwrap();
        // Note "Dinner split" must be the description and in extra.
        assert_eq!(alice.description, "Dinner split", "Note used as description");
        assert_eq!(
            alice.extra.get("note").and_then(|v| v.as_str()),
            Some("Dinner split"),
            "note preserved in extra"
        );
        assert_eq!(alice.source, "venmo");
    }

    #[test]
    fn scaffold_from_to_preserved_in_extra() {
        let v = temp_vault("scaffold-from-to");
        scaffold_import(&v, SCAFFOLD_CSV);
        let txns = v.finance_transactions(None, 100).unwrap();
        let alice = txns.iter().find(|t| t.id == "venmo-3141592653589793").unwrap();
        assert_eq!(
            alice.extra.get("from").and_then(|v| v.as_str()),
            Some("Alice Smith"),
            "From preserved in extra"
        );
        assert_eq!(
            alice.extra.get("to").and_then(|v| v.as_str()),
            Some("You"),
            "To preserved in extra"
        );
    }

    #[test]
    fn scaffold_reimport_is_idempotent() {
        let v = temp_vault("scaffold-reimport");
        let first = scaffold_import(&v, SCAFFOLD_CSV);
        assert_eq!(first.new_transactions, 3);

        let second = scaffold_import(&v, SCAFFOLD_CSV);
        assert_eq!(second.new_transactions, 0, "re-import adds nothing");
        assert_eq!(second.duplicates, 3, "all are duplicates");

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(
            v.finance_transactions(Some(&accounts[0].id), 100).unwrap().len(),
            3
        );
    }

    #[test]
    fn scaffold_raw_layer_written() {
        let v = temp_vault("scaffold-raw");
        scaffold_import(&v, SCAFFOLD_CSV);
        let accounts = v.load_finance_accounts().unwrap();
        let acct_id = &accounts[0].id;
        let raw_2026 = v.root()
            .join("finance/venmo/raw")
            .join(acct_id)
            .join("2026.jsonl");
        assert!(raw_2026.exists(), "raw JSONL written for 2026");
        let content = fs::read_to_string(&raw_2026).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 3, "3 verbatim rows in raw");

        // Verbatim: original column names and original string values.
        let row0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert!(row0.get("ID").is_some(), "original 'ID' key in raw");
        assert!(row0.get("Datetime").is_some(), "original 'Datetime' key in raw");
        assert!(row0.get("Amount (total)").is_some(), "original 'Amount (total)' key in raw");
        assert!(row0.get("Note").is_some(), "original 'Note' key in raw");
        // No synthetic/normalized keys.
        assert!(row0.get("posted").is_none(), "no synthetic 'posted' key in raw");
        assert!(row0.get("amount").is_none(), "no synthetic 'amount' key in raw");
    }

    #[test]
    fn scaffold_date_strips_time_component() {
        // "2026-06-10T14:35:12" must parse to "2026-06-10".
        let v = temp_vault("scaffold-date");
        scaffold_import(&v, SCAFFOLD_CSV);
        let txns = v.finance_transactions(None, 100).unwrap();
        let alice = txns.iter().find(|t| t.id == "venmo-3141592653589793").unwrap();
        assert_eq!(alice.posted, "2026-06-10", "date must be YYYY-MM-DD only");
    }

    #[test]
    fn scaffold_bad_rows_skipped_gracefully() {
        let csv = "\
ID,Datetime,Type,Status,Note,From,To,Amount (total),Amount (tip),Amount (tax),Amount (fee),Tax Rate,Tax Exempt Status,Funding Source,Destination,Beginning Balance,Ending Balance,Statement Period Venmo Fees,Terminal Location,Year to Date Venmo Fees,Disclaimer
GOODID,2026-06-10T14:35:12,Payment,Complete,Lunch,Alice,You,+ $30.00,,,, , ,Venmo balance,Venmo balance,$0.00,$30.00, , ,$0.00,
MISSINGID,2026-06-09T09:20:00,Payment,Complete,Missing ID but date ok,You,Bob,- $5.00,,,, , ,Venmo balance,Venmo balance,$30.00,$25.00, , ,$0.00,
BADDATE,,Payment,Complete,No date,You,Carol,- $10.00,,,, , ,Venmo balance,Venmo balance,$25.00,$15.00, , ,$0.00,
BADAMT,2026-06-08T18:00:00,Payment,Complete,No amount,You,Dave,,,,, , ,Venmo balance,Venmo balance,$15.00,$15.00, , ,$0.00,
";
        let v = temp_vault("scaffold-skip");
        let stats = scaffold_import(&v, csv);
        // Row 1: GOODID — valid.
        // Row 2: MISSINGID — has valid date and amount, so valid.
        // Row 3: BADDATE — no date → skipped.
        // Row 4: BADAMT — no amount → skipped.
        assert_eq!(stats.rows, 4);
        assert_eq!(stats.new_transactions, 2, "two valid rows imported");
        assert_eq!(stats.skipped, 2, "two bad rows skipped");
    }

    #[test]
    fn scaffold_no_note_falls_back_to_from_to() {
        // When Note is empty, description should be "From → To".
        let csv = "\
ID,Datetime,Type,Status,Note,From,To,Amount (total),Amount (tip),Amount (tax),Amount (fee),Tax Rate,Tax Exempt Status,Funding Source,Destination,Beginning Balance,Ending Balance,Statement Period Venmo Fees,Terminal Location,Year to Date Venmo Fees,Disclaimer
NOID1,2026-06-10T10:00:00,Bank Transfer,Settled,,You,Chase Bank,- $100.00,,,, , ,Venmo balance,Bank,$100.00,$0.00, , ,$0.00,
";
        let v = temp_vault("scaffold-desc-fallback");
        scaffold_import(&v, csv);
        let txns = v.finance_transactions(None, 100).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(txns[0].description, "You → Chase Bank", "falls back to 'From → To'");
    }
}

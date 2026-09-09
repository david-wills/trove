//! Fidelity brokerage activity CSV import — transaction history downloaded
//! from fidelity.com → Accounts & Trade → Activity & Orders → History →
//! Download. No consumer API exists (Fidelity ended OFX export January 17,
//! 2026), so CSV is the only programmatic path.
//!
//! **Vault layout:**
//! - Contract: `finance/transactions/<account>/YYYY.jsonl` — one
//!   [`crate::finance::Transaction`] row per activity row, keyed by a
//!   synthesized id; source="fidelity". Reuses the existing
//!   [`crate::finance::Vault::upsert_finance_transactions`] path.
//! - Raw: `finance/fidelity/raw/<account>/YYYY.jsonl` — verbatim row
//!   including Action, Symbol, Quantity, Price, Commission.
//!
//! **90-day window:** each portal download covers 90 days; four downloads
//! cover a year. The onboarding copy tells the user to drop all windows.
//! Re-importing an overlapping window is a clean no-op (synthesized-id + fuzzy
//! dedupe).
//!
//! **Sign convention:** Fidelity signs BUYS and fees negative, SELLS and
//! DIVIDENDS positive — outflow-negative, matching the vault convention.
//!
//! **Holdings:** the Portfolio export has a different, undocumented format.
//! The `holdings` import card is wired (accepts CSV) but the parser is parked
//! until a real sample arrives.
//!
//! `docs/integrations/fidelity.md` — authoritative brief.

use std::path::Path;

use anyhow::Result;

use crate::finance::import::{CsvImportStats, fidelity_columns};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Activity import.

fn run_activity_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Parse headers to confirm this is a Fidelity activity CSV before calling
    // the shared importer, so the error message names "fidelity-activity" and
    // not the generic CSV error.
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let raw = raw.trim_start_matches('\u{feff}');
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(raw.as_bytes());
    let headers: Vec<String> = reader
        .headers()
        .map_err(|e| anyhow::anyhow!("reading CSV header: {e}"))?
        .iter()
        .map(|s| s.to_string())
        .collect();
    if fidelity_columns(&headers).is_none() {
        return Err(anyhow::anyhow!(
            "this CSV does not look like a Fidelity activity history export — expected columns \
             including: Run Date, Action, Symbol, Description, Amount ($). \
             (Per-account export: fidelity.com → Accounts & Trade → Activity & Orders → History → Download. \
             All-Accounts export also has an Account Number column.) Found: {}",
            headers.join(", ")
        ));
    }
    // Delegate to the shared importer (which detects Fidelity internally).
    let s = vault.finance_import_csv(path, None, None)?;
    outcome_from(s)
}

static ACTIVITY_IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_activity_import,
};

fn activity_last_data(vault: &Vault) -> Option<String> {
    vault.read_finance_import_state().map(|s| s.updated).filter(|u| !u.is_empty())
}

// ---------------------------------------------------------------------------
// DEF.

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "fidelity",
        name: "Fidelity",
        kind: IntegrationKind::Import,
        // Financial detail — ships opt-in.
        default_on: false,
        description: "Import your Fidelity brokerage transaction history from CSVs downloaded \
                      through the Fidelity portal. Covers retirement accounts, taxable brokerage, \
                      and cash management accounts. Each download covers a 90-day window; drop \
                      multiple files to cover a longer period — re-imports dedupe cleanly.",
        domain: "finance",
        vault_path: "finance/fidelity/",
        toggleable: false,
        setup: &[
            "fidelity.com → Accounts & Trade → Activity & Orders → History → Download (CSV).",
            "Each download covers 90 days — repeat for each window you want to import, or use \
             the longest available date range. Drop each file here.",
            "Fidelity ended OFX export in January 2026; CSV is the only available format.",
        ],
        caveats: "Each portal download covers a 90-day window; four downloads cover a year. \
                  Holdings (Portfolio export) are not yet supported — the finance-holdings \
                  contract is an unbound Phase-3 draft; the parser will land once the contract binds.",
    },
    behavior: Behavior::Import(&ACTIVITY_IMPORT),
    permission: None,
    last_data: Some(activity_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Shared outcome formatter.

fn outcome_from(s: CsvImportStats) -> Result<ImportOutcome> {
    Ok(ImportOutcome {
        headline: format!(
            "{} new transactions into {} (fidelity), {} duplicates skipped",
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
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-fidelity-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A synthetic Fidelity activity CSV matching the REAL per-account export
    /// format (confirmed by TradingDiaryPro, TradeLog, Moneydance/InfiniteKind,
    /// and community samples):
    ///
    ///   Run Date,Action,Symbol,Description,Type,Quantity,Price ($),
    ///   Commission ($),Fees ($),Accrued Interest ($),Amount ($),
    ///   Cash Balance ($),Settlement Date
    ///
    /// Real action strings: "YOU BOUGHT", "YOU SOLD", "DIVIDEND RECEIVED",
    /// "REINVESTMENT", etc.  Sign: buys negative, dividends/sells positive.
    ///
    /// Per-account export (no Account Number column) — all rows belong to
    /// whichever vault account the user assigns.
    const ACTIVITY_CSV: &str = "\
Run Date,Action,Symbol,Description,Type,Quantity,Price ($),Commission ($),Fees ($),Accrued Interest ($),Amount ($),Cash Balance ($),Settlement Date
06/10/2026,YOU BOUGHT,VOO,VANGUARD 500 INDEX FUND,Cash,2,530.00,0.00,0.00,0.00,-1060.00,8940.00,06/12/2026
06/09/2026,DIVIDEND RECEIVED,VOO,VANGUARD 500 INDEX FUND ETF DIVIDEND,Cash,,,0.00,0.00,0.00,12.34,10012.34,
06/15/2026,YOU SOLD,AAPL,APPLE INC,Cash,5,189.50,0.00,0.00,0.00,947.50,10959.84,06/17/2026
06/10/2026,REINVESTMENT,FCNTX,FIDELITY CONTRAFUND (FCNTX),Cash,0.523,191.30,0.00,0.00,0.00,-100.15,10859.69,06/10/2026
";

    /// Multi-account ("All Accounts") export — has Account Number + Account Name
    /// prepended before Run Date. Two accounts: X12345678 and Z98765432.
    const ACTIVITY_CSV_MULTI: &str = "\
Account Number,Account Name,Run Date,Action,Symbol,Description,Type,Quantity,Price ($),Commission ($),Fees ($),Accrued Interest ($),Amount ($),Cash Balance ($),Settlement Date
X12345678,Individual Brokerage,06/10/2026,YOU BOUGHT,VOO,VANGUARD 500 INDEX FUND,Cash,2,530.00,0.00,0.00,0.00,-1060.00,8940.00,06/12/2026
X12345678,Individual Brokerage,06/09/2026,DIVIDEND RECEIVED,VOO,VANGUARD 500 INDEX FUND ETF DIVIDEND,Cash,,,0.00,0.00,0.00,12.34,10012.34,
X12345678,Individual Brokerage,05/15/2026,YOU SOLD,AAPL,APPLE INC,Cash,5,189.50,0.00,0.00,0.00,947.50,10959.84,05/17/2026
Z98765432,Roth IRA,06/10/2026,YOU BOUGHT,SPAXX,FIDELITY GOVT MONEY MARKET,Cash,100,1.00,0.00,0.00,0.00,-100.00,9900.00,06/10/2026
";

    fn import_activity(v: &Vault, csv_body: &str) -> ImportOutcome {
        let path = v.root().join("activity.csv");
        fs::write(&path, csv_body).unwrap();
        (ACTIVITY_IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// Per-account export: all 4 rows belong to one vault account (no account
    /// column), including the dividend which has a blank Settlement Date.
    #[test]
    fn per_account_export_imports_all_rows_including_dividend() {
        let v = temp_vault("per-account");
        let out = import_activity(&v, ACTIVITY_CSV);
        assert_eq!(out.counts.get("rows"), Some(&4), "all data rows counted");
        assert_eq!(out.counts.get("new_transactions"), Some(&4), "all 4 imported");
        assert_eq!(out.counts.get("duplicates"), Some(&0));
        assert_eq!(out.counts.get("skipped"), Some(&0), "dividend row NOT skipped");

        // Exactly one vault account created (per-account fallback).
        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 1, "one account for a per-account file");

        let acct_id = accounts[0].id.clone();
        let rows = v.finance_transactions(Some(&acct_id), 100).unwrap();
        assert_eq!(rows.len(), 4, "4 transactions on disk");

        // Buy row: posted = Run Date (2026-06-10), raw action in extra, normalized verb in desc.
        let buy = rows.iter().find(|t| t.description.contains("Buy VOO")).unwrap();
        assert_eq!(buy.posted, "2026-06-10", "posted from Run Date");
        assert_eq!(buy.amount, "-1060.00", "buys are negative");
        assert_eq!(buy.source, "fidelity");
        // Raw action string preserved in extra.
        assert_eq!(buy.extra.get("action").and_then(|v| v.as_str()), Some("YOU BOUGHT"));
        assert_eq!(buy.extra.get("symbol").and_then(|v| v.as_str()), Some("VOO"));
        assert_eq!(buy.extra.get("quantity").and_then(|v| v.as_str()), Some("2"));
        assert_eq!(buy.extra.get("price").and_then(|v| v.as_str()), Some("530.00"));
        // Settlement date stored in extra when present.
        assert_eq!(buy.extra.get("settlement_date").and_then(|v| v.as_str()), Some("2026-06-12"));
        // No account_number in extra for per-account files.
        assert!(buy.extra.get("account_number").is_none());

        // Dividend row: settlement date is blank — must NOT be skipped.
        let div = rows.iter().find(|t| t.description.contains("Dividend VOO")).unwrap();
        assert_eq!(div.posted, "2026-06-09", "dividend posted from Run Date");
        assert_eq!(div.amount, "12.34", "dividends are positive");
        assert!(div.extra.get("settlement_date").is_none(), "blank settlement not stored");

        // Sell is positive.
        let sell = rows.iter().find(|t| t.description.contains("Sell AAPL")).unwrap();
        assert_eq!(sell.amount, "947.50", "sells are positive");

        // Reinvestment.
        let reinv = rows.iter().find(|t| t.description.contains("Reinvestment FCNTX")).unwrap();
        assert_eq!(reinv.amount, "-100.15");
    }

    /// Multi-account ("All Accounts") export self-routes rows across two vault accounts.
    #[test]
    fn multi_account_export_routes_by_account_number() {
        let v = temp_vault("multi");
        let out = import_activity(&v, ACTIVITY_CSV_MULTI);
        assert_eq!(out.counts.get("rows"), Some(&4), "all data rows counted");
        assert_eq!(out.counts.get("new_transactions"), Some(&4), "all 4 imported");
        assert_eq!(out.counts.get("skipped"), Some(&0));

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 2, "two Fidelity account numbers → two vault accounts");
        assert!(accounts.iter().any(|a| a.aliases.get("fidelity") == Some(&"X12345678".to_string())));
        assert!(accounts.iter().any(|a| a.aliases.get("fidelity") == Some(&"Z98765432".to_string())));

        let x_acct = accounts
            .iter()
            .find(|a| a.aliases.get("fidelity") == Some(&"X12345678".to_string()))
            .unwrap();
        let x_rows = v.finance_transactions(Some(&x_acct.id), 100).unwrap();
        assert_eq!(x_rows.len(), 3, "3 trades in X12345678");

        let buy = x_rows.iter().find(|t| t.description.contains("Buy VOO")).unwrap();
        // account_number stored in extra for multi-account exports.
        assert_eq!(buy.extra.get("account_number").and_then(|v| v.as_str()), Some("X12345678"));

        // Dividend (blank settlement date) is not skipped.
        let div = x_rows.iter().find(|t| t.description.contains("Dividend")).unwrap();
        assert_eq!(div.amount, "12.34");
    }

    #[test]
    fn reimport_per_account_is_idempotent() {
        let v = temp_vault("reimport");
        let first = import_activity(&v, ACTIVITY_CSV);
        assert_eq!(first.counts.get("new_transactions"), Some(&4));

        let second = import_activity(&v, ACTIVITY_CSV);
        assert_eq!(second.counts.get("new_transactions"), Some(&0), "re-import adds nothing");
        assert_eq!(second.counts.get("duplicates"), Some(&4), "all are duplicates");

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(v.finance_transactions(Some(&accounts[0].id), 100).unwrap().len(), 4);
    }

    #[test]
    fn raw_layer_is_written_per_account_year() {
        let v = temp_vault("raw");
        import_activity(&v, ACTIVITY_CSV);

        let accounts = v.load_finance_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        let acct_id = &accounts[0].id;
        let raw_2026 = v.root()
            .join("finance/fidelity/raw")
            .join(acct_id)
            .join("2026.jsonl");
        assert!(raw_2026.exists(), "raw file written for account 2026");
        let raw_content = fs::read_to_string(&raw_2026).unwrap();
        let raw_lines: Vec<&str> = raw_content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert!(raw_lines.len() >= 4, "all 4 rows in 2026 raw");
        assert!(raw_lines.iter().any(|l| l.contains("\"VOO\"")), "VOO in raw");
    }

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "fidelity").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        assert_eq!(import_info.accepts, &["csv"]);
        assert!(!card.enabled, "default_on=false: opt-in");
    }

    #[test]
    fn rejects_non_fidelity_csv_with_clear_error() {
        let v = temp_vault("reject");
        let path = v.root().join("notfidelity.csv");
        fs::write(&path, "Date,Description,Amount\n2026-06-10,Coffee,-4.50\n").unwrap();
        let err = (ACTIVITY_IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("Run Date") || err.contains("Action"), "error names the expected columns: {err}");
    }

    #[test]
    fn summary_footer_rows_are_skipped_gracefully() {
        // Fidelity appends a text summary block at the end; rows with no Run
        // Date must be skipped without panicking.
        let csv_with_footer = format!(
            "{ACTIVITY_CSV}\
\"Total\",,,,,,,,,,,-200.16,,\n\
\"\",,,,,,,,,,,,\n"
        );
        let v = temp_vault("footer");
        let out = import_activity(&v, &csv_with_footer);
        assert_eq!(out.counts.get("new_transactions"), Some(&4), "4 data rows only");
        assert!(out.counts.get("skipped").is_some_and(|&n| n >= 2), "footer rows skipped");
    }
}

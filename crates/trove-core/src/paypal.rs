//! PayPal activity download import — payments and transfers.
//! Brief: docs/integrations/paypal.md
//!
//! The PayPal activity CSV (paypal.com/reports/dlog → CSV, up to 7 years)
//! is the only complete source: the REST API requires developer-held OAuth
//! credentials (not distributable) and covers only 3 years. The CSV is the
//! deliberate, complete path — not a fallback.
//!
//! The parser lives in `finance/import.rs` (same family as Chase / Copilot /
//! Cash App); this module provides the hub card DEF and the import box.

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportParam, ImportSpec, IntegrationDef};
use crate::vault::Vault;
use std::path::Path;

fn csv_run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(crate::health::ImportProgress),
) -> anyhow::Result<crate::registry::ImportOutcome> {
    let get = |k: &str| params.get(k).map(String::as_str).map(str::trim).filter(|v| !v.is_empty());
    let s = vault.finance_import_csv(path, get("account"), get("new_account"))?;
    Ok(crate::registry::ImportOutcome {
        headline: format!(
            "{} new PayPal transactions ({} format), {} duplicates merged or skipped",
            s.new_transactions, s.format, s.duplicates
        ),
        counts: [
            ("rows", s.rows as u64),
            ("new_transactions", s.new_transactions as u64),
            ("duplicates", s.duplicates as u64),
            ("merged", s.merged as u64),
            ("skipped", s.skipped as u64),
        ]
        .into(),
    })
}

fn last_data(vault: &Vault) -> Option<String> {
    vault
        .read_finance_import_state()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
}

static IMPORT_SPEC: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[
        ImportParam {
            key: "account",
            label: "Existing account id",
            placeholder: "pick or leave empty to use the default PayPal account",
            required: false,
        },
        ImportParam {
            key: "new_account",
            label: "New account name",
            placeholder: "e.g. PayPal Personal",
            required: false,
        },
    ],
    run: csv_run_import,
};

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "paypal",
        name: "PayPal",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your PayPal account activity — purchases, peer payments, \
                      refunds, and balance transfers — from the official CSV download. \
                      Covers up to 7 years of history. PayPal sits outside most bank feeds, \
                      so the export is the only complete record.",
        domain: "finance",
        vault_path: "finance/",
        toggleable: false,
        setup: &[
            "Log in at paypal.com and go to Activity → Statements → Activity download \
             (or visit paypal.com/reports/dlog).",
            "Choose a date range (up to 7 years), select CSV format, and download.",
            "If you have more than 50,000 transactions, download in chunks and import \
             each file — overlapping ranges dedupe cleanly.",
            "Drag the CSV file here to import.",
        ],
        caveats: "The PayPal REST API requires developer-held credentials and covers only \
                  3 years, so the CSV download is the deliberate path. Bank-side settlements \
                  from PayPal may also appear in your bank account rows — the vault keeps both; \
                  reconciliation happens at read time.",
    },
    behavior: Behavior::Import(&IMPORT_SPEC),
    permission: None,
    last_data: Some(last_data),
    connection: None,
    pull: None,
};

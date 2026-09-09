//! Finance: bank, credit-card, and (later) merchant-level purchase data.
//!
//! Layout under `finance/` — plain JSONL, files are the source of truth:
//!
//! ```text
//! finance/
//!   accounts.jsonl                    # registry: one line per known account
//!   transactions/<account-id>/<year>.jsonl
//!   balances/<account-id>.jsonl       # daily balance snapshots
//! ```
//!
//! Backends normalize to the canonical types in [`model`] before anything
//! lands here, so alternate aggregators and file importers (phase 2) slot in
//! without schema churn. v1 backend: SimpleFIN ([`simplefin`]) — the user's
//! own Bridge credential, claimed once and kept in the macOS Keychain
//! ([`keychain`]), never in the vault.
//!
//! Design notes live in `docs/finance-integrations-plan.md`.

pub mod import;
pub mod keychain;
pub mod model;
pub mod purchases;
pub mod simplefin;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;
pub use model::{Account, BalanceSnapshot, Transaction};
pub use purchases::LineItem;

const STATE_FILE: &str = ".trove/sync/finance-state.json";

// A silent no-op until a SimpleFIN credential is connected; standing errors
// land in the state file for the hub card. The daily gate commits only on
// success, so a failed pull retries on the next slow tick.
fn bank_sync_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let stats = vault.finance_sync()?;
    Ok(crate::registry::CollectOutcome::note_if(stats.accounts > 0, || {
        format!(
            "finance synced — {} accounts, {} new transactions",
            stats.accounts, stats.new_transactions
        )
    }))
}

// Manual "Sync now" off the hub card — same engine as the scheduled pass,
// but the user asked, so a no-op gets named instead of staying silent.
fn bank_sync_pull(vault: &Vault) -> Result<crate::registry::PullOutcome> {
    let stats = vault.finance_sync()?;
    let headline = if stats.accounts > 0 {
        format!(
            "{} accounts synced — {} new transactions, {} updated",
            stats.accounts, stats.new_transactions, stats.updated_transactions
        )
    } else if keychain::load_access_url().unwrap_or(None).is_some() {
        "the Bridge returned no accounts — connect banks at bridge.simplefin.org".to_string()
    } else {
        "no SimpleFIN credential connected — nothing pulled".to_string()
    };
    Ok(crate::registry::PullOutcome {
        headline,
        counts: [
            ("accounts", stats.accounts as u64),
            ("new_transactions", stats.new_transactions as u64),
            ("updated_transactions", stats.updated_transactions as u64),
        ]
        .into(),
    })
}

fn bank_sync_last_data(vault: &Vault) -> Option<String> {
    vault.read_finance_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

fn csv_import_last_data(vault: &Vault) -> Option<String> {
    vault.read_finance_import_state().map(|s| s.updated).filter(|u| !u.is_empty())
}

// Exactly one of `account` (existing vault account id) / `new_account` (name
// to register) must be given — validated inside `finance_import_csv`.
fn csv_run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(crate::health::ImportProgress),
) -> Result<crate::registry::ImportOutcome> {
    let get = |k: &str| params.get(k).map(String::as_str).map(str::trim).filter(|v| !v.is_empty());
    let s = vault.finance_import_csv(path, get("account"), get("new_account"))?;
    Ok(crate::registry::ImportOutcome {
        headline: format!(
            "{} new transactions into {} ({} format), {} duplicates merged or skipped",
            s.new_transactions, s.account, s.format, s.duplicates
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

static CSV_IMPORT: crate::registry::ImportSpec = crate::registry::ImportSpec {
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
    run: csv_run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static BANK_SYNC_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bank-sync",
        name: "Banks & cards (SimpleFIN)",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Balances and transactions for your bank and credit-card accounts, pulled once a day through your own SimpleFIN Bridge credential. The first sync backfills as much history as your banks provide.",
        domain: "finance",
        vault_path: "finance/",
        toggleable: true,
        setup: &[
            "Create a SimpleFIN Bridge account at bridge.simplefin.org ($1.50/mo, paid to SimpleFIN — the fee is the price of Trove not having a cloud).",
            "Connect your banks there through the hosted login pages (~16k institutions).",
            "Bridge dashboard → New connection/app → copy the one-time setup token and paste it here. It's claimed once and burned; the resulting credential lives in the macOS Keychain, never in the vault.",
        ],
        caveats: "Read-only and roughly once-daily by design — that's the Bridge's refresh rate. Bank connections occasionally need repair (MFA resets) at bridge.simplefin.org, not in Trove; a stale 'last data' here is the signal. Apple Card, Venmo, and Cash App can't be reached by any aggregator — the statement import alongside covers those.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::daily(crate::browser::BROWSER_SYNC_SECS), collect: bank_sync_collect },
    permission: None,
    last_data: Some(bank_sync_last_data),
    connection: Some("simplefin"),
    pull: Some(bank_sync_pull),
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static CSV_IMPORT_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "csv-import",
        name: "Bank statement import (CSV)",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import transaction CSVs exported from your bank or a Copilot Money export — backfills history past the aggregator's ~90-day window, and covers accounts no aggregator reaches (Apple Card, Venmo, Cash App). Re-runnable: overlap with synced data merges instead of duplicating.",
        domain: "finance",
        vault_path: "finance/",
        toggleable: false,
        setup: &[
            "Export account activity as CSV from your bank's website, choosing the longest date range offered (Chase: account → download icon → CSV).",
            "Pick the matching account below (or name a new one) and import the file.",
            "Copilot Money users: Settings → Export data gives one transactions.csv covering every account and year — pick the Copilot option in the dropdown; accounts map themselves.",
            "Repeat per account and date range — re-imports and overlaps dedupe cleanly.",
        ],
        caveats: "Chase checking/credit-card layouts and Copilot exports are recognized exactly; other banks go through a generic column reader (date, amount, description headers) — a refused file's error names the columns it found. Copilot rewrites merchant names, so its overlap-dedupe falls back to amount+date matching. QFX/OFX import is planned next.",
    },
    behavior: Behavior::Import(&CSV_IMPORT),
    permission: None,
    last_data: Some(csv_import_last_data),
    connection: None,
    pull: None,
};

/// First connect pulls as much history as the Bridge will give (~5 years
/// requested; most institutions return far less).
const FIRST_SYNC_LOOKBACK_SECS: i64 = 5 * 365 * 24 * 3600;
/// Later syncs re-request a trailing window so late postings and
/// pending→posted changes are caught.
const RESYNC_OVERLAP_SECS: i64 = 7 * 24 * 3600;

/// Sync metadata for the hub and Finance tab (`.trove/sync/finance-state.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct FinanceSyncState {
    /// RFC3339 local time of the last successful sync; "" if never.
    #[serde(default)]
    pub updated: String,
    /// Unix epoch of the last successful sync — drives the next window.
    #[serde(default)]
    pub last_epoch: Option<i64>,
    /// Standing failure (auth, network) or Bridge-reported connection
    /// warnings ("Chase may need attention"), for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub accounts: usize,
    #[serde(default)]
    pub new_transactions: usize,
}

/// Outcome of one sync pass.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct FinanceSyncStats {
    pub accounts: usize,
    pub new_transactions: usize,
    pub updated_transactions: usize,
}

/// One account row for the Finance tab: registry entry + latest balance.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AccountOverview {
    pub id: String,
    pub name: String,
    pub org: String,
    pub currency: String,
    /// Latest snapshot, if any. Decimal strings, never floats.
    pub balance: Option<String>,
    pub available: Option<String>,
    /// Day of the latest snapshot (YYYY-MM-DD).
    pub balance_date: Option<String>,
}

/// Everything the Finance tab and hub card need in one read.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct FinanceOverview {
    /// A SimpleFIN credential is present in the Keychain.
    pub connected: bool,
    pub state: Option<FinanceSyncState>,
    pub accounts: Vec<AccountOverview>,
}

impl Vault {
    fn finance_dir(&self) -> PathBuf {
        self.root().join("finance")
    }

    /// The account registry (missing file = no accounts yet).
    pub fn load_finance_accounts(&self) -> Result<Vec<Account>> {
        read_jsonl(&self.finance_dir().join("accounts.jsonl"))
    }

    pub(crate) fn save_finance_accounts(&self, accounts: &[Account]) -> Result<()> {
        write_jsonl(&self.finance_dir().join("accounts.jsonl"), accounts)
    }

    /// Find the vault account carrying `source`/`source_id` as an alias, or
    /// register a new one. First sight of an unknown source account
    /// auto-creates it — it surfaces in the UI for optional rename/merge.
    pub fn resolve_finance_account(
        &self,
        accounts: &mut Vec<Account>,
        source: &str,
        source_id: &str,
        name: &str,
        org: &str,
        currency: &str,
    ) -> String {
        if let Some(a) = accounts
            .iter()
            .find(|a| a.aliases.get(source).is_some_and(|id| id == source_id))
        {
            return a.id.clone();
        }
        let taken: Vec<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
        let id = model::unique_slug(&model::account_slug(org, name), &taken);
        let mut aliases = BTreeMap::new();
        aliases.insert(source.to_string(), source_id.to_string());
        accounts.push(Account {
            id: id.clone(),
            name: name.to_string(),
            org: org.to_string(),
            currency: currency.to_string(),
            aliases,
            created: Local::now().to_rfc3339(),
        });
        id
    }

    /// Idempotent upsert into the account's year files, keyed by transaction
    /// id. With `replace_pending`, the account's existing pending rows are
    /// dropped first — pendings are ephemeral until posted (the id usually
    /// changes at posting), so each sync replaces the whole pending set.
    /// Returns (new, updated).
    pub fn upsert_finance_transactions(
        &self,
        account: &str,
        incoming: &[Transaction],
        replace_pending: bool,
    ) -> Result<(usize, usize)> {
        check_account_id(account)?;
        let dir = self.finance_dir().join("transactions").join(account);
        // Years to touch: every incoming year, plus (for the pending purge)
        // every year file that already exists.
        let mut years: Vec<String> = incoming
            .iter()
            .map(|t| year_of(&t.posted))
            .collect::<Result<Vec<_>>>()?;
        if replace_pending {
            if let Ok(entries) = fs::read_dir(&dir) {
                for e in entries.flatten() {
                    if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str()) {
                        years.push(stem.to_string());
                    }
                }
            }
        }
        years.sort();
        years.dedup();

        let (mut new, mut updated) = (0, 0);
        for year in years {
            let path = dir.join(format!("{year}.jsonl"));
            let mut rows: Vec<Transaction> = read_jsonl(&path)?;
            let mut changed = false;
            // Existing pendings the sync didn't re-send are stale (posted
            // under a new id, or evaporated) and get dropped at the end;
            // re-sent ones are ordinary upserts, so an unchanged payload is
            // a true no-op.
            let mut stale_pending: std::collections::HashSet<String> = if replace_pending {
                rows.iter().filter(|t| t.pending).map(|t| t.id.clone()).collect()
            } else {
                Default::default()
            };
            for t in incoming.iter().filter(|t| t.posted.starts_with(&year)) {
                stale_pending.remove(&t.id);
                match rows.iter_mut().find(|r| r.id == t.id) {
                    Some(existing) => {
                        if existing != t {
                            *existing = t.clone();
                            updated += 1;
                            changed = true;
                        }
                    }
                    None => {
                        rows.push(t.clone());
                        new += 1;
                        changed = true;
                    }
                }
            }
            if !stale_pending.is_empty() {
                rows.retain(|r| !stale_pending.contains(&r.id));
                changed = true;
            }
            if changed {
                rows.sort_by(|a, b| (&a.posted, &a.id).cmp(&(&b.posted, &b.id)));
                write_jsonl(&path, &rows)?;
            }
        }
        Ok((new, updated))
    }

    /// Record a balance snapshot — one line per local day, last write wins.
    pub fn record_finance_balance(&self, account: &str, snap: &BalanceSnapshot) -> Result<()> {
        check_account_id(account)?;
        let path = self
            .finance_dir()
            .join("balances")
            .join(format!("{account}.jsonl"));
        let mut rows: Vec<BalanceSnapshot> = read_jsonl(&path)?;
        rows.retain(|r| r.date != snap.date);
        rows.push(snap.clone());
        rows.sort_by(|a, b| a.date.cmp(&b.date));
        write_jsonl(&path, &rows)
    }

    /// Transactions across all accounts (or one), newest first, capped at
    /// `limit`. Pending rows sort with their posted date like everything else.
    pub fn finance_transactions(
        &self,
        account: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Transaction>> {
        let base = self.finance_dir().join("transactions");
        let mut dirs = Vec::new();
        match account {
            Some(a) => {
                check_account_id(a)?;
                dirs.push(base.join(a));
            }
            None => {
                if let Ok(entries) = fs::read_dir(&base) {
                    dirs.extend(entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
                }
            }
        }
        let mut all = Vec::new();
        for dir in dirs {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                all.append(&mut read_jsonl::<Transaction>(&e.path())?);
            }
        }
        all.sort_by(|a, b| (&b.posted, &b.id).cmp(&(&a.posted, &a.id)));
        all.truncate(limit);
        Ok(all)
    }

    /// Everything the Finance tab and hub card render, in one cheap read.
    pub fn finance_overview(&self) -> Result<FinanceOverview> {
        let accounts = self
            .load_finance_accounts()?
            .into_iter()
            .map(|a| {
                let latest = read_jsonl::<BalanceSnapshot>(
                    &self
                        .finance_dir()
                        .join("balances")
                        .join(format!("{}.jsonl", a.id)),
                )
                .ok()
                .and_then(|rows| rows.into_iter().max_by(|x, y| x.date.cmp(&y.date)));
                AccountOverview {
                    id: a.id,
                    name: a.name,
                    org: a.org,
                    currency: a.currency,
                    balance: latest.as_ref().map(|b| b.balance.clone()),
                    available: latest.as_ref().and_then(|b| b.available.clone()),
                    balance_date: latest.map(|b| b.date),
                }
            })
            .collect();
        Ok(FinanceOverview {
            connected: keychain::load_access_url().unwrap_or(None).is_some(),
            state: self.read_finance_sync(),
            accounts,
        })
    }

    /// Sync metadata, if any sync has been attempted.
    pub fn read_finance_sync(&self) -> Option<FinanceSyncState> {
        let raw = fs::read_to_string(self.root().join(STATE_FILE)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub(crate) fn write_finance_sync(&self, state: &FinanceSyncState) -> Result<()> {
        let path = self.root().join(STATE_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("creating .trove/sync")?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(state)?)
            .context("writing finance sync state")?;
        fs::rename(&tmp, &path).context("publishing finance sync state")
    }

    /// One SimpleFIN pull: fetch the window, normalize, upsert. A silent
    /// no-op when no credential is in the Keychain (machines that never
    /// connected just skip — same contract as the TickTick sync). Failures
    /// are recorded in the state file for the UI, then returned.
    pub fn finance_sync(&self) -> Result<FinanceSyncStats> {
        let Some(access_url) = keychain::load_access_url()? else {
            return Ok(FinanceSyncStats::default());
        };
        let mut state = self.read_finance_sync().unwrap_or_default();
        let now = Local::now();
        let start = state
            .last_epoch
            .map(|t| t - RESYNC_OVERLAP_SECS)
            .unwrap_or_else(|| now.timestamp() - FIRST_SYNC_LOOKBACK_SECS);
        let set = match simplefin::fetch_accounts(&access_url, Some(start)) {
            Ok(set) => set,
            Err(e) => {
                state.error = Some(format!("{e:#}"));
                self.write_finance_sync(&state)?;
                return Err(e);
            }
        };
        let stats = self.apply_account_set(&set, &now)?;
        self.write_finance_sync(&FinanceSyncState {
            updated: now.to_rfc3339(),
            last_epoch: Some(now.timestamp()),
            // Bridge "errors" alongside data are connection warnings
            // ("Chase may need attention") — surfaced, not fatal.
            error: (!set.errors.is_empty()).then(|| set.errors.join("; ")),
            accounts: stats.accounts,
            new_transactions: stats.new_transactions,
        })?;
        Ok(stats)
    }

    /// Land one fetched account set in the vault: registry, balances,
    /// transactions. Separated from [`Vault::finance_sync`] so tests feed
    /// parsed payloads without a network.
    pub(crate) fn apply_account_set(
        &self,
        set: &simplefin::SfinAccountSet,
        now: &DateTime<Local>,
    ) -> Result<FinanceSyncStats> {
        let mut accounts = self.load_finance_accounts()?;
        let mut stats = FinanceSyncStats::default();
        for sf in &set.accounts {
            let vault_id = self.resolve_finance_account(
                &mut accounts,
                "simplefin",
                &sf.id,
                &sf.name,
                sf.org.display_name(),
                &sf.currency,
            );
            self.record_finance_balance(&vault_id, &sf.balance_snapshot(now))?;
            let txns: Vec<Transaction> =
                sf.transactions.iter().map(|t| simplefin::normalize(&vault_id, &sf.currency, t)).collect();
            let (new, updated) = self.upsert_finance_transactions(&vault_id, &txns, true)?;
            stats.new_transactions += new;
            stats.updated_transactions += updated;
        }
        stats.accounts = set.accounts.len();
        self.save_finance_accounts(&accounts)?;
        Ok(stats)
    }
}

/// Account ids are Trove-assigned slugs, but they arrive back over IPC as
/// strings and become path segments — keep them boring.
fn check_account_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("invalid account id: {id}");
    }
    Ok(())
}

fn year_of(posted: &str) -> Result<String> {
    let year = posted.get(..4).unwrap_or_default();
    if year.len() != 4 || !year.chars().all(|c| c.is_ascii_digit()) {
        bail!("bad posted date: {posted}");
    }
    Ok(year.to_string())
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).with_context(|| format!("parsing {}", path.display()))
        })
        .collect()
}

/// Full atomic rewrite (write temp, rename). Finance files are small enough
/// that rewrite-on-change keeps every reader simple and every file sorted.
fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut out = String::new();
    for row in rows {
        out.push_str(&serde_json::to_string(row)?);
        out.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("publishing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-finance-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn txn(id: &str, posted: &str, amount: &str, pending: bool) -> Transaction {
        Transaction {
            id: id.into(),
            account: "test-checking".into(),
            posted: posted.into(),
            transacted: None,
            amount: amount.into(),
            currency: "USD".into(),
            description: format!("txn {id}"),
            payee: None,
            category: None,
            pending,
            source: "simplefin".into(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn account_resolution_is_stable_and_collision_safe() {
        let v = temp_vault("accounts");
        let mut accounts = v.load_finance_accounts().unwrap();
        let a = v.resolve_finance_account(&mut accounts, "simplefin", "ACT-1", "Checking", "Chase", "USD");
        let again = v.resolve_finance_account(&mut accounts, "simplefin", "ACT-1", "Checking", "Chase", "USD");
        assert_eq!(a, again, "same alias resolves to the same account");
        // Different source account, same human name → distinct slug.
        let b = v.resolve_finance_account(&mut accounts, "simplefin", "ACT-2", "Checking", "Chase", "USD");
        assert_ne!(a, b);
        assert_eq!(b, format!("{a}-2"));
        v.save_finance_accounts(&accounts).unwrap();
        assert_eq!(v.load_finance_accounts().unwrap().len(), 2);
    }

    #[test]
    fn upsert_is_idempotent_and_sorted() {
        let v = temp_vault("upsert");
        let batch = [txn("t1", "2026-06-10", "-5.00", false), txn("t2", "2026-06-09", "-7.50", false)];
        let (new, updated) = v.upsert_finance_transactions("test-checking", &batch, true).unwrap();
        assert_eq!((new, updated), (2, 0));
        // Same batch again: nothing changes.
        let (new, updated) = v.upsert_finance_transactions("test-checking", &batch, true).unwrap();
        assert_eq!((new, updated), (0, 0));
        // One row modified: counted as updated, not duplicated.
        let mut changed = batch.to_vec();
        changed[0].description = "renamed".into();
        let (new, updated) = v.upsert_finance_transactions("test-checking", &changed, true).unwrap();
        assert_eq!((new, updated), (0, 1));
        let rows = v.finance_transactions(Some("test-checking"), 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].posted, "2026-06-10", "newest first");
    }

    #[test]
    fn pending_set_is_replaced_each_sync() {
        let v = temp_vault("pending");
        v.upsert_finance_transactions(
            "test-checking",
            &[txn("pend-1", "2026-06-10", "-9.99", true), txn("t1", "2026-06-09", "-1.00", false)],
            true,
        )
        .unwrap();
        // Next sync: the pending posted under a new id.
        v.upsert_finance_transactions(
            "test-checking",
            &[txn("post-1", "2026-06-10", "-9.99", false)],
            true,
        )
        .unwrap();
        let rows = v.finance_transactions(Some("test-checking"), 100).unwrap();
        let ids: Vec<&str> = rows.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["post-1", "t1"], "stale pending dropped, no duplicate");
    }

    #[test]
    fn transactions_split_by_year() {
        let v = temp_vault("years");
        v.upsert_finance_transactions(
            "test-checking",
            &[txn("a", "2025-12-31", "-1.00", false), txn("b", "2026-01-01", "-2.00", false)],
            true,
        )
        .unwrap();
        let dir = v.root().join("finance/transactions/test-checking");
        assert!(dir.join("2025.jsonl").exists());
        assert!(dir.join("2026.jsonl").exists());
        assert_eq!(v.finance_transactions(None, 100).unwrap().len(), 2);
    }

    #[test]
    fn balance_snapshots_upsert_by_day() {
        let v = temp_vault("balances");
        let snap = |date: &str, balance: &str| BalanceSnapshot {
            date: date.into(),
            ts: format!("{date}T08:00:00-07:00"),
            balance: balance.into(),
            available: None,
            currency: "USD".into(),
            as_of: None,
        };
        v.record_finance_balance("test-checking", &snap("2026-06-10", "100.00")).unwrap();
        v.record_finance_balance("test-checking", &snap("2026-06-11", "90.00")).unwrap();
        v.record_finance_balance("test-checking", &snap("2026-06-11", "85.00")).unwrap();
        let rows: Vec<BalanceSnapshot> =
            read_jsonl(&v.root().join("finance/balances/test-checking.jsonl")).unwrap();
        assert_eq!(rows.len(), 2, "same-day snapshot replaced, not appended");
        assert_eq!(rows[1].balance, "85.00");
    }

    #[test]
    fn rejects_sketchy_account_ids() {
        let v = temp_vault("ids");
        assert!(v.finance_transactions(Some("../escape"), 10).is_err());
        assert!(v.upsert_finance_transactions("Bad Name", &[], true).is_err());
    }

    #[test]
    fn apply_account_set_lands_everything() {
        let v = temp_vault("apply");
        let set: simplefin::SfinAccountSet = serde_json::from_str(simplefin::SAMPLE_RESPONSE).unwrap();
        let now = Local::now();
        let stats = v.apply_account_set(&set, &now).unwrap();
        assert_eq!(stats.accounts, 1);
        assert_eq!(stats.new_transactions, 2);

        let overview = v.finance_overview().unwrap();
        assert_eq!(overview.accounts.len(), 1);
        let acct = &overview.accounts[0];
        assert_eq!(acct.balance.as_deref(), Some("210.13"), "balance stays a string");

        // Re-applying the same payload is a no-op.
        let stats = v.apply_account_set(&set, &now).unwrap();
        assert_eq!(stats.new_transactions, 0);
        assert_eq!(stats.updated_transactions, 0);
    }
}

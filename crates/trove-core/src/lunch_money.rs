//! Lunch Money personal finance app — transactions, accounts, and balance
//! data via the official Lunch Money REST API (`dev.lunchmoney.app/v1`).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/lunch-money.md.
//!
//! A **Periodic** cloud pull. The user pastes a personal access token from the
//! Lunch Money Developers page (`my.lunchmoney.app/developers`). Auth is a
//! Bearer header; no Trove-held app credential required.
//!
//! ## Two layers
//!
//! - **Raw:** `finance/lunch-money/raw/YYYY-MM.jsonl` — full-fidelity API
//!   transaction objects, partitioned by `date` month, appended and deduped by
//!   id. Source-specific fields (tags, recurring linkage, category tree, Plaid
//!   metadata, group info) are all present verbatim.
//! - **Contract:** canonical `finance/` ledger using the existing
//!   [`crate::finance::Transaction`] type + `resolve_finance_account` +
//!   `upsert_finance_transactions` + `record_finance_balance` — the same
//!   machinery SimpleFIN uses.  Account identified by the Lunch Money account
//!   id (asset id or plaid_account_id); balance written from `/assets` and
//!   `/plaid_accounts`.  Category, tags, recurring metadata, and all other
//!   source-native fields ride in `extra` — nothing dropped.
//!
//! ## API — Lunch Money v1
//!
//! Base: `https://dev.lunchmoney.app/v1`. All calls send
//! `Authorization: Bearer <token>`.
//!
//! Endpoints used:
//! - `GET /transactions?start_date=YYYY-MM-DD&end_date=YYYY-MM-DD&offset=N&limit=500&debit_as_negative=true`
//!   → `{ transactions: [...], has_more: bool }`. `debit_as_negative=true`
//!   makes expenses negative and credits/income positive — the vault-wide
//!   outflow-negative convention. Transaction shape confirmed at lunchmoney.dev:
//!   `id` (number), `date` (YYYY-MM-DD), `amount` (signed decimal string),
//!   `currency` (3-letter code), `payee`, `notes`, `category_id`,
//!   `category_name`, `category_group_id`, `category_group_name`, `is_income`,
//!   `status` (cleared|uncleared|pending|null), `is_pending`, `recurring_id`,
//!   `recurring_payee`, `recurring_cadence`, `asset_id`, `asset_name`,
//!   `asset_institution_name`, `plaid_account_id`, `plaid_account_name`,
//!   `plaid_account_display_name`, `institution_name`, `tags: [{id,name}]`,
//!   `external_id`, `source`, `display_name`, `original_name`, `to_base`.
//! - `GET /assets` → `{ assets: [{id, type_name, subtype_name, name,
//!   display_name, balance, balance_as_of, currency, institution_name,
//!   exclude_transactions, created_at}] }`.
//! - `GET /plaid_accounts` → `{ plaid_accounts: [{id, name, display_name,
//!   type, subtype, mask, institution_name, status, balance, currency,
//!   balance_last_update}] }`.
//!
//! ## Cursor
//!
//! `.trove/lunch-money-sync.json` (non-secret, rebuildable) stores `last_date`
//! (YYYY-MM-DD) — the `end_date` of the last fully-drained window. Each sync
//! re-requests a 7-day overlap window (`start_date = last_date − 7d`) so late
//! postings and pending→posted changes are caught. A first sync backfills 2
//! years. The whole window is drained (paged until `has_more` is false or a
//! page is shorter than the limit) before the cursor advances. Id-based dedupe
//! in both layers makes re-draining idempotent.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::finance::{BalanceSnapshot, Transaction};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Raw layer — full-fidelity API transaction objects.
const RAW_DIR: &str = "finance/lunch-money/raw";

/// Non-secret rebuildable cursor (NOT under `.trove/sync/` — that is 0600 secrets).
const SYNC_FILE: &str = ".trove/lunch-money-sync.json";

/// Service id under `.trove/sync/` for the API token.
const SERVICE: &str = "lunch-money";

const API_BASE: &str = "https://dev.lunchmoney.app/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_LIMIT: usize = 500;

/// Seconds between syncs — every 6 hours keeps data fresh without hammering
/// the API (Lunch Money syncs Plaid accounts roughly hourly internally).
const LUNCH_MONEY_SYNC_SECS: u64 = 6 * 3600;

/// Days of transaction history to backfill on the first sync (2 years).
const FIRST_SYNC_LOOKBACK_DAYS: i64 = 365 * 2;

/// Overlap window on incremental syncs to catch late postings and
/// pending→posted status changes.
const RESYNC_OVERLAP_DAYS: i64 = 7;

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The `end_date` of the last successfully completed pull (YYYY-MM-DD).
    /// The next pull's `start_date` is `last_date − RESYNC_OVERLAP_DAYS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_date: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_lunch_money_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_lunch_money_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable trait so tests run fully offline.

trait LunchMoneyApi {
    /// Paginated transaction fetch. Returns (rows, has_more).
    fn transactions(
        &self,
        start_date: &str,
        end_date: &str,
        offset: usize,
    ) -> Result<(Vec<Value>, bool)>;

    fn assets(&self) -> Result<Vec<Value>>;
    fn plaid_accounts(&self) -> Result<Vec<Value>>;
}

/// Thin HTTP client; token injected at construction.
struct LunchMoneyClient {
    token: String,
}

impl LunchMoneyClient {
    fn new(token: String) -> Self {
        LunchMoneyClient { token }
    }

    fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        let mut url = format!("{API_BASE}{path}");
        if !params.is_empty() {
            url.push('?');
            for (i, (k, v)) in params.iter().enumerate() {
                if i > 0 {
                    url.push('&');
                }
                url.push_str(&format!("{k}={v}"));
            }
        }
        let resp = ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HTTP_TIMEOUT)
            .call()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if status == 401 {
            bail!("Lunch Money token rejected (401) — reconnect with a valid token from my.lunchmoney.app/developers");
        }
        if status == 429 {
            bail!("Lunch Money rate limited (429) — will retry on the next sync");
        }
        if status >= 400 {
            bail!("Lunch Money API error {status} at {url}");
        }
        resp.into_json().with_context(|| format!("parsing response from {url}"))
    }
}

impl LunchMoneyApi for LunchMoneyClient {
    fn transactions(
        &self,
        start_date: &str,
        end_date: &str,
        offset: usize,
    ) -> Result<(Vec<Value>, bool)> {
        let limit_s = PAGE_LIMIT.to_string();
        let offset_s = offset.to_string();
        let v = self.get(
            "/transactions",
            &[
                ("start_date", start_date),
                ("end_date", end_date),
                ("offset", &offset_s),
                ("limit", &limit_s),
                ("debit_as_negative", "true"),
            ],
        )?;
        let txs =
            v.get("transactions").and_then(Value::as_array).cloned().unwrap_or_default();
        let has_more = v.get("has_more").and_then(Value::as_bool).unwrap_or(false);
        Ok((txs, has_more))
    }

    fn assets(&self) -> Result<Vec<Value>> {
        let v = self.get("/assets", &[])?;
        Ok(v.get("assets").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    fn plaid_accounts(&self) -> Result<Vec<Value>> {
        let v = self.get("/plaid_accounts", &[])?;
        Ok(v.get("plaid_accounts").and_then(Value::as_array).cloned().unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Pure mapping helpers.

/// Date window endpoints given the sync state and today's date.
fn window(state: &SyncState, today: NaiveDate) -> (NaiveDate, NaiveDate) {
    let start = if let Some(ref last) = state.last_date {
        NaiveDate::parse_from_str(last, "%Y-%m-%d")
            .map(|d| d - chrono::Duration::days(RESYNC_OVERLAP_DAYS))
            .unwrap_or_else(|_| today - chrono::Duration::days(FIRST_SYNC_LOOKBACK_DAYS))
    } else {
        today - chrono::Duration::days(FIRST_SYNC_LOOKBACK_DAYS)
    };
    (start, today)
}

/// Lunch Money `date` (YYYY-MM-DD) → RFC3339 local midnight.
fn date_to_ts(date: &str) -> Option<String> {
    use chrono::TimeZone;
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| {
            Local
                .from_local_datetime(&dt)
                .single()
                .map(|ld| ld.to_rfc3339())
                .unwrap_or_else(|| format!("{date}T00:00:00+00:00"))
        })
}

/// Returns `(source_id, account_name, institution)` for the account a
/// transaction belongs to. Assets and Plaid accounts are separate namespaces.
/// Returns `None` when no account is linked (can happen for manually created
/// transactions not tied to any account).
fn account_source_id<'a>(tx: &'a Value) -> Option<(String, &'a str, &'a str)> {
    // Prefer asset_id (manual assets) over plaid_account_id.
    if let Some(asset_id) = tx.get("asset_id").and_then(Value::as_u64) {
        let name =
            tx.get("asset_name").and_then(Value::as_str).unwrap_or("Lunch Money Asset");
        let institution =
            tx.get("asset_institution_name").and_then(Value::as_str).unwrap_or("");
        return Some((format!("asset-{asset_id}"), name, institution));
    }
    if let Some(plaid_id) = tx.get("plaid_account_id").and_then(Value::as_u64) {
        let name = tx
            .get("plaid_account_display_name")
            .or_else(|| tx.get("plaid_account_name"))
            .and_then(Value::as_str)
            .unwrap_or("Lunch Money Plaid");
        let institution = tx.get("institution_name").and_then(Value::as_str).unwrap_or("");
        return Some((format!("plaid-{plaid_id}"), name, institution));
    }
    None
}

/// Map one Lunch Money transaction object → a canonical [`Transaction`].
/// Returns `None` when the object lacks a usable `id` or `date`.
fn map_transaction(tx: &Value, vault_account: &str) -> Option<Transaction> {
    let id = tx.get("id").and_then(Value::as_u64)?.to_string();
    let date = tx.get("date").and_then(Value::as_str).filter(|s| !s.is_empty())?;

    // `amount` is a decimal string in Lunch Money. We request
    // `debit_as_negative=true` so expenses arrive as negative values and
    // credits/income arrive as positive — matching the vault-wide
    // outflow-negative convention used by SimpleFIN, CSV import, and every
    // other finance source. e.g. a $42.50 grocery expense arrives as "-42.50".
    let amount = tx.get("amount").and_then(Value::as_str).unwrap_or("0").to_string();
    let currency =
        tx.get("currency").and_then(Value::as_str).unwrap_or("USD").to_uppercase();

    // display_name is the enriched name Lunch Money shows the user; prefer it
    // over raw payee for the description field.
    let description = tx
        .get("display_name")
        .or_else(|| tx.get("payee"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let payee = tx
        .get("payee")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let category = tx
        .get("category_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let is_pending = tx.get("is_pending").and_then(Value::as_bool).unwrap_or(false);
    let status_str = tx.get("status").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let pending = is_pending || status_str == "pending";

    // Extra: every source-native field not captured by the canonical schema.
    let mut extra = Map::new();
    for key in &[
        "category_id",
        "category_group_id",
        "category_group_name",
        "is_income",
        "exclude_from_budget",
        "exclude_from_totals",
        "status",
        "notes",
        "original_name",
        "recurring_id",
        "recurring_payee",
        "recurring_description",
        "recurring_cadence",
        "recurring_type",
        "recurring_amount",
        "recurring_currency",
        "parent_id",
        "has_children",
        "group_id",
        "is_group",
        "asset_id",
        "plaid_account_id",
        "plaid_account_mask",
        "external_id",
        "source",
        "display_notes",
        "to_base",
    ] {
        if let Some(v) = tx.get(*key) {
            if !v.is_null() {
                extra.insert(key.to_string(), v.clone());
            }
        }
    }
    // Tags array — omit when empty.
    if let Some(tags) = tx.get("tags").and_then(Value::as_array) {
        if !tags.is_empty() {
            extra.insert("tags".into(), Value::Array(tags.clone()));
        }
    }

    Some(Transaction {
        id,
        account: vault_account.to_string(),
        posted: date.to_string(),
        transacted: None,
        amount,
        currency,
        description,
        payee,
        category,
        pending,
        source: "lunch-money".to_string(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Raw layer.

/// A raw row: carries `ts` for month-partition routing (not serialized itself),
/// then flattens the API object verbatim.
#[derive(Serialize)]
struct RawLine {
    /// RFC3339 local date of the transaction — drives partition routing only;
    /// not written to the JSONL line (the API object already has `date`).
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Append new raw rows, deduped by numeric `id`. All existing ids across all
/// partitions are loaded first; only unseen ids are appended.
fn append_raw(vault: &Vault, rows: &[RawLine]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen: HashSet<u64> = HashSet::new();
    for key in stream.partitions().unwrap_or_default() {
        for v in stream.read::<Value>(&key)? {
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                seen.insert(id);
            }
        }
    }
    let new_rows: Vec<&RawLine> = rows
        .iter()
        .filter(|r| {
            r.value
                .get("id")
                .and_then(Value::as_u64)
                .is_some_and(|id| seen.insert(id))
        })
        .collect();
    // `stream.append` requires a concrete slice, not a slice of refs.
    // Re-collect into owned to satisfy the bound.
    let owned: Vec<&RawLine> = new_rows;
    stream.append(owned.as_slice(), |r| r.ts.as_str())
}

// ---------------------------------------------------------------------------
// The pull (testable through an injected API).

fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|s| !s.trim().is_empty())
        .context("Lunch Money is not connected — add your access token in the Integrations tab")?;
    let client = LunchMoneyClient::new(token);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl LunchMoneyApi) -> Result<PullOutcome> {
    let state = vault.read_lunch_money_sync();
    let today = Local::now().date_naive();
    let (start, end) = window(&state, today);
    let start_s = start.format("%Y-%m-%d").to_string();
    let end_s = end.format("%Y-%m-%d").to_string();

    // Drain all transactions in the window before advancing the cursor.
    let mut all_txs: Vec<Value> = Vec::new();
    let mut offset = 0;
    loop {
        let (page, has_more) = api.transactions(&start_s, &end_s, offset)?;
        let page_len = page.len();
        all_txs.extend(page);
        if !has_more || page_len < PAGE_LIMIT {
            break;
        }
        offset += page_len;
    }

    // Pull accounts for balance snapshots (soft-fail: a 401 on the transaction
    // pull already bailed; a 429/network blip here is acceptable).
    let assets = api.assets().unwrap_or_default();
    let plaid_accounts = api.plaid_accounts().unwrap_or_default();

    // --- Raw layer (unconditional, full fidelity) ---
    let raw_rows: Vec<RawLine> = all_txs
        .iter()
        .filter_map(|tx| {
            let date = tx.get("date").and_then(Value::as_str)?;
            let ts = date_to_ts(date)?;
            Partition::Month.key(&ts)?; // must be partitionable
            Some(RawLine { ts, value: tx.clone() })
        })
        .collect();
    append_raw(vault, &raw_rows)?;

    // --- Contract layer ---
    let mut accounts = vault.load_finance_accounts()?;
    let mut new_txns: usize = 0;

    // Group by account (vault_id).
    let mut by_account: BTreeMap<String, Vec<Transaction>> = BTreeMap::new();
    for tx in &all_txs {
        let Some((source_id, acct_name, institution)) = account_source_id(tx) else {
            // Transactions not linked to any account are still in the raw
            // layer; they just don't appear in the ledger.
            continue;
        };
        let currency =
            tx.get("currency").and_then(Value::as_str).unwrap_or("USD").to_uppercase();
        let vault_id = vault.resolve_finance_account(
            &mut accounts,
            "lunch-money",
            &source_id,
            acct_name,
            institution,
            &currency,
        );
        if let Some(t) = map_transaction(tx, &vault_id) {
            by_account.entry(vault_id).or_default().push(t);
        }
    }
    for (acct_id, txns) in &by_account {
        // We do NOT replace_pending: the overlap window catches pending→posted
        // transitions; we merge, never purge an unseen account's pending set.
        let (new, _) = vault.upsert_finance_transactions(acct_id, txns, false)?;
        new_txns += new;
    }

    // --- Balance snapshots ---
    let now_ts = Local::now().to_rfc3339();
    let today_str = today.format("%Y-%m-%d").to_string();

    for asset in &assets {
        let Some(asset_id) = asset.get("id").and_then(Value::as_u64) else { continue };
        let source_id = format!("asset-{asset_id}");
        let name = asset.get("name").and_then(Value::as_str).unwrap_or("Asset");
        let institution =
            asset.get("institution_name").and_then(Value::as_str).unwrap_or("");
        let currency =
            asset.get("currency").and_then(Value::as_str).unwrap_or("USD").to_uppercase();
        let Some(balance) = asset.get("balance").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        let vault_id = vault.resolve_finance_account(
            &mut accounts,
            "lunch-money",
            &source_id,
            name,
            institution,
            &currency,
        );
        let as_of = asset.get("balance_as_of").and_then(Value::as_str).map(str::to_string);
        vault.record_finance_balance(
            &vault_id,
            &BalanceSnapshot {
                date: today_str.clone(),
                ts: now_ts.clone(),
                balance: balance.to_string(),
                available: None,
                currency,
                as_of,
            },
        )?;
    }

    for pa in &plaid_accounts {
        let Some(plaid_id) = pa.get("id").and_then(Value::as_u64) else { continue };
        let source_id = format!("plaid-{plaid_id}");
        let name = pa
            .get("display_name")
            .or_else(|| pa.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("Plaid Account");
        let institution = pa.get("institution_name").and_then(Value::as_str).unwrap_or("");
        let currency =
            pa.get("currency").and_then(Value::as_str).unwrap_or("USD").to_uppercase();
        let Some(balance) = pa.get("balance").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        let vault_id = vault.resolve_finance_account(
            &mut accounts,
            "lunch-money",
            &source_id,
            name,
            institution,
            &currency,
        );
        let as_of =
            pa.get("balance_last_update").and_then(Value::as_str).map(str::to_string);
        vault.record_finance_balance(
            &vault_id,
            &BalanceSnapshot {
                date: today_str.clone(),
                ts: now_ts.clone(),
                balance: balance.to_string(),
                available: None,
                currency,
                as_of,
            },
        )?;
    }

    vault.save_finance_accounts(&accounts)?;

    // Advance cursor only after the full drain + writes succeed.
    vault.write_lunch_money_sync(&SyncState {
        last_date: Some(today_str),
        updated: Some(now_ts),
    })?;

    Ok(PullOutcome {
        headline: if new_txns == 0 {
            "Lunch Money is up to date — no new transactions".into()
        } else {
            format!("Lunch Money synced — {new_txns} new transactions")
        },
        counts: BTreeMap::from([("transactions", new_txns as u64)]),
    })
}

// ---------------------------------------------------------------------------
// Connection — TokenPaste (personal access token from Developers page).

/// Verify the token with a cheap `/me` call; store if valid.
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let token = pasted.trim();
    if token.is_empty() {
        bail!(
            "paste your Lunch Money access token — find it at my.lunchmoney.app/developers"
        );
    }
    let client = LunchMoneyClient::new(token.to_string());
    let me = client
        .get("/me", &[])
        .context("could not verify Lunch Money token — check it at my.lunchmoney.app/developers")?;
    // A valid /me response includes at minimum `user_name` or `user_id`.
    if me.get("user_name").is_none() && me.get("user_id").is_none() {
        bail!(
            "Lunch Money did not recognize the token — double-check it at my.lunchmoney.app/developers"
        );
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Lunch Money account".into(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts: accts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "lunch-money",
    display_name: "Lunch Money",
    methods: &[ConnectMethod::TokenPaste {
        label: "Access token",
        help: "In Lunch Money go to Settings → Developers (my.lunchmoney.app/developers) and \
               click \"Request for an Access Token\". Copy the token and paste it here. \
               Tokens do not expire unless you revoke them.",
        placeholder: "lm_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["lunch-money"],
    setup: &[
        "Open my.lunchmoney.app/developers and click \"Request for an Access Token\".",
        "Copy the token and paste it into the field below.",
    ],
};

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_lunch_money_sync().last_date
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("transactions").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("Lunch Money synced — {n} new transactions")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Lunch Money sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "lunch-money",
        name: "Lunch Money",
        kind: IntegrationKind::CloudSync,
        // 🔒 financial detail — ships opt-in with explicit acknowledgement.
        default_on: false,
        description: "Syncs your Lunch Money transactions, categories, tags, recurring items, \
                      and account balances using your personal access token. No Trove-held app \
                      credential required — the token lives only in your vault's secure store.",
        domain: "finance",
        vault_path: "finance/lunch-money/",
        toggleable: true,
        setup: &[
            "Open my.lunchmoney.app/developers and request an access token.",
            "Paste it into the connection card and click Connect.",
            "The first sync backfills up to 2 years of transaction history; \
             later syncs are incremental with a 7-day overlap to catch late postings.",
        ],
        caveats: "Lunch Money amounts follow the vault-wide outflow-negative convention \
                  (expenses are negative, credits/income are positive) because the pull \
                  requests `debit_as_negative=true`. Transactions present in both Lunch Money \
                  and SimpleFIN are kept in separate account folders (keyed by source id); \
                  read-time deduplication on amount + date links them at the query layer \
                  rather than at write time.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(LUNCH_MONEY_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("lunch-money"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-lm-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixtures — synthesized from the confirmed API schema at lunchmoney.dev.

    /// A cleared transaction linked to a Plaid account.
    /// Amount is negative (expense) — as returned by `debit_as_negative=true`.
    fn tx_plaid_cleared() -> Value {
        json!({
            "id": 12345,
            "date": "2026-05-15",
            "amount": "-42.50",
            "currency": "usd",
            "payee": "Whole Foods",
            "display_name": "Whole Foods Market",
            "original_name": "WHOLEFDS MKT 123",
            "notes": "groceries",
            "category_id": 7,
            "category_name": "Groceries",
            "category_group_id": 2,
            "category_group_name": "Food",
            "is_income": false,
            "exclude_from_budget": false,
            "exclude_from_totals": false,
            "status": "cleared",
            "is_pending": false,
            "recurring_id": null,
            "recurring_payee": null,
            "recurring_cadence": null,
            "asset_id": null,
            "asset_name": null,
            "asset_institution_name": null,
            "plaid_account_id": 88,
            "plaid_account_name": "Chase Checking",
            "plaid_account_display_name": "Chase Checking \u{2022}\u{2022}3210",
            "institution_name": "Chase",
            "tags": [{"id": 1, "name": "essentials"}],
            "external_id": null,
            "source": "plaid",
            "to_base": 42.50
        })
    }

    /// A pending transaction linked to a manual asset, with recurring info.
    /// Amount is negative (expense) — as returned by `debit_as_negative=true`.
    fn tx_asset_pending() -> Value {
        json!({
            "id": 67890,
            "date": "2026-05-20",
            "amount": "-9.99",
            "currency": "usd",
            "payee": "Netflix",
            "display_name": "Netflix",
            "original_name": "NETFLIX.COM",
            "notes": null,
            "category_id": 5,
            "category_name": "Entertainment",
            "category_group_id": 3,
            "category_group_name": "Subscriptions",
            "is_income": false,
            "exclude_from_budget": false,
            "exclude_from_totals": false,
            "status": "pending",
            "is_pending": true,
            "recurring_id": 101,
            "recurring_payee": "Netflix",
            "recurring_cadence": "monthly",
            "recurring_type": "cleared",
            "recurring_amount": "9.99",
            "recurring_currency": 1,
            "asset_id": 72,
            "asset_name": "Chase Checking (Manual)",
            "asset_institution_name": "Chase",
            "asset_display_name": null,
            "plaid_account_id": null,
            "plaid_account_name": null,
            "institution_name": null,
            "tags": [],
            "external_id": null,
            "source": "recurring",
            "to_base": 9.99
        })
    }

    fn asset_obj() -> Value {
        json!({
            "id": 72,
            "type_name": "checking",
            "subtype_name": null,
            "name": "Chase Checking (Manual)",
            "display_name": null,
            "balance": "1201.01",
            "to_base": 1201.01,
            "balance_as_of": "2026-05-20T12:00:00.000Z",
            "closed_on": null,
            "currency": "usd",
            "institution_name": "Chase",
            "exclude_transactions": false,
            "created_at": "2024-01-10T08:00:00.000Z"
        })
    }

    fn plaid_acct_obj() -> Value {
        json!({
            "id": 88,
            "date_linked": "2024-01-10T08:00:00.000Z",
            "name": "Chase Checking",
            "display_name": "Chase Checking \u{00b7}\u{00b7}3210",
            "type": "depository",
            "subtype": "checking",
            "mask": "3210",
            "institution_name": "Chase",
            "status": "active",
            "balance": "850.50",
            "currency": "usd",
            "balance_last_update": "2026-05-20T10:30:00.000Z",
            "limit": null
        })
    }

    // ---------------------------------------------------------------------------
    // Window computation.

    #[test]
    fn window_first_sync_backtracks_two_years() {
        let state = SyncState::default();
        let today = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let (start, end) = window(&state, today);
        assert_eq!(end, today);
        assert_eq!(start, today - chrono::Duration::days(365 * 2));
    }

    #[test]
    fn window_incremental_overlaps_seven_days() {
        let state = SyncState { last_date: Some("2026-06-08".into()), updated: None };
        let today = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let (start, end) = window(&state, today);
        assert_eq!(end, today);
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap());
    }

    #[test]
    fn window_corrupted_last_date_falls_back_to_full_backfill() {
        let state = SyncState { last_date: Some("not-a-date".into()), updated: None };
        let today = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let (start, _) = window(&state, today);
        assert_eq!(start, today - chrono::Duration::days(365 * 2));
    }

    // ---------------------------------------------------------------------------
    // Pure mapping.

    #[test]
    fn maps_plaid_transaction_to_canonical_form() {
        let tx = tx_plaid_cleared();
        let t = map_transaction(&tx, "chase-checking-3210").unwrap();
        assert_eq!(t.id, "12345", "numeric id stringified");
        assert_eq!(t.account, "chase-checking-3210");
        assert_eq!(t.posted, "2026-05-15");
        assert_eq!(t.amount, "-42.50", "expense is outflow-negative");
        assert_eq!(t.currency, "USD", "currency uppercased");
        assert_eq!(t.description, "Whole Foods Market", "display_name preferred");
        assert_eq!(t.payee.as_deref(), Some("Whole Foods"));
        assert_eq!(t.category.as_deref(), Some("Groceries"));
        assert!(!t.pending, "cleared is not pending");
        assert_eq!(t.source, "lunch-money");
        // Extra carries source-native metadata.
        assert!(t.extra.get("tags").is_some(), "tags in extra");
        assert_eq!(t.extra.get("category_id"), Some(&json!(7)));
        assert_eq!(t.extra.get("category_group_name"), Some(&json!("Food")));
        // Null-valued fields must be absent from extra.
        assert!(t.extra.get("recurring_id").is_none(), "null fields omitted from extra");
    }

    #[test]
    fn maps_pending_asset_transaction() {
        let tx = tx_asset_pending();
        let t = map_transaction(&tx, "chase-checking-manual").unwrap();
        assert_eq!(t.id, "67890");
        assert_eq!(t.amount, "-9.99", "expense is outflow-negative");
        assert!(t.pending, "is_pending=true → pending");
        assert_eq!(t.category.as_deref(), Some("Entertainment"));
        // Recurring metadata in extra.
        assert_eq!(t.extra.get("recurring_id"), Some(&json!(101)));
        assert_eq!(t.extra.get("recurring_cadence"), Some(&json!("monthly")));
        // Empty tags: not written to extra.
        assert!(t.extra.get("tags").is_none(), "empty tags omitted");
    }

    #[test]
    fn amount_sign_convention_expense_is_negative() {
        // With debit_as_negative=true the API returns expenses as negative
        // strings. map_transaction must preserve that sign so the vault-wide
        // outflow-negative convention holds.
        let tx = json!({
            "id": 1,
            "date": "2026-05-15",
            "amount": "-42.50",
            "currency": "usd",
            "is_income": false
        });
        let t = map_transaction(&tx, "acct").unwrap();
        assert!(
            t.amount.starts_with('-'),
            "expense amount must be negative (outflow-negative), got: {}",
            t.amount
        );
        assert_eq!(t.amount, "-42.50");
    }

    #[test]
    fn amount_sign_convention_income_is_positive() {
        // With debit_as_negative=true the API returns income/credits as
        // positive strings. map_transaction must preserve the positive sign.
        let tx = json!({
            "id": 2,
            "date": "2026-05-15",
            "amount": "1500.00",
            "currency": "usd",
            "is_income": true
        });
        let t = map_transaction(&tx, "acct").unwrap();
        assert!(
            !t.amount.starts_with('-'),
            "income/credit amount must be positive, got: {}",
            t.amount
        );
        assert_eq!(t.amount, "1500.00");
    }

    #[test]
    fn map_transaction_returns_none_without_id() {
        let tx = json!({"date": "2026-05-15", "amount": "-5.00", "currency": "usd"});
        assert!(map_transaction(&tx, "acct").is_none());
    }

    #[test]
    fn map_transaction_returns_none_without_date() {
        let tx = json!({"id": 1, "amount": "-5.00", "currency": "usd"});
        assert!(map_transaction(&tx, "acct").is_none());
    }

    // ---------------------------------------------------------------------------
    // date_to_ts.

    #[test]
    fn date_to_ts_produces_partitionable_rfc3339() {
        let ts = date_to_ts("2026-05-15").unwrap();
        assert!(Partition::Month.key(&ts).is_some(), "month-partitionable: {ts}");
        assert!(ts.starts_with("2026-05-15"), "starts with date: {ts}");
    }

    #[test]
    fn date_to_ts_rejects_invalid_input() {
        assert!(date_to_ts("not-a-date").is_none());
        assert!(date_to_ts("").is_none());
    }

    // ---------------------------------------------------------------------------
    // account_source_id.

    #[test]
    fn account_source_id_prefers_asset_id_over_plaid() {
        // When both are present, asset_id takes precedence.
        let tx = tx_asset_pending();
        let (src_id, name, institution) = account_source_id(&tx).unwrap();
        assert_eq!(src_id, "asset-72");
        assert_eq!(name, "Chase Checking (Manual)");
        assert_eq!(institution, "Chase");
    }

    #[test]
    fn account_source_id_falls_back_to_plaid() {
        let tx = tx_plaid_cleared();
        let (src_id, name, institution) = account_source_id(&tx).unwrap();
        assert_eq!(src_id, "plaid-88");
        // display_name preferred over name.
        assert!(name.contains("3210"), "display_name used: {name}");
        assert_eq!(institution, "Chase");
    }

    #[test]
    fn account_source_id_none_when_no_account() {
        let tx = json!({"id": 1, "date": "2026-05-15", "amount": "5.00"});
        assert!(account_source_id(&tx).is_none());
    }

    // ---------------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        /// Pages (txs, has_more), consumed in order.
        pages: std::cell::RefCell<std::collections::VecDeque<(Vec<Value>, bool)>>,
        assets: Vec<Value>,
        plaid_accounts: Vec<Value>,
    }

    impl MockApi {
        fn new(
            pages: Vec<(Vec<Value>, bool)>,
            assets: Vec<Value>,
            plaid: Vec<Value>,
        ) -> Self {
            MockApi {
                pages: std::cell::RefCell::new(pages.into()),
                assets,
                plaid_accounts: plaid,
            }
        }
    }

    impl LunchMoneyApi for MockApi {
        fn transactions(
            &self,
            _start: &str,
            _end: &str,
            _offset: usize,
        ) -> Result<(Vec<Value>, bool)> {
            Ok(self.pages.borrow_mut().pop_front().unwrap_or((Vec::new(), false)))
        }
        fn assets(&self) -> Result<Vec<Value>> {
            Ok(self.assets.clone())
        }
        fn plaid_accounts(&self) -> Result<Vec<Value>> {
            Ok(self.plaid_accounts.clone())
        }
    }

    #[test]
    fn full_pull_writes_raw_and_contract_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(
            vec![(vec![tx_plaid_cleared(), tx_asset_pending()], false)],
            vec![asset_obj()],
            vec![plaid_acct_obj()],
        );
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("transactions"), Some(&2));

        // Raw layer has 2 rows.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0;
        for key in raw.partitions().unwrap() {
            raw_count += raw.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(raw_count, 2, "raw layer has both transactions");

        // Contract layer has at least one row.
        let txns = v.finance_transactions(None, 100).unwrap();
        assert!(!txns.is_empty(), "contract has transactions");
        // Whole Foods transaction is correct.
        let wf = txns.iter().find(|t| t.id == "12345").unwrap();
        assert_eq!(wf.amount, "-42.50", "expense stored outflow-negative");
        assert_eq!(wf.currency, "USD");
        assert_eq!(wf.category.as_deref(), Some("Groceries"));
        assert_eq!(wf.source, "lunch-money");

        // Cursor advanced to today.
        let state = v.read_lunch_money_sync();
        assert!(state.last_date.is_some(), "cursor set after successful pull");
        assert!(state.updated.is_some());

        // Account registry has entries.
        let overview = v.finance_overview().unwrap();
        assert!(!overview.accounts.is_empty(), "accounts registered");
    }

    #[test]
    fn re_pull_with_same_data_is_idempotent() {
        let v = temp_vault("idempotent");
        let api1 = MockApi::new(
            vec![(vec![tx_plaid_cleared()], false)],
            vec![],
            vec![],
        );
        let out1 = pull_with(&v, &api1).unwrap();
        assert_eq!(out1.counts.get("transactions"), Some(&1));

        let api2 = MockApi::new(
            vec![(vec![tx_plaid_cleared()], false)],
            vec![],
            vec![],
        );
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("transactions"), Some(&0), "deduped on re-run");

        // Raw layer still has exactly 1 row.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0;
        for key in raw.partitions().unwrap() {
            raw_count += raw.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(raw_count, 1, "raw deduped too");
    }

    #[test]
    fn unlinked_transactions_skip_contract_but_land_in_raw() {
        // A transaction with no asset_id and no plaid_account_id should
        // appear in raw but not in the contract ledger.
        let v = temp_vault("unlinked");
        let tx = json!({
            "id": 99999,
            "date": "2026-05-15",
            "amount": "5.00",
            "currency": "usd",
            "payee": "Cash",
            "display_name": "Cash",
            "status": "cleared",
            "is_pending": false,
            "tags": []
        });
        let api = MockApi::new(vec![(vec![tx], false)], vec![], vec![]);
        let out = pull_with(&v, &api).unwrap();
        // No contract rows (no account to file under).
        assert_eq!(out.counts.get("transactions"), Some(&0));

        // Raw row still present.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0;
        for key in raw.partitions().unwrap() {
            raw_count += raw.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(raw_count, 1, "unlinked tx appears in raw");
    }

    #[test]
    fn pull_requires_connection() {
        let v = temp_vault("noconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn connection_stores_token_and_status_shows_connected() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "lm_test".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].key, "lunch-money");

        def_disconnect(&v, "lunch-money").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn sync_state_roundtrips_and_empty_deserializes() {
        let state = SyncState {
            last_date: Some("2026-06-15".into()),
            updated: Some("2026-06-15T12:00:00-07:00".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: SyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_date, state.last_date);
        assert_eq!(back.updated, state.updated);

        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_date.is_none());
    }

    #[test]
    fn def_and_connection_are_wired_correctly() {
        assert_eq!(DEF.connection, Some("lunch-money"));
        assert_eq!(DEF.meta.id, "lunch-money");
        assert_eq!(CONNECTION.id, "lunch-money");
        assert!(CONNECTION.method("token-paste").is_some());
        assert!(DEF.pull.is_some());
        assert!(DEF.last_data.is_some());
        // toggleable=true so the user can switch it on/off.
        assert!(DEF.meta.toggleable);
        // default_on=false because financial data is privacy-sensitive.
        assert!(!DEF.meta.default_on);
    }
}

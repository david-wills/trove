//! YNAB (You Need A Budget) — transactions and budget data via the official
//! YNAB REST API (`api.ynab.com/v1`) with a personal access token.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/ynab.md
//!
//! A **Periodic** cloud pull. The user pastes a personal access token from
//! YNAB → Account Settings → Developer Settings. No Trove-held app credential.
//!
//! ## Two layers
//!
//! - **Raw:** `finance/ynab/raw/YYYY-MM.jsonl` — full-fidelity API transaction
//!   objects (amounts still in milliunits), partitioned by `date` month,
//!   appended and deduped by id.
//! - **Contract:** canonical `finance/` ledger using [`crate::finance::Transaction`]
//!   + `resolve_finance_account` + `upsert_finance_transactions` + `record_finance_balance`
//!   — the same machinery SimpleFIN and Lunch Money use. Amount converted from
//!   milliunits to a decimal string (outflow-negative). YNAB-specific fields
//!   (category, flag, import_id, cleared status, budget_id) ride in `extra`.
//!
//! ## API — YNAB v1
//!
//! Base: `https://api.ynab.com/v1`. All calls send
//! `Authorization: Bearer <token>`.
//!
//! Endpoints used:
//! - `GET /budgets` → `{ data: { budgets: [{id, name, last_modified_on, ...}],
//!   default_budget: BudgetSummary | null } }`.
//! - `GET /budgets/{budget_id}/transactions?last_knowledge_of_server={N}`
//!   → `{ data: { transactions: [...], server_knowledge: N } }`.
//!   When `last_knowledge_of_server` is omitted, the full history is returned.
//!   This is the canonical delta-sync cursor; store `server_knowledge` per budget.
//! - `GET /budgets/{budget_id}/accounts`
//!   → `{ data: { accounts: [{id, name, type, balance, ...}], server_knowledge: N } }`.
//!
//! Transaction field names (confirmed against open_api_spec.yaml):
//!   `id`, `date` (YYYY-MM-DD), `amount` (milliunits int64), `memo`, `cleared`
//!   (cleared|uncleared|reconciled), `approved`, `flag_color`, `account_id`,
//!   `account_name`, `payee_id`, `payee_name`, `category_id`, `category_name`,
//!   `transfer_account_id`, `transfer_transaction_id`, `import_id`,
//!   `import_payee_name`, `import_payee_name_original`, `debt_transaction_type`,
//!   `deleted`, `subtransactions`.
//!
//! ## Cursor
//!
//! `.trove/ynab-sync.json` (non-secret, rebuildable) stores per-budget
//! `server_knowledge` (an i64). On first sync the parameter is omitted (full
//! history). After each successful drain the new `server_knowledge` is persisted
//! and used on the next sync. Deleted transactions arrive with `deleted: true` —
//! the raw layer retains them (full fidelity). The contract ledger does NOT
//! remove a previously-written row when a deletion delta arrives: deleted
//! transactions are skipped before reaching the upsert path, so the contract
//! row persists until the ledger is rebuilt from raw. This is a known
//! limitation shared with other finance integrations (lunch_money.rs) — a
//! delete-primitive or read-time tombstone filter is deferred to Phase 4.
//!
//! ## Amount convention
//!
//! YNAB returns amounts in milliunits (1/1000 of the currency unit). Outflows
//! are negative integers (e.g. -42500 = -$42.50); inflows are positive.
//! The vault-wide convention is outflow-negative decimal strings, so we divide
//! by 1000 and format with two decimal places (e.g. "-42.50").

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
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
const RAW_DIR: &str = "finance/ynab/raw";

/// Non-secret rebuildable cursor (not under `.trove/sync/` — that is secrets).
const SYNC_FILE: &str = ".trove/ynab-sync.json";

/// Service id under `.trove/sync/` for the API token.
const SERVICE: &str = "ynab";

const API_BASE: &str = "https://api.ynab.com/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between syncs — every 4 hours. YNAB rate limit is ~200 req/hour;
/// a personal sync touches 1–5 budgets so we are far under the limit.
const YNAB_SYNC_SECS: u64 = 4 * 3600;

// ---------------------------------------------------------------------------
// Cursor.

/// Per-budget sync state: server_knowledge is the YNAB delta cursor.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct BudgetState {
    /// The `server_knowledge` returned by the last successful transaction fetch.
    /// Absent = never synced this budget (will fetch full history).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_knowledge: Option<i64>,
    /// RFC3339 timestamp of the last successful drain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

/// Outer sync state — keyed by budget id.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// budget_id → BudgetState
    #[serde(default)]
    budgets: BTreeMap<String, BudgetState>,
    /// Last time any budget was successfully synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_updated: Option<String>,
}

impl Vault {
    fn read_ynab_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ynab_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable trait so tests run fully offline.

trait YnabApi {
    /// `GET /budgets` — returns budget summaries.
    fn budgets(&self) -> Result<Vec<Value>>;
    /// `GET /budgets/{id}/transactions?last_knowledge_of_server={cursor}`
    /// Returns (transactions, server_knowledge).
    fn transactions(
        &self,
        budget_id: &str,
        last_knowledge: Option<i64>,
    ) -> Result<(Vec<Value>, i64)>;
    /// `GET /budgets/{id}/accounts` — returns (accounts, server_knowledge).
    fn accounts(&self, budget_id: &str) -> Result<(Vec<Value>, i64)>;
}

struct YnabClient {
    token: String,
}

impl YnabClient {
    fn new(token: String) -> Self {
        YnabClient { token }
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
            bail!(
                "YNAB token rejected (401) — reconnect with a valid token from \
                 app.ynab.com/settings/developer"
            );
        }
        if status == 429 {
            bail!("YNAB rate limited (429) — will retry on the next sync");
        }
        if status >= 400 {
            bail!("YNAB API error {status} at {url}");
        }
        resp.into_json().with_context(|| format!("parsing response from {url}"))
    }
}

impl YnabApi for YnabClient {
    fn budgets(&self) -> Result<Vec<Value>> {
        let v = self.get("/budgets", &[])?;
        Ok(v.pointer("/data/budgets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn transactions(
        &self,
        budget_id: &str,
        last_knowledge: Option<i64>,
    ) -> Result<(Vec<Value>, i64)> {
        let path = format!("/budgets/{budget_id}/transactions");
        let sk_s;
        let params: &[(&str, &str)] = if let Some(sk) = last_knowledge {
            sk_s = sk.to_string();
            &[("last_knowledge_of_server", sk_s.as_str())]
        } else {
            &[]
        };
        let v = self.get(&path, params)?;
        let txs = v
            .pointer("/data/transactions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let server_knowledge =
            v.pointer("/data/server_knowledge").and_then(Value::as_i64).unwrap_or(0);
        Ok((txs, server_knowledge))
    }

    fn accounts(&self, budget_id: &str) -> Result<(Vec<Value>, i64)> {
        let path = format!("/budgets/{budget_id}/accounts");
        let v = self.get(&path, &[])?;
        let accts = v
            .pointer("/data/accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let server_knowledge =
            v.pointer("/data/server_knowledge").and_then(Value::as_i64).unwrap_or(0);
        Ok((accts, server_knowledge))
    }
}

// ---------------------------------------------------------------------------
// Pure helpers.

/// Convert YNAB milliunits (i64) → decimal string with two decimal places,
/// preserving the outflow-negative convention. e.g. -42500 → "-42.50".
///
/// Rounds half-up to the nearest cent (hundredth), so 42505 → "42.51" and
/// 42504 → "42.50". The raw layer retains the exact milliunit value.
fn milliunits_to_decimal(milliunits: i64) -> String {
    let abs = milliunits.unsigned_abs();
    // Round half-up: add 5 (half of 10 milliunits per cent) before dividing.
    let cents = (abs + 5) / 10;
    let whole = cents / 100;
    let frac = cents % 100;
    // Guard: if the rounded value is zero, never emit a leading minus sign.
    if milliunits < 0 && cents > 0 {
        format!("-{whole}.{frac:02}")
    } else {
        format!("{whole}.{frac:02}")
    }
}

/// Map a YNAB transaction object → a canonical [`Transaction`].
/// Returns `None` when the object lacks `id` or `date`, or when
/// `deleted: true` (deleted rows are in raw but not in the contract ledger).
fn map_transaction(tx: &Value, vault_account: &str, budget_id: &str) -> Option<Transaction> {
    // Skip deleted transactions in the contract layer (they stay in raw).
    if tx.get("deleted").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }

    let id = tx.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let date = tx.get("date").and_then(Value::as_str).filter(|s| !s.is_empty())?;

    // Amount in milliunits → outflow-negative decimal string.
    let amount_millis = tx.get("amount").and_then(Value::as_i64).unwrap_or(0);
    let amount = milliunits_to_decimal(amount_millis);

    // Currency: YNAB doesn't embed currency per transaction; it comes from the
    // budget's currency_format (pulled via the /budgets endpoint). Default USD;
    // resolve_finance_account will carry through the budget currency when it is
    // threaded in at the call site.
    let currency = "USD".to_string();

    // Description: prefer import_payee_name (bank-provided name), fall back to
    // payee_name (user-entered). YNAB always emits both keys; hand-entered txns
    // have import_payee_name=null, so we must unwrap to &str first before
    // falling back — using .or_else on Option<&Value> would let Some(Null) block
    // the fallthrough.
    let description = tx
        .get("import_payee_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| tx.get("payee_name").and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string();

    let payee =
        tx.get("payee_name").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);

    let category = tx
        .get("category_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let cleared_str = tx.get("cleared").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let pending = cleared_str == "uncleared";

    // Build extra from all source-native fields not in the canonical schema.
    let mut extra: Map<String, Value> = Map::new();
    extra.insert("budget_id".to_string(), Value::String(budget_id.to_string()));
    for key in &[
        "payee_id",
        "category_id",
        "transfer_account_id",
        "transfer_transaction_id",
        "matched_transaction_id",
        "import_id",
        "import_payee_name",
        "import_payee_name_original",
        "cleared",
        "approved",
        "flag_color",
        "flag_name",
        "debt_transaction_type",
        "account_id",
    ] {
        if let Some(v) = tx.get(*key) {
            if !v.is_null() {
                extra.insert(key.to_string(), v.clone());
            }
        }
    }
    // Subtransactions — include when non-empty (split transactions).
    if let Some(subs) = tx.get("subtransactions").and_then(Value::as_array) {
        if !subs.is_empty() {
            extra.insert("subtransactions".to_string(), Value::Array(subs.clone()));
        }
    }

    Some(Transaction {
        id: id.to_string(),
        account: vault_account.to_string(),
        posted: date.to_string(),
        transacted: None,
        amount,
        currency,
        description,
        payee,
        category,
        pending,
        source: "ynab".to_string(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Raw layer.

/// A raw row: carries `ts` for month-partition routing (not serialized),
/// then flattens the API object verbatim.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Append new raw rows, deduped by string `id`. Existing ids across all
/// partitions are loaded first; only unseen ids are appended.
fn append_raw(vault: &Vault, rows: &[RawLine]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions().unwrap_or_default() {
        for v in stream.read::<Value>(&key)? {
            if let Some(id) = v.get("id").and_then(Value::as_str) {
                seen.insert(id.to_string());
            }
        }
    }
    let owned: Vec<&RawLine> = rows
        .iter()
        .filter(|r| {
            r.value
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| seen.insert(id.to_string()))
        })
        .collect();
    stream.append(owned.as_slice(), |r| r.ts.as_str())
}

/// YNAB `date` (YYYY-MM-DD) → RFC3339 local midnight for partition routing.
fn date_to_ts(date: &str) -> Option<String> {
    use chrono::{NaiveDate, TimeZone};
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

// ---------------------------------------------------------------------------
// The pull (testable through an injected API).

fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|s| !s.trim().is_empty())
        .context(
            "YNAB is not connected — add your personal access token in the Integrations tab",
        )?;
    let client = YnabClient::new(token);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl YnabApi) -> Result<PullOutcome> {
    let mut state = vault.read_ynab_sync();
    let budgets = api.budgets()?;
    if budgets.is_empty() {
        return Ok(PullOutcome {
            headline: "YNAB returned no budgets — check your token".into(),
            counts: BTreeMap::new(),
        });
    }

    let mut total_new = 0usize;
    let now_ts = Local::now().to_rfc3339();
    let today_str = Local::now().date_naive().format("%Y-%m-%d").to_string();

    for budget in &budgets {
        let Some(budget_id) = budget.get("id").and_then(Value::as_str) else { continue };
        let budget_name =
            budget.get("name").and_then(Value::as_str).unwrap_or("YNAB Budget");

        let budget_state = state.budgets.entry(budget_id.to_string()).or_default();
        let last_sk = budget_state.server_knowledge;

        // Fetch transactions (delta if cursor present, full history otherwise).
        let (txs, new_sk) = api.transactions(budget_id, last_sk)?;

        // Fetch accounts for balance snapshots (soft-fail on error).
        let accounts_result = api.accounts(budget_id);

        // --- Raw layer (unconditional, full fidelity) ---
        let raw_rows: Vec<RawLine> = txs
            .iter()
            .filter_map(|tx| {
                let date = tx.get("date").and_then(Value::as_str)?;
                let ts = date_to_ts(date)?;
                Partition::Month.key(&ts)?;
                Some(RawLine { ts, value: tx.clone() })
            })
            .collect();
        append_raw(vault, &raw_rows)?;

        // --- Contract layer ---
        let mut vault_accounts = vault.load_finance_accounts()?;

        // Build per-vault-account transaction batches.
        let mut by_account: BTreeMap<String, Vec<Transaction>> = BTreeMap::new();
        for tx in &txs {
            // Deleted transactions are in raw but not in the contract ledger.
            if tx.get("deleted").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let Some(acct_id) = tx.get("account_id").and_then(Value::as_str) else {
                continue;
            };
            let acct_name = tx
                .get("account_name")
                .and_then(Value::as_str)
                .unwrap_or("YNAB Account");
            // YNAB doesn't embed currency per transaction; default USD.
            let currency = "USD";
            let vault_id = vault.resolve_finance_account(
                &mut vault_accounts,
                "ynab",
                &format!("{budget_id}:{acct_id}"),
                acct_name,
                budget_name,
                currency,
            );
            if let Some(t) = map_transaction(tx, &vault_id, budget_id) {
                by_account.entry(vault_id).or_default().push(t);
            }
        }

        let mut budget_new = 0usize;
        for (acct_id, txns) in &by_account {
            // Don't replace_pending: YNAB cleared/uncleared transitions come
            // through the delta; pending rows survive until explicitly updated.
            let (new, _) = vault.upsert_finance_transactions(acct_id, txns, false)?;
            budget_new += new;
        }
        total_new += budget_new;

        // --- Balance snapshots from accounts endpoint ---
        if let Ok((accts, _)) = accounts_result {
            for acct in &accts {
                let Some(acct_id) = acct.get("id").and_then(Value::as_str) else {
                    continue;
                };
                // Skip closed/deleted accounts.
                if acct.get("deleted").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                if acct.get("closed").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                let acct_name =
                    acct.get("name").and_then(Value::as_str).unwrap_or("YNAB Account");
                let balance_millis =
                    acct.get("balance").and_then(Value::as_i64).unwrap_or(0);
                let balance = milliunits_to_decimal(balance_millis);
                let cleared_balance =
                    acct.get("cleared_balance").and_then(Value::as_i64).map(milliunits_to_decimal);

                let vault_id = vault.resolve_finance_account(
                    &mut vault_accounts,
                    "ynab",
                    &format!("{budget_id}:{acct_id}"),
                    acct_name,
                    budget_name,
                    "USD",
                );
                vault.record_finance_balance(
                    &vault_id,
                    &BalanceSnapshot {
                        date: today_str.clone(),
                        ts: now_ts.clone(),
                        balance,
                        available: cleared_balance,
                        currency: "USD".to_string(),
                        as_of: None,
                    },
                )?;
            }
        }

        vault.save_finance_accounts(&vault_accounts)?;

        // Advance cursor only after the full drain + writes succeed.
        budget_state.server_knowledge = Some(new_sk);
        budget_state.updated = Some(now_ts.clone());
    }

    state.last_updated = Some(now_ts);
    vault.write_ynab_sync(&state)?;

    Ok(PullOutcome {
        headline: if total_new == 0 {
            "YNAB is up to date — no new transactions".into()
        } else {
            format!("YNAB synced — {total_new} new transactions")
        },
        counts: BTreeMap::from([("transactions", total_new as u64)]),
    })
}

// ---------------------------------------------------------------------------
// Connection — TokenPaste (personal access token from Developer Settings).

/// Verify the token with a cheap `/user` call; store if valid.
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let token = pasted.trim();
    if token.is_empty() {
        bail!(
            "paste your YNAB personal access token — find it at \
             app.ynab.com/settings/developer"
        );
    }
    let client = YnabClient::new(token.to_string());
    let resp = client
        .get("/user", &[])
        .context("could not verify YNAB token — check it at app.ynab.com/settings/developer")?;
    // A valid /user response includes `data.user.id`.
    if resp.pointer("/data/user/id").is_none() {
        bail!(
            "YNAB did not recognize the token — double-check it at \
             app.ynab.com/settings/developer"
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
            label: "YNAB account".into(),
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
    id: "ynab",
    display_name: "YNAB",
    methods: &[ConnectMethod::TokenPaste {
        label: "Personal access token",
        help: "In YNAB go to Account Settings → Developer Settings \
               (app.ynab.com/settings/developer) and click \"New Token\". \
               Copy the token and paste it here. Tokens do not expire unless revoked.",
        placeholder: "ynab_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["ynab"],
    setup: &[
        "Open app.ynab.com/settings/developer and click \"New Token\".",
        "Copy the token and paste it into the field below.",
    ],
};

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_ynab_sync().last_updated
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("transactions").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("YNAB synced — {n} new transactions")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "YNAB sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ynab",
        name: "YNAB",
        kind: IntegrationKind::CloudSync,
        // 🔒 financial detail — ships opt-in with explicit acknowledgement.
        default_on: false,
        description: "Syncs your YNAB (You Need A Budget) transactions, categories, and account \
                      balances using your personal access token. Pulls from all budgets with a \
                      delta cursor so incremental syncs are fast. YNAB categories ride in the \
                      vault as metadata alongside the canonical finance ledger.",
        domain: "finance",
        vault_path: "finance/ynab/",
        toggleable: true,
        setup: &[
            "Open app.ynab.com/settings/developer and click \"New Token\".",
            "Copy the token and paste it into the connection card and click Connect.",
            "The first sync fetches your full transaction history across all budgets; \
             later syncs are incremental via the YNAB delta cursor.",
        ],
        caveats: "YNAB amounts use the outflow-negative convention (expenses are negative, \
                  credits/income are positive). Transactions present in both YNAB and \
                  SimpleFIN are stored in separate account folders keyed by source; \
                  read-time deduplication by amount + date is planned for the query layer \
                  (not yet implemented).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(YNAB_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("ynab"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-ynab-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixtures — synthesized from confirmed TransactionDetail schema at
    // raw.githubusercontent.com/ynab/ynab-sdk-js/master/open_api_spec.yaml

    /// A cleared transaction (outflow/expense) linked to a checking account.
    /// Amount -42500 milliunits = -$42.50 (expense, outflow-negative).
    fn tx_cleared() -> Value {
        json!({
            "id": "6a4dd0f2-1234-4b5c-9abc-000000000001",
            "date": "2026-05-15",
            "amount": -42500_i64,
            "memo": "groceries run",
            "cleared": "cleared",
            "approved": true,
            "flag_color": null,
            "flag_name": null,
            "account_id": "acct-uuid-0001",
            "account_name": "Chase Checking",
            "payee_id": "payee-uuid-0001",
            "payee_name": "Whole Foods",
            "category_id": "cat-uuid-0007",
            "category_name": "Groceries",
            "transfer_account_id": null,
            "transfer_transaction_id": null,
            "matched_transaction_id": null,
            "import_id": "YNAB:42500:2026-05-15:1",
            "import_payee_name": "Whole Foods Market",
            "import_payee_name_original": "WHOLEFDS MKT 123",
            "debt_transaction_type": null,
            "deleted": false,
            "subtransactions": []
        })
    }

    /// A pending (uncleared) transaction.
    /// Amount -9990 milliunits = -$9.99.
    fn tx_uncleared() -> Value {
        json!({
            "id": "6a4dd0f2-1234-4b5c-9abc-000000000002",
            "date": "2026-05-20",
            "amount": -9990_i64,
            "memo": null,
            "cleared": "uncleared",
            "approved": false,
            "flag_color": "red",
            "flag_name": "Important",
            "account_id": "acct-uuid-0002",
            "account_name": "Amex Platinum",
            "payee_id": "payee-uuid-0002",
            "payee_name": "Netflix",
            "category_id": "cat-uuid-0005",
            "category_name": "Entertainment",
            "transfer_account_id": null,
            "transfer_transaction_id": null,
            "matched_transaction_id": null,
            "import_id": null,
            "import_payee_name": null,
            "import_payee_name_original": null,
            "debt_transaction_type": null,
            "deleted": false,
            "subtransactions": []
        })
    }

    /// A deleted transaction — should appear in raw but NOT in the contract.
    fn tx_deleted() -> Value {
        json!({
            "id": "6a4dd0f2-1234-4b5c-9abc-000000000003",
            "date": "2026-05-10",
            "amount": -5000_i64,
            "cleared": "cleared",
            "approved": true,
            "account_id": "acct-uuid-0001",
            "account_name": "Chase Checking",
            "payee_name": "Deleted Merchant",
            "deleted": true,
            "subtransactions": []
        })
    }

    /// An inflow/income transaction.
    /// Amount 1500000 milliunits = $1500.00 (income, positive).
    fn tx_income() -> Value {
        json!({
            "id": "6a4dd0f2-1234-4b5c-9abc-000000000004",
            "date": "2026-05-01",
            "amount": 1500000_i64,
            "memo": "paycheck",
            "cleared": "cleared",
            "approved": true,
            "account_id": "acct-uuid-0001",
            "account_name": "Chase Checking",
            "payee_name": "Employer Inc",
            "category_name": "Inflow: Ready to Assign",
            "deleted": false,
            "subtransactions": []
        })
    }

    /// A split transaction with subtransactions.
    fn tx_split() -> Value {
        json!({
            "id": "6a4dd0f2-1234-4b5c-9abc-000000000005",
            "date": "2026-05-18",
            "amount": -30000_i64,
            "cleared": "cleared",
            "approved": true,
            "account_id": "acct-uuid-0001",
            "account_name": "Chase Checking",
            "payee_name": "Target",
            "category_name": null,
            "deleted": false,
            "subtransactions": [
                {
                    "id": "sub-0001",
                    "transaction_id": "6a4dd0f2-1234-4b5c-9abc-000000000005",
                    "amount": -15000_i64,
                    "memo": "groceries",
                    "category_name": "Groceries",
                    "deleted": false
                },
                {
                    "id": "sub-0002",
                    "transaction_id": "6a4dd0f2-1234-4b5c-9abc-000000000005",
                    "amount": -15000_i64,
                    "memo": "household",
                    "category_name": "Household Goods",
                    "deleted": false
                }
            ]
        })
    }

    fn budget_obj() -> Value {
        json!({
            "id": "budget-uuid-0001",
            "name": "My Budget",
            "last_modified_on": "2026-05-20T18:00:00+00:00",
            "first_month": "2022-01-01",
            "last_month": "2026-05-01",
            "date_format": {"format": "MM/DD/YYYY"},
            "currency_format": {"iso_code": "USD"}
        })
    }

    fn account_obj() -> Value {
        json!({
            "id": "acct-uuid-0001",
            "name": "Chase Checking",
            "type": "checking",
            "on_budget": true,
            "closed": false,
            "note": null,
            "balance": 125050_i64,
            "cleared_balance": 120000_i64,
            "uncleared_balance": 5050_i64,
            "transfer_payee_id": "tp-uuid-0001",
            "direct_import_linked": false,
            "direct_import_in_error": false,
            "last_reconciled_at": null,
            "debt_original_balance": null,
            "deleted": false
        })
    }

    // ---------------------------------------------------------------------------
    // milliunits_to_decimal.

    #[test]
    fn milliunits_expense_is_outflow_negative() {
        assert_eq!(milliunits_to_decimal(-42500), "-42.50");
        assert_eq!(milliunits_to_decimal(-9990), "-9.99");
        assert_eq!(milliunits_to_decimal(-1000), "-1.00");
        assert_eq!(milliunits_to_decimal(-500), "-0.50");
        assert_eq!(milliunits_to_decimal(-1), "0.00"); // sub-cent rounds to zero — no leading minus
    }

    #[test]
    fn milliunits_income_is_positive() {
        assert_eq!(milliunits_to_decimal(1500000), "1500.00");
        assert_eq!(milliunits_to_decimal(100), "0.10");
        assert_eq!(milliunits_to_decimal(0), "0.00");
    }

    #[test]
    fn milliunits_large_amounts() {
        // $12,345.67
        assert_eq!(milliunits_to_decimal(12345670), "12345.67");
    }

    #[test]
    fn milliunits_rounds_half_up() {
        // 42999 milliunits: the 3rd milliunit digit is 9; rounds up to $43.00
        assert_eq!(milliunits_to_decimal(42999), "43.00");
        // 42505 milliunits: the 3rd digit is 5; rounds up to $42.51
        assert_eq!(milliunits_to_decimal(42505), "42.51");
        // 42504 milliunits: the 3rd digit is 4; rounds down to $42.50
        assert_eq!(milliunits_to_decimal(42504), "42.50");
        // Negative amounts round the same way.
        assert_eq!(milliunits_to_decimal(-42999), "-43.00");
        assert_eq!(milliunits_to_decimal(-42505), "-42.51");
    }

    // ---------------------------------------------------------------------------
    // map_transaction.

    #[test]
    fn maps_cleared_transaction_to_canonical_form() {
        let tx = tx_cleared();
        let t = map_transaction(&tx, "chase-checking", "budget-uuid-0001").unwrap();
        assert_eq!(t.id, "6a4dd0f2-1234-4b5c-9abc-000000000001");
        assert_eq!(t.account, "chase-checking");
        assert_eq!(t.posted, "2026-05-15");
        assert_eq!(t.amount, "-42.50", "milliunits → decimal, outflow-negative");
        assert_eq!(t.currency, "USD");
        assert_eq!(t.description, "Whole Foods Market", "import_payee_name preferred");
        assert_eq!(t.payee.as_deref(), Some("Whole Foods"));
        assert_eq!(t.category.as_deref(), Some("Groceries"));
        assert!(!t.pending, "cleared is not pending");
        assert_eq!(t.source, "ynab");
        assert_eq!(t.extra.get("budget_id"), Some(&json!("budget-uuid-0001")));
        assert_eq!(t.extra.get("cleared"), Some(&json!("cleared")));
        assert_eq!(t.extra.get("import_id"), Some(&json!("YNAB:42500:2026-05-15:1")));
        // Null-valued fields must be absent from extra.
        assert!(t.extra.get("flag_color").is_none(), "null flag_color omitted");
        assert!(t.extra.get("transfer_account_id").is_none(), "null transfer omitted");
    }

    #[test]
    fn maps_uncleared_pending_transaction() {
        let tx = tx_uncleared();
        let t = map_transaction(&tx, "amex-platinum", "budget-uuid-0001").unwrap();
        assert_eq!(t.amount, "-9.99");
        assert!(t.pending, "uncleared → pending");
        // import_payee_name is null here; must fall through to payee_name.
        assert_eq!(t.description, "Netflix", "null import_payee_name must fall through to payee_name");
        assert_eq!(t.extra.get("flag_color"), Some(&json!("red")));
        assert_eq!(t.extra.get("flag_name"), Some(&json!("Important")));
    }

    /// Hand-entered transactions (the majority of a typical YNAB user's ledger)
    /// have `import_payee_name: null` and `payee_name: "<user-entered>"`.
    /// The description fallback must see through the null and use payee_name.
    #[test]
    fn description_falls_back_through_null_import_payee_name() {
        let tx = json!({
            "id": "hand-entered-001",
            "date": "2026-05-22",
            "amount": -15000_i64,
            "cleared": "cleared",
            "approved": true,
            "account_id": "acct-uuid-0001",
            "account_name": "Chase Checking",
            "payee_name": "Netflix",
            "import_payee_name": null,
            "import_payee_name_original": null,
            "deleted": false,
            "subtransactions": []
        });
        let t = map_transaction(&tx, "chase-checking", "budget-uuid-0001").unwrap();
        assert_eq!(
            t.description, "Netflix",
            "present-but-null import_payee_name must not block fallthrough to payee_name"
        );
    }

    /// When both import_payee_name and payee_name are null/absent, description is empty.
    #[test]
    fn description_is_empty_when_both_payee_fields_null() {
        let tx = json!({
            "id": "no-payee-001",
            "date": "2026-05-22",
            "amount": -5000_i64,
            "cleared": "cleared",
            "approved": true,
            "account_id": "acct-uuid-0001",
            "account_name": "Chase Checking",
            "import_payee_name": null,
            "payee_name": null,
            "deleted": false,
            "subtransactions": []
        });
        let t = map_transaction(&tx, "chase-checking", "budget-uuid-0001").unwrap();
        assert_eq!(t.description, "", "both null → empty description");
    }

    #[test]
    fn deleted_transaction_returns_none() {
        let tx = tx_deleted();
        assert!(
            map_transaction(&tx, "chase-checking", "budget-uuid-0001").is_none(),
            "deleted transactions must not enter the contract layer"
        );
    }

    #[test]
    fn income_transaction_is_positive() {
        let tx = tx_income();
        let t = map_transaction(&tx, "chase-checking", "budget-uuid-0001").unwrap();
        assert_eq!(t.amount, "1500.00");
        assert!(!t.amount.starts_with('-'), "income is positive");
    }

    #[test]
    fn split_transaction_carries_subtransactions_in_extra() {
        let tx = tx_split();
        let t = map_transaction(&tx, "chase-checking", "budget-uuid-0001").unwrap();
        assert_eq!(t.amount, "-30.00");
        let subs = t.extra.get("subtransactions").and_then(Value::as_array).unwrap();
        assert_eq!(subs.len(), 2);
    }

    #[test]
    fn map_transaction_returns_none_without_id() {
        let tx = json!({
            "date": "2026-05-15",
            "amount": -1000_i64,
            "cleared": "cleared",
            "deleted": false
        });
        assert!(map_transaction(&tx, "acct", "bud").is_none());
    }

    #[test]
    fn map_transaction_returns_none_without_date() {
        let tx = json!({
            "id": "some-id",
            "amount": -1000_i64,
            "cleared": "cleared",
            "deleted": false
        });
        assert!(map_transaction(&tx, "acct", "bud").is_none());
    }

    // ---------------------------------------------------------------------------
    // date_to_ts.

    #[test]
    fn date_to_ts_produces_partitionable_rfc3339() {
        let ts = date_to_ts("2026-05-15").unwrap();
        assert!(Partition::Month.key(&ts).is_some(), "month-partitionable: {ts}");
        assert!(ts.starts_with("2026-05-15"));
    }

    #[test]
    fn date_to_ts_rejects_invalid() {
        assert!(date_to_ts("not-a-date").is_none());
        assert!(date_to_ts("").is_none());
    }

    // ---------------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        budgets: Vec<Value>,
        /// budget_id → (transactions, server_knowledge)
        transactions: BTreeMap<String, (Vec<Value>, i64)>,
        /// budget_id → (accounts, server_knowledge)
        accounts: BTreeMap<String, (Vec<Value>, i64)>,
        /// Track last_knowledge passed for each budget.
        last_knowledge_seen: std::cell::RefCell<BTreeMap<String, Option<i64>>>,
    }

    impl MockApi {
        fn new(
            budgets: Vec<Value>,
            transactions: Vec<(String, Vec<Value>, i64)>,
            accounts: Vec<(String, Vec<Value>, i64)>,
        ) -> Self {
            MockApi {
                budgets,
                transactions: transactions.into_iter().map(|(id, txs, sk)| (id, (txs, sk))).collect(),
                accounts: accounts.into_iter().map(|(id, accts, sk)| (id, (accts, sk))).collect(),
                last_knowledge_seen: Default::default(),
            }
        }
    }

    impl YnabApi for MockApi {
        fn budgets(&self) -> Result<Vec<Value>> {
            Ok(self.budgets.clone())
        }

        fn transactions(
            &self,
            budget_id: &str,
            last_knowledge: Option<i64>,
        ) -> Result<(Vec<Value>, i64)> {
            self.last_knowledge_seen
                .borrow_mut()
                .insert(budget_id.to_string(), last_knowledge);
            Ok(self.transactions.get(budget_id).cloned().unwrap_or((Vec::new(), 0)))
        }

        fn accounts(&self, budget_id: &str) -> Result<(Vec<Value>, i64)> {
            Ok(self.accounts.get(budget_id).cloned().unwrap_or((Vec::new(), 0)))
        }
    }

    fn make_api() -> MockApi {
        MockApi::new(
            vec![budget_obj()],
            vec![(
                "budget-uuid-0001".to_string(),
                vec![tx_cleared(), tx_uncleared(), tx_deleted(), tx_income(), tx_split()],
                42,
            )],
            vec![(
                "budget-uuid-0001".to_string(),
                vec![account_obj()],
                42,
            )],
        )
    }

    // ---------------------------------------------------------------------------
    // Integration tests.

    #[test]
    fn full_pull_writes_raw_contract_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = make_api();
        let out = pull_with(&v, &api).unwrap();

        // 4 non-deleted transactions → 4 new in contract.
        assert_eq!(out.counts.get("transactions"), Some(&4));

        // Raw layer has all 5 (including deleted).
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0;
        for key in raw.partitions().unwrap() {
            raw_count += raw.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(raw_count, 5, "raw layer has all 5 including deleted");

        // Contract layer has 4 rows.
        let txns = v.finance_transactions(None, 100).unwrap();
        assert_eq!(txns.len(), 4, "deleted row not in contract");

        let groceries = txns.iter().find(|t| t.id.contains("000000000001")).unwrap();
        assert_eq!(groceries.amount, "-42.50");
        assert_eq!(groceries.currency, "USD");
        assert_eq!(groceries.category.as_deref(), Some("Groceries"));
        assert_eq!(groceries.source, "ynab");

        // Cursor advanced for the budget.
        let state = v.read_ynab_sync();
        let bs = state.budgets.get("budget-uuid-0001").unwrap();
        assert_eq!(bs.server_knowledge, Some(42));
        assert!(bs.updated.is_some());
        assert!(state.last_updated.is_some());

        // Accounts registered.
        let overview = v.finance_overview().unwrap();
        assert!(!overview.accounts.is_empty(), "accounts registered");
        let acct = overview.accounts.iter().find(|a| a.name == "Chase Checking");
        assert!(acct.is_some(), "Chase Checking account registered");

        // Balance stored (125050 milliunits = $125.05).
        assert_eq!(acct.unwrap().balance.as_deref(), Some("125.05"));
    }

    #[test]
    fn second_pull_uses_delta_cursor_and_is_idempotent() {
        let v = temp_vault("delta");
        let api1 = make_api();
        pull_with(&v, &api1).unwrap();

        // Verify cursor was set.
        let state = v.read_ynab_sync();
        assert_eq!(
            state.budgets.get("budget-uuid-0001").and_then(|bs| bs.server_knowledge),
            Some(42)
        );

        // Second pull with same data → no new transactions.
        let api2 = MockApi::new(
            vec![budget_obj()],
            // Return empty delta (no changes since server_knowledge=42).
            vec![("budget-uuid-0001".to_string(), vec![], 42)],
            vec![("budget-uuid-0001".to_string(), vec![account_obj()], 42)],
        );
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("transactions"), Some(&0), "idempotent on re-run");

        // Cursor was passed to the API.
        assert_eq!(
            *api2.last_knowledge_seen.borrow().get("budget-uuid-0001").unwrap(),
            Some(42),
            "server_knowledge cursor passed on second pull"
        );
    }

    #[test]
    fn deleted_transactions_land_in_raw_only() {
        let v = temp_vault("deleted");
        let api = MockApi::new(
            vec![budget_obj()],
            vec![(
                "budget-uuid-0001".to_string(),
                vec![tx_deleted()],
                10,
            )],
            vec![],
        );
        let out = pull_with(&v, &api).unwrap();
        // No contract rows.
        assert_eq!(out.counts.get("transactions"), Some(&0));

        // Raw has the deleted row.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0;
        for key in raw.partitions().unwrap() {
            raw_count += raw.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(raw_count, 1, "deleted tx in raw");
    }

    #[test]
    fn empty_budgets_returns_graceful_message() {
        let v = temp_vault("nobudgets");
        let api = MockApi::new(vec![], vec![], vec![]);
        let out = pull_with(&v, &api).unwrap();
        assert!(out.headline.contains("no budgets"), "graceful: {}", out.headline);
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
                access_token: "ynab_testtoken".into(),
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
        assert_eq!(status.accounts[0].key, "ynab");

        def_disconnect(&v, "ynab").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn sync_state_roundtrips_and_empty_deserializes() {
        let mut state = SyncState::default();
        state.budgets.insert(
            "budget-uuid-0001".to_string(),
            BudgetState { server_knowledge: Some(42), updated: Some("2026-06-15T12:00:00-07:00".to_string()) },
        );
        state.last_updated = Some("2026-06-15T12:00:00-07:00".to_string());
        let json = serde_json::to_string(&state).unwrap();
        let back: SyncState = serde_json::from_str(&json).unwrap();
        let bs = back.budgets.get("budget-uuid-0001").unwrap();
        assert_eq!(bs.server_knowledge, Some(42));
        assert_eq!(back.last_updated.as_deref(), Some("2026-06-15T12:00:00-07:00"));

        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.budgets.is_empty());
        assert!(empty.last_updated.is_none());
    }

    #[test]
    fn def_and_connection_are_wired_correctly() {
        assert_eq!(DEF.connection, Some("ynab"));
        assert_eq!(DEF.meta.id, "ynab");
        assert_eq!(CONNECTION.id, "ynab");
        assert!(CONNECTION.method("token-paste").is_some());
        assert!(DEF.pull.is_some());
        assert!(DEF.last_data.is_some());
        assert!(DEF.meta.toggleable);
        assert!(!DEF.meta.default_on, "financial data is privacy-sensitive");
    }
}

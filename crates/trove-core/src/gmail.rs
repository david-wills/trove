//! Gmail collector — full-history backfill, then `historyId` incremental,
//! for every connected Google account. The OAuth side (connect, token store,
//! refresh, reconnect flagging) is owned by [`crate::sync::google`]; this
//! module only asks it for a fresh token per account via
//! [`crate::sync::google::fresh_token`].
//!
//! **Target.** Messages land in the unified correspondence stream
//! (`correspondence/email/YYYY-MM.jsonl`, see [`crate::correspondence`]) —
//! the *same* stream the `.mbox` importer ([`crate::email`]) writes — and
//! share its Message-ID dedupe, so a Gmail pull and a Takeout mbox of the
//! same mailbox never double a message. Each record's `service` is the
//! connected account's address, so N accounts coexist in one stream.
//!
//! **Conversion** reuses the importer's `email_to_message`: Gmail's
//! `format=raw` payload is the verbatim RFC822 bytes, so the exact same
//! parse (text body preferred, attachment *metadata* only — the lean
//! default) applies. On top we layer the message's Gmail `labelIds` (a
//! read-time filter — categories, INBOX/SENT) and an authoritative
//! `SENT`-label `from_me` that survives send-as aliases.
//!
//! **Two phases, per account, resumable** (mirrors [`crate::oura`]):
//!
//! 1. **Backfill** — page `users.messages.list` (which excludes Spam/Trash by
//!    default) oldest-token-forward, fetching each id's raw bytes. The list
//!    `pageToken` is persisted after every page, so an interrupted or
//!    budget-capped backfill resumes exactly where it stopped. A baseline
//!    `historyId` is captured from `users.getProfile` *before* the first page
//!    so nothing that arrives mid-backfill is missed.
//! 2. **Incremental** — once backfill completes, `users.history.list` from the
//!    stored `historyId` yields only newly-added messages; the cursor
//!    advances each pass. A `404` (cursor aged out of Gmail's bounded
//!    history) resets the account to a fresh backfill — dedupe makes the
//!    re-list cheap (nothing already stored is rewritten).
//!
//! Like Oura, a pass runs request-budgeted inside the watcher owner loop so a
//! large backfill can't monopolize it; the manual "Sync now" pull is
//! unbudgeted and drains the backfill in one go.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::correspondence::Message;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::vault::Vault;

/// Seconds between Gmail incremental passes in the watcher loop. Mail is
/// time-sensitive enough to want sub-hourly freshness, cheap enough (one
/// `history.list` when nothing changed) to afford it.
pub const GMAIL_SYNC_SECS: u64 = 900;

/// Request budget for one watcher-loop pass. Ample for steady-state
/// incremental (a `history.list` plus a handful of `messages.get`); caps how
/// long a first-connect backfill can occupy the owner loop. Far under
/// Gmail's 250 quota-units/user/second (list & get are 5 units each).
pub const GMAIL_LOOP_BUDGET: u32 = 200;

// Incremental is one cheap history.list when nothing changed; each pass is
// request-budgeted so a first-connect backfill can't monopolize the owner
// loop. A silent no-op when no Google account is connected.
fn def_collect(vault: &Vault, _now: chrono::DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_gmail(Some(GMAIL_LOOP_BUDGET))?;
    Ok(crate::registry::CollectOutcome::note_if(s.messages > 0, || {
        format!(
            "gmail synced — {} messages across {} accounts",
            s.messages, s.accounts
        )
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_gmail_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-gmail",
        name: "Gmail",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Full-text messages from every connected Google account into the same email stream as mbox imports, deduped by Message-ID; backfilled on connect, then kept current by Gmail's history cursor.",
        domain: "correspondence",
        vault_path: "correspondence/email/",
        toggleable: true,
        setup: &[],
        caveats: "Connect a Google account on the card above to start. Everything except Spam and Trash is pulled, Gmail category labels preserved. Raw .eml archives and attachment files are opt-in per account; by default only message text and attachment metadata are stored.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(GMAIL_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("google"),
    pull: Some(pull),
};

/// [`crate::registry::IntegrationDef::pull`] adapter: the unbudgeted manual
/// pull ([`Vault::gmail_pull`]), mapped into the generic outcome shape.
/// Per-account failures never abort the pass — they land in
/// `.trove/gmail-sync.json` — so the headline re-reads the state to surface
/// them rather than reporting a clean sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.gmail_pull()?;
    let errors = vault
        .read_gmail_sync()
        .map(|st| st.accounts.values().filter(|a| a.error.is_some()).count() as u64)
        .unwrap_or(0);
    let mut headline = if s.messages > 0 {
        format!("{} new messages across {} accounts", s.messages, s.accounts)
    } else {
        "no new messages".to_string()
    };
    if !s.backfill_done {
        headline.push_str(" — backfill still in progress");
    }
    if errors > 0 {
        headline.push_str(&format!(
            " — {errors} account{} failed",
            if errors == 1 { "" } else { "s" }
        ));
    }
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("accounts", s.accounts as u64),
            ("messages", s.messages),
            ("account_errors", errors),
        ]),
    })
}

const SYNC_FILE: &str = ".trove/gmail-sync.json";
const GMAIL_API: &str = "https://gmail.googleapis.com";
/// Kept short so a hung connection can't stall the watcher owner loop for
/// long (the `oura.rs` reasoning).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// `messages.list` page size. Small enough that a budget cut-off mid-page
/// re-fetches at most this many already-stored messages next pass (the list
/// cursor is page-granular), large enough to keep list calls infrequent.
const LIST_PAGE_SIZE: u32 = 100;
/// `history.list` page size.
const HISTORY_PAGE_SIZE: u32 = 100;

/// Base64url engine that tolerates Gmail's `raw` field whether or not it
/// carries `=` padding.
const B64URL: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Result of one Gmail sync pass, for logging / the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GmailSyncStats {
    /// Accounts that gained at least one message this pass.
    pub accounts: u32,
    /// Messages newly written to the email stream this pass.
    pub messages: u64,
    /// Every connected account has finished its history backfill.
    pub backfill_done: bool,
}

/// Per-account sync progress, persisted in `.trove/gmail-sync.json` (keyed by
/// Google `sub`). Deleting an account's entry re-runs its backfill.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GmailAccountState {
    /// Display address (for the index / UI; the map key is the `sub`).
    #[serde(default)]
    pub email: String,
    /// The incremental cursor: Gmail mailbox `historyId`. Captured from the
    /// profile before backfill begins, then advanced by each `history.list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_id: Option<String>,
    /// `messages.list` resume token for an in-progress backfill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill_page_token: Option<String>,
    /// The baseline `historyId` has been captured and backfill has begun.
    #[serde(default)]
    pub backfill_started: bool,
    /// Backfill has walked the whole mailbox; incremental now drives updates.
    #[serde(default)]
    pub backfill_done: bool,
    /// Total messages ever written for this account (drives index.md).
    #[serde(default)]
    pub messages: u64,
    /// Why this account's last pass failed, for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The whole Gmail sync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GmailSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Per-account progress, keyed by Google `sub`.
    pub accounts: BTreeMap<String, GmailAccountState>,
}

/// Status-level fetch errors needing distinct handling.
enum FetchError {
    RateLimited,
    Unauthorized,
    /// `history.list` cursor aged out — reset to a fresh backfill.
    HistoryGone,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::HistoryGone => write!(f, "history cursor expired (HTTP 404)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// One message fetched with `format=raw`: the API's labels plus the decoded
/// RFC822 bytes ready for [`crate::email::email_to_message`].
struct FetchedMessage {
    label_ids: Vec<String>,
    raw: Vec<u8>,
}

/// Thin Gmail API client. The base URL is injected so the orchestration is
/// testable against a local stub (the `oura.rs`/`tasks.rs` pattern).
struct GmailClient {
    base: String,
    token: String,
}

impl GmailClient {
    fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, FetchError> {
        let mut req = ureq::get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HTTP_TIMEOUT);
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(404, _)) => Err(FetchError::HistoryGone),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }

    /// Mailbox profile — used once per account to seed the baseline
    /// `historyId` before backfill.
    fn profile(&self) -> Result<Value, FetchError> {
        self.get("/gmail/v1/users/me/profile", &[])
    }

    /// One page of message ids (Spam/Trash excluded by default).
    fn list_messages(&self, page_token: Option<&str>) -> Result<(Vec<String>, Option<String>), FetchError> {
        let mut params = vec![("maxResults", LIST_PAGE_SIZE.to_string())];
        if let Some(t) = page_token {
            params.push(("pageToken", t.to_string()));
        }
        let v = self.get("/gmail/v1/users/me/messages", &params)?;
        let ids = v
            .get("messages")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let next = v.get("nextPageToken").and_then(Value::as_str).map(str::to_string);
        Ok((ids, next))
    }

    /// One message's labels + raw RFC822 bytes.
    fn get_raw(&self, id: &str) -> Result<FetchedMessage, FetchError> {
        let v = self.get(
            &format!("/gmail/v1/users/me/messages/{id}"),
            &[("format", "raw".to_string())],
        )?;
        let raw_b64 = v
            .get("raw")
            .and_then(Value::as_str)
            .ok_or_else(|| FetchError::Other(format!("message {id} had no raw payload")))?;
        let raw = B64URL
            .decode(raw_b64)
            .map_err(|e| FetchError::Other(format!("decoding message {id}: {e}")))?;
        Ok(FetchedMessage {
            label_ids: label_ids(&v),
            raw,
        })
    }

    /// One page of history records since `start`, returning the message ids
    /// added, the next page token, and the mailbox's current `historyId`.
    fn list_history(
        &self,
        start: &str,
        page_token: Option<&str>,
    ) -> Result<HistoryPage, FetchError> {
        let mut params = vec![
            ("startHistoryId", start.to_string()),
            ("historyTypes", "messageAdded".to_string()),
            ("maxResults", HISTORY_PAGE_SIZE.to_string()),
        ];
        if let Some(t) = page_token {
            params.push(("pageToken", t.to_string()));
        }
        let v = self.get("/gmail/v1/users/me/history", &params)?;
        let mut added = Vec::new();
        if let Some(records) = v.get("history").and_then(Value::as_array) {
            for rec in records {
                let Some(msgs) = rec.get("messagesAdded").and_then(Value::as_array) else {
                    continue;
                };
                for m in msgs {
                    if let Some(id) = m
                        .get("message")
                        .and_then(|mm| mm.get("id"))
                        .and_then(Value::as_str)
                    {
                        added.push(id.to_string());
                    }
                }
            }
        }
        Ok(HistoryPage {
            added,
            next_token: v.get("nextPageToken").and_then(Value::as_str).map(str::to_string),
            history_id: v.get("historyId").and_then(Value::as_str).map(str::to_string),
        })
    }
}

struct HistoryPage {
    added: Vec<String>,
    next_token: Option<String>,
    history_id: Option<String>,
}

/// Pull the `labelIds` array off a message JSON.
fn label_ids(v: &Value) -> Vec<String> {
    v.get("labelIds")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Per-pass request allowance. `None` = unlimited (manual pull).
struct Budget(Option<u32>);

impl Budget {
    fn take(&mut self) -> bool {
        match &mut self.0 {
            None => true,
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
        }
    }
}

/// A Gmail-fetched raw message → a correspondence record, with the labels and
/// authoritative `SENT`-based `from_me` layered onto the shared mbox parse.
/// `None` when the bytes don't parse or carry no usable date.
fn gmail_to_record(fetched: &FetchedMessage, account_email: &str) -> Option<Message> {
    let mut m = crate::email::email_to_message(&fetched.raw, account_email)?;
    // The SENT label is the ground truth for authorship — it survives
    // send-as aliases the From-address comparison can't see.
    if fetched.label_ids.iter().any(|l| l == "SENT") {
        m.from_me = true;
    }
    m.labels = fetched.label_ids.clone();
    Some(m)
}

impl Vault {
    /// One Gmail sync pass across every connected Google account: refresh each
    /// account's token, backfill any that haven't finished, then pull
    /// incrementally from the `historyId` cursor. A silent no-op when no
    /// Google account is connected. Per-account failures are recorded in
    /// `.trove/gmail-sync.json` and never corrupt progress — the next pass
    /// resumes from the persisted page token / history id.
    pub fn collect_gmail(&self, budget: Option<u32>) -> Result<GmailSyncStats> {
        let accounts = self.google_status()?.accounts;
        let mut state = self.read_gmail_sync().unwrap_or_default();
        // Forget state for accounts that have been disconnected.
        let live: HashSet<&str> = accounts.iter().map(|a| a.sub.as_str()).collect();
        state.accounts.retain(|sub, _| live.contains(sub.as_str()));

        if accounts.is_empty() {
            // Persist the pruning above so a disconnected account's row doesn't
            // linger in the state or the index.
            if self.resolve(SYNC_FILE).map(|p| p.exists()).unwrap_or(false) {
                state.updated = Local::now().to_rfc3339();
                self.write_gmail_sync(&state)?;
                self.write_gmail_index(&state)?;
            }
            return Ok(GmailSyncStats::default());
        }

        // The shared email dedupe set, loaded once and grown as we write — so
        // Gmail pulls coexist with mbox imports and re-runs never duplicate.
        let mut seen = self.correspondence_guids("email")?;
        let mut budget = Budget(budget);
        let mut stats = GmailSyncStats::default();

        for acct in &accounts {
            // A flagged account can't refresh non-interactively; skip it (the
            // card surfaces the reconnect prompt).
            if acct.needs_reconnect {
                continue;
            }
            let token = match crate::sync::google::fresh_token(self, &acct.sub) {
                Ok(t) => t.access_token,
                Err(e) => {
                    let astate = state.accounts.entry(acct.sub.clone()).or_default();
                    astate.email = acct.email.clone();
                    astate.error = Some(format!("{e:#}"));
                    continue;
                }
            };
            let client = GmailClient {
                base: GMAIL_API.to_string(),
                token,
            };
            let before = {
                let astate = state.accounts.entry(acct.sub.clone()).or_default();
                astate.email = acct.email.clone();
                astate.error = None;
                astate.messages
            };
            match self.gmail_sync_account(&client, &acct.sub, &acct.email, &mut state, &mut seen, &mut budget) {
                Ok(()) => {}
                Err(e) => {
                    let msg = format!("{}", status_error(&acct.email, e));
                    let astate = state.accounts.entry(acct.sub.clone()).or_default();
                    astate.error = Some(msg);
                }
            }
            let after = state.accounts.get(&acct.sub).map(|s| s.messages).unwrap_or(before);
            if after > before {
                stats.accounts += 1;
                stats.messages += after - before;
            }
        }

        stats.backfill_done = state.accounts.values().all(|s| s.backfill_done);
        state.updated = Local::now().to_rfc3339();
        self.write_gmail_sync(&state)?;
        self.write_gmail_index(&state)?;
        Ok(stats)
    }

    /// Backfill (if unfinished) then incremental for one account. Persists the
    /// account's cursor after every page so any interruption resumes cleanly.
    fn gmail_sync_account(
        &self,
        client: &GmailClient,
        sub: &str,
        email: &str,
        state: &mut GmailSyncState,
        seen: &mut HashSet<String>,
        budget: &mut Budget,
    ) -> Result<(), FetchError> {
        // Phase 0 — capture the baseline historyId before the first page, so
        // mail arriving during a long backfill is caught by incremental later.
        if !state.accounts.get(sub).map(|s| s.backfill_started).unwrap_or(false) {
            if !budget.take() {
                return Ok(());
            }
            let profile = client.profile()?;
            let astate = state.accounts.entry(sub.to_string()).or_default();
            astate.history_id = profile.get("historyId").and_then(Value::as_str).map(str::to_string);
            astate.backfill_started = true;
            astate.backfill_page_token = None;
            self.write_gmail_sync(state).map_err(soft)?;
        }

        // Phase A — backfill.
        while !state.accounts.get(sub).map(|s| s.backfill_done).unwrap_or(false) {
            if !budget.take() {
                return Ok(());
            }
            let page_token = state.accounts.get(sub).and_then(|s| s.backfill_page_token.clone());
            let (ids, next) = client.list_messages(page_token.as_deref())?;
            let (written, exhausted) = self.fetch_and_store(client, email, &ids, seen, budget)?;
            let astate = state.accounts.entry(sub.to_string()).or_default();
            astate.messages += written;
            // Only advance the page cursor when the whole page was fetched —
            // otherwise the next pass would skip this page's unfetched tail.
            // On budget exhaustion the token is left unchanged, so the next
            // pass re-lists this page and dedupe skips what's already stored.
            if !exhausted {
                match next {
                    Some(t) => astate.backfill_page_token = Some(t),
                    None => {
                        astate.backfill_done = true;
                        astate.backfill_page_token = None;
                    }
                }
            }
            self.write_gmail_sync(state).map_err(soft)?;
            if exhausted {
                return Ok(());
            }
        }

        // Phase B — incremental from the historyId cursor.
        let Some(start) = state.accounts.get(sub).and_then(|s| s.history_id.clone()) else {
            return Ok(());
        };
        let mut added: Vec<String> = Vec::new();
        let mut newest_history = start.clone();
        let mut page_token: Option<String> = None;
        loop {
            if !budget.take() {
                // Don't advance the cursor on a partial walk — re-walk next
                // pass (dedupe absorbs the overlap).
                return Ok(());
            }
            let page = match client.list_history(&start, page_token.as_deref()) {
                Ok(p) => p,
                Err(FetchError::HistoryGone) => {
                    // Cursor aged out of Gmail's bounded history → full
                    // re-backfill. Dedupe keeps the re-list cheap.
                    let astate = state.accounts.entry(sub.to_string()).or_default();
                    astate.backfill_started = false;
                    astate.backfill_done = false;
                    astate.backfill_page_token = None;
                    self.write_gmail_sync(state).map_err(soft)?;
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            added.extend(page.added);
            if let Some(h) = page.history_id {
                newest_history = h;
            }
            match page.next_token {
                Some(t) => page_token = Some(t),
                None => break,
            }
        }
        // Dedupe ids within the pass (a message can recur across records).
        let mut id_seen = HashSet::new();
        added.retain(|id| id_seen.insert(id.clone()));
        let (written, exhausted) = self.fetch_and_store(client, email, &added, seen, budget)?;
        let astate = state.accounts.entry(sub.to_string()).or_default();
        astate.messages += written;
        // Only advance the cursor when every added message was fetched —
        // otherwise the next pass re-walks history from `start` and re-fetches
        // the tail (dedupe absorbs the overlap), rather than skipping it.
        if !exhausted {
            astate.history_id = Some(newest_history);
        }
        self.write_gmail_sync(state).map_err(soft)?;
        Ok(())
    }

    /// Fetch each id's raw bytes, convert, dedupe by Message-ID, and append
    /// the new ones to the email stream. Returns `(written, budget_exhausted)`
    /// — `budget_exhausted` is true if the budget ran out before every id was
    /// fetched, which tells the caller not to advance its cursor past the
    /// unfetched tail. Whatever was fetched is always written.
    fn fetch_and_store(
        &self,
        client: &GmailClient,
        email: &str,
        ids: &[String],
        seen: &mut HashSet<String>,
        budget: &mut Budget,
    ) -> Result<(u64, bool), FetchError> {
        let mut batch: Vec<Message> = Vec::new();
        let mut exhausted = false;
        for id in ids {
            if !budget.take() {
                // Budget ran out before fetching this id (and any after it).
                exhausted = true;
                break;
            }
            let fetched = client.get_raw(id)?;
            if let Some(m) = gmail_to_record(&fetched, email) {
                if seen.insert(m.guid.clone()) {
                    batch.push(m);
                }
            }
        }
        let written = batch.len() as u64;
        if !batch.is_empty() {
            self.append_messages(&batch).map_err(soft)?;
        }
        Ok((written, exhausted))
    }

    /// The persisted Gmail sync progress, if a sync has ever run.
    pub fn read_gmail_sync(&self) -> Option<GmailSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Atomic write so readers never see a torn file — called after every
    /// page, which is what makes the backfill resumable.
    fn write_gmail_sync(&self, state: &GmailSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Regenerate the human-readable summary at `correspondence/email/gmail.md`.
    fn write_gmail_index(&self, state: &GmailSyncState) -> Result<()> {
        let mut md = format!(
            "# Gmail\n\nLast sync: {}\n\n| Account | Messages | History | Cursor | Error |\n|---|---|---|---|---|\n",
            state.updated
        );
        for s in state.accounts.values() {
            let history = if s.backfill_done {
                "complete".to_string()
            } else if s.backfill_started {
                "backfilling…".to_string()
            } else {
                "—".to_string()
            };
            md.push_str(&format!(
                "| {} | {} | {history} | {} | {} |\n",
                s.email,
                s.messages,
                s.history_id.as_deref().unwrap_or("—"),
                s.error.as_deref().unwrap_or(""),
            ));
        }
        let path = self.resolve("correspondence/email/gmail.md")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("md.tmp");
        fs::write(&tmp, md)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Pull every connected account's Gmail into the vault, unbudgeted — the
    /// manual "Sync now" / first-connect path that runs the full backfill to
    /// completion. Blocking (network).
    pub fn gmail_pull(&self) -> Result<GmailSyncStats> {
        if self.google_status()?.accounts.is_empty() {
            bail!("no Google account is connected");
        }
        self.collect_gmail(None)
    }
}

/// A vault write error inside the API-fetch path → a soft `FetchError`.
fn soft(e: anyhow::Error) -> FetchError {
    FetchError::Other(format!("{e:#}"))
}

/// Map a status-level Gmail error to a user-facing message for the sync log.
fn status_error(account: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::RateLimited => {
            anyhow!("Gmail rate limited the sync ({account}) — it resumes next pass")
        }
        FetchError::Unauthorized => {
            anyhow!("Gmail rejected the token ({account}, 401) — reconnect from the Integrations tab")
        }
        FetchError::HistoryGone => {
            anyhow!("Gmail history cursor expired ({account}) — re-backfilling")
        }
        FetchError::Other(m) => anyhow!("gmail {account}: {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-gmail-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    const RAW: &str = "Message-ID: <one@example.com>\r\n\
Date: Wed, 10 Jun 2026 09:00:00 -0700\r\n\
From: Alice Example <alice@example.com>\r\n\
To: David Wills <me@gmail.com>\r\n\
Subject: Lunch plans\r\n\
\r\n\
Want to grab lunch?\r\n";

    fn fetched(raw: &str, labels: &[&str]) -> FetchedMessage {
        FetchedMessage {
            label_ids: labels.iter().map(|s| s.to_string()).collect(),
            raw: raw.as_bytes().to_vec(),
        }
    }

    #[test]
    fn record_carries_labels_and_service() {
        let m = gmail_to_record(&fetched(RAW, &["INBOX", "CATEGORY_PERSONAL"]), "me@gmail.com")
            .unwrap();
        assert_eq!(m.source, "email");
        assert_eq!(m.guid, "<one@example.com>");
        assert_eq!(m.service, "me@gmail.com");
        assert_eq!(m.sender, "alice@example.com");
        assert!(!m.from_me);
        assert_eq!(m.labels, vec!["INBOX", "CATEGORY_PERSONAL"]);
        assert_eq!(m.subject, "Lunch plans");
    }

    #[test]
    fn sent_label_makes_from_me_despite_alias() {
        // The From address isn't the connected account (a send-as alias), but
        // the SENT label is authoritative.
        let raw = "Message-ID: <s@x>\r\nDate: Wed, 10 Jun 2026 09:00:00 -0700\r\n\
From: My Alias <alias@work.com>\r\nTo: bob@example.com\r\nSubject: hi\r\n\r\nyo\r\n";
        let m = gmail_to_record(&fetched(raw, &["SENT"]), "me@gmail.com").unwrap();
        assert!(m.from_me, "SENT label overrides the alias From");
    }

    #[test]
    fn base64url_raw_round_trips_through_get_raw_decode() {
        // The engine must accept Gmail's url-safe base64 with or without pad.
        let padded = B64URL.encode(RAW.as_bytes());
        let unpadded = padded.trim_end_matches('=');
        assert_eq!(B64URL.decode(&padded).unwrap(), RAW.as_bytes());
        assert_eq!(B64URL.decode(unpadded).unwrap(), RAW.as_bytes());
    }

    #[test]
    fn collect_without_an_account_is_a_silent_noop() {
        let v = temp_vault("noaccount");
        let stats = v.collect_gmail(Some(10)).unwrap();
        assert_eq!(stats.messages, 0);
        assert!(v.read_gmail_sync().is_none());
    }

    #[test]
    fn pull_without_an_account_is_a_clean_error() {
        // Unlike the silent scheduled pass, a user-triggered pull must say
        // why nothing happened.
        let v = temp_vault("pull-noaccount");
        let err = pull(&v).unwrap_err();
        assert!(err.to_string().contains("no Google account"), "{err}");
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("state");
        assert!(v.read_gmail_sync().is_none());
        let mut state = GmailSyncState {
            updated: "2026-06-11T10:00:00-07:00".into(),
            ..Default::default()
        };
        state.accounts.insert(
            "12345".into(),
            GmailAccountState {
                email: "me@gmail.com".into(),
                history_id: Some("99999".into()),
                backfill_page_token: Some("tok".into()),
                backfill_started: true,
                backfill_done: false,
                messages: 4200,
                error: None,
            },
        );
        v.write_gmail_sync(&state).unwrap();
        let loaded = v.read_gmail_sync().unwrap();
        let a = &loaded.accounts["12345"];
        assert_eq!(a.email, "me@gmail.com");
        assert_eq!(a.history_id.as_deref(), Some("99999"));
        assert_eq!(a.backfill_page_token.as_deref(), Some("tok"));
        assert_eq!(a.messages, 4200);
    }

    #[test]
    fn index_lists_accounts_and_writes_to_email_dir() {
        let v = temp_vault("index");
        let mut state = GmailSyncState {
            updated: "2026-06-11T10:00:00-07:00".into(),
            ..Default::default()
        };
        state.accounts.insert(
            "1".into(),
            GmailAccountState {
                email: "a@gmail.com".into(),
                backfill_done: true,
                messages: 100,
                ..Default::default()
            },
        );
        v.write_gmail_index(&state).unwrap();
        let md = fs::read_to_string(v.root().join("correspondence/email/gmail.md")).unwrap();
        assert!(md.contains("a@gmail.com"));
        assert!(md.contains("| a@gmail.com | 100 | complete |"));
    }

    #[test]
    fn budget_counts_down_and_unlimited_never_blocks() {
        let mut b = Budget(Some(2));
        assert!(b.take());
        assert!(b.take());
        assert!(!b.take());
        let mut u = Budget(None);
        for _ in 0..1000 {
            assert!(u.take());
        }
    }
}

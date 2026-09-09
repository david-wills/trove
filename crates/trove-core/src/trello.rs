//! Trello — Kanban board and task tracker by Atlassian.
//!
//! Pulls boards → lists → cards into the bound [`crate::tasks`] contract and a
//! raw firehose. Two destinations in one periodic pass:
//!
//! - **tasks contract** under `tasks/trello/` — snapshot + event stream via
//!   [`crate::tasks::apply_tasks_sync`], exactly like the Todoist/Asana legs.
//!   Each card becomes a [`Task`]; status is derived from list membership
//!   (`dueComplete` + `closed` + list-name heuristic).
//! - **raw firehose** under `tasks/trello/raw/YYYY-MM.jsonl` — full-fidelity
//!   board snapshot objects (as returned from the API), partitioned by card
//!   `dateLastActivity` month, upserted by card `id`.
//!
//! # Auth
//!
//! Two values pasted together in one TokenPaste field:
//! - **API key** — identifies the app; generated at
//!   <https://trello.com/power-ups/admin>, never changes, treated as public.
//! - **User token** — authorises access to the user's boards; generated via
//!   the "Token" link next to the key. Must be kept secret (0600 via the
//!   sync token store). Expires in 30 days (Trello's default) unless the user
//!   requests no expiry.
//!
//! Both are pasted as `key=<KEY> token=<TOKEN>` (space-separated KV pair) and
//! stored in `.trove/sync/trello` (0600). The pull parses them back out.
//!
//! # API
//!
//! REST v1 (`api.trello.com/1`). Every call includes `?key=…&token=…`.
//! Strategy: per the brief's "single full-snapshot call" note —
//! - `GET /1/members/me/boards?key=…&token=…&filter=open` — list open boards.
//! - `GET /1/boards/{id}?key=…&token=…&cards=open&lists=all&checklists=all`
//!   — one call per board returns lists, open cards, and checklists in one go.
//! - `GET /1/boards/{id}/cards/closed?key=…&token=…` — closed/archived cards
//!   per board (to detect done status regardless of list name).
//!
//! Rate limits: 100 req/10 s per token, 300 req/10 s per key — fine for
//! periodic polling of personal boards.
//!
//! # Status derivation
//!
//! Trello has no native "done" flag on cards. Status is derived:
//! 1. `closed=true` on the card → archived → treat as `"done"`.
//! 2. `dueComplete=true` → the user checked the due-date complete marker → `"done"`.
//! 3. List name (lowercased) contains `"done"` or `"complete"` → `"done"`.
//! 4. Otherwise → `"open"`.
//! The raw list name always lands in `extra.list_name` so readers can re-derive.
//!
//! # Card timestamp / id
//!
//! Trello card `id` is a 24-char hex Mongo ObjectId. The first 8 hex chars are a
//! Unix timestamp (seconds since epoch). We decode it to get `created`.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/trello.md.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::sync::oauth::TokenSet;
use crate::tasks::{ProjectInfo, Subtask, Task, TaskFate};
use crate::vault::Vault;

/// The source id: folder name under `tasks/`, and every task row's `source`.
const SOURCE: &str = "trello";

/// Raw firehose directory (full-fidelity API card objects).
const RAW_DIR: &str = "tasks/trello/raw";

/// Non-secret state file — reserved; no watermark needed (full-snapshot diff).
const SYNC_FILE: &str = ".trove/trello-sync.json";

/// The service id under `.trove/sync/` where key+token is stored (0600).
const SERVICE: &str = "trello";

const API_BASE: &str = "https://api.trello.com/1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs: 15 min, matching the other task sources.
pub const TRELLO_SYNC_SECS: u64 = 900;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::tasks::source_last_data(vault, SOURCE)
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "trello synced — {} open, {} completed, {} deleted",
                    c("open"),
                    c("completed"),
                    c("deleted"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "trello sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!(
            "Trello synced — {} open cards, {} completed, {} deleted",
            c("open"),
            c("completed"),
            c("deleted"),
        ),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "trello",
        name: "Trello",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Trello boards, lists, and cards into the unified task store \
                      every 15 minutes. Each card becomes a task; done status is derived from \
                      the card's list name and archived/due-complete flags.",
        domain: "tasks",
        vault_path: "tasks/trello/",
        toggleable: true,
        setup: &[
            "Connect with your Trello API key and user token on this card.",
            "Each sync snapshots your open cards; archived cards are treated as done.",
        ],
        caveats: "Status is inferred from list names containing \"Done\" or \"Complete\", \
                  the dueComplete flag, or whether the card is archived. Completed cards \
                  older than the first sync are not backfilled.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(TRELLO_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("trello"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — two values in one string: `key=K token=T`).

/// Parse the combined credential: `key=<KEY> token=<TOKEN>` or just
/// `<KEY> <TOKEN>` (space-separated). Returns `(key, token)`.
fn parse_creds(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    // Try "key=K token=T" / "key=K token=T" forms first.
    let extract = |prefix: &str| -> Option<String> {
        raw.split_whitespace()
            .find(|part| part.starts_with(prefix))
            .and_then(|part| part.strip_prefix(prefix))
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let (Some(k), Some(t)) = (extract("key="), extract("token=")) {
        return Some((k, t));
    }
    // Fallback: bare two tokens, whitespace-separated.
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() == 2 {
        return Some((parts[0].to_string(), parts[1].to_string()));
    }
    None
}

/// Verify that the credentials work by fetching `/1/members/me` and return
/// the connected username so we can label the account.
fn verify_creds(key: &str, token: &str) -> Result<String, FetchError> {
    let url = format!("{API_BASE}/members/me?key={key}&token={token}&fields=username,fullName");
    let resp = ureq::get(&url).timeout(HTTP_TIMEOUT).call();
    match resp {
        Ok(r) => {
            let v: Value =
                r.into_json().map_err(|e| FetchError::Other(format!("parse /members/me: {e}")))?;
            let username = v
                .get("username")
                .or_else(|| v.get("fullName"))
                .and_then(Value::as_str)
                .unwrap_or("trello")
                .to_string();
            Ok(username)
        }
        Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Err(FetchError::Other(format!(
                "HTTP {code}: {}",
                body.chars().take(200).collect::<String>()
            )))
        }
        Err(e) => Err(FetchError::Other(e.to_string())),
    }
}

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (key, token) = parse_creds(pasted)
        .ok_or_else(|| anyhow::anyhow!(
            "paste your Trello API key and token as: key=<YOUR_KEY> token=<YOUR_TOKEN>"
        ))?;
    let _username = verify_creds(&key, &token).map_err(|e| match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Trello rejected the key/token (401) — check both values from \
             https://trello.com/power-ups/admin"
        ),
        FetchError::Other(m) => anyhow::anyhow!("Trello verify failed: {m}"),
    })?;
    // Store as `key=K token=T` in the access_token slot (0600).
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: format!("key={key} token={token}"),
            refresh_token: None,
            token_type: Some("trello-creds".into()),
            scope: None,
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Trello".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. One line needed in
/// CONNECTIONS: `&crate::trello::CONNECTION,`
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "trello",
    display_name: "Trello",
    methods: &[ConnectMethod::TokenPaste {
        label: "Trello API key and token",
        help: "Paste both values from https://trello.com/power-ups/admin as: \
               key=YOUR_KEY token=YOUR_TOKEN",
        placeholder: "key=0123abc… token=def456…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["trello"],
    setup: &[
        "Open https://trello.com/power-ups/admin and create or select a Power-Up.",
        "Copy the API Key shown on that page.",
        "Click the \"Token\" link next to the key and approve access — copy the token.",
        "Paste both here as: key=YOUR_KEY token=YOUR_TOKEN",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The API calls the pull needs. A trait so tests drive logic with fixtures.
trait TrelloApi {
    /// `GET /1/members/me/boards?filter=open` → list of board objects.
    fn boards(&self) -> Result<Vec<Value>, FetchError>;

    /// `GET /1/boards/{id}?cards=open&lists=all&checklists=all` → board
    /// snapshot with embedded open cards, lists, and checklists.
    fn board_snapshot(&self, board_id: &str) -> Result<Value, FetchError>;

    /// `GET /1/boards/{id}/cards/closed` → archived cards on this board.
    fn closed_cards(&self, board_id: &str) -> Result<Vec<Value>, FetchError>;
}

/// Live HTTP client.
struct TrelloClient {
    key: String,
    token: String,
}

impl TrelloClient {
    fn new(key: String, token: String) -> Self {
        TrelloClient { key, token }
    }

    fn get_json(&self, path: &str) -> Result<Value, FetchError> {
        // Append key+token to whatever query string is already on `path`.
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!("{API_BASE}{path}{sep}key={}&token={}", self.key, self.token);
        match ureq::get(&url).timeout(HTTP_TIMEOUT).call() {
            Ok(r) => r
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
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
}

impl TrelloApi for TrelloClient {
    fn boards(&self) -> Result<Vec<Value>, FetchError> {
        let v = self.get_json("/members/me/boards?filter=open&fields=id,name,desc,closed,url")?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    fn board_snapshot(&self, board_id: &str) -> Result<Value, FetchError> {
        self.get_json(&format!(
            "/boards/{board_id}?cards=open&lists=all&checklists=all\
             &fields=id,name,desc,closed,url,dateLastActivity"
        ))
    }

    fn closed_cards(&self, board_id: &str) -> Result<Vec<Value>, FetchError> {
        let v = self.get_json(&format!("/boards/{board_id}/cards/closed"))?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    // Reserved; no live fields yet. The full-snapshot diff approach means no
    // watermark cursor is needed — kept as a file stub for future use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_sync: Option<String>,
}

impl Vault {
    fn read_trello_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_trello_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity card, partitioned by dateLastActivity month).

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawCard {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawCard {
    fn guid(&self) -> String {
        self.fields
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// Partition timestamp: the card's STABLE creation time (decoded from the
    /// Mongo ObjectId prefix), so a card that is edited or moved never migrates
    /// to a different month file. Falls back to `dateLastActivity` only for
    /// non-ObjectId ids (very old/external ids), then to wall-clock-now.
    fn partition_ts(&self) -> String {
        // Primary: stable creation time decoded from the ObjectId.
        if let Some(id) = self.fields.get("id").and_then(Value::as_str) {
            if let Some(ts) = id_to_created(id) {
                return ts;
            }
        }
        // Fallback for non-ObjectId ids: use dateLastActivity.
        if let Some(s) = self.fields.get("dateLastActivity").and_then(Value::as_str) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
        chrono::Utc::now().to_rfc3339()
    }
}

fn raw_card(value: &Value) -> Option<RawCard> {
    let obj = value.as_object()?;
    obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    Some(RawCard { fields: obj.clone() })
}

// ---------------------------------------------------------------------------
// Mapping helpers.

/// Pull a string field, trimmed; `None` when missing / non-string / empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// ISO-8601 UTC → RFC3339 local. Falls through verbatim on parse failure.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Decode the creation timestamp from the first 8 hex chars of a Trello id
/// (Mongo ObjectId convention: Unix seconds, big-endian).
fn id_to_created(id: &str) -> Option<String> {
    if id.len() < 8 {
        return None;
    }
    let hex = &id[..8];
    let secs = u32::from_str_radix(hex, 16).ok()? as i64;
    let dt = Utc.timestamp_opt(secs, 0).single()?;
    Some(dt.with_timezone(&Local).to_rfc3339())
}

/// Derive status from the card + list info.
fn card_status(card: &Value, list_name: Option<&str>, closed_ids: &std::collections::HashSet<String>) -> &'static str {
    // Archived card.
    if card.get("closed").and_then(Value::as_bool).unwrap_or(false) {
        return "done";
    }
    // Card id in the closed set (from the closed-cards endpoint).
    if let Some(id) = card.get("id").and_then(Value::as_str) {
        if closed_ids.contains(id) {
            return "done";
        }
    }
    // Due date marked complete.
    if card.get("dueComplete").and_then(Value::as_bool).unwrap_or(false) {
        return "done";
    }
    // List name heuristic.
    if let Some(name) = list_name {
        let lower = name.to_lowercase();
        if lower.contains("done") || lower.contains("complete") || lower.contains("finished") {
            return "done";
        }
    }
    "open"
}

/// Extract checklist items from the board's `checklists` array, building a
/// map from card id → Vec<Subtask>.
fn build_checklist_map(board: &Value) -> HashMap<String, Vec<Subtask>> {
    let mut map: HashMap<String, Vec<Subtask>> = HashMap::new();
    let checklists = board
        .get("checklists")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for cl in &checklists {
        let card_id = match cl.get("idCard").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let items = cl.get("checkItems").and_then(Value::as_array).cloned().unwrap_or_default();
        for item in &items {
            let title = item.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if title.is_empty() {
                continue;
            }
            let done = item
                .get("state")
                .and_then(Value::as_str)
                .map(|s| s == "complete")
                .unwrap_or(false);
            map.entry(card_id.clone()).or_default().push(Subtask { title, done, completed: None });
        }
    }
    map
}

/// Map one card API object → [`Task`]. Returns `None` if there is no id or name.
fn task_from_card(
    card: &Value,
    board_name: &str,
    list_map: &HashMap<String, String>,
    checklist_map: &HashMap<String, Vec<Subtask>>,
    closed_ids: &std::collections::HashSet<String>,
) -> Option<Task> {
    let obj = card.as_object()?;
    let id = obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_string();
    let title = str_opt(card, "name")?;

    // List (column) the card sits in.
    let list_id = obj.get("idList").and_then(Value::as_str).unwrap_or("").to_string();
    let list_name = list_map.get(&list_id).map(String::as_str);

    let status = card_status(card, list_name, closed_ids);

    let due = str_opt(card, "due").map(|s| to_local(&s));
    let all_day = false; // Trello due dates always include time.

    // Labels → tags (use the label name; colour in extra).
    let labels_arr = obj.get("labels").and_then(Value::as_array).cloned().unwrap_or_default();
    let tags: Vec<String> = labels_arr
        .iter()
        .filter_map(|l| l.get("name").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let subtasks = checklist_map.get(&id).cloned().unwrap_or_default();
    let created = id_to_created(&id);
    let modified = str_opt(card, "dateLastActivity").map(|s| to_local(&s));

    // Source-specific overflow → extra.
    let mut extra = Map::new();
    extra.insert("board".into(), Value::from(board_name));
    if let Some(ln) = list_name {
        extra.insert("list_name".into(), Value::from(ln));
    }
    extra.insert("list_id".into(), Value::from(list_id.as_str()));
    if let Some(url) = str_opt(card, "url").or_else(|| str_opt(card, "shortUrl")) {
        extra.insert("url".into(), Value::from(url));
    }
    if let Some(pos) = obj.get("pos") {
        extra.insert("pos".into(), pos.clone());
    }
    if !labels_arr.is_empty() {
        extra.insert("labels".into(), Value::Array(labels_arr));
    }
    if let Some(badges) = obj.get("badges") {
        extra.insert("badges".into(), badges.clone());
    }
    if let Some(members) = obj.get("idMembers") {
        extra.insert("idMembers".into(), members.clone());
    }

    Some(Task {
        source: SOURCE.into(),
        id,
        title,
        project: list_name.map(str::to_string).unwrap_or_default(),
        notes: str_opt(card, "desc").unwrap_or_default(),
        status: status.into(),
        priority: 0, // Trello has no native priority
        due,
        start: None,
        all_day,
        recurrence: None,
        tags,
        subtasks,
        created,
        modified,
        completed: None,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Raw upsert-into-partition (the todoist/asana idiom).

fn upsert_raw(vault: &Vault, rows: Vec<RawCard>) -> Result<u64> {
    use crate::store::Partition;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawCard>> = BTreeMap::new();
    for r in rows {
        let ts = r.partition_ts();
        let key = Partition::Month
            .key(&ts)
            .with_context(|| format!("trello: raw card ts {ts:?} has no month"))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<RawCard> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> =
            existing.iter().enumerate().map(|(i, r)| (r.guid(), i)).collect();
        for r in fresh {
            match idx.get(&r.guid()).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(r.guid(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| a.partition_ts().cmp(&b.partition_ts()).then_with(|| a.guid().cmp(&b.guid())));
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Load key+token from the secret store.
fn load_creds(vault: &Vault) -> Result<(String, String)> {
    let stored = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Trello is not connected — add your API key and token in the Integrations tab")?;
    parse_creds(&stored)
        .ok_or_else(|| anyhow::anyhow!("trello: stored creds are malformed — reconnect"))
}

/// Resolve credentials and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let (key, token) = load_creds(vault)?;
    let client = TrelloClient::new(key, token);
    pull_with(vault, &client, Local::now())
}

/// The pull body over an injected API + clock.
fn pull_with(vault: &Vault, api: &impl TrelloApi, now: DateTime<Local>) -> Result<PullOutcome> {
    let mut state = vault.read_trello_sync();

    // Fetch all open boards.
    let boards = api.boards().map_err(fetch_err)?;

    let mut all_open_tasks: Vec<Task> = Vec::new();
    let mut all_raw: Vec<RawCard> = Vec::new();
    let mut all_projects: Vec<ProjectInfo> = Vec::new();
    // Map of done card id → archive timestamp RFC3339 string (if known) across
    // all boards, so the fate closure can provide a closer-to-true completion
    // time for archived cards (defect fix: use dateLastActivity instead of None).
    let mut done_ids: HashMap<String, Option<String>> = HashMap::new();

    for board in &boards {
        let board_id = match board.get("id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let board_name =
            board.get("name").and_then(Value::as_str).unwrap_or("").to_string();

        // Full board snapshot (open cards + all lists + checklists).
        let snapshot = api.board_snapshot(&board_id).map_err(fetch_err)?;

        // Build list id → name map.
        let lists_arr = snapshot
            .get("lists")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let list_map: HashMap<String, String> = lists_arr
            .iter()
            .filter_map(|l| {
                let id = l.get("id").and_then(Value::as_str)?.to_string();
                let name = l.get("name").and_then(Value::as_str)?.to_string();
                Some((id, name))
            })
            .collect();

        // Closed (archived) cards on this board — raw firehose + done set.
        let closed_cards = api.closed_cards(&board_id).map_err(fetch_err)?;
        // Build a local set of closed ids for card_status lookups on this board.
        let closed_ids: std::collections::HashSet<String> = closed_cards
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        // All archived cards are "done" for the fate closure; carry their
        // dateLastActivity (≈ archive time) so the completion event has a real ts.
        for c in &closed_cards {
            if let Some(id) = c.get("id").and_then(Value::as_str) {
                // dateLastActivity ≈ archive time; convert to local RFC3339.
                let ts: Option<String> = c
                    .get("dateLastActivity")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.with_timezone(&Local).to_rfc3339());
                done_ids.entry(id.to_string()).or_insert(ts);
            }
        }

        // Checklist map (card id → subtasks).
        let checklist_map = build_checklist_map(&snapshot);

        // Open cards returned by the board snapshot (cards=open filter).
        // We still evaluate status so we can correctly file "Done-list" cards
        // into done_ids and skip them from fresh_open.
        let cards_arr = snapshot
            .get("cards")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for card in &cards_arr {
            let list_id = card.get("idList").and_then(Value::as_str).unwrap_or("");
            let list_name = list_map.get(list_id).map(String::as_str);
            let status = card_status(card, list_name, &closed_ids);

            if let Some(raw) = raw_card(card) {
                all_raw.push(raw);
            }

            if status == "done" {
                // The card is "done" (dueComplete flag or Done-list heuristic)
                // even though the API returned it in the open set. No reliable
                // archive time is available for these (completion time unknown).
                if let Some(id) = card.get("id").and_then(Value::as_str) {
                    done_ids.entry(id.to_string()).or_insert(None);
                }
            } else if let Some(task) = task_from_card(card, &board_name, &list_map, &checklist_map, &closed_ids) {
                all_open_tasks.push(task);
            }
        }

        // Raw firehose: upsert closed cards too (full fidelity).
        for card in &closed_cards {
            if let Some(raw) = raw_card(card) {
                all_raw.push(raw);
            }
        }

        // Each open list is a "project" in the tasks contract.
        for l in lists_arr.iter().filter(|l| !l.get("closed").and_then(Value::as_bool).unwrap_or(false)) {
            if let (Some(lid), Some(lname)) = (
                l.get("id").and_then(Value::as_str),
                l.get("name").and_then(Value::as_str),
            ) {
                all_projects.push(ProjectInfo { id: lid.to_string(), name: lname.to_string() });
            }
        }
    }

    let raw_new = upsert_raw(vault, all_raw)?;

    // --- diff into the bound task contract ---
    // Fate: a task that vanishes from fresh is "done" if its id is in done_ids
    // (archived or moved to a Done list), otherwise Deleted.
    // For archived cards, carry the dateLastActivity as the completion timestamp
    // (≈ archive time) so the event stream has better-than-wall-clock fidelity.
    let stats = vault
        .apply_tasks_sync(SOURCE, &all_projects, all_open_tasks, |t| {
            if let Some(maybe_ts) = done_ids.get(&t.id) {
                TaskFate::Completed(maybe_ts.clone())
            } else {
                TaskFate::Deleted
            }
        })
        .context("trello: applying task sync")?;

    state.last_sync = Some(now.to_rfc3339());
    vault.write_trello_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("open", stats.open);
    counts.insert("completed", stats.completed);
    counts.insert("deleted", stats.deleted);
    counts.insert("created", stats.created);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!("{} open Trello cards", stats.open),
        counts,
    })
}

fn fetch_err(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Trello rejected the key/token (401) — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Trello fetch failed: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-trello-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn now() -> DateTime<Local> {
        DateTime::parse_from_rfc3339("2026-06-14T12:00:00-07:00")
            .unwrap()
            .with_timezone(&Local)
    }

    // ---- fixtures ---------------------------------------------------------

    fn board_json(id: &str, name: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "desc": "",
            "closed": false,
            "url": format!("https://trello.com/b/{id}/"),
            "dateLastActivity": "2026-06-10T10:00:00.000Z"
        })
    }

    fn list_json(id: &str, name: &str, board_id: &str, closed: bool) -> Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "closed": closed,
            "idBoard": board_id,
            "pos": 16384
        })
    }

    /// A card with a due date and two labels. The id starts with `5e9c4a00`
    /// which decodes to 2020-04-19 (valid ObjectId prefix for testing).
    fn card_with_due(id: &str, list_id: &str, board_id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": "Finish the report",
            "desc": "quarterly summary",
            "closed": false,
            "dueComplete": false,
            "idList": list_id,
            "idBoard": board_id,
            "idMembers": [],
            "labels": [
                {"id": "lbl1", "name": "work", "color": "red"},
                {"id": "lbl2", "name": "urgent", "color": "orange"}
            ],
            "idLabels": ["lbl1", "lbl2"],
            "due": "2026-06-20T17:00:00.000Z",
            "start": null,
            "dateLastActivity": "2026-06-10T12:00:00.000Z",
            "badges": {"checkItems": 2, "checkItemsChecked": 1, "comments": 3},
            "url": "https://trello.com/c/abc123/finish-the-report",
            "shortUrl": "https://trello.com/c/abc123",
            "pos": 32768
        })
    }

    fn card_archived(id: &str, list_id: &str, board_id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": "Old task",
            "desc": "",
            "closed": true,
            "dueComplete": false,
            "idList": list_id,
            "idBoard": board_id,
            "idMembers": [],
            "labels": [],
            "idLabels": [],
            "due": null,
            "dateLastActivity": "2026-05-01T08:00:00.000Z",
            "url": "https://trello.com/c/zzz/old-task",
            "pos": 65536
        })
    }

    fn card_done_complete(id: &str, list_id: &str, board_id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": "Deploy to prod",
            "desc": "",
            "closed": false,
            "dueComplete": true,
            "idList": list_id,
            "idBoard": board_id,
            "idMembers": [],
            "labels": [],
            "idLabels": [],
            "due": "2026-06-12T17:00:00.000Z",
            "dateLastActivity": "2026-06-12T18:00:00.000Z",
            "url": "https://trello.com/c/def456/deploy",
            "pos": 16384
        })
    }

    fn checklist_json(id: &str, card_id: &str, board_id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": "Steps",
            "idCard": card_id,
            "idBoard": board_id,
            "checkItems": [
                {"id": "ci1", "name": "Write tests", "state": "complete", "pos": 1},
                {"id": "ci2", "name": "Review PR", "state": "incomplete", "pos": 2}
            ]
        })
    }

    fn board_snapshot_json(
        board_id: &str,
        board_name: &str,
        lists: Vec<Value>,
        cards: Vec<Value>,
        checklists: Vec<Value>,
    ) -> Value {
        serde_json::json!({
            "id": board_id,
            "name": board_name,
            "desc": "",
            "closed": false,
            "url": format!("https://trello.com/b/{board_id}/"),
            "dateLastActivity": "2026-06-10T10:00:00.000Z",
            "lists": lists,
            "cards": cards,
            "checklists": checklists
        })
    }

    // ---- mock API ---------------------------------------------------------

    struct MockApi {
        boards: RefCell<Vec<Value>>,
        snapshots: RefCell<HashMap<String, Value>>,
        closed: RefCell<HashMap<String, Vec<Value>>>,
    }

    impl MockApi {
        fn new(boards: Vec<Value>) -> Self {
            MockApi {
                boards: RefCell::new(boards),
                snapshots: RefCell::new(HashMap::new()),
                closed: RefCell::new(HashMap::new()),
            }
        }

        fn add_snapshot(&self, board_id: &str, snap: Value) {
            self.snapshots.borrow_mut().insert(board_id.to_string(), snap);
        }

        fn add_closed(&self, board_id: &str, cards: Vec<Value>) {
            self.closed.borrow_mut().insert(board_id.to_string(), cards);
        }
    }

    impl TrelloApi for MockApi {
        fn boards(&self) -> Result<Vec<Value>, FetchError> {
            Ok(self.boards.borrow().clone())
        }

        fn board_snapshot(&self, board_id: &str) -> Result<Value, FetchError> {
            self.snapshots
                .borrow()
                .get(board_id)
                .cloned()
                .ok_or_else(|| FetchError::Other(format!("no snapshot for {board_id}")))
        }

        fn closed_cards(&self, board_id: &str) -> Result<Vec<Value>, FetchError> {
            Ok(self.closed.borrow().get(board_id).cloned().unwrap_or_default())
        }
    }

    // ---- pure mapping tests -----------------------------------------------

    #[test]
    fn maps_card_with_due_labels_and_list_name() {
        let list_map: HashMap<String, String> =
            [("L1".to_string(), "In Progress".to_string())].into_iter().collect();
        let closed = std::collections::HashSet::new();
        let cmap: HashMap<String, Vec<Subtask>> = HashMap::new();
        let card = card_with_due("5e9c4a00abc123def456789a", "L1", "B1");
        let task = task_from_card(&card, "My Board", &list_map, &cmap, &closed).unwrap();
        assert_eq!(task.source, "trello");
        assert_eq!(task.id, "5e9c4a00abc123def456789a");
        assert_eq!(task.title, "Finish the report");
        assert_eq!(task.notes, "quarterly summary");
        assert_eq!(task.project, "In Progress", "project = list name");
        assert_eq!(task.status, "open");
        assert_eq!(task.tags, vec!["work", "urgent"]);
        // due present and parses.
        let due = task.due.as_deref().unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(due).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-20T17:00:00.000Z").unwrap().timestamp(),
        );
        assert!(!task.all_day);
        // extra carries board + list_name + url + badges.
        assert_eq!(task.extra.get("board").and_then(Value::as_str), Some("My Board"));
        assert_eq!(task.extra.get("list_name").and_then(Value::as_str), Some("In Progress"));
        assert!(task.extra.get("url").and_then(Value::as_str).unwrap().contains("abc123"));
        assert!(task.extra.contains_key("badges"));
        assert!(task.created.is_some(), "created decoded from ObjectId");
    }

    #[test]
    fn archived_card_maps_to_done() {
        let list_map: HashMap<String, String> = HashMap::new();
        let closed = std::collections::HashSet::new();
        let cmap: HashMap<String, Vec<Subtask>> = HashMap::new();
        let card = card_archived("arc1", "L1", "B1");
        let task = task_from_card(&card, "B", &list_map, &cmap, &closed).unwrap();
        assert_eq!(task.status, "done", "closed=true → done");
    }

    #[test]
    fn due_complete_card_maps_to_done() {
        let list_map: HashMap<String, String> =
            [("L1".to_string(), "In Progress".to_string())].into_iter().collect();
        let closed = std::collections::HashSet::new();
        let cmap: HashMap<String, Vec<Subtask>> = HashMap::new();
        let card = card_done_complete("dc1", "L1", "B1");
        let task = task_from_card(&card, "B", &list_map, &cmap, &closed).unwrap();
        assert_eq!(task.status, "done", "dueComplete=true → done");
    }

    #[test]
    fn done_list_name_maps_to_done() {
        let list_map: HashMap<String, String> =
            [("L1".to_string(), "Done".to_string())].into_iter().collect();
        let closed = std::collections::HashSet::new();
        let cmap: HashMap<String, Vec<Subtask>> = HashMap::new();
        let card = card_with_due("abc", "L1", "B1");
        let task = task_from_card(&card, "B", &list_map, &cmap, &closed).unwrap();
        assert_eq!(task.status, "done", "list named 'Done' → done");
    }

    #[test]
    fn checklist_items_become_subtasks() {
        let list_map: HashMap<String, String> =
            [("L1".to_string(), "Doing".to_string())].into_iter().collect();
        let closed = std::collections::HashSet::new();
        let board = serde_json::json!({
            "checklists": [checklist_json("cl1", "card1", "B1")]
        });
        let cmap = build_checklist_map(&board);
        let card = card_with_due("card1", "L1", "B1");
        let task = task_from_card(&card, "B", &list_map, &cmap, &closed).unwrap();
        assert_eq!(task.subtasks.len(), 2);
        assert_eq!(task.subtasks[0].title, "Write tests");
        assert!(task.subtasks[0].done, "state=complete → done");
        assert_eq!(task.subtasks[1].title, "Review PR");
        assert!(!task.subtasks[1].done, "state=incomplete → not done");
    }

    #[test]
    fn id_to_created_decodes_objectid_prefix() {
        // "5e9c4a00..." → 2020-04-19 (0x5E9C4A00 = 1587254784)
        let ts = id_to_created("5e9c4a00abc123def456789a").unwrap();
        assert!(ts.starts_with("2020-04-19"), "decoded: {ts}");
        assert!(id_to_created("abc").is_none(), "short id → None");
    }

    #[test]
    fn parse_creds_accepts_key_token_pairs() {
        let (k, t) = parse_creds("key=abc123 token=def456").unwrap();
        assert_eq!(k, "abc123");
        assert_eq!(t, "def456");
        // Bare two tokens.
        let (k2, t2) = parse_creds("abc123 def456").unwrap();
        assert_eq!(k2, "abc123");
        assert_eq!(t2, "def456");
        // Malformed.
        assert!(parse_creds("onlyone").is_none());
        assert!(parse_creds("").is_none());
    }

    // ---- raw upsert -------------------------------------------------------

    #[test]
    fn raw_upsert_dedupes_by_id() {
        let v = temp_vault("rawdedup");
        // id "5e9c4a00..." decodes to 2020-04-19, so the raw file is 2020-04.jsonl
        // (partition_ts now uses stable creation time, not mutable dateLastActivity).
        let card = card_with_due("5e9c4a00abc123def456789a", "L1", "B1");
        let rows = vec![raw_card(&card).unwrap()];
        let new1 = upsert_raw(&v, rows.clone()).unwrap();
        assert_eq!(new1, 1, "first upsert: 1 new row");
        let new2 = upsert_raw(&v, rows).unwrap();
        assert_eq!(new2, 0, "re-upsert same id: 0 new rows");
        // Check the file has exactly one line.
        let raw_file = v.root().join("tasks/trello/raw/2020-04.jsonl");
        assert!(raw_file.exists(), "raw file at creation-time month, not activity month");
        let body = std::fs::read_to_string(&raw_file).unwrap();
        assert_eq!(body.lines().count(), 1, "dedup → one line");
    }

    #[test]
    fn raw_upsert_dedupes_across_month_boundary() {
        // Regression for the cross-month duplication bug: if a card's
        // dateLastActivity changes to a different month, the card must still
        // land in exactly one month file (the creation-time month) with the
        // latest data — not duplicated.
        let v = temp_vault("crossmonth");

        // id "5e9c4a00..." → created 2020-04-19 → partition month 2020-04.
        // First upsert: dateLastActivity in May.
        let mut card_may = card_with_due("5e9c4a00abc123def456789a", "L1", "B1");
        if let Some(obj) = card_may.as_object_mut() {
            obj.insert("dateLastActivity".into(), serde_json::json!("2020-05-15T10:00:00.000Z"));
            obj.insert("name".into(), serde_json::json!("May version"));
        }
        let new1 = upsert_raw(&v, vec![raw_card(&card_may).unwrap()]).unwrap();
        assert_eq!(new1, 1, "first upsert: 1 new row");

        // Second upsert: dateLastActivity bumped to June (simulates edit/move).
        let mut card_jun = card_with_due("5e9c4a00abc123def456789a", "L1", "B1");
        if let Some(obj) = card_jun.as_object_mut() {
            obj.insert("dateLastActivity".into(), serde_json::json!("2020-06-20T08:00:00.000Z"));
            obj.insert("name".into(), serde_json::json!("June version"));
        }
        let new2 = upsert_raw(&v, vec![raw_card(&card_jun).unwrap()]).unwrap();
        assert_eq!(new2, 0, "re-upsert: 0 new rows (same id, even across months)");

        // The card must exist in exactly one month file: 2020-04 (creation month).
        let apr_file = v.root().join("tasks/trello/raw/2020-04.jsonl");
        let may_file = v.root().join("tasks/trello/raw/2020-05.jsonl");
        let jun_file = v.root().join("tasks/trello/raw/2020-06.jsonl");

        assert!(apr_file.exists(), "card lives in creation-month file");
        assert!(!may_file.exists(), "no stale May file (card migrated)");
        assert!(!jun_file.exists(), "no spurious June file");

        let body = std::fs::read_to_string(&apr_file).unwrap();
        assert_eq!(body.lines().count(), 1, "exactly one line in 2020-04.jsonl");
        // The stored data is the latest version.
        assert!(body.contains("June version"), "latest version stored: {body}");
    }

    // ---- full pull --------------------------------------------------------

    #[test]
    fn full_pull_writes_snapshot_raw_and_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(vec![board_json("B1", "Work")]);
        api.add_snapshot(
            "B1",
            board_snapshot_json(
                "B1",
                "Work",
                vec![
                    list_json("L1", "To Do", "B1", false),
                    list_json("L2", "Done", "B1", false),
                ],
                vec![
                    card_with_due("5e9c4a00abc123def456789a", "L1", "B1"),
                    card_done_complete("5e9c4a00bcd234ef56789ab1", "L2", "B1"),
                ],
                vec![],
            ),
        );

        let out = pull_with(&v, &api, now()).unwrap();
        assert_eq!(out.counts.get("open"), Some(&1), "1 open card (the done one excluded)");
        assert_eq!(out.counts.get("raw"), Some(&2), "2 raw cards total");

        // Tasks snapshot.
        let snap = v.load_tasks_snapshot(SOURCE).unwrap();
        assert_eq!(snap.len(), 1, "snapshot holds only open tasks");
        assert_eq!(snap[0].title, "Finish the report");
        assert_eq!(snap[0].project, "To Do");
        assert_eq!(snap[0].status, "open");

        // Raw file exists at the cards' creation-time month (ObjectId decodes to 2020-04).
        assert!(v.root().join("tasks/trello/raw/2020-04.jsonl").exists());

        // Cursor advanced.
        let state = v.read_trello_sync();
        assert!(state.last_sync.is_some());
    }

    #[test]
    fn task_that_vanishes_is_deleted() {
        let v = temp_vault("vanish");
        let api1 = MockApi::new(vec![board_json("B1", "Work")]);
        api1.add_snapshot(
            "B1",
            board_snapshot_json(
                "B1",
                "Work",
                vec![list_json("L1", "To Do", "B1", false)],
                vec![card_with_due("5e9c4a00abc123def456789a", "L1", "B1")],
                vec![],
            ),
        );
        pull_with(&v, &api1, now()).unwrap();
        assert_eq!(v.load_tasks_snapshot(SOURCE).unwrap().len(), 1);

        // Second pull: board still exists but the card is gone.
        let api2 = MockApi::new(vec![board_json("B1", "Work")]);
        api2.add_snapshot(
            "B1",
            board_snapshot_json("B1", "Work", vec![list_json("L1", "To Do", "B1", false)], vec![], vec![]),
        );
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("deleted"), Some(&1));
        assert!(v.load_tasks_snapshot(SOURCE).unwrap().is_empty());
    }

    #[test]
    fn connection_stores_creds_0600_and_status() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "key=abc token=def".into(),
                refresh_token: None,
                token_type: Some("trello-creds".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].key, "trello");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("key=abc") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret file must be 0600");
                }
            }
            assert!(found, "creds stored under .trove/sync");
        }

        def_disconnect(&v, "trello").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn pull_without_credentials_returns_clear_error() {
        let v = temp_vault("nocreds");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn cursor_back_compat_empty_deserializes() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_sync.is_none());
        let with_sync: SyncState =
            serde_json::from_str(r#"{"last_sync":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert!(with_sync.last_sync.is_some());
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "trello");
    }

    #[test]
    fn raw_card_roundtrips_full_fidelity() {
        let card = card_with_due("5e9c4a00abc123def456789a", "L1", "B1");
        let r = raw_card(&card).unwrap();
        assert_eq!(r.guid(), "5e9c4a00abc123def456789a");
        let line = serde_json::to_string(&r).unwrap();
        // No synthetic guid column on disk.
        assert!(!line.contains("\"guid\":"), "no synthetic guid column");
        let back: RawCard = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r, "round-trips identically");
        // The raw layer keeps all source fields.
        assert!(line.contains("\"badges\""), "badges kept in raw");
        assert!(line.contains("quarterly summary"), "desc kept in raw");
    }
}

//! Literal — social reading tracker via the official GraphQL API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/literal.md.
//!
//! A **Periodic** cloud pull (hourly) against the Literal GraphQL endpoint at
//! `https://literal.club/graphql/`, writing into the bound [`crate::reading`]
//! contract — the same domain as Hardcover and Readwise.  The brief originally
//! mapped to media-plays; rerouted to `reading.Item` per REUSE MAP guidance that
//! book-trackers fit reading better (state/progress fields are the right shape).
//!
//! ## Auth
//!
//! Literal's only published auth path is a `login` mutation (email + password →
//! JWT Bearer token).  No permanent API key is available from the settings page.
//! To avoid expiry surprises the connector stores the email+password composite
//! and re-authenticates at the start of every sync.  The composite is pasted as a
//! single string `email:password` and stored under `.trove/sync/` (0600).
//!
//! ## What is collected
//!
//! - `myReadingStates` (all books with their status and `createdAt`) — one
//!   `reading.Item` per book, `guid = "lit-<reading-state-id>"`, `ts = createdAt`.
//!   State mapping: `WANTS_TO_READ → "saved"`, `IS_READING → "saved"` (in
//!   progress), `FINISHED → "read"`, `DROPPED → "archived"`.
//! - Raw layer unconditionally under `reading/literal/raw/YYYY-MM.jsonl`.
//!
//! ## Pagination / cursor
//!
//! `myReadingStates` returns all states at once (no pagination documented).  The
//! sync is a full drain each time; new guids are appended; existing guids are
//! skipped (the reading contract is append-only).  A non-secret cursor file
//! `.trove/literal-sync.json` records the last sync timestamp only.
//!
//! ## Graceful degradation
//!
//! On HTTP or GraphQL errors the pull returns a descriptive Err — the caller logs
//! it and the periodic runner retries next tick.  Schema drift (missing fields)
//! silently skips the row rather than crashing the whole sync.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::reading::Item;
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract-layer items; raw layer alongside.
const DIR: &str = "reading/literal";
const RAW_DIR: &str = "reading/literal/raw";
/// Non-secret rebuildable cursor.
const SYNC_FILE: &str = ".trove/literal-sync.json";
/// Service id under `.trove/sync/` for the stored composite credential.
const SERVICE: &str = "literal";
/// Literal GraphQL endpoint.
const API_BASE: &str = "https://literal.club/graphql/";
/// HTTP timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs.
pub const LITERAL_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!("literal synced — {} library entries", c("items"))
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("literal sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "literal",
        name: "Literal",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Your Literal reading library and statuses, pulled via the official \
                      GraphQL API into the unified reading store. First sync backfills your \
                      full library; later syncs append new entries.",
        domain: "reading",
        vault_path: "reading/literal/",
        toggleable: true,
        setup: &[
            "Connect with your Literal account email and password on this card.",
            "Credentials are stored locally and used only to obtain a session token.",
        ],
        caveats: "Uses the official but publicly undocumented GraphQL API — Literal provides \
                  no permanent API key, so email + password are stored to refresh the session \
                  each sync. Schema changes may break parsing without notice.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(LITERAL_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("literal"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection — TokenPaste of composite `email:password`.

fn def_connect(vault: &Vault, cred: &str) -> Result<()> {
    let cred = cred.trim();
    if cred.is_empty() {
        bail!("empty credential — paste your Literal email and password as `email:password`");
    }
    // Validate by attempting a login right now.
    let (email, password) = split_cred(cred)?;
    match do_login(&email, &password) {
        Ok(_token) => {}
        Err(e) => bail!("Literal login failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: cred.to_string(),
            refresh_token: None,
            token_type: Some("composite".into()),
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
    if let Some(ts) = vault.load_sync_token(SERVICE)? {
        let label = split_cred(&ts.access_token)
            .map(|(email, _)| email)
            .unwrap_or_else(|_| "Literal".to_string());
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "literal",
    display_name: "Literal",
    methods: &[ConnectMethod::TokenPaste {
        label: "Literal credentials",
        help: "Paste your Literal account email and password separated by a colon: \
               `email@example.com:yourpassword`. They are stored locally (0600) and \
               used only to obtain a session token each sync — never sent anywhere \
               but Literal's own API.",
        placeholder: "you@example.com:yourpassword",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["literal"],
    setup: &[
        "Enter your Literal account email and password separated by a colon.",
        "Example: you@example.com:yourpassword",
        "Your credentials are stored locally and never sent anywhere but Literal.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP / GraphQL layer — trait-injectable so tests run offline.

/// GraphQL errors: 401 signals reconnect; everything else is a message.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized — reconnect your Literal account"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Injectable API for tests.
trait LiteralApi {
    /// Call the `login` mutation; return the JWT on success.
    fn login(&self, email: &str, password: &str) -> Result<String, FetchError>;
    /// Call `myReadingStates`; return the raw array of state objects.
    fn my_reading_states(&self, token: &str) -> Result<Vec<Value>, FetchError>;
}

/// Production client.
struct LiteralClient {
    base: String,
}

impl LiteralClient {
    fn new(base: String) -> Self {
        LiteralClient { base }
    }

    /// POST a GraphQL query/mutation; return the parsed response body or an
    /// error.  The `Authorization` header is omitted when `token` is `None`.
    fn gql_post(&self, token: Option<&str>, body: Value) -> Result<Value, FetchError> {
        let mut req = ureq::post(&self.base)
            .timeout(HTTP_TIMEOUT)
            .set("Content-Type", "application/json");
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        match req.send_json(body) {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                if let Some(Value::Array(errs)) = v.get("errors") {
                    if !errs.is_empty() {
                        let msg = errs
                            .first()
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown GraphQL error");
                        let msg_lower = msg.to_lowercase();
                        if msg_lower.contains("unauthorized")
                            || msg_lower.contains("not authenticated")
                        {
                            return Err(FetchError::Unauthorized);
                        }
                        return Err(FetchError::Other(msg.to_string()));
                    }
                }
                Ok(v)
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
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

impl LiteralApi for LiteralClient {
    fn login(&self, email: &str, password: &str) -> Result<String, FetchError> {
        let body = serde_json::json!({
            "query": r#"
                mutation login($email: String!, $password: String!) {
                    login(email: $email, password: $password) {
                        token
                    }
                }
            "#,
            "variables": { "email": email, "password": password }
        });
        let v = self.gql_post(None, body)?;
        let token = v
            .get("data")
            .and_then(|d| d.get("login"))
            .and_then(|l| l.get("token"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string());
        token.ok_or(FetchError::Unauthorized)
    }

    fn my_reading_states(&self, token: &str) -> Result<Vec<Value>, FetchError> {
        let body = serde_json::json!({
            "query": r#"
                query myReadingStates {
                    myReadingStates {
                        id
                        status
                        bookId
                        createdAt
                        book {
                            id
                            title
                            subtitle
                            slug
                            cover
                            isbn10
                            isbn13
                            pageCount
                            publishedDate
                            description
                            authors {
                                name
                            }
                        }
                    }
                }
            "#
        });
        let v = self.gql_post(Some(token), body)?;
        let states = v
            .get("data")
            .and_then(|d| d.get("myReadingStates"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(states)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_literal_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_literal_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping helpers.

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// RFC3339 or ISO-8601 datetime → RFC3339 local.  Bare dates → midnight UTC.
/// Unparseable values pass through verbatim so callers can detect them.
fn to_local(s: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Local).to_rfc3339();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return ndt.and_utc().with_timezone(&Local).to_rfc3339();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return ndt.and_utc().with_timezone(&Local).to_rfc3339();
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            return dt.and_utc().with_timezone(&Local).to_rfc3339();
        }
    }
    s.to_string()
}

/// Map Literal `status` string to the reading contract `state`.
fn status_to_state(status: &str) -> &'static str {
    match status {
        "FINISHED" => "read",
        "DROPPED" => "archived",
        _ => "saved", // WANTS_TO_READ, IS_READING, NONE, unknown
    }
}

/// Extract primary author name from the `authors` array `[{name: "..."}]`.
/// Field shape confirmed from literal.club/developers: `authors [Author!]!`
/// where `Author.name` is the only documented string field.
fn author_from_book(book: &Value) -> String {
    book.get("authors")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// `readingState` object → a contract [`Item`].  Returns `None` when the state
/// has no usable `id` or no `createdAt` that can be partitioned by month.
fn item_from(rs: &Value) -> Option<Item> {
    let id = str_field(rs, "id");
    if id.is_empty() {
        return None;
    }

    // ts = createdAt (when the reading status was set) — always present per schema.
    let raw_ts = str_field(rs, "createdAt");
    if raw_ts.is_empty() {
        return None;
    }
    let ts = to_local(&raw_ts);
    // Partition must resolve; otherwise the row cannot be filed.
    Partition::Month.key(&ts)?;

    let status = str_field(rs, "status");
    let state = status_to_state(&status).to_string();

    let book = rs.get("book").unwrap_or(&Value::Null);
    let title = str_field(book, "title");
    let author = author_from_book(book);

    let mut extra: Map<String, Value> = Map::new();
    extra.insert("status".into(), Value::String(status.clone()));

    let slug = str_field(book, "slug");
    if !slug.is_empty() {
        extra.insert("slug".into(), Value::String(slug));
    }
    let cover = str_field(book, "cover");
    if !cover.is_empty() {
        extra.insert("cover".into(), Value::String(cover));
    }
    let isbn10 = str_field(book, "isbn10");
    if !isbn10.is_empty() {
        extra.insert("isbn10".into(), Value::String(isbn10));
    }
    let isbn13 = str_field(book, "isbn13");
    if !isbn13.is_empty() {
        extra.insert("isbn13".into(), Value::String(isbn13));
    }
    let subtitle = str_field(book, "subtitle");
    if !subtitle.is_empty() {
        extra.insert("subtitle".into(), Value::String(subtitle));
    }
    if let Some(pc) = book.get("pageCount").and_then(Value::as_i64) {
        extra.insert("page_count".into(), Value::from(pc));
    }
    let pub_date = str_field(book, "publishedDate");
    if !pub_date.is_empty() {
        extra.insert("published_date".into(), Value::String(pub_date));
    }
    let book_id = str_field(book, "id");
    if !book_id.is_empty() {
        extra.insert("book_id".into(), Value::String(book_id));
    }

    Some(Item {
        ts,
        source: "literal".into(),
        guid: format!("lit-{id}"),
        url: String::new(),
        title,
        author,
        site: String::new(),
        feed: String::new(),
        excerpt: String::new(),
        tags: Vec::new(),
        state,
        progress: if status == "FINISHED" { Some(100) } else { None },
        read_at: String::new(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write helpers (deduped, partitioned).

fn write_items(vault: &Vault, pairs: Vec<(Item, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Collect already-seen guids to dedupe across syncs.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_items: Vec<Item> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (item, raw_val) in pairs {
        if item.guid.is_empty() || !seen.insert(item.guid.clone()) {
            continue;
        }
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val });
        new_items.push(item);
    }

    contract.append(&new_items, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_items.len() as u64)
}

// ---------------------------------------------------------------------------
// Credential parsing.

/// Split `email:password` composite; the password may itself contain colons.
fn split_cred(cred: &str) -> Result<(String, String)> {
    let mut parts = cred.splitn(2, ':');
    let email = parts.next().unwrap_or("").trim().to_string();
    let password = parts.next().unwrap_or("").to_string();
    if email.is_empty() || password.is_empty() {
        bail!(
            "credentials must be in the format `email:password` — \
             paste your Literal email and password separated by a colon"
        );
    }
    Ok((email, password))
}

/// Perform the login mutation (production path — used at connect time).
fn do_login(email: &str, password: &str) -> Result<String> {
    let client = LiteralClient::new(API_BASE.to_string());
    client.login(email, password).map_err(|e| anyhow::anyhow!("{e}"))
}

// ---------------------------------------------------------------------------
// The pull.

/// Load credentials and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let cred = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Literal is not connected — add your email:password in the Integrations tab")?;
    let (email, password) = split_cred(&cred)?;
    let client = LiteralClient::new(API_BASE.to_string());
    pull_with(vault, &client, &email, &password)
}

/// Testable pull body over an injected API.
fn pull_with(
    vault: &Vault,
    api: &impl LiteralApi,
    email: &str,
    password: &str,
) -> Result<PullOutcome> {
    // Obtain a fresh JWT every sync to avoid token expiry.
    let token = api
        .login(email, password)
        .map_err(|e| anyhow::anyhow!("Literal login failed: {e}"))?;

    let states = api
        .my_reading_states(&token)
        .map_err(|e| anyhow::anyhow!("Literal fetch failed: {e}"))?;

    let mut pairs: Vec<(Item, Value)> = Vec::new();
    for rs in &states {
        if let Some(item) = item_from(rs) {
            pairs.push((item, rs.clone()));
        }
    }

    let items_written = write_items(vault, pairs)?;

    let mut sync_state = vault.read_literal_sync();
    sync_state.updated = Some(Local::now().to_rfc3339());
    vault.write_literal_sync(&sync_state)?;

    Ok(PullOutcome {
        headline: format!("{items_written} library entries"),
        counts: BTreeMap::from([("items", items_written)]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-literal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Stub API — drives all tests offline.
    ///
    /// Field names confirmed from literal.club/developers:
    ///   ReadingState: `id`, `status`, `bookId`, `profileId`, `createdAt`
    ///   Book: `id`, `title`, `subtitle`, `slug`, `cover`, `isbn10`, `isbn13`,
    ///         `pageCount`, `publishedDate`, `description`, `authors[]{name}`
    ///   ReadingStatus enum: WANTS_TO_READ / IS_READING / FINISHED / DROPPED / NONE
    struct StubApi {
        /// If set, login returns this token; otherwise Unauthorized.
        token: Option<String>,
        /// The reading-states array the stub returns after a successful login.
        states: Vec<Value>,
    }

    impl LiteralApi for StubApi {
        fn login(&self, _email: &str, _password: &str) -> Result<String, FetchError> {
            self.token.clone().ok_or(FetchError::Unauthorized)
        }

        fn my_reading_states(&self, _token: &str) -> Result<Vec<Value>, FetchError> {
            Ok(self.states.clone())
        }
    }

    fn sample_states() -> Vec<Value> {
        vec![
            serde_json::json!({
                "id": "rs-001",
                "status": "FINISHED",
                "bookId": "book-001",
                "createdAt": "2024-06-10T14:00:00.000Z",
                "book": {
                    "id": "book-001",
                    "title": "The Dawn of Everything",
                    "subtitle": "A New History of Humanity",
                    "slug": "the-dawn-of-everything",
                    "cover": "https://assets.literal.club/cover.jpg",
                    "isbn10": "0374157359",
                    "isbn13": "9780374157357",
                    "pageCount": 704,
                    "publishedDate": "2021-11-09T00:00:00.000Z",
                    "description": "A monumental new history of humanity.",
                    "authors": [{"name": "David Graeber"}, {"name": "David Wengrow"}]
                }
            }),
            serde_json::json!({
                "id": "rs-002",
                "status": "IS_READING",
                "bookId": "book-002",
                "createdAt": "2024-07-01T09:30:00.000Z",
                "book": {
                    "id": "book-002",
                    "title": "Piranesi",
                    "subtitle": null,
                    "slug": "piranesi",
                    "cover": "https://assets.literal.club/piranesi.jpg",
                    "isbn13": "9781635575637",
                    "pageCount": 272,
                    "publishedDate": "2020-09-15T00:00:00.000Z",
                    "description": null,
                    "authors": [{"name": "Susanna Clarke"}]
                }
            }),
            serde_json::json!({
                "id": "rs-003",
                "status": "WANTS_TO_READ",
                "bookId": "book-003",
                "createdAt": "2024-08-15T11:00:00.000Z",
                "book": {
                    "id": "book-003",
                    "title": "Braiding Sweetgrass",
                    "slug": "braiding-sweetgrass",
                    "cover": null,
                    "isbn13": "9781571313560",
                    "pageCount": 408,
                    "publishedDate": "2013-10-15T00:00:00.000Z",
                    "authors": [{"name": "Robin Wall Kimmerer"}]
                }
            }),
        ]
    }

    #[test]
    fn full_sync_writes_items_to_reading_contract() {
        let vault = temp_vault("full_sync");
        let api = StubApi {
            token: Some("jwt-test-token".into()),
            states: sample_states(),
        };

        let out = pull_with(&vault, &api, "test@example.com", "pass").unwrap();
        assert_eq!(out.counts["items"], 3, "three states → three items");

        let contract = vault.stream(DIR, Partition::Month);
        let partitions = contract.partitions().unwrap();
        assert!(!partitions.is_empty(), "at least one partition written");

        let mut all: Vec<Item> = Vec::new();
        for key in &partitions {
            all.extend(contract.read::<Item>(key).unwrap());
        }
        assert_eq!(all.len(), 3);

        // FINISHED → state="read", progress=100.
        let dawn = all.iter().find(|i| i.guid == "lit-rs-001").unwrap();
        assert_eq!(dawn.state, "read");
        assert_eq!(dawn.progress, Some(100));
        assert_eq!(dawn.title, "The Dawn of Everything");
        assert_eq!(dawn.author, "David Graeber");
        assert_eq!(
            dawn.extra.get("subtitle").and_then(Value::as_str),
            Some("A New History of Humanity")
        );

        // IS_READING → state="saved".
        let piranesi = all.iter().find(|i| i.guid == "lit-rs-002").unwrap();
        assert_eq!(piranesi.state, "saved");
        assert_eq!(piranesi.progress, None);
        assert_eq!(piranesi.author, "Susanna Clarke");

        // WANTS_TO_READ → state="saved".
        let sweetgrass = all.iter().find(|i| i.guid == "lit-rs-003").unwrap();
        assert_eq!(sweetgrass.state, "saved");
        assert_eq!(sweetgrass.title, "Braiding Sweetgrass");
    }

    #[test]
    fn idempotent_resync_skips_existing_guids() {
        let vault = temp_vault("idempotent");
        let api = StubApi {
            token: Some("jwt-test-token".into()),
            states: sample_states(),
        };

        let out1 = pull_with(&vault, &api, "test@example.com", "pass").unwrap();
        assert_eq!(out1.counts["items"], 3);

        // Second sync with same data: all guids already seen, nothing new.
        let out2 = pull_with(&vault, &api, "test@example.com", "pass").unwrap();
        assert_eq!(out2.counts["items"], 0, "second sync writes nothing new");

        let contract = vault.stream(DIR, Partition::Month);
        let mut total = 0usize;
        for key in contract.partitions().unwrap() {
            total += contract.read::<Item>(&key).unwrap().len();
        }
        assert_eq!(total, 3);
    }

    #[test]
    fn new_book_appended_on_next_sync() {
        let vault = temp_vault("incremental");
        // First sync: only the first two books.
        let api1 = StubApi {
            token: Some("tok".into()),
            states: sample_states()[..2].to_vec(),
        };
        let out1 = pull_with(&vault, &api1, "u@x.com", "p").unwrap();
        assert_eq!(out1.counts["items"], 2);

        // Second sync: the same two books plus a new dropped book (three total
        // returned from the API, but only one is genuinely new).
        let new_states = vec![
            sample_states()[0].clone(),
            sample_states()[1].clone(),
            serde_json::json!({
                "id": "rs-004",
                "status": "DROPPED",
                "bookId": "book-004",
                "createdAt": "2024-09-01T08:00:00.000Z",
                "book": {
                    "id": "book-004",
                    "title": "The Last Wish",
                    "slug": "the-last-wish",
                    "isbn13": "9780316452458",
                    "pageCount": 288,
                    "authors": [{"name": "Andrzej Sapkowski"}]
                }
            }),
        ];
        let api2 = StubApi { token: Some("tok2".into()), states: new_states };
        let out2 = pull_with(&vault, &api2, "u@x.com", "p").unwrap();
        assert_eq!(out2.counts["items"], 1, "only the new book is written");

        let contract = vault.stream(DIR, Partition::Month);
        let mut all: Vec<Item> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<Item>(&key).unwrap());
        }
        assert_eq!(all.len(), 3, "two from first sync + one new");

        let dropped = all.iter().find(|i| i.guid == "lit-rs-004").unwrap();
        assert_eq!(dropped.state, "archived");
    }

    #[test]
    fn login_failure_bubbles_error() {
        let vault = temp_vault("auth_fail");
        let api = StubApi { token: None, states: vec![] };
        let err = pull_with(&vault, &api, "bad@example.com", "wrong").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("login failed") || msg.contains("unauthorized"),
            "got: {msg}"
        );
    }

    #[test]
    fn raw_layer_written_unconditionally() {
        let vault = temp_vault("raw_layer");
        let api = StubApi {
            token: Some("tok".into()),
            states: sample_states()[..1].to_vec(),
        };
        pull_with(&vault, &api, "u@x.com", "p").unwrap();

        let raw = vault.stream(RAW_DIR, Partition::Month);
        let partitions = raw.partitions().unwrap();
        assert!(!partitions.is_empty());
        let rows: Vec<Value> = raw.read(&partitions[0]).unwrap();
        assert_eq!(rows.len(), 1);
        // Raw row carries original API field names.
        assert_eq!(rows[0].get("id").and_then(Value::as_str), Some("rs-001"));
        assert_eq!(
            rows[0]
                .get("book")
                .and_then(|b| b.get("title"))
                .and_then(Value::as_str),
            Some("The Dawn of Everything")
        );
    }

    #[test]
    fn split_cred_handles_password_with_colon() {
        let (email, password) = split_cred("user@x.com:p@ss:word").unwrap();
        assert_eq!(email, "user@x.com");
        assert_eq!(password, "p@ss:word");
    }

    #[test]
    fn split_cred_rejects_missing_password() {
        assert!(split_cred("user@x.com").is_err());
        assert!(split_cred("").is_err());
    }

    #[test]
    fn status_mapping() {
        assert_eq!(status_to_state("FINISHED"), "read");
        assert_eq!(status_to_state("DROPPED"), "archived");
        assert_eq!(status_to_state("IS_READING"), "saved");
        assert_eq!(status_to_state("WANTS_TO_READ"), "saved");
        assert_eq!(status_to_state("NONE"), "saved");
        assert_eq!(status_to_state("UNKNOWN_FUTURE_STATUS"), "saved");
    }
}

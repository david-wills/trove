//! Granola — AI meeting-notes service with a public REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/granola.md.
//!
//! Two destinations, written in one pass:
//!
//! - **meetings contract** under `meetings/granola/YYYY-MM.jsonl` (the
//!   [`crate::meetings`] contract): one [`Meeting`] per note. Rows are
//!   **upserted by `guid`** into the month partition — so a re-poll never
//!   duplicates a meeting.
//! - **raw layer** under `meetings/granola/raw/YYYY-MM.jsonl`: the verbatim
//!   API note objects, full fidelity, partitioned by the meeting's start month
//!   (falls back to `created_at` month), upserted by `id`.
//! - **transcript sidecars** under
//!   `meetings/granola/raw/transcripts/<note_id>.jsonl`: per-meeting utterance
//!   stream (one utterance per line). [`Meeting::transcript_ref`] points here.
//!
//! ## API — Granola public API v1 (`https://public-api.granola.ai/v1`)
//!
//! Every call sends `Authorization: Bearer <api_key>`. Two endpoints are used:
//!
//! - `GET /v1/notes?page_size=30[&updated_after=<t>][&cursor=<c>]` — list
//!   notes (summary only: id, title, owner, created_at, updated_at). Supports
//!   `updated_after` for incremental pulls. Pagination: `hasMore` boolean +
//!   `cursor` token. Drain all pages — sort order is undocumented.
//! - `GET /v1/notes/{id}?include=transcript` — full note: attendees,
//!   `calendar_event` (start/end/invitees/organiser), `summary_markdown`,
//!   `web_url`, `folder_membership`, `transcript[]`. Called for every
//!   new/updated note.
//!
//! **Important API constraint:** the list endpoint returns ONLY notes that
//! already have a generated AI summary AND transcript. Unprocessed notes never
//! appear. So a meeting that just ended may not appear for several minutes.
//!
//! Rate limits: 25 req/5s burst, 5 req/s sustained — well within budget for
//! a personal pull (30 notes/page list + 1 detail per new note).
//!
//! ## Cursor
//!
//! `.trove/granola-sync.json` (non-secret, rebuildable) holds
//! `last_updated_at`: the newest `updated_at` timestamp we've stored.
//! On the next poll, `updated_after=<last_updated_at>` skips already-seen
//! notes. On first sync the filter is absent — all notes are fetched.
//!
//! The watermark is advanced ONLY after the full drain + all detail fetches,
//! never on a partial run. A guid/parse failure that empties the write set
//! does NOT advance the cursor.
//!
//! 🔒 **Default-off opt-in.** Meeting notes and transcripts are conversation
//! content (≈ message bodies). The hub renders the opt-in gate for default-off.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::meetings::Meeting;
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// The collector id and the source folder name.
const SOURCE: &str = "granola";
/// Contract layer (one [`Meeting`] per note, upserted by guid).
const CONTRACT_DIR: &str = "meetings/granola";
/// Raw firehose (verbatim API note objects from the detail endpoint).
const RAW_DIR: &str = "meetings/granola/raw";
/// Per-note transcript sidecars (`<note_id>.jsonl`). `transcript_ref` points here.
const TRANSCRIPT_DIR: &str = "meetings/granola/raw/transcripts";

/// Non-secret rebuildable cursor. Deleting it triggers a full re-pull.
const SYNC_FILE: &str = ".trove/granola-sync.json";
/// The service id under `.trove/sync/` where the API key is stored.
const SERVICE: &str = "granola";

const API_BASE: &str = "https://public-api.granola.ai";
/// Short timeout so a hung connection can't stall the watcher loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs — every 30 minutes (notes process in minutes; no
/// need to poll faster than the Granola processing pipeline).
pub const GRANOLA_SYNC_SECS: u64 = 1_800;
/// Notes per list page (API max is 30).
const PAGE_SIZE: u32 = 30;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_granola_sync().last_updated_at.filter(|s| !s.is_empty())
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "granola synced — {} meetings, {} transcripts",
                    c("meetings"),
                    c("transcripts"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "granola sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "granola",
        name: "Granola",
        kind: IntegrationKind::CloudSync,
        // 🔒 Opt-in: meeting notes/transcripts are conversation content.
        default_on: false,
        description: "Pulls your Granola meeting notes — titles, attendees, AI summaries, \
                      and transcripts — into the vault after each recorded call. \
                      Requires a Granola Business plan or higher (API keys are Business+ only).",
        domain: "meetings",
        vault_path: "meetings/granola/",
        toggleable: true,
        setup: &[
            "Connect with your Granola API key on this card.",
            "Each sync pulls new and updated meeting notes every 30 minutes.",
        ],
        caveats: "Requires a Granola Business plan or higher — API key creation is restricted \
                  to Business+ workspaces (docs.granola.ai: \"Any workspace member on a \
                  Business plan can create API keys\"). Meeting notes and transcripts are \
                  conversation content, so this source is off by default — turn it on \
                  deliberately. The Granola API only returns notes that already have a \
                  generated AI summary; a just-ended meeting may not appear for several minutes.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(GRANOLA_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("granola"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the Bearer token, a SECRET).

fn def_connect(vault: &Vault, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("empty key — paste your Granola API key");
    }
    let client = GranolaClient::new(API_BASE.to_string(), key.to_string());
    // Cheap verification: list one note to prove the key works.
    match client.list_notes(None, None, 1) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Granola rejected the key (401) — check it's your API key from \
             Settings → Integrations → API and hasn't been revoked"
        ),
        Err(e) => bail!("Granola /notes check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: key.to_string(),
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
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Granola".to_string(),
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
    id: "granola",
    display_name: "Granola",
    methods: &[ConnectMethod::TokenPaste {
        label: "Granola API key",
        help: "Granola → Settings → Integrations → API → create an API key.",
        placeholder: "grn_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["granola"],
    setup: &[
        "In Granola, open Settings → Integrations → API.",
        "Create an API key.",
        "Paste it here — it's stored locally (0600) and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — trait-injectable so tests run fully offline.

/// Status-level fetch errors.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    NotFound,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::NotFound => write!(f, "not found (HTTP 404)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The API surface the pull needs. A trait so tests drive the mapping/persist
/// logic with fixtures, never the network.
trait GranolaApi {
    /// `GET /v1/notes?page_size=<n>[&updated_after=<t>][&cursor=<c>]`
    fn list_notes(
        &self,
        updated_after: Option<&str>,
        cursor: Option<&str>,
        page_size: u32,
    ) -> Result<Value, FetchError>;

    /// `GET /v1/notes/{id}?include=transcript`
    fn get_note(&self, id: &str) -> Result<Value, FetchError>;
}

/// Thin live client; base URL injected for testability.
struct GranolaClient {
    base: String,
    key: String,
}

impl GranolaClient {
    fn new(base: String, key: String) -> Self {
        GranolaClient { base, key }
    }

    fn get_json_raw(&self, path: &str) -> Result<Value, FetchError> {
        let url = format!("{}{path}", self.base);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.key))
            .set("Accept", "application/json")
            .call();
        match resp {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
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
}

impl GranolaApi for GranolaClient {
    fn list_notes(
        &self,
        updated_after: Option<&str>,
        cursor: Option<&str>,
        page_size: u32,
    ) -> Result<Value, FetchError> {
        let mut qs = format!("/v1/notes?page_size={page_size}");
        if let Some(after) = updated_after {
            qs.push_str(&format!("&updated_after={}", urlencode(after)));
        }
        if let Some(c) = cursor {
            qs.push_str(&format!("&cursor={}", urlencode(c)));
        }
        self.get_json_raw(&qs)
    }

    fn get_note(&self, id: &str) -> Result<Value, FetchError> {
        let path = format!("/v1/notes/{id}?include=transcript");
        self.get_json_raw(&path)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct SyncState {
    /// `updated_at` (RFC3339) of the newest note we've stored.
    /// Passed as `updated_after` on the next poll to skip already-seen notes.
    /// Not a secret; rebuildable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_updated_at: Option<String>,
}

impl Vault {
    fn read_granola_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_granola_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row (verbatim full note object from GET /v1/notes/{id}).

/// One raw API note object in `meetings/granola/raw/YYYY-MM.jsonl`. Stored
/// verbatim (flattened) — no synthetic keys added.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawNote {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawNote {
    /// The note id (dedup key).
    fn id(&self) -> String {
        self.fields.get("id").and_then(Value::as_str).unwrap_or("").to_string()
    }

    /// Partition timestamp: `calendar_event.scheduled_start_time` when
    /// present, falling back to `created_at`. Both are RFC3339.
    fn start_ts(&self) -> &str {
        self.fields
            .get("calendar_event")
            .and_then(|ce| ce.get("scheduled_start_time"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| self.fields.get("created_at").and_then(Value::as_str))
            .unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Pure mapping helpers (fixture-tested).

/// Pull a string field off a JSON value, trimmed, non-empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// An RFC3339 UTC string → RFC3339 local time. Unparseable values pass
/// through verbatim (the todoist/fathom `to_local` idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Duration in seconds between two RFC3339 instants (end − start), `None`
/// when either is missing/unparseable or end precedes start.
fn duration_secs(start: &str, end: Option<&str>) -> Option<i64> {
    let end = end?;
    let s = DateTime::parse_from_rfc3339(start).ok()?;
    let e = DateTime::parse_from_rfc3339(end).ok()?;
    let secs = (e - s).num_seconds();
    (secs >= 0).then_some(secs)
}

/// Vault-relative `transcript_ref` for a note (per-meeting sidecar).
fn transcript_ref(note_id: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{note_id}.jsonl")
}

/// Map a full note API object → a [`Meeting`] contract row.
/// Returns `None` only if the note lacks an `id` (can't dedup).
fn meeting_from_note(v: &Value) -> Option<Meeting> {
    let obj = v.as_object()?;
    let guid = str_opt(v, "id")?;

    // ts: calendar_event.scheduled_start_time, fallback created_at. Required.
    let cal = obj.get("calendar_event");
    let start_raw = cal
        .and_then(|ce| str_opt(ce, "scheduled_start_time"))
        .or_else(|| str_opt(v, "created_at"))?;
    let end_raw = cal.and_then(|ce| str_opt(ce, "scheduled_end_time"));

    let mut m = Meeting::new(SOURCE, &guid, to_local(&start_raw));
    m.started = to_local(&start_raw);
    if let Some(end) = &end_raw {
        m.ended = to_local(end);
    }
    m.duration_secs = duration_secs(&start_raw, end_raw.as_deref());

    // title: prefer top-level `title`, fall back to calendar_event.event_title.
    if let Some(title) =
        str_opt(v, "title").or_else(|| cal.and_then(|ce| str_opt(ce, "event_title")))
    {
        m.title = title;
    }

    // attendees: from `attendees[]` (email + optional name); fall back to
    // calendar_event.invitees[] (email only) when attendees absent.
    let attendee_objs = obj
        .get("attendees")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty());
    let invitee_objs = cal
        .and_then(|ce| ce.get("invitees"))
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty());

    let source_list = attendee_objs.or(invitee_objs);
    if let Some(people) = source_list {
        let emails: Vec<String> = people
            .iter()
            .filter_map(|p| str_opt(p, "email").map(|e| e.to_lowercase()))
            .collect();
        let names: Vec<String> =
            people.iter().filter_map(|p| str_opt(p, "name")).collect();
        let aligned =
            !emails.is_empty() && emails.len() == people.len() && names.len() == emails.len();
        if !emails.is_empty() {
            m.attendees = emails;
        }
        if aligned {
            m.attendee_names = names;
        } else if !people.is_empty() {
            // Partial names → preserve raw list in extra rather than misalign.
            m.extra.insert("attendees_raw".into(), Value::Array(people.clone()));
        }
    }

    // host: calendar_event.organiser (an email string), fallback owner.email.
    if let Some(organiser) = cal.and_then(|ce| str_opt(ce, "organiser")) {
        m.host = organiser.to_lowercase();
    }
    if m.host.is_empty() {
        if let Some(owner_email) = obj.get("owner").and_then(|o| str_opt(o, "email")) {
            m.host = owner_email.to_lowercase();
        }
    }

    // summary: prefer summary_markdown, fall back to summary_text.
    if let Some(summary) = str_opt(v, "summary_markdown").or_else(|| str_opt(v, "summary_text")) {
        m.summary = summary;
    }

    // meeting_url: the schema describes this as the join/conference URL (e.g. a Zoom
    // join link). The Granola API exposes no platform join URL — `web_url` is the
    // note permalink (notes.granola.ai/d/<uuid>), which is a different concept.
    // Store it in extra.web_url so it's preserved without misrepresenting the schema
    // field's meaning. meeting_url is left empty.
    if let Some(url) = str_opt(v, "web_url") {
        m.extra.insert("web_url".into(), Value::from(url));
    }

    // folder: first folder membership name.
    if let Some(folders) = obj.get("folder_membership").and_then(Value::as_array) {
        if let Some(first_name) = folders.first().and_then(|f| str_opt(f, "name")) {
            m.folder = first_name;
        }
    }

    // created_at and updated_at preserved in extra for sorting/queries.
    if let Some(created) = str_opt(v, "created_at") {
        m.extra.insert("created_at".into(), Value::from(created));
    }
    if let Some(updated) = str_opt(v, "updated_at") {
        m.extra.insert("updated_at".into(), Value::from(updated));
    }

    Some(m)
}

/// Extract transcript utterances from a note object's `transcript` array.
/// Returns empty when absent, null, or not an array.
fn transcript_utterances(v: &Value) -> Vec<Value> {
    v.get("transcript")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Upsert-by-guid into month partitions (the fathom pattern).

fn upsert_contract(vault: &Vault, rows: Vec<Meeting>) -> Result<u64> {
    upsert_partition(vault, CONTRACT_DIR, rows, |m| m.ts.clone(), |m| m.guid.clone())
}

fn upsert_raw(vault: &Vault, rows: Vec<RawNote>) -> Result<u64> {
    upsert_partition(vault, RAW_DIR, rows, |r| r.start_ts().to_string(), |r| r.id())
}

fn upsert_partition<T, FT, FG>(
    vault: &Vault,
    dir: &str,
    rows: Vec<T>,
    ts_of: FT,
    guid_of: FG,
) -> Result<u64>
where
    T: Serialize + serde::de::DeserializeOwned,
    FT: Fn(&T) -> String,
    FG: Fn(&T) -> String,
{
    let stream = vault.stream(dir, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for r in rows {
        let ts = ts_of(&r);
        let key = Partition::Month
            .key(&ts)
            .with_context(|| format!("granola: ts {ts:?} has no month (dir {dir})"))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<T> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> =
            existing.iter().enumerate().map(|(i, r)| (guid_of(r), i)).collect();
        for r in fresh {
            let g = guid_of(&r);
            match idx.get(&g).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(g, existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing
            .sort_by(|a, b| ts_of(a).cmp(&ts_of(b)).then_with(|| guid_of(a).cmp(&guid_of(b))));
        vault.write_snapshot(&format!("{dir}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let key = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|k| !k.trim().is_empty())
        .context("Granola is not connected — add your API key in the Integrations tab")?;
    let client = GranolaClient::new(API_BASE.to_string(), key);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl GranolaApi) -> Result<PullOutcome> {
    let mut state = vault.read_granola_sync();
    let watermark = state.last_updated_at.clone();

    // --- 1. Drain all list pages, collecting note summaries (id + updated_at).
    //        Sort order undocumented → drain everything before deciding what to
    //        fetch in detail.
    let mut summaries: BTreeMap<String, String> = BTreeMap::new(); // id → updated_at
    let mut list_cursor: Option<String> = None;
    loop {
        let body = api
            .list_notes(watermark.as_deref(), list_cursor.as_deref(), PAGE_SIZE)
            .map_err(|e| fetch_err("listing notes", e))?;
        let (notes, has_more, next_cursor) = parse_list(body);
        for note in &notes {
            if let Some(id) = note.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                let updated =
                    note.get("updated_at").and_then(Value::as_str).unwrap_or("").to_string();
                summaries.insert(id.to_string(), updated);
            }
        }
        if !has_more {
            break;
        }
        match next_cursor {
            Some(c) => list_cursor = Some(c),
            None => break, // hasMore=true but no cursor is a server inconsistency — stop
        }
    }

    // --- 2. Fetch full detail for each note (attendees + transcript).
    //        All notes from the updated_after-filtered list are new or updated.
    let mut contract_rows: Vec<Meeting> = Vec::new();
    let mut raw_rows: Vec<RawNote> = Vec::new();
    let mut transcripts_written = 0u64;
    let mut newest_updated: Option<String> = watermark.clone();

    for (id, updated_at) in &summaries {
        let detail = match api.get_note(id) {
            Ok(v) => v,
            Err(FetchError::NotFound) => {
                // Note was deleted between list and detail — skip silently.
                continue;
            }
            Err(FetchError::RateLimited) => {
                // Stop this pass; watermark not advanced → same point on retry.
                return Err(anyhow::anyhow!(
                    "Granola rate limit hit during detail fetch — \
                     will resume from the same point on the next sync"
                ));
            }
            Err(FetchError::Unauthorized) => {
                return Err(anyhow::anyhow!(
                    "Granola rejected the key (401) — reconnect from the Integrations tab"
                ));
            }
            Err(e) => {
                // Transient error on one note — skip it; watermark won't
                // advance past this updated_at so it'll be retried next poll.
                eprintln!("granola: detail fetch for {id} failed: {e}");
                continue;
            }
        };

        // Raw firehose: verbatim detail object (full note, including transcript).
        raw_rows.push(RawNote { fields: detail.as_object().cloned().unwrap_or_default() });

        // Transcript sidecar.
        let utterances = transcript_utterances(&detail);
        let has_transcript = !utterances.is_empty();

        let Some(mut row) = meeting_from_note(&detail) else {
            // No usable id → can't place a contract row; raw is still kept.
            continue;
        };

        if has_transcript {
            write_transcript_sidecar(vault, id, &utterances)?;
            transcripts_written += 1;
            row.transcript_ref = transcript_ref(id);
        }
        contract_rows.push(row);

        // Track the newest updated_at across this batch.
        if !updated_at.is_empty() {
            newest_updated = max_ts(newest_updated, updated_at.clone());
        }
    }

    // --- 3. Persist: raw + contract upsert + advance cursor.
    //        Watermark advances only after a full successful drain.
    let raw_new = upsert_raw(vault, raw_rows)?;
    let contract_new = upsert_contract(vault, contract_rows)?;

    state.last_updated_at = newest_updated;
    vault.write_granola_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("meetings", contract_new);
    counts.insert("transcripts", transcripts_written);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!(
            "Granola synced — {contract_new} new meetings, {transcripts_written} transcripts"
        ),
        counts,
    })
}

/// The later of two RFC3339 timestamps (lexical compare correct for UTC `…Z`).
fn max_ts(cur: Option<String>, candidate: String) -> Option<String> {
    match cur {
        Some(prev) if prev.as_str() >= candidate.as_str() => Some(prev),
        _ => Some(candidate),
    }
}

/// Write the per-note transcript sidecar, one utterance per JSONL line.
fn write_transcript_sidecar(vault: &Vault, note_id: &str, utterances: &[Value]) -> Result<()> {
    vault.write_snapshot(&transcript_ref(note_id), utterances)
}

/// Parse the list response:
/// `{ "notes": [...], "hasMore": bool, "cursor": str|null }`.
fn parse_list(v: Value) -> (Vec<Value>, bool, Option<String>) {
    match v {
        Value::Object(o) => {
            let notes =
                o.get("notes").and_then(Value::as_array).cloned().unwrap_or_default();
            let has_more = o.get("hasMore").and_then(Value::as_bool).unwrap_or(false);
            let cursor = o
                .get("cursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            (notes, has_more, cursor)
        }
        // Tolerate a bare array (future shape change).
        Value::Array(a) => (a, false, None),
        _ => (Vec::new(), false, None),
    }
}

/// Map a [`FetchError`] into an anyhow error with a clear message.
fn fetch_err(context: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Granola rejected the key (401) — reconnect from the Integrations tab \
             (context: {context})"
        ),
        FetchError::RateLimited => {
            anyhow::anyhow!("Granola rate limit hit (429) — will retry on the next sync")
        }
        other => anyhow::anyhow!("Granola {context} failed: {other}"),
    }
}

/// Minimal percent-encoding for query string values.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-granola-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixtures — built from the confirmed Granola public-API v1 OpenAPI schema
    // (docs.granola.ai/api-reference/openapi.json + get-note.md).

    /// A full GET /v1/notes/{id}?include=transcript response (the detail object).
    fn note_full(id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "object": "note",
            "title": "Q3 Roadmap Sync",
            "owner": {"name": "David Wills", "email": "dwills@example.com"},
            "created_at": "2026-06-10T16:50:00Z",
            "updated_at": "2026-06-10T17:05:00Z",
            "web_url": format!("https://granola.ai/notes/{id}"),
            "calendar_event": {
                "event_title": "Q3 Roadmap Planning",
                "scheduled_start_time": "2026-06-10T16:00:00Z",
                "scheduled_end_time": "2026-06-10T16:49:00Z",
                "organiser": "dwills@example.com",
                "invitees": [
                    {"email": "DWills@Example.com"},
                    {"email": "Sam@Example.com"}
                ],
                "calendar_event_id": "abc123"
            },
            "attendees": [
                {"email": "DWills@Example.com", "name": "David Wills"},
                {"email": "Sam@Example.com", "name": "Sam Ortiz"}
            ],
            "folder_membership": [
                {
                    "id": "fol_workxyz1234567",
                    "object": "folder",
                    "name": "Work",
                    "parent_folder_id": null
                }
            ],
            "summary_text": "Decisions: Ship the meetings contract first.",
            "summary_markdown": "## Decisions\n- Ship the meetings contract first\n\n## Action items\n- [ ] Ana to draft the Q3 deck",
            "transcript": [
                {
                    "speaker": {"source": "microphone", "diarization_label": "Speaker A"},
                    "text": "Let's kick off.",
                    "start_time": "2026-06-10T16:00:01Z",
                    "end_time": "2026-06-10T16:00:03Z"
                },
                {
                    "speaker": {"source": "speaker", "diarization_label": "Speaker B"},
                    "text": "Ready.",
                    "start_time": "2026-06-10T16:00:04Z",
                    "end_time": "2026-06-10T16:00:05Z"
                }
            ]
        })
    }

    /// A note with no transcript (null).
    fn note_no_transcript(id: &str) -> Value {
        let mut n = note_full(id);
        n["transcript"] = Value::Null;
        n
    }

    /// A note with partial attendee names (names misaligned — some missing).
    fn note_partial_names(id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "object": "note",
            "title": "Customer Discovery",
            "owner": {"name": "David Wills", "email": "dwills@example.com"},
            "created_at": "2026-06-12T18:00:00Z",
            "updated_at": "2026-06-12T18:50:00Z",
            "web_url": format!("https://granola.ai/notes/{id}"),
            "calendar_event": {
                "scheduled_start_time": "2026-06-12T18:00:00Z",
                "scheduled_end_time": "2026-06-12T18:30:00Z",
                "organiser": "dwills@example.com",
                "invitees": []
            },
            "attendees": [
                {"email": "jordan@acme.com", "name": "Jordan"},
                {"email": "dwills@example.com"}
            ],
            "folder_membership": [],
            "summary_markdown": "Customer wants SSO.",
            "transcript": []
        })
    }

    /// A NoteSummary as returned by the list endpoint.
    fn note_summary(id: &str, updated_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "object": "note",
            "title": "Q3 Roadmap Sync",
            "owner": {"name": "David Wills", "email": "dwills@example.com"},
            "created_at": "2026-06-10T16:50:00Z",
            "updated_at": updated_at
        })
    }

    // ---------------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        /// (cursor-in → list page body). `None` = first (uncursored) page.
        list_pages: RefCell<Vec<(Option<String>, Value)>>,
        /// note_id → detail body.
        details: RefCell<HashMap<String, Value>>,
        /// Paths requested.
        requests: RefCell<Vec<String>>,
        /// Note ids that trigger NotFound.
        not_found: RefCell<std::collections::HashSet<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                list_pages: RefCell::new(Vec::new()),
                details: RefCell::new(Default::default()),
                requests: RefCell::new(Vec::new()),
                not_found: RefCell::new(Default::default()),
            }
        }

        fn list_page(&self, notes: Vec<Value>, has_more: bool, cursor: Option<&str>) {
            self.list_page_at(None, notes, has_more, cursor);
        }

        fn list_page_at(
            &self,
            cursor_in: Option<&str>,
            notes: Vec<Value>,
            has_more: bool,
            cursor_out: Option<&str>,
        ) {
            let body = serde_json::json!({
                "notes": notes,
                "hasMore": has_more,
                "cursor": cursor_out
            });
            self.list_pages.borrow_mut().push((cursor_in.map(str::to_string), body));
        }

        fn detail(&self, id: &str, note: Value) {
            self.details.borrow_mut().insert(id.to_string(), note);
        }

        fn requested(&self, needle: &str) -> bool {
            self.requests.borrow().iter().any(|p| p.contains(needle))
        }
    }

    impl GranolaApi for MockApi {
        fn list_notes(
            &self,
            _updated_after: Option<&str>,
            cursor: Option<&str>,
            _page_size: u32,
        ) -> Result<Value, FetchError> {
            let path = format!("/v1/notes?cursor={cursor:?}");
            self.requests.borrow_mut().push(path);
            let want = cursor.map(str::to_string);
            for (cur, body) in self.list_pages.borrow().iter() {
                if *cur == want {
                    return Ok(body.clone());
                }
            }
            Ok(serde_json::json!({"notes": [], "hasMore": false, "cursor": null}))
        }

        fn get_note(&self, id: &str) -> Result<Value, FetchError> {
            self.requests.borrow_mut().push(format!("/v1/notes/{id}"));
            if self.not_found.borrow().contains(id) {
                return Err(FetchError::NotFound);
            }
            self.details
                .borrow()
                .get(id)
                .cloned()
                .ok_or_else(|| FetchError::Other(format!("no detail registered for {id}")))
        }
    }

    // ---------------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_guid_ts_duration_attendees_summary_folder() {
        let m = meeting_from_note(&note_full("not_abc1234567890A")).unwrap();
        assert_eq!(m.source, "granola");
        assert_eq!(m.guid, "not_abc1234567890A");
        // ts = calendar start, converted to local.
        assert_eq!(
            DateTime::parse_from_rfc3339(&m.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T16:00:00Z").unwrap().timestamp(),
        );
        assert_eq!(m.title, "Q3 Roadmap Sync");
        // 49 min = 2940 s.
        assert_eq!(m.duration_secs, Some(2940));
        // attendees lowercased; names aligned.
        assert_eq!(m.attendees, vec!["dwills@example.com", "sam@example.com"]);
        assert_eq!(m.attendee_names, vec!["David Wills", "Sam Ortiz"]);
        assert_eq!(m.host, "dwills@example.com");
        assert!(m.summary.contains("Ship the meetings contract first"));
        // meeting_url is the join/conference URL (schema spec); Granola exposes no join URL —
        // web_url (the note permalink) goes to extra.web_url instead.
        assert!(m.meeting_url.is_empty(), "no join URL from Granola; field must stay empty");
        assert!(
            m.extra
                .get("web_url")
                .and_then(|v| v.as_str())
                .map(|u| u.contains("granola.ai/notes/"))
                .unwrap_or(false),
            "note permalink preserved in extra.web_url"
        );
        assert_eq!(m.folder, "Work");
        // transcript_ref NOT set by mapping — set by the pull loop.
        assert_eq!(m.transcript_ref, "");
        // timestamps preserved in extra.
        assert!(m.extra.contains_key("created_at"));
        assert!(m.extra.contains_key("updated_at"));
    }

    #[test]
    fn partial_attendee_names_go_to_extra() {
        let m = meeting_from_note(&note_partial_names("not_partialnames00A")).unwrap();
        assert_eq!(m.attendees, vec!["jordan@acme.com", "dwills@example.com"]);
        assert!(m.attendee_names.is_empty(), "misaligned names not written");
        assert!(m.extra.contains_key("attendees_raw"), "raw list in extra");
    }

    #[test]
    fn no_calendar_event_falls_back_to_created_at() {
        let n = serde_json::json!({
            "id": "not_nocal00000001A",
            "created_at": "2026-05-15T10:00:00Z",
            "updated_at": "2026-05-15T10:30:00Z",
            "title": "Impromptu Call",
            "summary_text": "Quick sync.",
            "transcript": []
        });
        let m = meeting_from_note(&n).unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(&m.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-05-15T10:00:00Z").unwrap().timestamp(),
        );
    }

    #[test]
    fn duration_omitted_when_end_missing_or_inverted() {
        assert_eq!(duration_secs("2026-06-10T16:00:00Z", None), None, "no end → none");
        assert_eq!(
            duration_secs("2026-06-10T16:00:00Z", Some("2026-06-10T16:49:00Z")),
            Some(2940)
        );
        assert_eq!(
            duration_secs("2026-06-10T16:49:00Z", Some("2026-06-10T16:00:00Z")),
            None,
            "end before start → none"
        );
    }

    #[test]
    fn transcript_utterances_null_is_empty() {
        assert_eq!(transcript_utterances(&note_full("x")).len(), 2);
        assert!(transcript_utterances(&note_no_transcript("x")).is_empty());
    }

    #[test]
    fn transcript_ref_is_per_note_sidecar() {
        assert_eq!(
            transcript_ref("not_abc1234567890A"),
            "meetings/granola/raw/transcripts/not_abc1234567890A.jsonl"
        );
    }

    #[test]
    fn parse_list_reads_notes_hasmore_cursor() {
        let (notes, has_more, cursor) = parse_list(serde_json::json!({
            "notes": [{"id": "a"}, {"id": "b"}],
            "hasMore": true,
            "cursor": "tok1"
        }));
        assert_eq!(notes.len(), 2);
        assert!(has_more);
        assert_eq!(cursor.as_deref(), Some("tok1"));

        let (notes, has_more, cursor) = parse_list(serde_json::json!({
            "notes": [],
            "hasMore": false,
            "cursor": null
        }));
        assert_eq!(notes.len(), 0);
        assert!(!has_more);
        assert_eq!(cursor, None);

        // Bare array tolerance.
        let (notes, has_more, cursor) = parse_list(serde_json::json!([1, 2]));
        assert_eq!(notes.len(), 2);
        assert!(!has_more);
        assert_eq!(cursor, None);
    }

    // ---------------------------------------------------------------------------
    // Full pull tests.

    #[test]
    fn full_pull_writes_contract_raw_transcript_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new();
        let note_id = "not_fullpull1234AB";
        api.list_page(vec![note_summary(note_id, "2026-06-10T17:05:00Z")], false, None);
        api.detail(note_id, note_full(note_id));

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&1));
        assert_eq!(out.counts.get("transcripts"), Some(&1));
        assert_eq!(out.counts.get("raw"), Some(&1));

        // Contract row in the ts-month partition.
        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.guid, note_id);
        assert_eq!(
            m.transcript_ref,
            format!("meetings/granola/raw/transcripts/{note_id}.jsonl")
        );
        assert_eq!(m.attendees, vec!["dwills@example.com", "sam@example.com"]);
        assert_eq!(m.attendee_names, vec!["David Wills", "Sam Ortiz"]);

        // Transcript sidecar written with full utterance stream.
        let sidecar =
            v.root().join(format!("meetings/granola/raw/transcripts/{note_id}.jsonl"));
        assert!(sidecar.exists());
        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(body.lines().count(), 2, "two utterances");
        assert!(body.contains("diarization_label"));
        assert!(body.contains("start_time"));

        // Raw firehose written.
        let raw_path = v.root().join("meetings/granola/raw/2026-06.jsonl");
        assert!(raw_path.exists());
        let raw = std::fs::read_to_string(&raw_path).unwrap();
        assert!(raw.contains(note_id));
        assert!(raw.contains("summary_markdown"));
        assert!(raw.contains("calendar_event"));

        // Watermark advanced.
        let state = v.read_granola_sync();
        assert_eq!(state.last_updated_at.as_deref(), Some("2026-06-10T17:05:00Z"));
    }

    #[test]
    fn note_without_transcript_written_without_transcript_ref() {
        let v = temp_vault("notranscript");
        let api = MockApi::new();
        let note_id = "not_notranscript0AB";
        api.list_page(vec![note_summary(note_id, "2026-06-10T17:05:00Z")], false, None);
        api.detail(note_id, note_no_transcript(note_id));

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("transcripts"), Some(&0));

        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transcript_ref, "", "no transcript_ref");
        assert!(
            !v.root()
                .join(format!("meetings/granola/raw/transcripts/{note_id}.jsonl"))
                .exists()
        );
    }

    #[test]
    fn deleted_note_404_skipped_gracefully() {
        let v = temp_vault("notfound");
        let api = MockApi::new();
        let gone_id = "not_gone00000000AB";
        api.list_page(vec![note_summary(gone_id, "2026-06-10T17:05:00Z")], false, None);
        api.not_found.borrow_mut().insert(gone_id.to_string());

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&0));
    }

    #[test]
    fn pagination_drains_all_pages() {
        let v = temp_vault("paginate");
        let api = MockApi::new();
        let id1 = "not_page1note0001A";
        let id2 = "not_page2note0002A";
        let id3 = "not_page3note0003A";
        api.list_page_at(
            None,
            vec![note_summary(id1, "2026-06-10T17:00:00Z")],
            true,
            Some("CURSOR2"),
        );
        api.list_page_at(
            Some("CURSOR2"),
            vec![note_summary(id2, "2026-06-11T10:00:00Z")],
            true,
            Some("CURSOR3"),
        );
        api.list_page_at(
            Some("CURSOR3"),
            vec![note_summary(id3, "2026-06-12T09:00:00Z")],
            false,
            None,
        );
        for id in [id1, id2, id3] {
            api.detail(id, note_full(id));
        }

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&3), "all three pages' notes landed");
        assert!(api.requested("CURSOR2"));
        assert!(api.requested("CURSOR3"));
    }

    #[test]
    fn repoll_same_note_no_duplicate() {
        let v = temp_vault("repoll");
        let note_id = "not_repoll12345678A";

        let api1 = MockApi::new();
        api1.list_page(vec![note_summary(note_id, "2026-06-10T17:05:00Z")], false, None);
        api1.detail(note_id, note_full(note_id));
        pull_with(&v, &api1).unwrap();

        // Reset watermark to re-see the same note.
        v.write_granola_sync(&SyncState::default()).unwrap();
        let api2 = MockApi::new();
        api2.list_page(vec![note_summary(note_id, "2026-06-10T17:05:00Z")], false, None);
        api2.detail(note_id, note_full(note_id));
        pull_with(&v, &api2).unwrap();

        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1, "upsert by guid — one row after a re-poll");
        let raw = std::fs::read_to_string(v.root().join("meetings/granola/raw/2026-06.jsonl"))
            .unwrap();
        assert_eq!(raw.lines().count(), 1, "raw deduped by note id too");
    }

    #[test]
    fn watermark_advanced_to_newest_updated_at() {
        let v = temp_vault("watermark");
        let api = MockApi::new();
        let id1 = "not_wm_early0000001A";
        let id2 = "not_wm_latest000001A";
        api.list_page(
            vec![
                note_summary(id1, "2026-06-10T17:00:00Z"),
                note_summary(id2, "2026-06-11T09:00:00Z"),
            ],
            false,
            None,
        );
        api.detail(id1, note_full(id1));
        api.detail(id2, note_full(id2));

        pull_with(&v, &api).unwrap();
        let state = v.read_granola_sync();
        assert_eq!(
            state.last_updated_at.as_deref(),
            Some("2026-06-11T09:00:00Z"),
            "watermark is the newest updated_at"
        );
    }

    #[test]
    fn key_never_in_cursor() {
        let v = temp_vault("secret");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "grn_secret_key_xyz".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let api = MockApi::new();
        let secret_id = "not_secrettest0001A";
        api.list_page(vec![note_summary(secret_id, "2026-06-10T17:05:00Z")], false, None);
        api.detail(secret_id, note_full(secret_id));
        pull_with(&v, &api).unwrap();

        let cursor =
            std::fs::read_to_string(v.root().join(".trove/granola-sync.json")).unwrap();
        assert!(!cursor.contains("grn_secret"), "API key never in the cursor");
        assert!(!cursor.contains("access_token"), "no token field in the cursor");
    }

    #[test]
    fn empty_key_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn cursor_back_compat() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_updated_at.is_none());
        let partial: SyncState =
            serde_json::from_str(r#"{"last_updated_at":"2026-06-01T00:00:00Z"}"#).unwrap();
        assert_eq!(partial.last_updated_at.as_deref(), Some("2026-06-01T00:00:00Z"));
        // Unknown future fields are tolerated.
        let legacy: SyncState = serde_json::from_str(
            r#"{"last_updated_at":"2026-06-01T00:00:00Z","future_field":"ignored"}"#,
        )
        .unwrap();
        assert_eq!(legacy.last_updated_at.as_deref(), Some("2026-06-01T00:00:00Z"));
    }

    #[test]
    fn meeting_serde_back_compat() {
        // A sparse row written by an older writer still deserializes.
        let old = serde_json::json!({
            "ts": "2026-06-02T11:05:00-07:00",
            "source": "granola",
            "guid": "not_oldformat0000A",
            "title": "Old Meeting",
            "future_field": "ignored"
        });
        let m: Meeting = serde_json::from_value(old).unwrap();
        assert_eq!(m.guid, "not_oldformat0000A");
        assert_eq!(m.title, "Old Meeting");
        assert!(m.transcript_ref.is_empty());
        let re = serde_json::to_value(&m).unwrap();
        assert!(re.get("future_field").is_none());
        assert!(re.get("attendees").is_none(), "empty vec omitted");
    }

    #[test]
    fn raw_note_roundtrips_full_fidelity() {
        let r = RawNote { fields: note_full("not_rt123456789AB").as_object().unwrap().clone() };
        assert_eq!(r.id(), "not_rt123456789AB");
        assert_eq!(r.start_ts(), "2026-06-10T16:00:00Z");
        let line = serde_json::to_string(&r).unwrap();
        let back: RawNote = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r, "round-trips identically");
        assert!(line.contains("\"id\":\"not_rt123456789AB\""));
        assert!(line.contains("calendar_event"));
        assert!(line.contains("transcript"));
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "granola");
    }

    #[test]
    fn urlencode_handles_iso8601_timestamps() {
        let ts = "2026-06-10T16:00:00Z";
        let encoded = urlencode(ts);
        assert!(encoded.contains("%3A"), "colons encoded: {encoded}");
        assert!(encoded.contains('T'), "T passes through");
        assert!(!encoded.contains(':'), "raw colons absent: {encoded}");
    }
}

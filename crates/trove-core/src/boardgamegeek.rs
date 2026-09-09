//! BoardGameGeek — board-game *plays* and *collection* via the keyless
//! public XML API2 (`boardgamegeek.com/xmlapi2`).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/boardgamegeek.md.
//!
//! A **Periodic** cloud pull. `gaming/` is **raw-only** in the taxonomy — there
//! is NO write-time contract (like discord's `gaming/` and garmin's `health/`),
//! so this collector writes full-fidelity raw JSONL and touches no `DOMAINS`
//! struct or spec. Two streams:
//!
//! - **plays** — one row per logged play at
//!   `gaming/boardgamegeek/plays/YYYY-MM.jsonl`, partitioned by the play
//!   *date's* month, deduped/upserted by the BGG play id (`guid`). Full
//!   fidelity: id, date, quantity, length (minutes), location, incomplete,
//!   nowinstats, the item (name/objectid/objecttype/subtypes), every player,
//!   and the comment — unknown attributes overflow into a generic map rather
//!   than being dropped.
//! - **collection** — the latest snapshot at `gaming/boardgamegeek/collection.jsonl`,
//!   **rewritten whole** each pull (it is current-state, not an event log): one
//!   row per collection item (objectid, name, year, statuses, numplays,
//!   rating, plus full fidelity).
//!
//! `GET /xmlapi2/plays?username=X&page=N[&mindate=YYYY-MM-DD]` returns
//! `<plays total= page=>` paginated ~100/page. Every sync pages the whole
//! window from `mindate` (the latest play date already seen) forward; the first
//! sync (no watermark) backfills the entire history. The collection endpoint
//! `GET /xmlapi2/collection?username=X&stats=1` famously answers the FIRST
//! request with **HTTP 202** ("accepted, try again later") while it builds the
//! response, so it is polled with a short backoff and, if still queued after a
//! few tries, skipped for this pull (the plays are kept — graceful, no error).
//!
//! Auth is just a public username — public reads are **keyless**. The username
//! is the connection's single pasted field (TokenPaste), stored 0600 in the
//! `access_token` slot of a never-expiring [`crate::sync::oauth::TokenSet`]
//! under `.trove/sync/boardgamegeek.json`, exactly like [`crate::listenbrainz`].
//! The watermark (latest play date + last collection-sync time) lives in a
//! rebuildable, non-secret cursor at `.trove/boardgamegeek-sync.json`.
//!
//! XML shapes confirmed against real captured `xmlapi2` responses
//! (`tnaskali/bgg-api` test fixtures): the play container is
//! `<plays total= page=>`; each `<play id date quantity length incomplete
//! nowinstats location>` carries `<item name objecttype objectid>` (with
//! `<subtypes><subtype value/></subtypes>`), an optional `<players>` of
//! `<player username userid name startposition color score new rating win/>`,
//! and an optional `<comments>`-text. The collection container is
//! `<items totalitems=>`; each `<item objecttype objectid subtype collid>` has
//! `<name sortindex>`-text, `<yearpublished>`-text, `<numplays>`-text,
//! `<stats …><rating value=>` (the value is the USER's rating, `"N/A"` when
//! unrated), and `<status own prevowned … wishlist preordered lastmodified>`.
//! The 202 body is `<message>…try again later…</message>`.

use std::collections::{BTreeMap, HashSet};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Local};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

/// Plays stream directory (month-partitioned JSONL).
const PLAYS_DIR: &str = "gaming/boardgamegeek/plays";
/// Collection snapshot file (rewritten whole each pull).
const COLLECTION_REL: &str = "gaming/boardgamegeek/collection.jsonl";
/// Non-secret rebuildable cursor — *not* under `.trove/sync/` (that's for 0600
/// secrets); deleting it re-walks the whole play history on the next sync.
const SYNC_FILE: &str = ".trove/boardgamegeek-sync.json";

/// The service id under `.trove/sync/` where the username is stored (reusing
/// the secret store's `service-token.json` slot, like Last.fm/ListenBrainz).
const SERVICE: &str = "boardgamegeek";

const API_BASE: &str = "https://boardgamegeek.com";
/// BGG serves up to 100 plays per page.
const PLAYS_PAGE_SIZE: u32 = 100;
/// Hard cap on the plays pagination walk (a runaway guard; pagination normally
/// stops on a short/empty page). 1000 pages = ~100k plays — far beyond any real
/// log, but bounds the loop if the API never returns a short page.
const PLAYS_MAX_PAGES: u32 = 1000;
/// Politeness throttle: ~2 req/sec → 500ms between requests.
const REQ_INTERVAL: Duration = Duration::from_millis(500);
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// How many times to re-poll a 202-queued collection before giving up *for
/// this pull* (the plays are kept regardless; the next tick retries).
const COLLECTION_MAX_RETRIES: u32 = 5;
/// Backoff between collection 202 retries.
const COLLECTION_RETRY_DELAY: Duration = Duration::from_secs(3);
/// Seconds between syncs in the watcher loop. Hourly: plays trickle in and the
/// incremental `mindate` poll is one cheap request when idle.
pub const BGG_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    // Newest play partition stem, else the collection snapshot's mtime.
    crate::registry::newest_stem(&vault.root().join(PLAYS_DIR))
        .or_else(|| crate::registry::file_mtime(&vault.root().join(COLLECTION_REL)))
}

// Periodic pass: the same pull the manual "Sync now" runs, but it never errors
// the loop — a missing username or a network blip is just a quiet no-op until
// the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let plays = out.counts.get("plays").copied().unwrap_or(0);
            let coll = out.counts.get("collection").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(plays > 0 || coll > 0, || {
                format!("boardgamegeek synced — {plays} plays, {coll} collection items")
            }))
        }
        // Not connected / transient network: stay silent, retry next tick. A
        // real bug still surfaces in the log via the message.
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "boardgamegeek sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let plays = out.counts.get("plays").copied().unwrap_or(0);
    let coll = out.counts.get("collection").copied().unwrap_or(0);
    let headline = if plays == 0 {
        format!("BoardGameGeek is up to date — no new plays ({coll} collection items)")
    } else {
        format!("BoardGameGeek synced — {plays} new plays, {coll} collection items")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "boardgamegeek",
        name: "BoardGameGeek",
        kind: IntegrationKind::CloudSync,
        // Public play/collection data, not sensitive — on by default like
        // Last.fm/ListenBrainz (no 🔒 opt-in).
        default_on: true,
        description:
            "Syncs your logged board-game plays (date, game, duration, players, \
             location, comments) and your collection snapshot (owned, wishlist, \
             ratings) from BoardGameGeek's keyless public XML API. Only a public \
             username is needed.",
        domain: "gaming",
        vault_path: "gaming/boardgamegeek/",
        toggleable: true,
        setup: &[
            "Connect with your public BoardGameGeek username on this card.",
            "First sync backfills your whole play history; later syncs fetch only what's new.",
        ],
        caveats: "Reads your public BoardGameGeek profile — your profile must be public. \
                  Play durations are whatever you logged (0 when you left it blank). \
                  The collection endpoint is built on demand: the first request after a \
                  while is queued by BGG, so a sync may fetch only the plays and pick up \
                  the collection on the next run.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(BGG_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("boardgamegeek"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the public username).

/// Store the pasted username under `.trove/sync/boardgamegeek.json` (0600),
/// modeled on ListenBrainz: the username rides in the `access_token` slot of a
/// never-expiring [`crate::sync::oauth::TokenSet`]. Reads are keyless, so we
/// verify the profile exists with a cheap one-play probe; a clear error is
/// surfaced to the connect UI when the user doesn't exist.
fn def_connect(vault: &Vault, username: &str) -> Result<()> {
    let client = BggClient::new(API_BASE.to_string());
    connect_with(vault, &client, username)
}

/// The connect body over an injected fetcher — the testable seam (tests verify
/// against a stub, never the network).
fn connect_with(vault: &Vault, client: &impl BggApi, username: &str) -> Result<()> {
    let username = username.trim();
    if username.is_empty() {
        bail!("empty username");
    }
    // Keyless verification: a cheap 1-page plays probe. An explicit "invalid
    // username" (BGG answers with an <errors> body, sometimes a 4xx) blocks; a
    // network blip or anything else doesn't (the user may be briefly offline) —
    // the pull will retry.
    match client.plays(username, 1, None) {
        Ok(xml) => {
            if invalid_username(&xml) {
                bail!("BoardGameGeek could not find user {username:?} — check the spelling (the profile must be public)");
            }
        }
        Err(FetchError::InvalidUser) => {
            bail!("BoardGameGeek could not find user {username:?} — check the spelling (the profile must be public)")
        }
        Err(_) => {} // transient/other: store anyway, the pull will retry
    }
    let token = crate::sync::oauth::TokenSet {
        access_token: username.to_string(),
        refresh_token: None,
        token_type: None,
        scope: None,
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &token)
}

/// Forget the stored username. Synced data stays in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// `configured` is always true: public reads need no app credentials, so
/// connecting is just pasting a username. The connected account, if any, is the
/// stored username.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let username = token.access_token;
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: username,
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // a username never expires
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste a
/// public username. No api_key — public XML API reads are fully keyless.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "boardgamegeek",
    display_name: "BoardGameGeek",
    methods: &[ConnectMethod::TokenPaste {
        label: "BoardGameGeek username",
        help: "Enter your public BoardGameGeek username — your profile must be public. \
               Plays and collection are read from your public profile (no account or token needed).",
        placeholder: "e.g. Aldie",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["boardgamegeek"],
    setup: &[
        "Enter your public BoardGameGeek username and connect.",
        "Your profile must be public; plays and collection are read from it (no token needed).",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch outcomes. The collection endpoint's 202 "queued" is a
/// first-class case (we retry); invalid-user, rate-limited, and everything else
/// each want distinct handling.
#[derive(Debug)]
enum FetchError {
    /// HTTP 202 — collection is being built; retry shortly.
    Queued,
    /// The username doesn't exist / isn't public (BGG: 200 + <errors> body, or
    /// occasionally a 4xx).
    InvalidUser,
    /// HTTP 429 / 503 — back off.
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Queued => write!(f, "collection request queued (HTTP 202)"),
            FetchError::InvalidUser => write!(f, "invalid or non-public username"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429/503)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The two XML API2 endpoints this collector uses. Tests implement this against
/// fixtures; production hits the real API. Both return the raw XML body string.
trait BggApi {
    /// `GET /xmlapi2/plays?username=X&page=N[&mindate=YYYY-MM-DD]`.
    fn plays(&self, user: &str, page: u32, mindate: Option<&str>) -> Result<String, FetchError>;
    /// `GET /xmlapi2/collection?username=X&stats=1` — may answer [`FetchError::Queued`].
    fn collection(&self, user: &str) -> Result<String, FetchError>;
}

/// Thin client. The base URL is injected so the sync logic stays testable
/// against a local stub (the lastfm/listenbrainz pattern).
struct BggClient {
    base: String,
}

impl BggClient {
    fn new(base: String) -> Self {
        BggClient { base }
    }

    /// Map a `ureq` call result to a body string or a [`FetchError`]. 202 is
    /// surfaced as `Queued` (collection still building); 429/503 as
    /// `RateLimited`; a body that looks like BGG's `<errors>`/"invalid username"
    /// (or a 404) as `InvalidUser`; anything else as `Other`.
    fn finish(result: Result<ureq::Response, ureq::Error>) -> Result<String, FetchError> {
        match result {
            Ok(resp) => {
                // BGG returns 202 (no body of interest) while it builds a
                // collection; surface it so the caller can retry.
                if resp.status() == 202 {
                    return Err(FetchError::Queued);
                }
                let body = resp
                    .into_string()
                    .map_err(|e| FetchError::Other(format!("reading response: {e}")))?;
                // A 200 can still carry an <errors> body for a bad username.
                if invalid_username(&body) {
                    return Err(FetchError::InvalidUser);
                }
                Ok(body)
            }
            Err(ureq::Error::Status(202, _)) => Err(FetchError::Queued),
            Err(ureq::Error::Status(429 | 503, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                if invalid_username(&body) || code == 404 {
                    return Err(FetchError::InvalidUser);
                }
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl BggApi for BggClient {
    fn plays(&self, user: &str, page: u32, mindate: Option<&str>) -> Result<String, FetchError> {
        let mut req = ureq::get(&format!("{}/xmlapi2/plays", self.base))
            .timeout(HTTP_TIMEOUT)
            .query("username", user)
            .query("page", &page.to_string());
        if let Some(md) = mindate {
            req = req.query("mindate", md);
        }
        Self::finish(req.call())
    }

    fn collection(&self, user: &str) -> Result<String, FetchError> {
        let req = ureq::get(&format!("{}/xmlapi2/collection", self.base))
            .timeout(HTTP_TIMEOUT)
            .query("username", user)
            .query("stats", "1");
        Self::finish(req.call())
    }
}

/// True if an XML body is BGG's bad-username error envelope
/// (`<errors><error><message>Invalid username specified</message>…`). Cheap
/// substring check — the body is tiny in this case.
fn invalid_username(xml: &str) -> bool {
    xml.contains("<errors") && xml.to_lowercase().contains("invalid username")
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Latest play `date` (YYYY-MM-DD) ever written. The next incremental poll
    /// passes `mindate=<watermark>` so only plays on/after that date come back
    /// (guid dedupe drops the same-day overlap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mindate: Option<String>,
    /// RFC3339 local time of the last successful collection sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collection_synced: Option<String>,
    /// RFC3339 local time of the last successful sync (any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_bgg_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_bgg_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested. quick-xml streaming reader; every element &
// attribute is preserved into a flat JSON object (full fidelity, raw-only).

/// One parsed play, carrying both its partition key (`date`) and the full raw
/// JSON object that lands on disk.
struct ParsedPlay {
    /// The BGG play id — the dedupe `guid`.
    id: String,
    /// Play date, `YYYY-MM-DD` — the month-partition key and `ts`.
    date: String,
    /// The full-fidelity row written to disk.
    raw: Value,
}

/// The container `<plays total= page=>` attributes (strings in the API), plus
/// the count of `<play>` elements actually present on this page.
struct PlaysMeta {
    /// The `<plays total=>` attribute. BGG reports this as `0` for plays (a
    /// long-documented API quirk), so it is read for logging only and is
    /// **never** used to terminate pagination — see [`sync_plays`].
    #[allow(dead_code)]
    total: u32,
    #[allow(dead_code)] // parsed for completeness; pagination uses page length
    page: u32,
    /// Number of `<play>` elements on the page (counted before the id/date
    /// validity filter, so a skipped malformed play still counts toward "this
    /// page wasn't short"). A full page == [`PLAYS_PAGE_SIZE`]; fewer means the
    /// last page; zero means past the end.
    play_count: u32,
}

/// Parse one `/xmlapi2/plays` response into (rows, page meta). Each `<play>`
/// becomes a flat JSON object preserving every attribute, the `<item>` (with
/// its `<subtypes>`), every `<player>`, and the `<comments>` text. Plays with a
/// missing/blank `date` or `id` are skipped (can't be partitioned/deduped).
/// Never panics on malformed/empty XML — returns whatever parsed cleanly.
fn parse_plays(xml: &str) -> (Vec<ParsedPlay>, PlaysMeta) {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::with_capacity(4096);
    let mut meta = PlaysMeta { total: 0, page: 0, play_count: 0 };

    let mut plays: Vec<ParsedPlay> = Vec::new();

    // Per-play accumulators.
    let mut cur: Option<Map<String, Value>> = None; // the play attrs
    let mut cur_item: Option<Map<String, Value>> = None;
    let mut cur_subtypes: Vec<Value> = Vec::new();
    let mut cur_players: Vec<Value> = Vec::new();
    // Text capture: when inside <comments>, accumulate text.
    let mut in_comments = false;
    let mut comments = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            // Self-closing tags (`<subtype …/>`, `<player …/>`, `<item …/>`)
            // arrive as `Empty`; opening tags as `Start`. Both carry the same
            // attributes, so handle them together.
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => match e.name().as_ref() {
                b"plays" => {
                    let attrs = attrs_map(e);
                    meta.total = attrs.get("total").and_then(num).unwrap_or(0);
                    meta.page = attrs.get("page").and_then(num).unwrap_or(0);
                }
                b"play" => {
                    meta.play_count += 1;
                    cur = Some(attrs_map(e));
                    cur_item = None;
                    cur_subtypes.clear();
                    cur_players.clear();
                    comments.clear();
                    in_comments = false;
                }
                b"item" => {
                    cur_item = Some(attrs_map(e));
                }
                b"subtype" => {
                    if let Some(v) = attrs_map(e).get("value").and_then(Value::as_str) {
                        cur_subtypes.push(Value::String(v.to_string()));
                    }
                }
                b"player" => {
                    cur_players.push(Value::Object(attrs_map(e)));
                }
                b"comments" => {
                    in_comments = true;
                    comments.clear();
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if in_comments {
                    comments.push_str(&text_of(&e));
                }
            }
            // quick-xml splits text at entity references: `&amp;` arrives here,
            // between two Text events. Resolve and append it so comments keep
            // their `&`, accented chars, emoji-codes, etc.
            Ok(Event::GeneralRef(ref r)) => {
                if in_comments {
                    comments.push_str(&resolve_ref(r));
                }
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"comments" => in_comments = false,
                b"item" => {
                    // Attach the collected subtypes to the item.
                    if let Some(item) = cur_item.as_mut() {
                        if !cur_subtypes.is_empty() {
                            item.insert("subtypes".into(), Value::Array(std::mem::take(&mut cur_subtypes)));
                        }
                    }
                }
                b"play" => {
                    if let Some(mut play) = cur.take() {
                        let id = play.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                        let date =
                            play.get("date").and_then(Value::as_str).unwrap_or("").to_string();
                        // Attach children.
                        if let Some(item) = cur_item.take() {
                            play.insert("item".into(), Value::Object(item));
                        }
                        if !cur_players.is_empty() {
                            play.insert("players".into(), Value::Array(std::mem::take(&mut cur_players)));
                        }
                        let c = comments.trim();
                        if !c.is_empty() {
                            play.insert("comments".into(), Value::String(c.to_string()));
                        }
                        // Skip plays we can't partition (no/blank date) or
                        // dedupe (no id).
                        if !id.is_empty() && Partition::Month.key(&date).is_some() {
                            plays.push(ParsedPlay { id, date, raw: Value::Object(play) });
                        }
                    }
                    cur_subtypes.clear();
                    cur_players.clear();
                    comments.clear();
                    in_comments = false;
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            // Malformed XML: stop, keep whatever parsed cleanly (never panic).
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    (plays, meta)
}

/// One parsed collection item: the full-fidelity row.
struct ParsedCollItem {
    raw: Value,
}

/// Which depth-1 sub-block of an `<item>` we're inside, if any. Each is handled
/// specially rather than captured as a generic item-level scalar.
#[derive(PartialEq)]
enum CollBlock {
    /// Directly inside `<item>` (no sub-block).
    None,
    /// `<stats>` — only the user `<rating value=>` is taken from it.
    Stats,
    /// `<version>` — a heavy sub-block (only present with `version=1`, which we
    /// don't request); skipped wholesale so it can't clobber item-level fields.
    Version,
    /// `<privateinfo>` — private-pull-only; attributes + `<privatecomment>` kept.
    PrivateInfo,
}

/// Parse one `/xmlapi2/collection` response into rows, full-fidelity. Each
/// `<item>` becomes a flat JSON object: every attribute
/// (objectid/objecttype/subtype/collid), the `<status>` attributes under
/// `"status"`, the user rating from `<stats><rating value=>` (under `"rating"`,
/// dropped when `"N/A"`), the `<stats>` count attributes under `"stats"`, the
/// `<privateinfo>` block (attributes + `<privatecomment>`) under
/// `"privateinfo"`, and — captured **generically** — every other text-bearing
/// direct child by its element name (name, originalname, yearpublished,
/// numplays, image, thumbnail, wishlistcomment, wantpartslist, haspartslist,
/// comment, …) so nothing the API returns is dropped. The `<version>` sub-block
/// is skipped. Never panics on malformed/empty XML.
fn parse_collection(xml: &str) -> Vec<ParsedCollItem> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::with_capacity(4096);
    let mut items: Vec<ParsedCollItem> = Vec::new();

    let mut cur: Option<Map<String, Value>> = None;
    let mut block = CollBlock::None;
    // Depth of arbitrarily-nested elements *inside* the current sub-block, so we
    // know when its closing tag returns us to the item level. Only meaningful
    // when `block != None`.
    let mut block_depth: u32 = 0;
    let mut privateinfo: Option<Map<String, Value>> = None;
    // The item-level (or privatecomment) scalar currently being captured.
    let mut text_key: Option<String> = None;
    let mut text_buf = String::new();

    /// Depth-1 children of `<item>` that open a special sub-block or are
    /// summarized via attributes (not captured as generic text scalars).
    fn is_special_child(name: &[u8]) -> bool {
        matches!(name, b"stats" | b"status" | b"version" | b"privateinfo")
    }

    loop {
        // Distinguish self-closing (`Empty`, no matching End) from `Start`, so
        // depth/sub-block tracking stays balanced and a leaf scalar only arms
        // text capture when it can actually contain text.
        let event = reader.read_event_into(&mut buf);
        let (start, ev) = match event {
            Ok(Event::Start(e)) => (true, Event::Start(e)),
            Ok(Event::Empty(e)) => (false, Event::Empty(e)),
            Ok(other) => (false, other),
            Err(_) => break, // malformed: keep whatever parsed cleanly
        };
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = e.name();
                let name = name.as_ref();
                match &block {
                    // --- Inside a captured/skipped sub-block of the item. ---
                    CollBlock::Stats => {
                        if name == b"rating" {
                            // The USER's rating: <stats><rating value=>.
                            if let Some(item) = cur.as_mut() {
                                if let Some(v) =
                                    attrs_map(e).get("value").and_then(Value::as_str)
                                {
                                    // "N/A" means unrated — record only a real one.
                                    if !v.eq_ignore_ascii_case("n/a") && !v.is_empty() {
                                        item.insert("rating".into(), Value::String(v.to_string()));
                                    }
                                }
                            }
                        }
                        if start {
                            block_depth += 1;
                        }
                    }
                    CollBlock::Version => {
                        if start {
                            block_depth += 1;
                        }
                    }
                    CollBlock::PrivateInfo => {
                        if name == b"privatecomment" {
                            if start {
                                text_key = Some("privatecomment".into());
                                text_buf.clear();
                            }
                            // (An empty <privatecomment/> simply stays absent.)
                        }
                        if start {
                            block_depth += 1;
                        }
                    }
                    // --- At the item level (or outside any item). ---
                    CollBlock::None => match name {
                        b"item" if cur.is_none() => {
                            cur = Some(attrs_map(e));
                            privateinfo = None;
                            text_key = None;
                            text_buf.clear();
                        }
                        b"stats" if cur.is_some() => {
                            // Preserve the play-/player-count stats attributes.
                            if let Some(item) = cur.as_mut() {
                                let m = attrs_map(e);
                                if !m.is_empty() {
                                    item.insert("stats".into(), Value::Object(m));
                                }
                            }
                            if start {
                                block = CollBlock::Stats;
                                block_depth = 1;
                            }
                        }
                        b"status" if cur.is_some() => {
                            // `<status …/>` is self-closing (attributes only).
                            if let Some(item) = cur.as_mut() {
                                item.insert("status".into(), Value::Object(attrs_map(e)));
                            }
                            // If it ever carried a body, enter a skipped block so
                            // its children aren't mistaken for item-level fields.
                            if start {
                                block = CollBlock::Version;
                                block_depth = 1;
                            }
                        }
                        b"version" if cur.is_some() => {
                            if start {
                                block = CollBlock::Version;
                                block_depth = 1;
                            }
                        }
                        b"privateinfo" if cur.is_some() => {
                            privateinfo = Some(attrs_map(e));
                            if start {
                                block = CollBlock::PrivateInfo;
                                block_depth = 1;
                            }
                        }
                        // Any other DIRECT child of <item> → capture its text
                        // generically by element name (full fidelity). Only on
                        // Start: a self-closing leaf has no text to capture.
                        _ if cur.is_some() && start && !is_special_child(name) => {
                            text_key = Some(String::from_utf8_lossy(name).into_owned());
                            text_buf.clear();
                        }
                        _ => {}
                    },
                }
            }
            Event::Text(e) => {
                if text_key.is_some() {
                    text_buf.push_str(&text_of(&e));
                }
            }
            // An entity reference inside a captured text element (e.g. `&amp;`
            // in a game name) — resolve and append.
            Event::GeneralRef(ref r) => {
                if text_key.is_some() {
                    text_buf.push_str(&resolve_ref(r));
                }
            }
            Event::End(ref e) => {
                let name = e.name();
                let name = name.as_ref();
                if block != CollBlock::None {
                    // Closing a tag inside a sub-block.
                    if let Some(key) = text_key.take() {
                        // Only <privatecomment> is captured inside a sub-block.
                        let t = text_buf.trim();
                        if key == "privatecomment" && !t.is_empty() {
                            if let Some(pi) = privateinfo.as_mut() {
                                pi.insert("privatecomment".into(), Value::String(t.to_string()));
                            }
                        }
                        text_buf.clear();
                    }
                    block_depth = block_depth.saturating_sub(1);
                    if block_depth == 0 {
                        block = CollBlock::None; // back to item level
                    }
                } else if name == b"item" {
                    // Close the top-level item.
                    if let Some(mut item) = cur.take() {
                        if let Some(pi) = privateinfo.take() {
                            item.insert("privateinfo".into(), Value::Object(pi));
                        }
                        // An item must have an objectid to be useful.
                        if item
                            .get("objectid")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.is_empty())
                        {
                            items.push(ParsedCollItem { raw: Value::Object(item) });
                        }
                    }
                    text_key = None;
                    text_buf.clear();
                } else if let Some(key) = text_key.take() {
                    // Close a generic item-level scalar — store its text.
                    let t = text_buf.trim();
                    if !t.is_empty() {
                        if let Some(item) = cur.as_mut() {
                            item.insert(key, Value::String(t.to_string()));
                        }
                    }
                    text_buf.clear();
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    items
}

/// Decode a text event and un-escape its XML entities (`&amp;` → `&`, …).
/// In quick-xml 0.40 `decode()` handles the encoding/EOL but leaves entities,
/// so we run [`quick_xml::escape::unescape`] on top. Lenient: a decode failure
/// yields "", and a malformed entity falls back to the decoded-but-escaped
/// text (never drop a comment over one bad `&`).
fn text_of(e: &quick_xml::events::BytesText) -> String {
    match e.decode() {
        Ok(decoded) => match quick_xml::escape::unescape(&decoded) {
            Ok(unescaped) => unescaped.into_owned(),
            Err(_) => decoded.into_owned(),
        },
        Err(_) => String::new(),
    }
}

/// Resolve a [`Event::GeneralRef`] entity reference to its text. Numeric refs
/// (`&#39;`, `&#x2764;`) and the five XML predefined entities (`amp`/`lt`/`gt`/
/// `apos`/`quot`) resolve; an unknown named entity is re-emitted verbatim as
/// `&name;` so no character is silently dropped. Lenient — never panics.
fn resolve_ref(r: &quick_xml::events::BytesRef) -> String {
    // Numeric character references first.
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        return ch.to_string();
    }
    match r.decode() {
        Ok(name) => quick_xml::escape::resolve_predefined_entity(&name)
            .map(str::to_string)
            .unwrap_or_else(|| format!("&{name};")),
        Err(_) => String::new(),
    }
}

/// Every attribute of a start/empty element as a `{name: value}` JSON object,
/// with entities un-escaped. Full fidelity: nothing is filtered.
fn attrs_map(e: &quick_xml::events::BytesStart) -> Map<String, Value> {
    let mut m = Map::new();
    for a in e.attributes().flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let val = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_default();
        m.insert(key, Value::String(val));
    }
    m
}

/// Parse a non-negative integer attribute (BGG counts are strings).
fn num(v: &Value) -> Option<u32> {
    v.as_str().and_then(|s| s.trim().parse::<u32>().ok())
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the username and sync plays + collection. Returns a generic
/// [`PullOutcome`] with `plays` and `collection` counts.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let username = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("BoardGameGeek is not connected — add your username in the Integrations tab")
        })?;
    let client = BggClient::new(API_BASE.to_string());
    pull_with(vault, &client, &username)
}

/// The pull body over an injected fetcher — the testable seam.
fn pull_with(vault: &Vault, client: &impl BggApi, username: &str) -> Result<PullOutcome> {
    let mut state = vault.read_bgg_sync();

    // --- Plays: paginate the whole window from the watermark mindate. ---
    let plays_written = sync_plays(vault, client, username, &mut state)?;

    // Throttle between the plays pass and the collection request.
    thread::sleep(REQ_INTERVAL);

    // --- Collection: snapshot, with the 202-queue retry. ---
    let collection_count = match sync_collection(vault, client, username) {
        Ok(Some(n)) => {
            state.collection_synced = Some(Local::now().to_rfc3339());
            n
        }
        // Still queued after N tries, or a transient error: keep the plays,
        // skip the collection this pull (the next tick retries).
        Ok(None) | Err(_) => 0,
    };

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_bgg_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{plays_written} plays, {collection_count} collection items"),
        counts: BTreeMap::from([("plays", plays_written), ("collection", collection_count)]),
    })
}

/// Page through `/xmlapi2/plays` from the watermark `mindate`, writing every
/// new play (deduped by id) into its month partition. Advances the watermark to
/// the latest play date written. Returns the count of NEW plays written.
fn sync_plays(
    vault: &Vault,
    client: &impl BggApi,
    username: &str,
    state: &mut SyncState,
) -> Result<u64> {
    let stream = vault.stream(PLAYS_DIR, Partition::Month);

    // Existing guids across all partitions — re-runnable: a re-pull of the
    // same-date overlap (mindate is inclusive) never duplicates.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for row in stream.read::<Value>(&key)? {
            if let Some(id) = row.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    seen.insert(id.to_string());
                }
            }
        }
    }

    let mindate = state.mindate.clone();
    let mut total_written: u64 = 0;
    let mut latest_date = state.mindate.clone();

    let mut page: u32 = 1;
    loop {
        let xml = match fetch_plays_page(client, username, page, mindate.as_deref()) {
            Ok(x) => x,
            // No such user only matters at connect; here treat as "nothing".
            Err(FetchError::InvalidUser) => break,
            Err(e) => bail!("BoardGameGeek plays fetch failed: {e}"),
        };
        let (parsed, meta) = parse_plays(&xml);

        // Buffer the new rows for this page, deduped by id.
        let mut new_rows: Vec<RawRow> = Vec::new();
        for p in &parsed {
            // Track the latest play date regardless of dedupe (string compare
            // is correct for YYYY-MM-DD).
            if latest_date.as_deref().is_none_or(|d| p.date.as_str() > d) {
                latest_date = Some(p.date.clone());
            }
            if !seen.insert(p.id.clone()) {
                continue; // already stored
            }
            new_rows.push(RawRow { ts: p.date.clone(), value: p.raw.clone() });
        }
        if !new_rows.is_empty() {
            stream.append(&new_rows, |r| &r.ts)?;
            total_written += new_rows.len() as u64;
        }

        // Pagination — DO NOT trust `<plays total=>`: BGG reports it as 0 for
        // plays (long-documented quirk; the canonical lcosmin/boardgamegeek and
        // Gestwicki clients both "page until the page is empty"). Terminate on a
        // page that is EMPTY (past the end) or SHORT (fewer than the 100/page
        // size = the last page). `play_count` is the raw element count, so a
        // play skipped for a blank date doesn't falsely shorten the page.
        if meta.play_count == 0 || meta.play_count < PLAYS_PAGE_SIZE {
            break;
        }
        // Runaway guard: bound the walk even if the API never returns a short
        // page (a >100k-play account is ~1000 pages; cap there).
        if page >= PLAYS_MAX_PAGES {
            break;
        }
        page += 1;
        thread::sleep(REQ_INTERVAL); // ~2 req/s politeness
    }

    // Advance the watermark to the latest play date written (forward-only).
    if let Some(d) = latest_date {
        if state.mindate.as_deref().is_none_or(|w| d.as_str() > w) {
            state.mindate = Some(d);
        }
    }

    Ok(total_written)
}

/// One plays-page fetch with a single rate-limit back-off-and-retry (429/503).
fn fetch_plays_page(
    client: &impl BggApi,
    user: &str,
    page: u32,
    mindate: Option<&str>,
) -> Result<String, FetchError> {
    match client.plays(user, page, mindate) {
        Ok(x) => Ok(x),
        Err(FetchError::RateLimited) => {
            thread::sleep(Duration::from_secs(2));
            client.plays(user, page, mindate)
        }
        Err(e) => Err(e),
    }
}

/// Fetch the collection snapshot, honoring the 202-queue: poll with a short
/// backoff up to [`COLLECTION_MAX_RETRIES`]. Returns:
/// - `Ok(Some(n))` — fetched & rewrote the snapshot (`n` items);
/// - `Ok(None)` — still queued/limited after the retries (skip gracefully, keep
///   plays);
/// - `Err(_)` — a hard error (invalid user / network); the caller treats it as
///   a skip too, so plays survive.
fn sync_collection(vault: &Vault, client: &impl BggApi, username: &str) -> Result<Option<u64>> {
    let mut attempts = 0;
    let xml = loop {
        match client.collection(username) {
            Ok(x) => break x,
            // 202 (still building) or 429/503 (rate limited): back off and
            // retry, up to the cap, then give up gracefully for this pull.
            Err(FetchError::Queued) | Err(FetchError::RateLimited) => {
                attempts += 1;
                if attempts > COLLECTION_MAX_RETRIES {
                    return Ok(None);
                }
                thread::sleep(COLLECTION_RETRY_DELAY);
            }
            Err(FetchError::InvalidUser) => bail!("BoardGameGeek could not find user {username:?}"),
            Err(e) => bail!("BoardGameGeek collection fetch failed: {e}"),
        }
    };

    let items = parse_collection(&xml);
    let rows: Vec<Value> = items.into_iter().map(|i| i.raw).collect();
    // Current-state snapshot → rewritten whole each pull.
    vault.write_snapshot(COLLECTION_REL, &rows)?;
    Ok(Some(rows.len() as u64))
}

/// A raw play object carrying the partition ts (the play date) purely so the
/// month-partition writer files it under the play's month. Only `value` is
/// serialized to disk — flattened, so the line is the full-fidelity object.
#[derive(Serialize)]
struct RawRow {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-bgg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures synthesized from the CONFIRMED real XML shapes
    // (tnaskali/bgg-api captured responses): see the module docs. ---

    /// A plays page: 3 plays — one multi-player game with a comment, one solo
    /// play, one play with NO players element and a unicode/entity comment.
    /// `total="3" page="1"` → single page.
    fn plays_page1() -> String {
        r#"<?xml version="1.0" encoding="utf-8"?>
<plays username="tester" userid="42" total="3" page="1" termsofuse="https://boardgamegeek.com/xmlapi/termsofuse">
  <play id="100" date="2023-08-03" quantity="2" length="60" incomplete="0" nowinstats="0" location="Home">
    <item name="Azul" objecttype="thing" objectid="230802">
      <subtypes>
        <subtype value="boardgame" />
      </subtypes>
    </item>
    <comments>good game &amp; close finish</comments>
    <players>
      <player username="" userid="0" name="Friend" startposition="1" color="blue" score="55" new="0" rating="7" win="0" />
      <player username="tester" userid="42" name="Me" startposition="2" color="red" score="76" new="0" rating="0" win="1" />
    </players>
  </play>
  <play id="101" date="2023-08-04" quantity="1" length="0" incomplete="0" nowinstats="0" location="">
    <item name="Othello" objecttype="thing" objectid="2389">
      <subtypes>
        <subtype value="boardgame" />
      </subtypes>
    </item>
  </play>
  <play id="102" date="2021-12-05" quantity="1" length="30" incomplete="0" nowinstats="1" location="café">
    <item name="Unlock!" objecttype="thing" objectid="312267">
      <subtypes>
        <subtype value="boardgame" />
        <subtype value="boardgamecompilation" />
      </subtypes>
    </item>
    <comments>scénario: la bataille de Hoth
      score: 5 étoiles &amp; 0 hints</comments>
  </play>
</plays>"#
            .to_string()
    }

    /// Build a plays page with `n` `<play>` elements (each a minimal but valid
    /// play with a unique id and date), and a chosen `total` attribute. This is
    /// how pagination is exercised realistically: BGG reports `total="0"` for
    /// plays, so the loop must NOT trust it — termination is by page LENGTH
    /// (a short or empty page is the last one).
    fn plays_page_of(n: u32, total_attr: u32, page: u32, first_id: u32) -> String {
        let mut s = format!("<plays username=\"tester\" userid=\"42\" total=\"{total_attr}\" page=\"{page}\">");
        for i in 0..n {
            let id = first_id + i;
            // Spread dates across days so they all sort/partition cleanly; the
            // exact day doesn't matter for the pagination assertions.
            let day = 1 + (i % 27);
            s.push_str(&format!(
                "<play id=\"{id}\" date=\"2024-01-{day:02}\" quantity=\"1\" length=\"30\" incomplete=\"0\" nowinstats=\"0\" location=\"Home\">\
<item name=\"Game {id}\" objecttype=\"thing\" objectid=\"{id}\"><subtypes><subtype value=\"boardgame\"/></subtypes></item></play>"
            ));
        }
        s.push_str("</plays>");
        s
    }

    /// An empty plays page (used when a stub is asked past its last fixture, or
    /// when a pull should write zero plays but still fetch the collection).
    fn plays_empty() -> String {
        "<plays total=\"0\" page=\"0\"></plays>".to_string()
    }

    /// A collection response: an owned game WITH a real rating, and a wishlist
    /// item (no rating). Mirrors the captured shape (stats/rating value=, the
    /// TAB-separated stats attrs, status flags, name/yearpublished/numplays).
    fn collection_ok() -> String {
        "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n\
<items totalitems=\"2\" termsofuse=\"https://boardgamegeek.com/xmlapi/termsofuse\" pubdate=\"Sat, 21 Feb 2026 09:22:52 +0000\">\n\
  <item objecttype=\"thing\" objectid=\"230802\" subtype=\"boardgame\" collid=\"111\">\n\
    <name sortindex=\"1\">Azul</name>\n\
    <yearpublished>2017</yearpublished>\n\
    <image>https://example.com/azul.jpg</image>\n\
    <stats minplayers=\"2\"\tmaxplayers=\"4\"\tplayingtime=\"45\"\tnumowned=\"100000\" >\n\
      <rating value=\"9\">\n\
        <usersrated value=\"100\" />\n\
        <average value=\"7.8\" />\n\
      </rating>\n\
    </stats>\n\
    <status own=\"1\" prevowned=\"0\" fortrade=\"0\" want=\"0\" wanttoplay=\"0\" wanttobuy=\"0\" wishlist=\"0\" preordered=\"0\" lastmodified=\"2020-01-11 11:15:53\" />\n\
    <numplays>12</numplays>\n\
  </item>\n\
  <item objecttype=\"thing\" objectid=\"174430\" subtype=\"boardgame\" collid=\"222\">\n\
    <name sortindex=\"1\">Gloomhaven &amp; Friends</name>\n\
    <yearpublished>2017</yearpublished>\n\
    <stats minplayers=\"1\" maxplayers=\"4\" >\n\
      <rating value=\"N/A\">\n\
        <usersrated value=\"50000\" />\n\
      </rating>\n\
    </stats>\n\
    <status own=\"0\" prevowned=\"0\" fortrade=\"0\" want=\"0\" wanttoplay=\"0\" wanttobuy=\"0\" wishlist=\"1\" wishlistpriority=\"2\" preordered=\"0\" lastmodified=\"2021-05-01 10:00:00\" />\n\
    <numplays>0</numplays>\n\
  </item>\n\
</items>"
            .to_string()
    }

    // ---- Parsing ----

    #[test]
    fn parses_plays_full_fidelity() {
        let (rows, meta) = parse_plays(&plays_page1());
        assert_eq!(meta.total, 3);
        assert_eq!(meta.page, 1);
        assert_eq!(rows.len(), 3);

        // Multi-player play with a comment.
        let azul = &rows[0];
        assert_eq!(azul.id, "100");
        assert_eq!(azul.date, "2023-08-03");
        let r = &azul.raw;
        assert_eq!(r["id"], "100");
        assert_eq!(r["quantity"], "2");
        // length is MINUTES, preserved verbatim.
        assert_eq!(r["length"], "60");
        assert_eq!(r["location"], "Home");
        assert_eq!(r["nowinstats"], "0");
        // item + subtypes.
        assert_eq!(r["item"]["name"], "Azul");
        assert_eq!(r["item"]["objectid"], "230802");
        assert_eq!(r["item"]["objecttype"], "thing");
        assert_eq!(r["item"]["subtypes"], serde_json::json!(["boardgame"]));
        // entity un-escaped in the comment.
        assert_eq!(r["comments"], "good game & close finish");
        // both players preserved, full attrs.
        let players = r["players"].as_array().unwrap();
        assert_eq!(players.len(), 2);
        assert_eq!(players[0]["name"], "Friend");
        assert_eq!(players[0]["score"], "55");
        assert_eq!(players[0]["color"], "blue");
        assert_eq!(players[1]["name"], "Me");
        assert_eq!(players[1]["win"], "1");

        // Solo play: NO players element, empty location, no comment.
        let othello = &rows[1];
        assert_eq!(othello.id, "101");
        assert_eq!(othello.raw.get("players"), None, "no players element → key absent");
        assert_eq!(othello.raw.get("comments"), None);
        assert_eq!(othello.raw["location"], "");
        assert_eq!(othello.raw["length"], "0");

        // Play with unicode + multi-line entity comment, no players.
        let unlock = &rows[2];
        assert_eq!(unlock.date, "2021-12-05");
        assert_eq!(unlock.raw["location"], "café");
        assert_eq!(
            unlock.raw["item"]["subtypes"],
            serde_json::json!(["boardgame", "boardgamecompilation"])
        );
        let c = unlock.raw["comments"].as_str().unwrap();
        assert!(c.contains("scénario"), "unicode preserved: {c}");
        assert!(c.contains("5 étoiles & 0 hints"), "entity un-escaped + unicode: {c}");
    }

    #[test]
    fn parses_collection_owned_and_wishlist() {
        let items = parse_collection(&collection_ok());
        assert_eq!(items.len(), 2);

        // Owned game with a real rating.
        let azul = &items[0].raw;
        assert_eq!(azul["objectid"], "230802");
        assert_eq!(azul["subtype"], "boardgame");
        assert_eq!(azul["collid"], "111");
        assert_eq!(azul["name"], "Azul");
        assert_eq!(azul["yearpublished"], "2017");
        assert_eq!(azul["numplays"], "12");
        assert_eq!(azul["rating"], "9", "the USER rating from <stats><rating value=>");
        assert_eq!(azul["status"]["own"], "1");
        assert_eq!(azul["status"]["wishlist"], "0");
        // stats attrs preserved even with tab separators.
        assert_eq!(azul["stats"]["playingtime"], "45");
        assert_eq!(azul["stats"]["maxplayers"], "4");
        assert_eq!(azul["image"], "https://example.com/azul.jpg");

        // Wishlist item: N/A rating → NO rating key; wishlist flag set.
        let gloom = &items[1].raw;
        assert_eq!(gloom["objectid"], "174430");
        assert_eq!(gloom["name"], "Gloomhaven & Friends", "entity un-escaped in name");
        assert_eq!(gloom.get("rating"), None, "N/A rating omitted");
        assert_eq!(gloom["status"]["wishlist"], "1");
        assert_eq!(gloom["status"]["wishlistpriority"], "2", "extra status attr preserved");
        assert_eq!(gloom["numplays"], "0");
    }

    #[test]
    fn parses_collection_full_fidelity_extra_fields_and_skips_version() {
        // Exercises the documented fields the old parser dropped, captured
        // generically: originalname, wishlistcomment, wantpartslist,
        // haspartslist, and the <privateinfo> block (attrs + <privatecomment>).
        // Also asserts the heavy <version> sub-block is skipped wholesale and
        // never clobbers item-level name/image/yearpublished.
        let xml = "<items totalitems=\"1\">\
<item objecttype=\"thing\" objectid=\"500\" subtype=\"boardgame\" collid=\"900\">\
<name sortindex=\"1\">Descartes &amp; Co</name>\
<originalname>Descartes</originalname>\
<yearpublished>1998</yearpublished>\
<image>https://example.com/img.jpg</image>\
<thumbnail>https://example.com/thumb.jpg</thumbnail>\
<stats minplayers=\"2\" maxplayers=\"4\"><rating value=\"8\"><usersrated value=\"10\"/></rating></stats>\
<status own=\"1\" fortrade=\"1\" wishlist=\"1\" wishlistpriority=\"3\" lastmodified=\"2021-01-01 00:00:00\"/>\
<wishlistcomment>want the deluxe</wishlistcomment>\
<wantpartslist>papyrus trade card</wantpartslist>\
<haspartslist>all civilisation cards</haspartslist>\
<comment>my own note &amp; more</comment>\
<numplays>3</numplays>\
<privateinfo pp_currency=\"USD\" pricepaid=\"4.6\" acquisitiondate=\"2021-12-20\" acquiredfrom=\"toto\" inventorylocation=\"home\">\
<privatecomment>this is private</privatecomment>\
</privateinfo>\
<version>\
<item type=\"boardgameversion\" id=\"125483\">\
<name type=\"primary\" sortindex=\"1\" value=\"French edition\"/>\
<image>https://example.com/VERSION-SHOULD-NOT-LEAK.jpg</image>\
<yearpublished value=\"2011\"/>\
</item>\
</version>\
</item>\
</items>";
        let items = parse_collection(xml);
        assert_eq!(items.len(), 1);
        let it = &items[0].raw;

        // Core fields still correct.
        assert_eq!(it["objectid"], "500");
        assert_eq!(it["name"], "Descartes & Co", "item-level name (entity un-escaped)");
        assert_eq!(it["yearpublished"], "1998", "item-level year, NOT the version's 2011");
        assert_eq!(it["image"], "https://example.com/img.jpg", "item-level image, not the version's");
        assert_eq!(it["numplays"], "3");
        assert_eq!(it["rating"], "8");
        assert_eq!(it["status"]["fortrade"], "1");

        // The previously-DROPPED documented fields are now captured generically.
        assert_eq!(it["originalname"], "Descartes");
        assert_eq!(it["wishlistcomment"], "want the deluxe");
        assert_eq!(it["wantpartslist"], "papyrus trade card");
        assert_eq!(it["haspartslist"], "all civilisation cards");
        assert_eq!(it["comment"], "my own note & more", "comment captured + entity un-escaped");

        // The <privateinfo> block: attributes + <privatecomment>.
        let pi = &it["privateinfo"];
        assert_eq!(pi["pricepaid"], "4.6");
        assert_eq!(pi["acquisitiondate"], "2021-12-20");
        assert_eq!(pi["acquiredfrom"], "toto");
        assert_eq!(pi["inventorylocation"], "home");
        assert_eq!(pi["privatecomment"], "this is private");

        // The <version> sub-block is skipped: nothing from it leaks to the item.
        assert!(
            !it.as_object().unwrap().values().any(|v| v.as_str() == Some("https://example.com/VERSION-SHOULD-NOT-LEAK.jpg")),
            "version block must not clobber/leak into item-level fields"
        );
        assert!(it.get("version").is_none(), "version sub-block intentionally not captured");
    }

    #[test]
    fn malformed_and_empty_xml_degrade_without_panic() {
        // Truncated mid-element: the unterminated play is never pushed (no
        // </play>), but no panic.
        let (rows, _) = parse_plays(
            "<plays total=\"5\" page=\"1\"><play id=\"1\" date=\"2024-01-01\"><item name=\"X\" objectid=\"9\"",
        );
        assert!(rows.is_empty());
        // Total garbage.
        let (rows, meta) = parse_plays("not xml at all <<< &&&");
        assert!(rows.is_empty());
        assert_eq!(meta.total, 0);
        // Empty string.
        assert!(parse_plays("").0.is_empty());
        assert!(parse_collection("").is_empty());
        // A play missing its date is skipped (can't partition).
        let (rows, _) = parse_plays(
            "<plays total=\"1\" page=\"1\"><play id=\"1\" date=\"\"><item name=\"X\" objectid=\"9\"/></play></plays>",
        );
        assert!(rows.is_empty(), "blank date → skipped");
        // Empty collection (<items totalitems=0/>) yields no rows.
        assert!(parse_collection("<items totalitems=\"0\"></items>").is_empty());
    }

    // ---- A configurable stub fetcher ----

    struct StubApi {
        plays_pages: Vec<String>,
        /// Collection responses to serve in order; `Err(())` models an HTTP 202
        /// (Queued). The last entry is reused once the rest are exhausted.
        collection_seq: RefCell<Vec<Result<String, ()>>>,
        plays_calls: RefCell<Vec<(u32, Option<String>)>>,
        collection_calls: RefCell<u32>,
    }
    impl StubApi {
        fn new(plays_pages: Vec<String>, collection_seq: Vec<Result<String, ()>>) -> Self {
            StubApi {
                plays_pages,
                collection_seq: RefCell::new(collection_seq),
                plays_calls: RefCell::new(Vec::new()),
                collection_calls: RefCell::new(0),
            }
        }
    }
    impl BggApi for StubApi {
        fn plays(&self, _u: &str, page: u32, mindate: Option<&str>) -> Result<String, FetchError> {
            self.plays_calls.borrow_mut().push((page, mindate.map(str::to_string)));
            // page is 1-based; past the last fixture page → an empty page.
            match self.plays_pages.get((page - 1) as usize) {
                Some(x) => Ok(x.clone()),
                None => Ok(plays_empty()),
            }
        }
        fn collection(&self, _u: &str) -> Result<String, FetchError> {
            *self.collection_calls.borrow_mut() += 1;
            let mut seq = self.collection_seq.borrow_mut();
            let item = if seq.len() > 1 {
                seq.remove(0)
            } else {
                seq.first().cloned().unwrap_or(Ok(String::new()))
            };
            match item {
                Ok(x) => Ok(x),
                Err(()) => Err(FetchError::Queued),
            }
        }
    }

    #[test]
    fn pull_writes_plays_partitions_collection_snapshot_and_advances_cursor() {
        let v = temp_vault("pull");
        let stub = StubApi::new(vec![plays_page1()], vec![Ok(collection_ok())]);

        let out = pull_with(&v, &stub, "tester").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&3));
        assert_eq!(out.counts.get("collection"), Some(&2));

        // Plays partitioned by play-date month.
        let aug =
            std::fs::read_to_string(v.root().join("gaming/boardgamegeek/plays/2023-08.jsonl")).unwrap();
        assert_eq!(aug.lines().count(), 2, "100 (Aug 3) + 101 (Aug 4) land in 2023-08");
        let dec =
            std::fs::read_to_string(v.root().join("gaming/boardgamegeek/plays/2021-12.jsonl")).unwrap();
        assert_eq!(dec.lines().count(), 1, "102 lands in 2021-12");
        // The raw line is the full-fidelity object verbatim (no injected ts key).
        assert!(aug.contains("\"id\":\"100\""));
        assert!(aug.contains("\"players\""));
        assert!(!aug.contains("\"ts\""), "the partition ts is not serialized into the row");

        // Collection snapshot rewritten whole.
        let coll =
            std::fs::read_to_string(v.root().join("gaming/boardgamegeek/collection.jsonl")).unwrap();
        assert_eq!(coll.lines().count(), 2);
        assert!(coll.contains("\"objectid\":\"230802\""));
        assert!(coll.contains("\"rating\":\"9\""));

        // Cursor advanced to the latest play date + collection synced + updated.
        let state = v.read_bgg_sync();
        assert_eq!(state.mindate.as_deref(), Some("2023-08-04"), "latest play date");
        assert!(state.collection_synced.is_some());
        assert!(state.updated.is_some());
    }

    #[test]
    fn second_pull_passes_mindate_and_dedupes() {
        let v = temp_vault("incremental");
        // First pull writes everything and sets mindate=2023-08-04.
        let stub1 = StubApi::new(vec![plays_page1()], vec![Ok(collection_ok())]);
        pull_with(&v, &stub1, "tester").unwrap();

        // Second pull: serve the SAME page1 again (BGG's mindate is inclusive,
        // so the latest-date play comes back) — guid dedupe must drop it all.
        let stub2 = StubApi::new(vec![plays_page1()], vec![Ok(collection_ok())]);
        let out = pull_with(&v, &stub2, "tester").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&0), "all play ids already stored");

        // And the 2nd run's page-1 call passed mindate = the watermark from #1.
        let calls = stub2.plays_calls.borrow();
        assert_eq!(calls[0].1.as_deref(), Some("2023-08-04"), "incremental mindate sent");

        // Files unchanged in row count.
        let aug =
            std::fs::read_to_string(v.root().join("gaming/boardgamegeek/plays/2023-08.jsonl")).unwrap();
        assert_eq!(aug.lines().count(), 2, "no duplicate rows after re-pull");
    }

    /// The page-request trace (page numbers, in order) a pull made for plays.
    fn play_pages(stub: &StubApi) -> Vec<u32> {
        stub.plays_calls.borrow().iter().map(|(p, _)| *p).collect()
    }

    #[test]
    fn paginates_full_page_then_short_page() {
        // A realistic 2-page drain: page 1 is FULL (100), page 2 is short (50).
        // `total` is honest here (150) but must not be what terminates the loop.
        let v = temp_vault("paginate");
        let stub = StubApi::new(
            vec![
                plays_page_of(PLAYS_PAGE_SIZE, 150, 1, 1),       // 100 plays, ids 1..=100
                plays_page_of(50, 150, 2, 1 + PLAYS_PAGE_SIZE),  // 50 plays, ids 101..=150
            ],
            vec![Ok("<items totalitems=\"0\"></items>".to_string())],
        );
        let out = pull_with(&v, &stub, "tester").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&150), "both pages drained, none stranded");
        // Requested exactly pages [1, 2] and stopped on the short page.
        assert_eq!(play_pages(&stub), vec![1, 2], "page 1 (full) then page 2 (short), then stop");
        let jan =
            std::fs::read_to_string(v.root().join("gaming/boardgamegeek/plays/2024-01.jsonl")).unwrap();
        assert_eq!(jan.lines().count(), 150);
    }

    #[test]
    fn regression_total_zero_does_not_strand_page_two() {
        // DEFECT 1: BGG reports total="0" for plays. With a full first page and
        // the old `last_page = total.div_ceil(100)` logic, the loop stopped
        // after page 1 and stranded every play on page 2+, then advanced the
        // watermark past them = permanent loss. Here: total="0", 150 plays
        // (page 1 = 100, page 2 = 50). All 150 must be captured.
        let v = temp_vault("total-zero");
        let stub = StubApi::new(
            vec![
                plays_page_of(PLAYS_PAGE_SIZE, 0, 1, 1),         // total="0", 100 plays
                plays_page_of(50, 0, 2, 1 + PLAYS_PAGE_SIZE),    // total="0", 50 plays (short → stop)
            ],
            vec![Ok("<items totalitems=\"0\"></items>".to_string())],
        );
        let out = pull_with(&v, &stub, "tester").unwrap();
        assert_eq!(
            out.counts.get("plays"),
            Some(&150),
            "all 150 captured despite total=0 (the old code stranded 50)"
        );
        // Page-request trace: [1, 2], stop on the short page.
        assert_eq!(play_pages(&stub), vec![1, 2]);
        // Count rows actually on disk.
        let stream = v.stream(PLAYS_DIR, Partition::Month);
        let mut on_disk = 0usize;
        for key in stream.partitions().unwrap() {
            on_disk += stream.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(on_disk, 150, "150 play rows persisted, no gap");
        // Watermark advances only AFTER the full drain (max date = 2024-01-27,
        // the latest day produced by the generator).
        let state = v.read_bgg_sync();
        assert_eq!(state.mindate.as_deref(), Some("2024-01-27"), "watermark = latest play date after drain");
    }

    #[test]
    fn regression_exactly_one_full_page_then_empty_page() {
        // The boundary case: exactly 100 plays = one FULL page, so the loop
        // cannot tell it's done from length alone and must fetch page 2, which
        // comes back EMPTY (<plays/>) → stop. total="0" again.
        let v = temp_vault("full-then-empty");
        let stub = StubApi::new(
            vec![
                plays_page_of(PLAYS_PAGE_SIZE, 0, 1, 1),    // 100 plays (full)
                plays_page_of(0, 0, 2, 0),                  // empty page → stop
            ],
            vec![Ok("<items totalitems=\"0\"></items>".to_string())],
        );
        let out = pull_with(&v, &stub, "tester").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&100), "exactly the 100 captured");
        // Requested [1, 2]: page 1 was full so page 2 was probed; it was empty.
        assert_eq!(play_pages(&stub), vec![1, 2], "full page forces a page-2 probe; empty page stops");
        let state = v.read_bgg_sync();
        assert!(state.mindate.is_some(), "watermark advanced after the drain");
    }

    #[test]
    fn collection_202_retries_then_succeeds() {
        let v = temp_vault("queue-ok");
        // Two 202s, then a 200 with the collection.
        let stub = StubApi::new(vec![plays_page1()], vec![Err(()), Err(()), Ok(collection_ok())]);
        let out = pull_with(&v, &stub, "tester").unwrap();
        assert_eq!(out.counts.get("collection"), Some(&2), "collection fetched after retrying past 202s");
        assert_eq!(*stub.collection_calls.borrow(), 3, "polled twice, succeeded on the third");
        let state = v.read_bgg_sync();
        assert!(state.collection_synced.is_some());
    }

    #[test]
    fn collection_gives_up_gracefully_after_max_retries_keeping_plays() {
        let v = temp_vault("queue-giveup");
        // Always 202 → never resolves. Plays must still be written, no error.
        let stub = StubApi::new(vec![plays_page1()], vec![Err(())]);
        let out = pull_with(&v, &stub, "tester").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&3), "plays kept despite queued collection");
        assert_eq!(out.counts.get("collection"), Some(&0), "collection skipped this pull");
        // Polled MAX_RETRIES + 1 times then gave up.
        assert_eq!(*stub.collection_calls.borrow(), COLLECTION_MAX_RETRIES + 1);
        // The plays file exists; the collection snapshot does NOT (never written).
        assert!(v.root().join("gaming/boardgamegeek/plays/2023-08.jsonl").exists());
        assert!(!v.root().join("gaming/boardgamegeek/collection.jsonl").exists());
        let state = v.read_bgg_sync();
        assert!(state.collection_synced.is_none(), "no collection sync recorded");
        assert!(state.mindate.is_some(), "plays watermark still advanced");
    }

    #[test]
    fn collection_snapshot_is_rewritten_whole_each_pull() {
        let v = temp_vault("snapshot-rewrite");
        // Pull #1: a 2-item collection (no plays).
        let stub1 = StubApi::new(vec![plays_empty()], vec![Ok(collection_ok())]);
        pull_with(&v, &stub1, "tester").unwrap();
        let coll1 =
            std::fs::read_to_string(v.root().join("gaming/boardgamegeek/collection.jsonl")).unwrap();
        assert_eq!(coll1.lines().count(), 2);

        // Pull #2: a 1-item collection (e.g. user removed a game) → whole rewrite.
        let smaller = "<items totalitems=\"1\"><item objecttype=\"thing\" objectid=\"99\" subtype=\"boardgame\"><name>Only One</name><yearpublished>2020</yearpublished><status own=\"1\"/><numplays>1</numplays></item></items>";
        let stub2 = StubApi::new(vec![plays_empty()], vec![Ok(smaller.to_string())]);
        pull_with(&v, &stub2, "tester").unwrap();
        let coll2 =
            std::fs::read_to_string(v.root().join("gaming/boardgamegeek/collection.jsonl")).unwrap();
        assert_eq!(coll2.lines().count(), 1, "snapshot fully replaced, not appended");
        assert!(coll2.contains("\"objectid\":\"99\""));
        assert!(!coll2.contains("230802"), "old items gone after whole rewrite");
    }

    // ---- Connection ----

    #[test]
    fn connection_exposes_token_paste_and_disconnect_forgets_username() {
        assert!(CONNECTION.method("token-paste").is_some());
        let v = temp_vault("conn");
        // Verify offline via the stub (a 200 with a valid plays body).
        let stub = StubApi::new(vec![plays_page1()], vec![Ok(String::new())]);
        connect_with(&v, &stub, "tester").unwrap();
        let status = def_status(&v).unwrap();
        assert!(status.configured, "keyless: always configured");
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "tester");
        assert_eq!(status.accounts[0].key, "boardgamegeek");
        assert!(!status.accounts[0].needs_reconnect);
        def_disconnect(&v, "boardgamegeek").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn empty_and_invalid_username_rejected_and_pull_needs_connection() {
        let v = temp_vault("conn-bad");
        // Empty/whitespace rejected before any fetch.
        let stub = StubApi::new(vec![plays_page1()], vec![Ok(String::new())]);
        assert!(connect_with(&v, &stub, "   ").is_err());
        // A bad-username <errors> body (served as a 200) is a clear connect error.
        let bad = StubApi::new(
            vec![
                "<errors><error><message>Invalid username specified</message></error></errors>"
                    .to_string(),
            ],
            vec![Ok(String::new())],
        );
        let err = connect_with(&v, &bad, "ghost").unwrap_err().to_string();
        assert!(err.contains("could not find"), "clear not-found error: {err}");
        // Not connected: pull errors clearly (no api_key concept at all).
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn sync_state_back_compat() {
        // An older cursor (mindate only) must still deserialize, and bare {}.
        let old: SyncState = serde_json::from_str(r#"{"mindate":"2020-01-01"}"#).unwrap();
        assert_eq!(old.mindate.as_deref(), Some("2020-01-01"));
        assert!(old.collection_synced.is_none() && old.updated.is_none());
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.mindate.is_none() && empty.updated.is_none());
    }
}

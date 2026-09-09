//! Google Contacts collector — saved contacts plus Google's auto-collected
//! "Other contacts" from every connected account, into the shared `contacts/`
//! domain store ([`crate::contacts`]). The OAuth side (connect, token store,
//! refresh, reconnect flagging) is owned by [`crate::sync::google`]; this
//! module only asks it for a fresh token per account via
//! [`crate::sync::google::fresh_token`]. See [`crate::gmail`] for the worked
//! per-account-pull template this follows.
//!
//! **Target.** Contacts are *current-state* data, not an event stream, so
//! the store is the [`crate::contacts`] domain's `Snapshot` kind: one
//! atomically rewritten JSONL file per account at
//! `contacts/google-contacts/<sub>.jsonl` ([`Vault::write_snapshot`]) — the
//! folder is the source id (`google-contacts`), the file name is the Google
//! `sub` — one [`crate::contacts::Contact`] per line, ordered by People API
//! resource name (the contact `id`) so successive rewrites diff cleanly. A
//! human summary lives at `contacts/index.md`. Each record is the normalized
//! cross-source core (names, emails, phones, organizations, photo) with
//! everything else the API returned parked verbatim under `extra` — the
//! [`crate::tasks`] `normalize_ticktick` discipline: consume what you map,
//! never drop what you don't. Handles follow the domain convention: emails
//! lowercased, phones E.164 where derivable ([`crate::contacts::normalize_email`],
//! [`crate::contacts::normalize_phone`]).
//!
//! **Two lists, two cursors.** The People API splits a person's address
//! book into *connections* (`people.connections.list` — contacts the user
//! saved) and *other contacts* (`otherContacts.list` — the auto-collected
//! everyone-you've-emailed list, which is the interaction graph that lets
//! correspondence senders resolve to people). Both support sync tokens:
//! the first pull lists everything and stores the returned `syncToken`;
//! every later pass replays only what changed — including deletions, which
//! arrive as tombstones with `metadata.deleted` set. Google expires sync
//! tokens after about a week of disuse (HTTP 400, `EXPIRED_SYNC_TOKEN`);
//! that resets the affected list to a full re-list, which simply rebuilds
//! its half of the snapshot. Steady state is therefore two cheap requests
//! per account per pass, which is why no request budget is needed (unlike
//! Gmail, whose first backfill is tens of thousands of fetches).
//!
//! **Disconnects.** Sync state and the per-account snapshot file are both
//! pruned when an account disconnects. Removing vault data on disconnect is
//! safe *only* because this store is a rebuildable current-state snapshot —
//! there is no history stream here, and reconnecting re-lists in full. A
//! store with history (the email stream, calendar changes) keeps it.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::contacts::{normalize_email, normalize_phone, Contact, ContactOrg};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::sync::google::GoogleAccountInfo;
use crate::vault::Vault;

/// This collector's id — also the source folder name under `contacts/`
/// (`contacts/google-contacts/<sub>.jsonl`), per the contract's "source =
/// folder name" rule. Written into every record's `source` field.
const SOURCE: &str = "google-contacts";

/// Seconds between contacts passes in the watcher loop. Address books churn
/// slowly, and a pass where nothing changed is one sync-token request per
/// list — hourly is generous freshness at negligible cost.
pub const GOOGLE_CONTACTS_SYNC_SECS: u64 = 3600;

// A pass where nothing changed is two cheap sync-token requests per
// account; even a first full list is a handful of pages — no request
// budget needed. A silent no-op when no Google account is connected.
fn def_collect(vault: &Vault, _now: chrono::DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_google_contacts()?;
    Ok(crate::registry::CollectOutcome::note_if(s.changed > 0, || {
        format!(
            "google contacts synced — {} changes across {} accounts",
            s.changed, s.accounts
        )
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_google_contacts_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-contacts",
        name: "Google Contacts",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Your contacts plus Google's auto-collected \"Other contacts\" (everyone you've emailed) — the interaction graph that resolves correspondence senders to people.",
        domain: "contacts",
        vault_path: "contacts/",
        toggleable: true,
        setup: &[],
        caveats: "\"Other contacts\" is auto-collected by Google from your mail; it can be large and is sparse (often just a name and address).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(GOOGLE_CONTACTS_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("google"),
    pull: Some(pull),
};

/// Best-effort, never-fail removal of the orphaned legacy `contacts/google/`
/// directory (output moved to `contacts/google-contacts/`). A no-op when it's
/// already gone; errors are swallowed so the pull is never blocked on cleanup.
fn remove_legacy_dir(vault: &Vault) {
    if let Ok(dir) = vault.resolve("contacts/google") {
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

/// [`crate::registry::IntegrationDef::pull`] adapter:
/// [`Vault::google_contacts_pull`] mapped into the generic outcome shape.
/// Per-account failures never abort the pass — they land in
/// `.trove/google-contacts-sync.json` — so the headline re-reads the state
/// to surface them rather than reporting a clean sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    // One-time migration cleanup: the old `contacts/google/<sub>.jsonl`
    // output moved to `contacts/google-contacts/`. The legacy folder's lines
    // no longer deserialize (old `resource`/no-`source` shape) and show up as
    // a phantom source in the manifest; connected accounts rebuild fresh into
    // `contacts/google-contacts/`, so dropping it is safe regardless of fetch.
    remove_legacy_dir(vault);
    let s = vault.google_contacts_pull()?;
    let errors = vault
        .read_google_contacts_sync()
        .map(|st| st.accounts.values().filter(|a| a.error.is_some()).count() as u64)
        .unwrap_or(0);
    let mut headline = if s.changed > 0 {
        format!(
            "{} contact changes across {} accounts",
            s.changed, s.accounts
        )
    } else {
        "contacts up to date".to_string()
    };
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
            ("changed", s.changed),
            ("account_errors", errors),
        ]),
    })
}

const SYNC_FILE: &str = ".trove/google-contacts-sync.json";
const PEOPLE_API: &str = "https://people.googleapis.com";
/// Kept short so a hung connection can't stall the watcher owner loop for
/// long (the `oura.rs` reasoning).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Page size for both lists (each allows up to 1000; 500 keeps a several-
/// thousand-entry "Other contacts" list to a handful of requests without
/// jumbo responses).
const PAGE_SIZE: u32 = 500;
/// `people.connections.list` field selection — the full saved-contact shape.
/// Must stay constant across requests: the People API requires sync/page
/// continuations to repeat the first call's parameters.
const PERSON_FIELDS: &str =
    "names,emailAddresses,phoneNumbers,organizations,photos,biographies,birthdays,addresses,urls,metadata";
/// `otherContacts.list` field selection — Other contacts only support this
/// small set (names, emails, phones, plus metadata for deletions/stamps).
const OTHER_READ_MASK: &str = "names,emailAddresses,phoneNumbers,metadata";

/// Result of one contacts sync pass, for logging / the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GoogleContactsSyncStats {
    /// Accounts whose snapshot changed this pass.
    pub accounts: u32,
    /// Contacts added, updated, or removed across all accounts this pass.
    pub changed: u64,
}

/// Per-account sync progress, persisted in `.trove/google-contacts-sync.json`
/// (keyed by Google `sub`). Deleting an account's entry re-lists it in full;
/// the cursor is rebuildable, the snapshot is the data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GoogleContactsAccountState {
    /// Display address (for the index / UI; the map key is the `sub`).
    #[serde(default)]
    pub email: String,
    /// `people.connections.list` sync token. Absent = next pass lists in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections_sync_token: Option<String>,
    /// `otherContacts.list` sync token. Absent = next pass lists in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_sync_token: Option<String>,
    /// Saved contacts currently in the snapshot (drives index.md).
    #[serde(default)]
    pub contacts: u64,
    /// "Other contacts" currently in the snapshot.
    #[serde(default)]
    pub other_contacts: u64,
    /// RFC3339 local time of the last fully successful sync for this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<String>,
    /// Why this account's last pass failed, for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The whole contacts sync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GoogleContactsSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Per-account progress, keyed by Google `sub`.
    pub accounts: BTreeMap<String, GoogleContactsAccountState>,
}

/// Status-level fetch errors needing distinct handling.
#[derive(Debug)]
enum FetchError {
    RateLimited,
    Unauthorized,
    /// Sync token expired (HTTP 400, `EXPIRED_SYNC_TOKEN`) — reset the
    /// affected list to a full re-list.
    ExpiredSyncToken,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::ExpiredSyncToken => write!(f, "sync token expired (HTTP 400)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Which of the two People API lists a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    /// `people.connections.list` — contacts the user saved.
    Connections,
    /// `otherContacts.list` — Google's auto-collected interaction list.
    OtherContacts,
}

impl ListKind {
    /// The `other` flag value for contacts of this list (also implied by
    /// the resource-name prefix; this keys the full-relist half-swap).
    fn other(self) -> bool {
        matches!(self, ListKind::OtherContacts)
    }
}

/// One page of either list.
struct PersonPage {
    people: Vec<Value>,
    next_page: Option<String>,
    /// `nextSyncToken` — present only on the final page of a listing.
    next_sync: Option<String>,
}

/// The page fetch, as a seam: the real [`PeopleClient`] implements it over
/// HTTP; tests script it. This is what makes the sync-token orchestration
/// (incremental merge, expired-token reset, full-relist swap) testable
/// without a network.
trait PeopleSource {
    fn page(
        &self,
        kind: ListKind,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<PersonPage, FetchError>;
}

/// Thin People API client. The base URL is injected so the orchestration
/// stays testable against a local stub (the `oura.rs`/`tasks.rs` pattern).
struct PeopleClient {
    base: String,
    token: String,
}

impl PeopleClient {
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
            // An expired sync token is a 400 whose body names the reason —
            // the only 400 that isn't a programming error.
            Err(ureq::Error::Status(400, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                if body.contains("EXPIRED_SYNC_TOKEN") {
                    Err(FetchError::ExpiredSyncToken)
                } else {
                    Err(FetchError::Other(format!(
                        "HTTP 400: {}",
                        body.chars().take(300).collect::<String>()
                    )))
                }
            }
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
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

impl PeopleSource for PeopleClient {
    fn page(
        &self,
        kind: ListKind,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<PersonPage, FetchError> {
        let (path, mask_key, mask, items_key) = match kind {
            ListKind::Connections => (
                "/v1/people/me/connections",
                "personFields",
                PERSON_FIELDS,
                "connections",
            ),
            ListKind::OtherContacts => {
                ("/v1/otherContacts", "readMask", OTHER_READ_MASK, "otherContacts")
            }
        };
        // requestSyncToken rides on every call: the first call needs it to
        // get a token at all, and continuations must repeat the first
        // call's parameters.
        let mut params = vec![
            (mask_key, mask.to_string()),
            ("pageSize", PAGE_SIZE.to_string()),
            ("requestSyncToken", "true".to_string()),
        ];
        if let Some(t) = sync_token {
            params.push(("syncToken", t.to_string()));
        }
        if let Some(t) = page_token {
            params.push(("pageToken", t.to_string()));
        }
        let v = self.get(path, &params)?;
        Ok(PersonPage {
            people: v.get(items_key).and_then(Value::as_array).cloned().unwrap_or_default(),
            next_page: v.get("nextPageToken").and_then(Value::as_str).map(str::to_string),
            next_sync: v.get("nextSyncToken").and_then(Value::as_str).map(str::to_string),
        })
    }
}

// ---------------------------------------------------------------------------
// Normalization: People API person JSON → Contact / tombstone.

/// What one person object in an API response means for the snapshot.
#[derive(Debug, PartialEq)]
enum PersonChange {
    Upsert(Contact),
    /// A sync-response tombstone (`metadata.deleted`): remove this resource.
    Delete(String),
}

/// Take `obj[key]` if it's a string; anything else is put back, not lost
/// (the [`crate::tasks`] helper, duplicated because it's private there).
fn take_str(obj: &mut Map<String, Value>, key: &str) -> Option<String> {
    match obj.remove(key)? {
        Value::String(s) => Some(s),
        other => {
            obj.insert(key.into(), other);
            None
        }
    }
}

/// Take `obj[key]` if it's an array; anything else is put back, not lost.
fn take_array(obj: &mut Map<String, Value>, key: &str) -> Vec<Value> {
    match obj.remove(key) {
        Some(Value::Array(a)) => a,
        Some(other) => {
            obj.insert(key.into(), other);
            Vec::new()
        }
        None => Vec::new(),
    }
}

/// The entry Google marks primary (`metadata.primary`), else the first.
fn primary(items: &[Value]) -> Option<&Value> {
    items
        .iter()
        .find(|v| {
            v.pointer("/metadata/primary").and_then(Value::as_bool) == Some(true)
        })
        .or_else(|| items.first())
}

/// Each entry's `value` string, in source order, passed through `norm` (the
/// domain handle convention — lowercase for emails, E.164 for phones), empty
/// results and exact duplicates dropped *after* normalization (Other contacts
/// in particular often repeat an address across sources, and two casings of
/// one email collapse to one join key).
fn value_strings(items: &[Value], norm: impl Fn(&str) -> String) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for it in items {
        if let Some(s) = it.get("value").and_then(Value::as_str) {
            let s = norm(s);
            if !s.is_empty() && !out.iter().any(|e| e == &s) {
                out.push(s);
            }
        }
    }
    out
}

/// A raw People API person object → what it means for the snapshot.
/// Mapped fields are consumed; everything else the request's field mask
/// brought back (biographies, birthdays, addresses, urls, …) lands in
/// `extra` — never dropped. `metadata` is consumed for its two meaningful
/// facts (`deleted`, newest source `updateTime`) and otherwise discarded as
/// sync plumbing (etags, source ids restating the resource name) that would
/// churn the snapshot diff every pass. Returns `None` for objects without a
/// resource name.
fn normalize_person(value: Value, account: &str) -> Option<PersonChange> {
    let Value::Object(mut obj) = value else {
        return None;
    };
    let resource = take_str(&mut obj, "resourceName")?;
    let other = resource.starts_with("otherContacts/");

    let (deleted, updated) = match obj.remove("metadata") {
        Some(meta) => (
            meta.get("deleted").and_then(Value::as_bool).unwrap_or(false),
            meta.get("sources")
                .and_then(Value::as_array)
                .map(|srcs| {
                    srcs.iter()
                        .filter_map(|s| s.get("updateTime").and_then(Value::as_str))
                        .max() // RFC3339 sorts lexically
                        .map(str::to_string)
                })
                .unwrap_or(None),
        ),
        None => (false, None),
    };
    if deleted {
        return Some(PersonChange::Delete(resource));
    }

    let names = take_array(&mut obj, "names");
    let (name, given, family) = match primary(&names) {
        Some(n) => (
            n.get("displayName").and_then(Value::as_str).unwrap_or("").to_string(),
            n.get("givenName").and_then(Value::as_str).unwrap_or("").to_string(),
            n.get("familyName").and_then(Value::as_str).unwrap_or("").to_string(),
        ),
        None => (String::new(), String::new(), String::new()),
    };

    // Handles follow the domain convention. Google reports no per-contact
    // region, so a national phone stays verbatim and only `+`-prefixed
    // numbers reach E.164 (the correct, no-guess behavior).
    let emails = value_strings(&take_array(&mut obj, "emailAddresses"), normalize_email);
    let phones = value_strings(&take_array(&mut obj, "phoneNumbers"), |p| normalize_phone(p, None));

    let orgs: Vec<ContactOrg> = take_array(&mut obj, "organizations")
        .iter()
        .filter_map(|o| {
            let org = ContactOrg {
                name: o.get("name").and_then(Value::as_str).map(str::to_string),
                title: o.get("title").and_then(Value::as_str).map(str::to_string),
            };
            (org.name.is_some() || org.title.is_some()).then_some(org)
        })
        .collect();

    // Real photos only — entries flagged `default` are Google's generated
    // silhouettes, no information.
    let photo = take_array(&mut obj, "photos")
        .iter()
        .find(|p| p.get("default").and_then(Value::as_bool) != Some(true))
        .and_then(|p| p.get("url").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();

    Some(PersonChange::Upsert(Contact {
        // The source folder name and the dedupe key — `resource` is the
        // People API resource name (`people/c…` / `otherContacts/c…`), the
        // stable per-account id and the snapshot's sort key.
        source: SOURCE.to_string(),
        id: resource,
        account: account.to_string(),
        name,
        given,
        family,
        emails,
        phones,
        orgs,
        photo,
        other,
        updated,
        extra: obj,
    }))
}

// ---------------------------------------------------------------------------
// Snapshot merge: changes / full re-lists applied over the in-memory map.

/// Apply one change to the snapshot map. Returns whether anything actually
/// changed — a sync replay (after an interrupted pass) re-applies the same
/// changes idempotently and counts nothing.
fn apply_change(map: &mut BTreeMap<String, Contact>, change: PersonChange) -> bool {
    match change {
        PersonChange::Upsert(c) => {
            if map.get(&c.id) == Some(&c) {
                return false;
            }
            map.insert(c.id.clone(), c);
            true
        }
        PersonChange::Delete(resource) => map.remove(&resource).is_some(),
    }
}

/// Sync one list for one account: incremental from the stored sync token
/// when there is one (falling back to a full re-list if Google expired it),
/// full otherwise. Returns the new sync token and how many snapshot entries
/// changed.
fn sync_list(
    source: &impl PeopleSource,
    kind: ListKind,
    sync_token: Option<&str>,
    map: &mut BTreeMap<String, Contact>,
    account: &str,
) -> Result<(Option<String>, u64), FetchError> {
    if let Some(tok) = sync_token {
        match sync_list_incremental(source, kind, tok, map, account) {
            Ok(r) => return Ok(r),
            // Token expired (~a week unused) — reset to a full re-list,
            // which rebuilds this list's half of the snapshot from scratch.
            Err(FetchError::ExpiredSyncToken) => {}
            Err(e) => return Err(e),
        }
    }
    sync_list_full(source, kind, map, account)
}

/// Replay changes since `sync_token`: upserts plus `metadata.deleted`
/// tombstones, applied directly to the map (idempotent, so an interrupted
/// pass that never advanced the token replays harmlessly).
fn sync_list_incremental(
    source: &impl PeopleSource,
    kind: ListKind,
    sync_token: &str,
    map: &mut BTreeMap<String, Contact>,
    account: &str,
) -> Result<(Option<String>, u64), FetchError> {
    let mut changed = 0u64;
    let mut next_sync: Option<String> = None;
    let mut page_token: Option<String> = None;
    loop {
        let page = source.page(kind, Some(sync_token), page_token.as_deref())?;
        for person in page.people {
            if let Some(change) = normalize_person(person, account) {
                if apply_change(map, change) {
                    changed += 1;
                }
            }
        }
        if page.next_sync.is_some() {
            next_sync = page.next_sync;
        }
        match page.next_page {
            Some(t) => page_token = Some(t),
            None => break,
        }
    }
    // The API puts nextSyncToken on the final page; if it's ever absent,
    // keep the old token rather than forcing a full re-list next pass.
    Ok((next_sync.or_else(|| Some(sync_token.to_string())), changed))
}

/// List everything and swap in the result as this list's half of the
/// snapshot (entries of the *other* list are untouched). Collected into a
/// fresh map first so an error mid-listing leaves the existing snapshot
/// intact — a partial replacement would silently lose contacts.
fn sync_list_full(
    source: &impl PeopleSource,
    kind: ListKind,
    map: &mut BTreeMap<String, Contact>,
    account: &str,
) -> Result<(Option<String>, u64), FetchError> {
    let mut fresh: BTreeMap<String, Contact> = BTreeMap::new();
    let mut next_sync: Option<String> = None;
    let mut page_token: Option<String> = None;
    loop {
        let page = source.page(kind, None, page_token.as_deref())?;
        for person in page.people {
            if let Some(PersonChange::Upsert(c)) = normalize_person(person, account) {
                fresh.insert(c.id.clone(), c);
            }
        }
        if page.next_sync.is_some() {
            next_sync = page.next_sync;
        }
        match page.next_page {
            Some(t) => page_token = Some(t),
            None => break,
        }
    }
    // Swap: entries of this list that weren't re-listed are gone (a full
    // list's deletions are implicit); re-listed entries upsert.
    let mut changed = 0u64;
    let stale: Vec<String> = map
        .iter()
        .filter(|(k, c)| c.other == kind.other() && !fresh.contains_key(*k))
        .map(|(k, _)| k.clone())
        .collect();
    for k in stale {
        map.remove(&k);
        changed += 1;
    }
    for (k, c) in fresh {
        if map.get(&k) != Some(&c) {
            map.insert(k, c);
            changed += 1;
        }
    }
    Ok((next_sync, changed))
}

/// The snapshot file for one account, keyed by Google `sub`. The folder is
/// the source id (`contacts/google-contacts/`), per the contract's "source =
/// folder name" rule; the file name is the account's Google `sub`.
fn snapshot_rel(sub: &str) -> String {
    format!("contacts/{SOURCE}/{sub}.jsonl")
}

impl Vault {
    /// One contacts sync pass across every connected Google account: refresh
    /// each account's token, then sync both People API lists from their
    /// stored sync tokens (full list on first run or after token expiry) and
    /// rewrite the account's snapshot. A silent no-op when no Google account
    /// is connected. Per-account failures are recorded in
    /// `.trove/google-contacts-sync.json` and never abort other accounts.
    pub fn collect_google_contacts(&self) -> Result<GoogleContactsSyncStats> {
        let accounts = self.google_status()?.accounts;
        let mut state = self.read_google_contacts_sync().unwrap_or_default();
        // Forget state and snapshot for accounts that have been disconnected.
        // Deleting vault data on disconnect is safe only because the
        // snapshot is rebuildable current state with no history component —
        // reconnecting re-lists everything.
        let live: HashSet<&str> = accounts.iter().map(|a| a.sub.as_str()).collect();
        state.accounts.retain(|sub, _| live.contains(sub.as_str()));
        self.prune_contact_snapshots(&live)?;

        if accounts.is_empty() {
            // Persist the pruning above so a disconnected account's row
            // doesn't linger in the state or the index.
            if self.resolve(SYNC_FILE).map(|p| p.exists()).unwrap_or(false) {
                state.updated = Local::now().to_rfc3339();
                self.write_google_contacts_sync(&state)?;
                self.write_contacts_index(&state)?;
            }
            return Ok(GoogleContactsSyncStats::default());
        }

        let mut stats = GoogleContactsSyncStats::default();
        for acct in &accounts {
            // A flagged account can't refresh non-interactively; skip it
            // (the card surfaces the reconnect prompt).
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
            let client = PeopleClient { base: PEOPLE_API.to_string(), token };
            {
                let astate = state.accounts.entry(acct.sub.clone()).or_default();
                astate.email = acct.email.clone();
                astate.error = None;
            }
            match self.contacts_sync_account(&client, acct, &mut state) {
                Ok(changed) => {
                    if changed > 0 {
                        stats.accounts += 1;
                        stats.changed += changed;
                    }
                }
                Err(e) => {
                    let msg = format!("{}", status_error(&acct.email, e));
                    let astate = state.accounts.entry(acct.sub.clone()).or_default();
                    astate.error = Some(msg);
                }
            }
        }

        state.updated = Local::now().to_rfc3339();
        self.write_google_contacts_sync(&state)?;
        self.write_contacts_index(&state)?;
        Ok(stats)
    }

    /// Sync both lists for one account and rewrite its snapshot. The lists
    /// commit independently: if connections synced but Other contacts
    /// failed, the connections changes and token are kept and only the
    /// failed list retries from its old token next pass (idempotent
    /// replays make the overlap free). Returns how many snapshot entries
    /// changed.
    fn contacts_sync_account(
        &self,
        source: &impl PeopleSource,
        acct: &GoogleAccountInfo,
        state: &mut GoogleContactsSyncState,
    ) -> Result<u64, FetchError> {
        let rel = snapshot_rel(&acct.sub);
        let existing: Vec<Contact> = self.read_snapshot(&rel).map_err(soft)?;
        let mut map: BTreeMap<String, Contact> =
            existing.into_iter().map(|c| (c.id.clone(), c)).collect();

        let (conn_tok, other_tok) = {
            let s = state.accounts.get(&acct.sub);
            (
                s.and_then(|s| s.connections_sync_token.clone()),
                s.and_then(|s| s.other_sync_token.clone()),
            )
        };

        let mut changed = 0u64;
        let mut first_err: Option<FetchError> = None;

        match sync_list(source, ListKind::Connections, conn_tok.as_deref(), &mut map, &acct.email) {
            Ok((tok, n)) => {
                changed += n;
                state.accounts.entry(acct.sub.clone()).or_default().connections_sync_token = tok;
            }
            Err(e) => first_err = Some(e),
        }
        // Skip the second list once the first failed — a 401/429 would just
        // fail again; retry both next pass.
        if first_err.is_none() {
            match sync_list(source, ListKind::OtherContacts, other_tok.as_deref(), &mut map, &acct.email)
            {
                Ok((tok, n)) => {
                    changed += n;
                    state.accounts.entry(acct.sub.clone()).or_default().other_sync_token = tok;
                }
                Err(e) => first_err = Some(e),
            }
        }

        // Rewrite the snapshot when anything changed, and create it on the
        // first successful sync even if the account is empty (the store's
        // existence is the registration). Never create a file for an
        // account whose first sync failed outright.
        let exists = self.resolve(&rel).map(|p| p.exists()).unwrap_or(false);
        if changed > 0 || (!exists && first_err.is_none()) {
            let records: Vec<&Contact> = map.values().collect();
            self.write_snapshot(&rel, &records).map_err(soft)?;
        }

        let astate = state.accounts.entry(acct.sub.clone()).or_default();
        astate.contacts = map.values().filter(|c| !c.other).count() as u64;
        astate.other_contacts = map.values().filter(|c| c.other).count() as u64;
        match first_err {
            Some(e) => Err(e),
            None => {
                astate.last_sync = Some(Local::now().to_rfc3339());
                Ok(changed)
            }
        }
    }

    /// Remove snapshot files of accounts that are no longer connected.
    /// Only the rebuildable snapshot is removed — contacts have no history
    /// stream, and reconnecting re-lists in full.
    fn prune_contact_snapshots(&self, live: &HashSet<&str>) -> Result<()> {
        let dir = self.resolve(&format!("contacts/{SOURCE}"))?;
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "jsonl") {
                if let Some(sub) = path.file_stem().and_then(|s| s.to_str()) {
                    if !live.contains(sub) {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }
        Ok(())
    }

    /// The persisted contacts sync progress, if a sync has ever run.
    pub fn read_google_contacts_sync(&self) -> Option<GoogleContactsSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Atomic write so readers never see a torn file.
    fn write_google_contacts_sync(&self, state: &GoogleContactsSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }

    /// Regenerate the human-readable summary at `contacts/index.md`.
    fn write_contacts_index(&self, state: &GoogleContactsSyncState) -> Result<()> {
        let mut md = format!(
            "# Contacts\n\nLast sync: {}\n\n| Account | Contacts | Other contacts | Last synced | Error |\n|---|---|---|---|---|\n",
            state.updated
        );
        for s in state.accounts.values() {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                s.email,
                s.contacts,
                s.other_contacts,
                s.last_sync.as_deref().unwrap_or("—"),
                s.error.as_deref().unwrap_or(""),
            ));
        }
        let path = self.resolve("contacts/index.md")?;
        crate::store::write_atomic(&path, md.as_bytes())
    }

    /// Pull every connected account's contacts into the vault — the manual
    /// "Sync now" / first-connect path. Blocking (network).
    pub fn google_contacts_pull(&self) -> Result<GoogleContactsSyncStats> {
        if self.google_status()?.accounts.is_empty() {
            bail!("no Google account is connected");
        }
        self.collect_google_contacts()
    }
}

/// A vault write error inside the API-fetch path → a soft `FetchError`.
fn soft(e: anyhow::Error) -> FetchError {
    FetchError::Other(format!("{e:#}"))
}

/// Map a status-level People API error to a user-facing message for the
/// sync log.
fn status_error(account: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::RateLimited => {
            anyhow!("People API rate limited the sync ({account}) — it resumes next pass")
        }
        FetchError::Unauthorized => {
            anyhow!("Google rejected the token ({account}, 401) — reconnect from the Integrations tab")
        }
        // Handled inside sync_list (reset to a full re-list); only ever
        // surfaces if Google rejects the *reset* request too.
        FetchError::ExpiredSyncToken => {
            anyhow!("contacts sync token expired ({account}) — re-listing in full")
        }
        FetchError::Other(m) => anyhow!("google contacts {account}: {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-gcontacts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A scripted [`PeopleSource`]: pops one canned response per call.
    struct Scripted(RefCell<VecDeque<Result<PersonPage, FetchError>>>);

    impl Scripted {
        fn new(pages: Vec<Result<PersonPage, FetchError>>) -> Self {
            Scripted(RefCell::new(pages.into()))
        }
    }

    impl PeopleSource for Scripted {
        fn page(
            &self,
            _kind: ListKind,
            _sync_token: Option<&str>,
            _page_token: Option<&str>,
        ) -> Result<PersonPage, FetchError> {
            self.0.borrow_mut().pop_front().expect("script exhausted")
        }
    }

    fn page(people: Vec<Value>, next_page: Option<&str>, next_sync: Option<&str>) -> PersonPage {
        PersonPage {
            people,
            next_page: next_page.map(str::to_string),
            next_sync: next_sync.map(str::to_string),
        }
    }

    fn upsert(value: Value) -> Contact {
        match normalize_person(value, "me@gmail.com") {
            Some(PersonChange::Upsert(c)) => c,
            other => panic!("expected upsert, got {other:?}"),
        }
    }

    #[test]
    fn rich_person_maps_core_and_parks_rest_in_extra() {
        let c = upsert(json!({
            "resourceName": "people/c1",
            "etag": "ignored-at-top-level? no — preserved in extra",
            "metadata": {
                "sources": [
                    {"type": "CONTACT", "id": "c1", "updateTime": "2026-05-01T00:00:00Z", "etag": "x"},
                    {"type": "PROFILE", "id": "p1", "updateTime": "2026-06-02T00:00:00Z"}
                ],
                "objectType": "PERSON"
            },
            "names": [
                // Non-primary first: primary selection must win over order.
                {"displayName": "Ali E.", "metadata": {"primary": false}},
                {"displayName": "Alice Example", "givenName": "Alice", "familyName": "Example",
                 "metadata": {"primary": true}}
            ],
            "emailAddresses": [
                {"value": "Alice@Example.com", "type": "home"},  // lowercased
                {"value": "alice@example.com"},          // dupe after lowercasing — dropped
                {"value": " alice@work.com "}            // trimmed
            ],
            // A full international number → E.164; an unparseable one stays verbatim.
            "phoneNumbers": [{"value": "+1 (415) 555-0142"}, {"value": "ext. 99"}],
            "organizations": [
                {"name": "Example Corp", "title": "CTO"},
                {"metadata": {"primary": false}}         // empty — dropped
            ],
            "photos": [
                {"url": "https://lh3/default", "default": true},   // silhouette — skipped
                {"url": "https://lh3/real.jpg"}
            ],
            "biographies": [{"value": "Old friend from school."}],
            "birthdays": [{"date": {"month": 3, "day": 14}}],
            "urls": [{"value": "https://alice.example"}]
        }));
        assert_eq!(c.source, "google-contacts");
        assert_eq!(c.account, "me@gmail.com");
        assert_eq!(c.id, "people/c1");
        assert_eq!(c.name, "Alice Example");
        assert_eq!(c.given, "Alice");
        assert_eq!(c.family, "Example");
        assert_eq!(c.emails, vec!["alice@example.com", "alice@work.com"]);
        // First number E.164-normalized; the non-number kept verbatim.
        assert_eq!(c.phones, vec!["+14155550142", "ext. 99"]);
        assert_eq!(c.orgs.len(), 1);
        assert_eq!(c.orgs[0].name.as_deref(), Some("Example Corp"));
        assert_eq!(c.orgs[0].title.as_deref(), Some("CTO"));
        assert_eq!(c.photo, "https://lh3/real.jpg");
        assert!(!c.other);
        // Max updateTime across sources, lexical RFC3339.
        assert_eq!(c.updated.as_deref(), Some("2026-06-02T00:00:00Z"));
        // Unmapped fields preserved verbatim; consumed ones gone; metadata
        // deliberately dropped (sync plumbing).
        assert!(c.extra.contains_key("biographies"));
        assert!(c.extra.contains_key("birthdays"));
        assert!(c.extra.contains_key("urls"));
        assert!(c.extra.contains_key("etag"));
        assert!(!c.extra.contains_key("names"));
        assert!(!c.extra.contains_key("metadata"));
    }

    #[test]
    fn sparse_other_contact_normalizes_with_other_flag() {
        let c = upsert(json!({
            "resourceName": "otherContacts/o1",
            "emailAddresses": [{"value": "stranger@example.com"}]
        }));
        assert!(c.other, "otherContacts/ prefix sets the flag");
        assert_eq!(c.emails, vec!["stranger@example.com"]);
        assert!(c.name.is_empty());
        assert!(c.extra.is_empty());
        assert!(c.updated.is_none());
    }

    #[test]
    fn deleted_person_is_a_tombstone() {
        let change = normalize_person(
            json!({
                "resourceName": "people/c9",
                "metadata": {"deleted": true, "sources": []}
            }),
            "me@gmail.com",
        )
        .unwrap();
        assert_eq!(change, PersonChange::Delete("people/c9".into()));
        // And no resource name at all is unusable.
        assert!(normalize_person(json!({"names": []}), "me@gmail.com").is_none());
    }

    /// Build a minimal person JSON for merge tests.
    fn person(resource: &str, name: &str) -> Value {
        json!({"resourceName": resource, "names": [{"displayName": name}]})
    }

    #[test]
    fn incremental_changes_merge_over_existing_snapshot() {
        let mut map: BTreeMap<String, Contact> = BTreeMap::new();
        for v in [
            person("people/c1", "Alice"),
            person("people/c2", "Bob"),
            person("people/c4", "Dora"),
            person("otherContacts/o1", "Carol"),
        ] {
            let c = upsert(v);
            map.insert(c.id.clone(), c);
        }

        // One incremental page: update Alice, re-send Bob unchanged, add a
        // new contact, tombstone Dora.
        let source = Scripted::new(vec![Ok(page(
            vec![
                person("people/c1", "Alice Updated"),
                person("people/c2", "Bob"),
                person("people/c3", "Eve"),
                json!({"resourceName": "people/c4", "metadata": {"deleted": true}}),
            ],
            None,
            Some("sync-2"),
        ))]);
        let (tok, changed) =
            sync_list(&source, ListKind::Connections, Some("sync-1"), &mut map, "me@gmail.com")
                .unwrap();
        assert_eq!(tok.as_deref(), Some("sync-2"));
        assert_eq!(changed, 3, "update + add + delete; identical re-send not counted");
        assert_eq!(map["people/c1"].name, "Alice Updated");
        assert!(map.contains_key("people/c3"));
        assert!(!map.contains_key("people/c4"), "tombstone removed");
        assert!(map.contains_key("otherContacts/o1"), "other list untouched");
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn expired_sync_token_resets_to_a_full_relist() {
        let mut map: BTreeMap<String, Contact> = BTreeMap::new();
        for v in [person("people/c1", "Gone Soon"), person("otherContacts/o1", "Carol")] {
            let c = upsert(v);
            map.insert(c.id.clone(), c);
        }

        // The stale-token request 400s; the full re-list then pages through
        // and replaces the connections half of the snapshot.
        let source = Scripted::new(vec![
            Err(FetchError::ExpiredSyncToken),
            Ok(page(vec![person("people/c2", "New A")], Some("p2"), None)),
            Ok(page(vec![person("people/c3", "New B")], None, Some("fresh-token"))),
        ]);
        let (tok, changed) =
            sync_list(&source, ListKind::Connections, Some("stale"), &mut map, "me@gmail.com")
                .unwrap();
        assert_eq!(tok.as_deref(), Some("fresh-token"));
        assert_eq!(changed, 3, "one implicit delete + two adds");
        assert!(!map.contains_key("people/c1"), "not re-listed → gone");
        assert!(map.contains_key("people/c2") && map.contains_key("people/c3"));
        assert!(map.contains_key("otherContacts/o1"), "other half untouched by the swap");
    }

    #[test]
    fn non_expiry_errors_propagate_without_touching_the_map() {
        let mut map: BTreeMap<String, Contact> = BTreeMap::new();
        let c = upsert(person("people/c1", "Keep"));
        map.insert(c.id.clone(), c);
        let source = Scripted::new(vec![Err(FetchError::Unauthorized)]);
        let err = sync_list(&source, ListKind::Connections, Some("tok"), &mut map, "me@gmail.com")
            .unwrap_err();
        assert!(matches!(err, FetchError::Unauthorized));
        assert!(map.contains_key("people/c1"));
    }

    #[test]
    fn snapshot_round_trips_sorted_by_id() {
        let v = temp_vault("snapshot");
        let mut map: BTreeMap<String, Contact> = BTreeMap::new();
        for val in [
            person("people/c2", "B"),
            person("people/c1", "A"),
            person("otherContacts/o1", "O"),
        ] {
            let c = upsert(val);
            map.insert(c.id.clone(), c);
        }
        let records: Vec<&Contact> = map.values().collect();
        v.write_snapshot(&snapshot_rel("123"), &records).unwrap();
        let loaded: Vec<Contact> = v.read_snapshot(&snapshot_rel("123")).unwrap();
        let order: Vec<&str> = loaded.iter().map(|c| c.id.as_str()).collect();
        // BTreeMap order: deterministic, so rewrites diff cleanly.
        assert_eq!(order, vec!["otherContacts/o1", "people/c1", "people/c2"]);
        assert_eq!(loaded[1].name, "A");
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("state");
        assert!(v.read_google_contacts_sync().is_none());
        let mut state = GoogleContactsSyncState {
            updated: "2026-06-12T10:00:00-07:00".into(),
            ..Default::default()
        };
        state.accounts.insert(
            "12345".into(),
            GoogleContactsAccountState {
                email: "me@gmail.com".into(),
                connections_sync_token: Some("ct".into()),
                other_sync_token: Some("ot".into()),
                contacts: 250,
                other_contacts: 1800,
                last_sync: Some("2026-06-12T09:00:00-07:00".into()),
                error: None,
            },
        );
        v.write_google_contacts_sync(&state).unwrap();
        let loaded = v.read_google_contacts_sync().unwrap();
        assert_eq!(loaded.updated, "2026-06-12T10:00:00-07:00");
        let a = &loaded.accounts["12345"];
        assert_eq!(a.email, "me@gmail.com");
        assert_eq!(a.connections_sync_token.as_deref(), Some("ct"));
        assert_eq!(a.other_sync_token.as_deref(), Some("ot"));
        assert_eq!((a.contacts, a.other_contacts), (250, 1800));
    }

    #[test]
    fn collect_without_an_account_is_a_silent_noop() {
        let v = temp_vault("noaccount");
        let stats = v.collect_google_contacts().unwrap();
        assert_eq!(stats.changed, 0);
        assert!(v.read_google_contacts_sync().is_none());
        assert!(!v.root().join("contacts").exists(), "no store created");
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
    fn disconnected_account_state_and_snapshot_are_pruned() {
        let v = temp_vault("prune");
        // A previous sync left a snapshot and state for account 123…
        let c = upsert(person("people/c1", "Alice"));
        v.write_snapshot(&snapshot_rel("123"), &[c]).unwrap();
        let mut state = GoogleContactsSyncState {
            updated: "2026-06-12T08:00:00-07:00".into(),
            ..Default::default()
        };
        state.accounts.insert(
            "123".into(),
            GoogleContactsAccountState { email: "me@gmail.com".into(), ..Default::default() },
        );
        v.write_google_contacts_sync(&state).unwrap();

        // …then the account was disconnected: the pass prunes both. Only the
        // rebuildable snapshot is removed — contacts keep no history stream.
        v.collect_google_contacts().unwrap();
        assert!(!v.root().join(snapshot_rel("123")).exists(), "snapshot pruned");
        let loaded = v.read_google_contacts_sync().unwrap();
        assert!(loaded.accounts.is_empty(), "state row pruned");
        let md = fs::read_to_string(v.root().join("contacts/index.md")).unwrap();
        assert!(!md.contains("me@gmail.com"), "index row gone");
    }

    #[test]
    fn pull_removes_legacy_google_contacts_dir() {
        let v = temp_vault("legacy-dir");
        // A pre-migration pull left the orphaned `contacts/google/` folder…
        let legacy = v.root().join("contacts/google");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("old.jsonl"), "{\"resource\":\"people/c1\"}\n").unwrap();
        // …alongside the current-format sibling, which must survive.
        let sibling = v.root().join(format!("contacts/{SOURCE}"));
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("keep.jsonl"), "{\"source\":\"google-contacts\"}\n").unwrap();

        remove_legacy_dir(&v);

        assert!(!legacy.exists(), "legacy contacts/google/ removed");
        assert!(sibling.join("keep.jsonl").exists(), "google-contacts sibling untouched");
        // Idempotent: a second pass with the dir already gone is a clean no-op.
        remove_legacy_dir(&v);
        assert!(!legacy.exists());
    }

    #[test]
    fn index_lists_accounts() {
        let v = temp_vault("index");
        let mut state = GoogleContactsSyncState {
            updated: "2026-06-12T10:00:00-07:00".into(),
            ..Default::default()
        };
        state.accounts.insert(
            "1".into(),
            GoogleContactsAccountState {
                email: "a@gmail.com".into(),
                contacts: 250,
                other_contacts: 1800,
                last_sync: Some("2026-06-12T09:00:00-07:00".into()),
                ..Default::default()
            },
        );
        v.write_contacts_index(&state).unwrap();
        let md = fs::read_to_string(v.root().join("contacts/index.md")).unwrap();
        assert!(md.contains("| a@gmail.com | 250 | 1800 | 2026-06-12T09:00:00-07:00 |"));
    }
}

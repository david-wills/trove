//! Pocket Casts — podcast listening history via the unofficial cloud API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/pocket-casts.md.
//!
//! A **Periodic** cloud pull (hourly cadence): the endpoint returns at most
//! the 100 most recent history items, so polling frequently preserves the
//! window. Two layers are written per episode seen for the first time:
//!
//! - **raw** — the API episode object verbatim at
//!   `media/plays/pocket-casts/raw/YYYY-MM.jsonl`, partitioned by the
//!   first-seen month (full fidelity, unconditional).
//! - **contract** — one normalized [`MediaItem`] at
//!   `media/plays/pocket-casts/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! The endpoint returns no played timestamp, so `ts` is the first-seen time
//! (the RFC3339 timestamp of the poll that first observed the item). This is
//! documented honestly in `extra.ts_basis = "first_seen"`.
//!
//! Auth: user pastes their Pocket Casts email and password as
//! `email:password` in the connect card; the run fn exchanges those credentials
//! against `POST /user/login` and stores only the returned bearer token under
//! `.trove/sync/`. The raw credentials are never stored.
//!
//! The seen-set cursor lives at `.trove/pocket-casts-seen.json` (non-secret,
//! rebuildable by scanning the raw layer). A guid/parse miss that would empty
//! the write set does not advance the cursor.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::store::write_json_atomic;
use crate::vault::Vault;

/// Vault contract layer directory. Raw lines go one level deeper in `raw/`.
const DIR: &str = "media/plays/pocket-casts";
const RAW_DIR: &str = "media/plays/pocket-casts/raw";
/// Non-secret rebuildable seen-set cursor. Deleting it causes a silent
/// re-baseline (no duplicate contract rows — guid dedupe handles re-runs).
const CURSOR_FILE: &str = ".trove/pocket-casts-seen.json";
/// Service id under `.trove/sync/` where the bearer token is stored.
const SERVICE: &str = "pocket-casts";

const API_BASE: &str = "https://api.pocketcasts.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Hourly poll — the 100-item window is the binding constraint.
pub const POLL_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Podcast playing_status integer constants (from the Pocket Casts app source).

/// Episode has never been played.
const STATUS_UNPLAYED: i64 = 0;
/// Episode is in progress.
const STATUS_IN_PROGRESS: i64 = 2;
/// Episode has been fully played.
const STATUS_PLAYED: i64 = 3;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("episodes").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("pocket-casts synced — {n} episodes")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "pocket-casts sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("episodes").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Pocket Casts is up to date — no new episodes in history".to_string()
    } else {
        format!("Pocket Casts synced — {n} new episodes")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "pocket-casts",
        name: "Pocket Casts",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Sync your Pocket Casts listening history into the unified media stream. \
                      Uses the unofficial history endpoint (≤100 most recent items); connect \
                      early to avoid losing older listens beyond the window.",
        domain: "media",
        vault_path: "media/plays/pocket-casts/",
        toggleable: true,
        setup: &[
            "Paste your Pocket Casts login as email:password and connect.",
            "The history window is capped at ~100 items — connect early to avoid losing older listens.",
        ],
        caveats: "Uses an unofficial API (may change without notice) capped at ~100 recent items. \
                  No played timestamps are available; Trove records first-seen time instead.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(POLL_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("pocket-casts"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = email:password composite; only the bearer token
// is persisted, never the raw credentials).

fn def_connect(vault: &Vault, raw: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("enter your Pocket Casts email and password as email:password");
    }
    // Split on the first colon only — passwords may contain colons.
    let (email, password) = raw
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected email:password — separate with a colon"))?;
    let email = email.trim();
    let password = password.trim();
    if email.is_empty() || password.is_empty() {
        bail!("both email and password are required (email:password)");
    }

    // Exchange credentials for a bearer token. Store only the token.
    let token = login(email, password)?;
    let tok = crate::sync::oauth::TokenSet {
        access_token: token,
        refresh_token: None,
        token_type: Some("Bearer".to_string()),
        scope: None,
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &tok)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        // We store only the bearer token (no human-readable label). Display a
        // redacted form so the hub shows "connected" without exposing the value.
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Connected".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: {
                // Hint that the token is present without showing its value.
                let _ = token;
                BTreeMap::new()
            },
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "pocket-casts",
    display_name: "Pocket Casts",
    methods: &[ConnectMethod::TokenPaste {
        label: "Pocket Casts login (email:password)",
        help: "Enter your Pocket Casts account email and password separated by a colon, \
               e.g. you@example.com:yourpassword. Only the login token is stored — \
               your credentials are used once and then discarded.",
        placeholder: "you@example.com:yourpassword",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["pocket-casts"],
    setup: &[
        "Paste your Pocket Casts email and password as email:password.",
        "Your credentials are exchanged for a login token (only the token is stored).",
        "This uses an unofficial API — it may break without notice.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer.

/// Exchange email + password for a bearer token.
/// Errors clearly on bad credentials or network failure.
fn login(email: &str, password: &str) -> Result<String> {
    let body = serde_json::json!({
        "email": email,
        "password": password,
        "scope": "webplayer"
    });
    let resp = ureq::post(&format!("{API_BASE}/user/login"))
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| match e {
            ureq::Error::Status(401, _) => {
                anyhow::anyhow!("Pocket Casts rejected the login — check your email and password")
            }
            ureq::Error::Status(code, r) => {
                let body = r.into_string().unwrap_or_default();
                let body: String = body.chars().take(200).collect();
                anyhow::anyhow!("Pocket Casts login failed (HTTP {code}): {body}")
            }
            other => anyhow::anyhow!("Pocket Casts login network error: {other}"),
        })?;
    let v: Value = resp.into_json().context("parsing Pocket Casts login response")?;
    v.get("token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Pocket Casts login response had no token field"))
}

/// Fetch the listening history using the stored bearer token.
/// Returns the raw JSON value of the `episodes` array.
fn fetch_history(bearer: &str) -> Result<Vec<Value>> {
    let resp = ureq::post(&format!("{API_BASE}/user/history"))
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {bearer}"))
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({}))
        .map_err(|e| match e {
            ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => anyhow::anyhow!(
                "Pocket Casts rejected the request — try reconnecting in the Integrations tab"
            ),
            ureq::Error::Status(code, r) => {
                let body = r.into_string().unwrap_or_default();
                let body: String = body.chars().take(200).collect();
                anyhow::anyhow!("Pocket Casts history fetch failed (HTTP {code}): {body}")
            }
            other => anyhow::anyhow!("Pocket Casts history network error: {other}"),
        })?;
    let v: Value = resp.into_json().context("parsing Pocket Casts history response")?;
    // Response shape: { "episodes": [...] }
    Ok(match v.get("episodes") {
        Some(Value::Array(arr)) => arr.clone(),
        Some(_) => bail!("Pocket Casts history 'episodes' field is not an array"),
        None => Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Cursor (seen-set): a set of episode uuids ever written. Rebuildable by
// scanning the raw layer. Non-secret — sits in .trove/, not .trove/sync/.

#[derive(Debug, Default, Serialize, Deserialize)]
struct SeenState {
    /// Episode uuids ever written to the contract layer.
    #[serde(default)]
    seen: HashSet<String>,
    /// RFC3339 local time of the last successful pull.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_pc_seen(&self) -> SeenState {
        self.resolve(CURSOR_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_pc_seen(&self, state: &SeenState) -> Result<()> {
        let path = self.resolve(CURSOR_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing.

/// Parse one episode object from the history response into a MediaItem.
/// Returns `None` when the episode has no uuid (can't dedupe safely).
///
/// Field-name evidence for api.pocketcasts.com (new host, the one this module
/// targets):
///
/// - `podcastTitle` (flat string) — show name as observed in Chrome DevTools
///   captures against api.pocketcasts.com. Grounded by `podcastUuid`.
/// - `playingStatus` (camelCase int) — from the B-Lach Swift model and live
///   captures against the new host.
/// - `playedUpTo` (camelCase float) — same source.
/// - `published` (no `_at`) — from the Android Automattic history sync client.
///   The old play.pocketcasts.com host used `published_at` (snake_case, space-
///   separated datetime); we fall back to it so both hosts work.
///
/// All field reads are defensive: presence/type mismatches degrade gracefully
/// rather than producing wrong values.
fn parse_episode(ep: &Value, first_seen: &str) -> Option<MediaItem> {
    let uuid = ep.get("uuid").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let title = ep.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();

    // Show/podcast name.
    //
    // Primary path (new api.pocketcasts.com host): `podcastTitle` is a flat
    // string field alongside `podcastUuid`.
    //
    // Fallback paths for older or alternative API shapes:
    //   - `podcast` as an object with a nested `title` key
    //   - `podcast` as a plain string
    //
    // If none of these are present the show name is empty — still a valid
    // (degraded) row; we never skip an episode just because the show name is
    // missing.
    let podcast_title =
        ep.get("podcastTitle")
          .and_then(Value::as_str)
          .map(str::trim)
          .filter(|s| !s.is_empty())
          .map(str::to_string)
          .or_else(|| match ep.get("podcast") {
              Some(Value::Object(obj)) => obj
                  .get("title")
                  .and_then(Value::as_str)
                  .filter(|s| !s.is_empty())
                  .map(|s| s.trim().to_string()),
              Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
              _ => None,
          })
          .unwrap_or_default();

    // playing_status: 0=unplayed, 2=in_progress, 3=played.
    //
    // New api.pocketcasts.com host uses camelCase `playingStatus`; the old
    // play.pocketcasts.com host used snake_case `playing_status`. Read both
    // so this collector works against either host without config.
    let status = ep
        .get("playingStatus")
        .and_then(Value::as_i64)
        .or_else(|| ep.get("playing_status").and_then(Value::as_i64))
        .unwrap_or(STATUS_UNPLAYED);
    let kind = if status == STATUS_PLAYED {
        "play"
    } else if status == STATUS_IN_PROGRESS {
        "partial"
    } else {
        // Unplayed items can appear in the history endpoint (e.g. added to
        // queue); still record them as partial to be honest.
        "partial"
    };

    let guid = format!("pocket-casts-{uuid}");

    let mut extra: Map<String, Value> = Map::new();
    extra.insert("ts_basis".into(), Value::String("first_seen".into()));
    if status == STATUS_PLAYED {
        extra.insert("playing_status".into(), Value::String("played".into()));
    } else if status == STATUS_IN_PROGRESS {
        extra.insert("playing_status".into(), Value::String("in_progress".into()));
    } else {
        extra.insert("playing_status".into(), Value::String("unplayed".into()));
    }

    // Podcast UUID for grounding (new host).
    if let Some(v) = ep.get("podcastUuid").filter(|v| !v.is_null()) {
        extra.insert("podcast_uuid".into(), v.clone());
    }

    // Publish date. New host: `published` (format unverified — preserve raw
    // string). Old host: `published_at` (space-separated `YYYY-MM-DD HH:MM:SS`).
    // We store whichever is present under `published_at` for consistency; the
    // raw layer always has the original key/value intact.
    let pub_val = ep
        .get("published")
        .filter(|v| !v.is_null())
        .or_else(|| ep.get("published_at").filter(|v| !v.is_null()));
    if let Some(v) = pub_val {
        extra.insert("published_at".into(), v.clone());
    }

    if let Some(v) = ep.get("duration").filter(|v| !v.is_null()) {
        extra.insert("duration_secs".into(), v.clone());
    }
    if let Some(url) = ep.get("url").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        extra.insert("url".into(), Value::String(url.to_string()));
    }
    // playedUpTo (new host camelCase) / played_up_to (old host snake_case).
    let played_up_to = ep
        .get("playedUpTo")
        .filter(|v| !v.is_null())
        .or_else(|| ep.get("played_up_to").filter(|v| !v.is_null()));
    if let Some(v) = played_up_to {
        extra.insert("played_up_to_secs".into(), v.clone());
    }
    if let Some(v) = ep.get("starred").filter(|v| !v.is_null()) {
        extra.insert("starred".into(), v.clone());
    }
    // is_video: present on the old play.pocketcasts.com host; not confirmed on
    // the new api.pocketcasts.com host. Stored opportunistically when present.
    if let Some(v) = ep.get("is_video").filter(|v| !v.is_null()) {
        extra.insert("is_video".into(), v.clone());
    }

    Some(MediaItem {
        ts: first_seen.to_string(),
        source: "pocket-casts".into(),
        category: "podcast".into(),
        device: String::new(),
        kind: kind.into(),
        title,
        subtitle: podcast_title,
        detail: String::new(),
        seconds: 0, // no duration of playback available
        favicon: String::new(),
        guid,
        extra,
    })
}

/// A raw episode line carrying a `ts` so the month-partition writer files it
/// under the first-seen month. Only `value` is serialized to disk — flattened
/// so the raw line is the API object verbatim.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync the history window.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let bearer = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Pocket Casts is not connected — add your email:password in the Integrations tab")?;
    pull_with(vault, || fetch_history(&bearer))
}

/// Testable seam: `fetch` is injected so tests run fully offline.
fn pull_with(vault: &Vault, fetch: impl FnOnce() -> Result<Vec<Value>>) -> Result<PullOutcome> {
    // Load the seen-set BEFORE fetching to avoid TOCTOU if writing fails.
    let mut state = vault.read_pc_seen();

    let episodes = fetch()?;
    if episodes.is_empty() {
        // Empty response is not an error — history may simply be empty.
        state.updated = Some(Local::now().to_rfc3339());
        vault.write_pc_seen(&state)?;
        return Ok(PullOutcome {
            headline: "Pocket Casts history is empty".to_string(),
            counts: BTreeMap::from([("episodes", 0)]),
        });
    }

    let first_seen = Local::now().to_rfc3339();

    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    let mut new_rows: Vec<MediaItem> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();

    for ep in &episodes {
        let uuid = match ep.get("uuid").and_then(Value::as_str) {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => continue, // no uuid → cannot dedupe → skip
        };
        let guid = format!("pocket-casts-{uuid}");
        // Peek without inserting yet — insert only after a successful parse so
        // that a future parse failure (e.g. stricter field validation) leaves
        // the item retryable on the next poll rather than silently dropped.
        if state.seen.contains(&guid) {
            continue; // already seen in a previous poll
        }
        if let Some(item) = parse_episode(ep, &first_seen) {
            state.seen.insert(guid);
            new_rows.push(item);
            new_raws.push(RawLine { ts: first_seen.clone(), value: ep.clone() });
        }
        // If parse_episode returns None (currently only possible when uuid is
        // absent, which is already filtered above) the guid is NOT added to
        // seen, preserving retryability.
    }

    // Persist raw + contract only if there is something to write.
    // The seen-set is only updated for successfully parsed episodes (above),
    // so a parse miss leaves the item retryable on the next poll.
    if !new_rows.is_empty() {
        contract.append(&new_rows, |i| &i.ts)?;
        raw.append(&new_raws, |r| &r.ts)?;
    }

    let n = new_rows.len() as u64;
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_pc_seen(&state)?;

    Ok(PullOutcome {
        headline: format!("{n} episodes"),
        counts: BTreeMap::from([("episodes", n)]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-pc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A realistic history response using the api.pocketcasts.com (new host)
    /// camelCase field names: `podcastTitle`, `podcastUuid`, `playingStatus`,
    /// `playedUpTo`, `published`.
    ///
    /// This is the primary shape the collector targets. The old
    /// play.pocketcasts.com host used snake_case (`playing_status`,
    /// `played_up_to`, `published_at`) — fallback coverage is tested
    /// separately in `parses_old_host_snake_case_fields`.
    fn sample_history() -> Vec<Value> {
        vec![
            json!({
                "uuid": "epi-aaa-111",
                "title": "The AI Episode",
                "podcastTitle": "Hard Fork",
                "podcastUuid": "podcast-uuid-hf",
                "playingStatus": 3,
                "playedUpTo": 3600.0,
                "duration": 3600.0,
                "published": "2026-06-10 08:00:00",
                "url": "https://rss.example/aaa.mp3",
                "starred": false
            }),
            json!({
                "uuid": "epi-bbb-222",
                "title": "Deep Dive into Rust",
                "podcastTitle": "The Changelog",
                "podcastUuid": "podcast-uuid-cl",
                "playingStatus": 2,
                "playedUpTo": 900.0,
                "duration": 4500.0,
                "published": "2026-06-09 08:00:00",
                "url": "https://rss.example/bbb.mp3",
                "starred": true
            }),
            json!({
                "uuid": "epi-ccc-333",
                "title": "Unplayed Episode",
                "podcastTitle": "Some Show",
                "podcastUuid": "podcast-uuid-ss",
                "playingStatus": 0,
                "duration": 1800.0,
                "published": "2026-06-08 08:00:00"
            }),
        ]
    }

    #[test]
    fn parses_played_episode() {
        let ep = &sample_history()[0];
        let ts = "2026-06-16T10:00:00-07:00";
        let item = parse_episode(ep, ts).unwrap();

        assert_eq!(item.title, "The AI Episode");
        assert_eq!(item.subtitle, "Hard Fork", "podcastTitle → subtitle");
        assert_eq!(item.source, "pocket-casts");
        assert_eq!(item.category, "podcast");
        assert_eq!(item.kind, "play", "playingStatus=3 → play");
        assert_eq!(item.seconds, 0, "honest zero — no playback duration from API");
        assert_eq!(item.guid, "pocket-casts-epi-aaa-111");
        assert_eq!(item.ts, ts);
        assert_eq!(
            item.extra.get("ts_basis"),
            Some(&Value::String("first_seen".into()))
        );
        assert_eq!(
            item.extra.get("playing_status"),
            Some(&Value::String("played".into()))
        );
        assert!(item.extra.contains_key("url"));
        assert!(item.extra.contains_key("published_at"), "published → published_at in extra");
        assert!(item.extra.contains_key("duration_secs"));
        assert!(item.extra.contains_key("played_up_to_secs"), "playedUpTo → played_up_to_secs");
        assert!(item.extra.contains_key("podcast_uuid"), "podcastUuid → extra.podcast_uuid");
    }

    #[test]
    fn parses_in_progress_episode() {
        let ep = &sample_history()[1];
        let item = parse_episode(ep, "2026-06-16T10:00:00-07:00").unwrap();
        assert_eq!(item.kind, "partial", "playingStatus=2 → partial");
        assert_eq!(
            item.extra.get("playing_status"),
            Some(&Value::String("in_progress".into()))
        );
        assert_eq!(item.extra.get("starred"), Some(&Value::Bool(true)));
    }

    #[test]
    fn parses_unplayed_episode_subtitle() {
        // playingStatus=0 → partial; podcastTitle present → subtitle populated.
        let ep = &sample_history()[2];
        let item = parse_episode(ep, "2026-06-16T10:00:00-07:00").unwrap();
        assert_eq!(item.subtitle, "Some Show", "podcastTitle read on unplayed episode");
        assert_eq!(item.kind, "partial", "playingStatus=0 → partial");
    }

    #[test]
    fn parses_old_host_snake_case_fields() {
        // Old play.pocketcasts.com host used snake_case field names. The
        // collector must still extract the show name, play status, played
        // position, and publish date from these shapes.
        let ep_old_object = json!({
            "uuid": "epi-old-001",
            "title": "Old Episode",
            "podcast": { "title": "Old Show" },
            "playing_status": 3,
            "played_up_to": 1200.0,
            "duration": 2400.0,
            "published_at": "2024-03-15 12:00:00",
            "url": "https://rss.example/old.mp3"
        });
        let item = parse_episode(&ep_old_object, "2026-06-16T10:00:00-07:00").unwrap();
        assert_eq!(item.subtitle, "Old Show", "podcast object fallback");
        assert_eq!(item.kind, "play", "playing_status=3 → play (snake_case)");
        assert!(item.extra.contains_key("played_up_to_secs"), "played_up_to fallback");
        assert!(item.extra.contains_key("published_at"), "published_at fallback");

        // Plain-string `podcast` field (another old-host variant).
        let ep_old_string = json!({
            "uuid": "epi-old-002",
            "title": "Another Old Episode",
            "podcast": "Another Old Show",
            "playing_status": 2,
            "played_up_to": 300.0,
            "published_at": "2024-03-14 12:00:00"
        });
        let item2 = parse_episode(&ep_old_string, "2026-06-16T10:00:00-07:00").unwrap();
        assert_eq!(item2.subtitle, "Another Old Show", "podcast string fallback");
        assert_eq!(item2.kind, "partial", "playing_status=2 → partial (snake_case)");
    }

    #[test]
    fn episode_missing_uuid_is_skipped() {
        let ep = json!({ "title": "No UUID Here", "playingStatus": 3 });
        assert!(parse_episode(&ep, "2026-06-16T10:00:00-07:00").is_none());
    }

    #[test]
    fn writes_contract_and_raw_layers_and_dedupes() {
        let v = temp_vault("store");

        // First pull: all three episodes are new.
        let episodes = sample_history();
        let out = pull_with(&v, || Ok(episodes.clone())).unwrap();
        assert_eq!(out.counts.get("episodes"), Some(&3));

        // Contract layer exists.
        let contract_path = v
            .root()
            .join("media/plays/pocket-casts")
            .read_dir()
            .unwrap()
            .flatten()
            .find(|e| e.path().extension().map_or(false, |x| x == "jsonl"))
            .expect("at least one contract JSONL")
            .path();
        let contract = std::fs::read_to_string(&contract_path).unwrap();
        assert_eq!(contract.lines().count(), 3, "three contract rows");

        // Verify MediaItem fields on the first row.
        let first: MediaItem = serde_json::from_str(contract.lines().next().unwrap()).unwrap();
        assert_eq!(first.source, "pocket-casts");
        assert_eq!(first.category, "podcast");
        assert_eq!(first.seconds, 0, "honest zero");
        assert!(first.guid.starts_with("pocket-casts-"));
        // subtitle must be populated from podcastTitle — the grouping key.
        assert!(!first.subtitle.is_empty(), "subtitle (podcast name) must not be empty");
        assert_eq!(
            first.extra.get("ts_basis"),
            Some(&Value::String("first_seen".into()))
        );

        // Raw layer exists and has full-fidelity objects.
        let raw_dir = v.root().join("media/plays/pocket-casts/raw");
        let raw_path = raw_dir
            .read_dir()
            .unwrap()
            .flatten()
            .find(|e| e.path().extension().map_or(false, |x| x == "jsonl"))
            .expect("at least one raw JSONL")
            .path();
        let raw_content = std::fs::read_to_string(&raw_path).unwrap();
        assert_eq!(raw_content.lines().count(), 3, "three raw rows");
        assert!(
            raw_content.contains("\"epi-aaa-111\""),
            "raw has uuid verbatim"
        );
        assert!(
            raw_content.contains("Hard Fork"),
            "raw has podcast title"
        );

        // Seen-set cursor written.
        let state = v.read_pc_seen();
        assert!(state.seen.contains("pocket-casts-epi-aaa-111"));
        assert!(state.seen.contains("pocket-casts-bbb-222") == false, "wrong prefix");
        assert!(state.seen.contains("pocket-casts-epi-bbb-222"));
        assert!(state.updated.is_some());

        // Second pull with the same episodes: all deduped.
        let out2 = pull_with(&v, || Ok(episodes.clone())).unwrap();
        assert_eq!(
            out2.counts.get("episodes"),
            Some(&0),
            "all guids already in seen-set"
        );
        // Contract file unchanged.
        let contract2 = std::fs::read_to_string(&contract_path).unwrap();
        assert_eq!(contract, contract2, "contract file byte-identical after re-run");

        // The unified media stream sees the episodes via the contract arm.
        let ts_date = first.ts[..10].to_string();
        let timeline = v.media_timeline(&ts_date).unwrap();
        assert!(
            timeline.iter().any(|i| i.source == "pocket-casts"),
            "pocket-casts appears in unified media stream"
        );
    }

    #[test]
    fn empty_history_response_is_not_an_error() {
        let v = temp_vault("empty");
        let out = pull_with(&v, || Ok(vec![])).unwrap();
        assert_eq!(out.counts.get("episodes"), Some(&0));
        // Cursor written with no seen entries.
        let state = v.read_pc_seen();
        assert!(state.seen.is_empty());
        assert!(state.updated.is_some());
    }

    #[test]
    fn network_error_surfaces_clearly() {
        let v = temp_vault("err");
        let err = pull_with(&v, || bail!("simulated network error")).unwrap_err();
        assert!(
            err.to_string().contains("simulated network error"),
            "error propagates: {err}"
        );
    }

    #[test]
    fn not_connected_pull_fails_clearly() {
        let v = temp_vault("noconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(
            err.contains("not connected"),
            "clear error when no token: {err}"
        );
    }

    #[test]
    fn connection_status_and_disconnect() {
        assert!(CONNECTION.method("token-paste").is_some());

        let v = temp_vault("conn");
        // Store a fake bearer token directly (as connect() would after a real login).
        let tok = crate::sync::oauth::TokenSet {
            access_token: "fake-bearer-abc".to_string(),
            refresh_token: None,
            token_type: Some("Bearer".to_string()),
            scope: None,
            expires_at: None,
        };
        v.save_sync_token(SERVICE, &tok).unwrap();

        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].key, "pocket-casts");
        assert_eq!(status.accounts[0].label, "Connected");

        def_disconnect(&v, "pocket-casts").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }
}

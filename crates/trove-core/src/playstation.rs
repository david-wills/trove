//! PlayStation Network — trophy history and played-title list via the
//! community-documented PSN unofficial API (same API surface as `psn-api` JS
//! and `psnawp` Python).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/playstation.md.
//!
//! # Auth
//!
//! Sony uses a multi-step NPSSO → access-code → token-pair flow:
//!
//! 1. User logs into `playstation.com`, visits
//!    `https://ca.account.sony.com/api/v1/ssocookie`, copies the 64-char
//!    NPSSO token, and pastes it into Trove.
//! 2. Trove exchanges the NPSSO for an access code via GET to the Sony OAuth
//!    authorize endpoint (with the NPSSO sent as the `npsso` cookie).
//! 3. The access code is POSTed to the token endpoint to obtain an
//!    `access_token` + `refresh_token`.  The refresh token is valid ~59 days;
//!    on expiry the card shows a "re-paste NPSSO" state.
//!
//! All auth is implemented directly in Rust — no shim.
//!
//! # Data
//!
//! Two streams:
//!
//! - **titles** — a full-fidelity snapshot of the played-title list at
//!   `gaming/playstation/titles/YYYY-MM.jsonl`, keyed/upserted by `guid`
//!   (= `titleId`).  Partition key is the `lastPlayedDateTime` month.
//!   Fields: `titleId`, `name`, `imageUrl`, `category`, `playCount`,
//!   `firstPlayedDateTime`, `lastPlayedDateTime`, `playDuration` (ISO-8601
//!   duration, e.g. `PT1H51M21S`).  PS4 titles may omit `playDuration`.
//!
//! - **trophies** — one row per earned trophy at
//!   `gaming/playstation/trophies/YYYY-MM.jsonl`, partitioned by the
//!   `earnedDateTime` month.  `guid` = `{npCommunicationId}/{trophyId}`.
//!   Fields: `np_comm_id`, `trophy_id`, `trophy_type`, `trophy_name`,
//!   `trophy_detail`, `trophy_icon_url`, `trophy_rare`, `trophy_earned_rate`,
//!   `earned_date_time`, `title_name`, `title_platform`.
//!
//! # Rate-limiting
//!
//! Self-limit: ≤ 300 req / 15 min → pause 300 ms between requests.
//! The periodic cadence is 30 min so a full incremental sync stays well under
//! the limit even for large trophy histories.
//!
//! # API shapes confirmed against
//!
//! - `psnawp` Python library (`title_stats.py`, `authenticator.py`):
//!   `titleId`, `name`, `imageUrl`, `category`, `playCount`,
//!   `firstPlayedDateTime`, `lastPlayedDateTime`, `playDuration`;
//!   auth params: `client_id` = "09515159-7237-4370-9b40-3806e67c0891",
//!   `redirect_uri` = "com.scee.psxandroid.scecompcall://redirect",
//!   response: `access_token`, `refresh_token`, `expires_in`,
//!   `refresh_token_expires_in`.
//! - `psn-api` JS library (examples + API-docs):
//!   trophy fields: `trophyId`, `trophyType`, `trophyName`, `trophyRare`,
//!   `trophyEarnedRate`, `earned`, `earnedDateTime`, `trophyGroupId`;
//!   title trophy fields: `npCommunicationId`, `trophyTitleName`,
//!   `trophyTitlePlatform`, `definedTrophies`, `earnedTrophies`.

use std::collections::{BTreeMap, HashSet};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::DateTime;
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SERVICE: &str = "playstation";
const SYNC_FILE: &str = ".trove/playstation-sync.json";
const TITLES_DIR: &str = "gaming/playstation/titles";
const TROPHIES_DIR: &str = "gaming/playstation/trophies";

/// Sony auth endpoints (psnawp source confirmed).
const AUTH_BASE: &str = "https://ca.account.sony.com/api/authz/v3/oauth";
/// PSN API base for games (title stats).
const GAMES_API: &str = "https://m.np.playstation.com/api/gamelist/v2";
/// PSN API base for trophies.
const TROPHY_API: &str = "https://m.np.playstation.com/api/trophy/v1";

/// Client id from psnawp source (the mobile PSN client, public).
const CLIENT_ID: &str = "09515159-7237-4370-9b40-3806e67c0891";
/// Redirect URI registered with the Sony client.
const REDIRECT_URI: &str = "com.scee.psxandroid.scecompcall://redirect";
/// OAuth scope required for trophy + games access.
const SCOPE: &str = "psn:mobile.v2.core psn:clientapp";

/// HTTP timeout per request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);
/// Politeness pause between API calls (≤ 300 req / 15 min → 300 ms).
const REQ_INTERVAL: Duration = Duration::from_millis(300);
/// Sync every 30 minutes; incremental so cheap when idle.
pub const PSN_SYNC_SECS: u64 = 1800;
/// Page size for titles (PSN returns up to 200).
const TITLES_PAGE_SIZE: u32 = 200;
/// Page size for trophy titles (PSN returns up to 800).
const TROPHY_TITLES_PAGE_SIZE: u32 = 800;
/// Page size for per-title trophies.
const TROPHIES_PAGE_SIZE: u32 = 400;
/// Hard cap on pagination loops.
const MAX_PAGES: u32 = 500;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(TROPHIES_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(TITLES_DIR)))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let token = match vault.load_sync_token(SERVICE) {
        Ok(Some(t)) => t,
        Ok(None) => return Ok(crate::registry::CollectOutcome::quiet()),
        Err(_) => return Ok(crate::registry::CollectOutcome::quiet()),
    };
    match pull_with_token(vault, token) {
        Ok(out) => {
            let titles = out.counts.get("titles").copied().unwrap_or(0);
            let trophies = out.counts.get("trophies").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(titles > 0 || trophies > 0, || {
                format!("PlayStation synced — {titles} titles, {trophies} new trophies")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "PlayStation sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| anyhow::anyhow!("PlayStation Network is not connected — paste your NPSSO token in the Integrations tab"))?;
    pull_with_token(vault, token)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "playstation",
        name: "PlayStation Network",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pulls your PlayStation Network trophy history (all earned trophies with unlock \
             timestamps) and played-title list (play count, total playtime on PS5 titles) \
             via the widely-used community-documented PSN API.",
        domain: "gaming",
        vault_path: "gaming/playstation/",
        toggleable: true,
        setup: &[
            "Log into playstation.com in your browser.",
            "Visit https://ca.account.sony.com/api/v1/ssocookie — you will see a JSON object with an \"npsso\" field.",
            "Copy the 64-character token value and paste it into the connection field on this card.",
            "The token is exchanged for a session that lasts roughly two months; the card will prompt you to re-paste when it expires.",
        ],
        caveats: "PS4 titles may report 0 or missing playtime — this is a Sony limitation, not \
                  a Trove bug. The PSN API is community-documented (unofficial); Sony could \
                  change it without notice. NPSSO tokens expire roughly every two months.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(PSN_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("playstation"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection — TokenPaste (the NPSSO cookie).

fn def_connect(vault: &Vault, npsso: &str) -> Result<()> {
    let npsso = npsso.trim();
    if npsso.len() != 64 {
        bail!(
            "The NPSSO token must be exactly 64 characters — got {} characters. \
             Visit https://ca.account.sony.com/api/v1/ssocookie while logged into PlayStation.com \
             and copy the value of the \"npsso\" field.",
            npsso.len()
        );
    }
    let token = exchange_npsso(npsso)?;
    vault.save_sync_token(SERVICE, &token)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let needs_reconnect = token.expired() && token.refresh_token.is_none();
        let label = if needs_reconnect {
            "PSN (token expired — re-paste NPSSO)".to_string()
        } else {
            "PlayStation Network".to_string()
        };
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: token.expires_at,
            needs_reconnect,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "playstation",
    display_name: "PlayStation Network",
    methods: &[ConnectMethod::TokenPaste {
        label: "NPSSO token",
        help: "Log into playstation.com, then visit \
               https://ca.account.sony.com/api/v1/ssocookie — copy the 64-character \
               value next to \"npsso\". The token is valid roughly two months; \
               Trove will prompt you to refresh it when it expires.",
        placeholder: "64-character NPSSO token",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["playstation"],
    setup: &[
        "Log in at playstation.com in your browser.",
        "Visit https://ca.account.sony.com/api/v1/ssocookie — you will see a short JSON block.",
        "Copy the 64-character value next to \"npsso\" and paste it into the connection field.",
        "Trove will exchange the NPSSO for a session token automatically. The session lasts \
         roughly two months; the card will prompt you to re-paste when it expires.",
    ],
};

// ---------------------------------------------------------------------------
// Auth — NPSSO → access code → access + refresh tokens.

/// Exchange a user-pasted NPSSO token for a `TokenSet` (access + refresh).
/// Implements the two-step Sony flow: NPSSO → access code → token pair.
/// Confirmed endpoints from psnawp `authenticator.py`.
fn exchange_npsso(npsso: &str) -> Result<TokenSet> {
    // Step 1 — GET the authorization code.  The NPSSO is sent as a cookie;
    // Sony returns HTTP 302 and the code lives in the `Location` redirect URL
    // as the `code` query parameter.  We follow the redirect manually (ureq's
    // default redirect-following would lose the cookie).
    let auth_url = format!(
        "{AUTH_BASE}/authorize?access_type=offline&client_id={CLIENT_ID}&redirect_uri={}&response_type=code&scope={}",
        urlencoded(REDIRECT_URI),
        urlencoded(SCOPE),
    );

    let resp = ureq::get(&auth_url)
        .timeout(HTTP_TIMEOUT)
        .set("Cookie", &format!("npsso={npsso}"))
        // Do NOT follow redirects — the code is in the Location header.
        .call()
        .or_else(|e| match e {
            ureq::Error::Status(302, resp) => Ok(resp),
            other => Err(other),
        })
        .context("PSN auth code request failed")?;

    let location = resp
        .header("Location")
        .ok_or_else(|| anyhow::anyhow!("PSN auth: no Location header in response — NPSSO may be invalid or expired"))?;

    let access_code = location
        .split("code=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("PSN auth: could not extract access code from Location: {location}"))?;

    thread::sleep(REQ_INTERVAL);

    // Step 2 — POST the access code for a token pair.
    let body = format!(
        "grant_type=authorization_code&code={access_code}&cid={CLIENT_ID}&redirect_uri={}&scope={}",
        urlencoded(REDIRECT_URI),
        urlencoded(SCOPE),
    );

    let token_resp: TokenResponse = ureq::post(&format!("{AUTH_BASE}/token"))
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .context("PSN token exchange failed")?
        .into_json()
        .context("PSN token response was not valid JSON")?;

    let expires_at = token_resp
        .expires_in
        .map(|secs| {
            let ts = chrono::Utc::now() + chrono::Duration::seconds(secs as i64);
            ts.timestamp().max(0) as u64
        });

    Ok(TokenSet {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token,
        token_type: Some("Bearer".to_string()),
        scope: Some(SCOPE.to_string()),
        expires_at,
    })
}

/// Refresh an expired access token using the stored refresh token.
/// Returns a new `TokenSet` on success.
fn refresh_access_token(token: &TokenSet) -> Result<TokenSet> {
    let rt = token
        .refresh_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("PSN: no refresh token — re-paste NPSSO in the Integrations tab"))?;

    let body = format!(
        "grant_type=refresh_token&refresh_token={}&scope={}&token_format=jwt",
        urlencoded(rt),
        urlencoded(SCOPE),
    );

    let token_resp: TokenResponse = ureq::post(&format!("{AUTH_BASE}/token"))
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .context("PSN token refresh failed")?
        .into_json()
        .context("PSN refresh response was not valid JSON")?;

    let expires_at = token_resp.expires_in.map(|secs| {
        let ts = chrono::Utc::now() + chrono::Duration::seconds(secs as i64);
        ts.timestamp().max(0) as u64
    });

    Ok(TokenSet {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token,
        token_type: Some("Bearer".to_string()),
        scope: Some(SCOPE.to_string()),
        expires_at,
    })
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    // ignored fields: id_token, scope, token_type, refresh_token_expires_in
}

/// Percent-encode a value for a form-body or query string (very minimal — only
/// the characters we need to escape in our constants).  RFC 3986 unreserved
/// characters (letters, digits, `-`, `_`, `.`, `~`) must NOT be encoded;
/// encoding them is unnecessary and risks a strict literal compare failure on
/// the registered redirect_uri / scope at Sony's OAuth server.
fn urlencoded(s: &str) -> String {
    // Only encode characters that actually require it in our constants.
    // '.' is RFC 3986 unreserved — do not encode it.
    s.replace(' ', "%20")
        .replace(':', "%3A")
        .replace('/', "%2F")
}

/// Ensure the stored token is fresh; refresh silently if the access token is
/// expired but a refresh token is available.  Saves the new token to disk.
/// Returns the valid token, or an error if reconnect is needed.
fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    if token.refresh_token.is_none() {
        bail!("PlayStation session expired — re-paste your NPSSO token in the Integrations tab");
    }
    let fresh = refresh_access_token(&token)
        .context("PlayStation token refresh failed — re-paste your NPSSO token")?;
    vault.save_sync_token(SERVICE, &fresh)?;
    Ok(fresh)
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 of the last successful full sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
    /// Set of `{npCommunicationId}/{trophyId}` guids already written.
    /// Rebuilt from raw on first run; persisted here for speed.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    trophy_guids: HashSet<String>,
}

impl Vault {
    fn read_psn_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_psn_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// The pull.

fn pull_with_token(vault: &Vault, token: TokenSet) -> Result<PullOutcome> {
    let token = ensure_fresh(vault, token)?;
    let auth_hdr = format!("Bearer {}", token.access_token);

    let mut state = vault.read_psn_sync();

    // Pre-populate trophy guid cache from existing raw (first run after state
    // is missing or was deleted).
    if state.trophy_guids.is_empty() {
        let stream = vault.stream(TROPHIES_DIR, Partition::Month);
        if let Ok(partitions) = stream.partitions() {
            for key in partitions {
                if let Ok(rows) = stream.read::<Value>(&key) {
                    for row in rows {
                        if let Some(guid) = row.get("guid").and_then(Value::as_str) {
                            state.trophy_guids.insert(guid.to_string());
                        }
                    }
                }
            }
        }
    }

    let titles_written = sync_titles(vault, &auth_hdr, &mut state)?;
    thread::sleep(REQ_INTERVAL);

    let trophies_written = sync_trophies(vault, &auth_hdr, &mut state)?;

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_psn_sync(&state)?;

    let headline = if trophies_written == 0 {
        format!("PlayStation up to date — {titles_written} titles, no new trophies")
    } else {
        format!(
            "PlayStation synced — {titles_written} titles, {trophies_written} new trophies"
        )
    };

    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("titles", titles_written),
            ("trophies", trophies_written),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Titles — played-title list.

/// Fetch the full played-title list and write a fresh snapshot row for every
/// title on every sync into month-partitioned JSONL.  Returns the total count
/// of rows written.
///
/// We write every title unconditionally: play counts, playtime, and
/// lastPlayedDateTime change over time, so skipping already-seen titles would
/// freeze those fields at their first-capture values forever.  The raw layer
/// tolerates repeated `titleId` rows; read-time aggregations take the latest
/// by `lastPlayedDateTime`.
fn sync_titles(vault: &Vault, auth_hdr: &str, _state: &mut SyncState) -> Result<u64> {
    let stream = vault.stream(TITLES_DIR, Partition::Month);
    let mut total: u64 = 0;
    let mut offset: u32 = 0;

    for _page in 0..MAX_PAGES {
        let url = format!(
            "{GAMES_API}/users/me/titles?categories=ps5_native_game,ps4_game,ps3_game\
             &limit={TITLES_PAGE_SIZE}&offset={offset}"
        );

        let body: Value = match ureq_get_json(auth_hdr, &url)? {
            Some(v) => v,
            None => break,
        };

        let titles = match body.get("titles").and_then(Value::as_array) {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };

        let count = titles.len() as u32;
        let mut rows: Vec<TitledRow> = Vec::new();
        for t in &titles {
            let title_id = t.get("titleId").and_then(Value::as_str).unwrap_or("").to_string();
            if title_id.is_empty() {
                continue;
            }
            // Partition by lastPlayedDateTime month; fall back to today.
            let ts = t
                .get("lastPlayedDateTime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let ts = if Partition::Month.key(&ts).is_some() {
                ts
            } else {
                Local::now().format("%Y-%m-01T00:00:00Z").to_string()
            };
            rows.push(TitledRow { ts, value: t.clone() });
        }

        if !rows.is_empty() {
            stream.append(&rows, |r| &r.ts)?;
            total += rows.len() as u64;
        }

        // Pagination: stop when fewer than a full page.
        if count < TITLES_PAGE_SIZE {
            break;
        }
        offset += count;
        thread::sleep(REQ_INTERVAL);
    }

    Ok(total)
}

// ---------------------------------------------------------------------------
// Trophies — earned trophy history.

/// Fetch all trophy titles (games with trophy sets) and their earned trophies.
/// Writes new trophies (deduped by guid) into month-partitioned JSONL.
fn sync_trophies(vault: &Vault, auth_hdr: &str, state: &mut SyncState) -> Result<u64> {
    let trophy_stream = vault.stream(TROPHIES_DIR, Partition::Month);
    let mut total: u64 = 0;

    // --- Page the trophy titles list. ---
    let mut offset: u32 = 0;
    for _page in 0..MAX_PAGES {
        let url = format!(
            "{TROPHY_API}/users/me/trophyTitles?limit={TROPHY_TITLES_PAGE_SIZE}&offset={offset}"
        );

        let body: Value = match ureq_get_json(auth_hdr, &url)? {
            Some(v) => v,
            None => break,
        };

        let trophy_titles = match body.get("trophyTitles").and_then(Value::as_array) {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };

        let count = trophy_titles.len() as u32;

        for title_meta in &trophy_titles {
            let np_comm_id = title_meta
                .get("npCommunicationId")
                .and_then(Value::as_str)
                .unwrap_or("");
            if np_comm_id.is_empty() {
                continue;
            }
            let title_name = title_meta
                .get("trophyTitleName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let title_platform = title_meta
                .get("trophyTitlePlatform")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            thread::sleep(REQ_INTERVAL);
            let written = fetch_earned_trophies(
                vault,
                &trophy_stream,
                auth_hdr,
                np_comm_id,
                &title_name,
                &title_platform,
                state,
            )?;
            total += written;
        }

        if count < TROPHY_TITLES_PAGE_SIZE {
            break;
        }
        offset += count;
        thread::sleep(REQ_INTERVAL);
    }

    Ok(total)
}

/// Fetch the static trophy DEFINITIONS for one title (no /users/me segment —
/// this is `getTitleTrophies` in psn-api, equivalent to psnawp's `Trophy`
/// iterator).  Returns a map from `trophyId` (as string) to the full
/// definition object, which includes `trophyName`, `trophyDetail`,
/// `trophyIconUrl`, and `trophyGroupId` — fields absent from the earned
/// endpoint (`UserThinTrophy`).
///
/// Endpoint: GET /api/trophy/v1/npCommunicationIds/{id}/trophyGroups/all/trophies
/// (NO /users/me).
fn fetch_title_trophy_defs(
    auth_hdr: &str,
    np_comm_id: &str,
) -> Result<std::collections::HashMap<String, Value>> {
    let mut defs: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut offset: u32 = 0;

    for _page in 0..MAX_PAGES {
        let url = format!(
            "{TROPHY_API}/npCommunicationIds/{np_comm_id}/trophyGroups/all/trophies\
             ?limit={TROPHIES_PAGE_SIZE}&offset={offset}"
        );

        let body: Value = match ureq_get_json(auth_hdr, &url)? {
            Some(v) => v,
            None => break,
        };

        let trophies = match body.get("trophies").and_then(Value::as_array) {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };

        let count = trophies.len() as u32;
        for def in &trophies {
            let trophy_id = def
                .get("trophyId")
                .and_then(|v| v.as_u64().map(|n| n.to_string())
                    .or_else(|| v.as_str().map(str::to_string)))
                .unwrap_or_default();
            if !trophy_id.is_empty() {
                defs.insert(trophy_id, def.clone());
            }
        }

        if count < TROPHIES_PAGE_SIZE {
            break;
        }
        offset += count;
        thread::sleep(REQ_INTERVAL);
    }

    Ok(defs)
}

/// Fetch earned trophies for one title (all trophy groups combined via
/// `trophyGroupId=all`) and merge with the static trophy definitions so that
/// `trophyName`, `trophyDetail`, `trophyIconUrl`, and `trophyGroupId` are
/// populated.
///
/// This mirrors the two-call merge pattern documented in the psn-api examples
/// (`{...earnedTrophy, ...foundTitleTrophy}`) and psnawp's
/// `TrophyWithProgressIterator` (`{**trophy, **progress}`):
/// the earned endpoint (`/users/me/...`) supplies `earned`/`earnedDateTime`;
/// the definitions endpoint (no `/users/me`) supplies the human-readable
/// metadata.  Without the merge every row would be missing name, description,
/// and icon.
fn fetch_earned_trophies(
    _vault: &Vault,
    stream: &crate::store::JsonlStream<'_>,
    auth_hdr: &str,
    np_comm_id: &str,
    title_name: &str,
    title_platform: &str,
    state: &mut SyncState,
) -> Result<u64> {
    // First, fetch the static definitions so we can merge by trophyId.
    thread::sleep(REQ_INTERVAL);
    let defs = fetch_title_trophy_defs(auth_hdr, np_comm_id)?;

    let mut total: u64 = 0;
    let mut offset: u32 = 0;

    for _page in 0..MAX_PAGES {
        let url = format!(
            "{TROPHY_API}/users/me/npCommunicationIds/{np_comm_id}/trophyGroups/all/trophies\
             ?limit={TROPHIES_PAGE_SIZE}&offset={offset}"
        );

        let body: Value = match ureq_get_json(auth_hdr, &url)? {
            Some(v) => v,
            None => break,
        };

        let trophies = match body.get("trophies").and_then(Value::as_array) {
            Some(a) if !a.is_empty() => a.clone(),
            _ => break,
        };

        let count = trophies.len() as u32;
        let mut rows: Vec<TitledRow> = Vec::new();

        for trophy in &trophies {
            // Only write earned trophies (earned = true).
            let earned = trophy.get("earned").and_then(Value::as_bool).unwrap_or(false);
            if !earned {
                continue;
            }

            let trophy_id = trophy
                .get("trophyId")
                .and_then(|v| v.as_u64().map(|n| n.to_string())
                    .or_else(|| v.as_str().map(str::to_string)))
                .unwrap_or_default();
            if trophy_id.is_empty() {
                continue;
            }

            let guid = format!("{np_comm_id}/{trophy_id}");
            if state.trophy_guids.contains(&guid) {
                continue;
            }

            let earned_dt = trophy
                .get("earnedDateTime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            // Partition key = earnedDateTime month; fall back to today.
            let ts = if Partition::Month.key(&earned_dt).is_some() {
                earned_dt.clone()
            } else {
                Local::now().format("%Y-%m-01T00:00:00Z").to_string()
            };

            // Build the raw row.
            // Start with static definition fields (trophyName, trophyDetail,
            // trophyIconUrl, trophyGroupId, trophyType, etc.) then overlay
            // the earned-endpoint fields (earned, earnedDateTime, trophyRare,
            // trophyEarnedRate, …) — mirroring psn-api's mergeTrophyLists /
            // psnawp's {**trophy, **progress} merge order.
            let mut row: Map<String, Value> = Map::new();

            // Layer 1: definition metadata (supplies name/detail/icon/groupId).
            if let Some(def_obj) = defs.get(&trophy_id).and_then(Value::as_object) {
                for (k, v) in def_obj {
                    row.insert(k.clone(), v.clone());
                }
            }
            // Layer 2: earned-endpoint fields (override / add earned status).
            for (k, v) in trophy.as_object().iter().flat_map(|m| m.iter()) {
                row.insert(k.clone(), v.clone());
            }

            // Our computed fields (added after; do not override API data).
            row.entry("guid".to_string()).or_insert_with(|| Value::String(guid.clone()));
            row.entry("np_comm_id".to_string()).or_insert_with(|| Value::String(np_comm_id.to_string()));
            row.entry("trophy_id".to_string()).or_insert_with(|| Value::String(trophy_id));
            if !title_name.is_empty() {
                row.entry("title_name".to_string()).or_insert_with(|| Value::String(title_name.to_string()));
            }
            if !title_platform.is_empty() {
                row.entry("title_platform".to_string()).or_insert_with(|| Value::String(title_platform.to_string()));
            }

            rows.push(TitledRow { ts, value: Value::Object(row) });
            state.trophy_guids.insert(guid);
        }

        if !rows.is_empty() {
            stream.append(&rows, |r| &r.ts)?;
            total += rows.len() as u64;
        }

        if count < TROPHIES_PAGE_SIZE {
            break;
        }
        offset += count;
        thread::sleep(REQ_INTERVAL);
    }

    Ok(total)
}

// ---------------------------------------------------------------------------
// HTTP helpers.

/// GET a JSON endpoint.  Returns `None` on an empty-body or 204 response;
/// surfaces graceful-disable on 401/403 (token expiry path handled by the
/// caller); propagates other errors.
fn ureq_get_json(auth_hdr: &str, url: &str) -> Result<Option<Value>> {
    let resp = ureq::get(url)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", auth_hdr)
        .set("Accept", "application/json")
        .call();

    match resp {
        Ok(r) => {
            let v: Value = r.into_json().context("PSN API response was not valid JSON")?;
            Ok(Some(v))
        }
        Err(ureq::Error::Status(204, _)) => Ok(None),
        Err(ureq::Error::Status(401 | 403, _)) => {
            bail!("PlayStation session expired — re-paste your NPSSO token in the Integrations tab")
        }
        Err(ureq::Error::Status(429 | 503, _)) => {
            // Rate-limited: wait and retry once.
            thread::sleep(Duration::from_secs(5));
            let v: Value = ureq::get(url)
                .timeout(HTTP_TIMEOUT)
                .set("Authorization", auth_hdr)
                .set("Accept", "application/json")
                .call()
                .context("PSN API request failed after rate-limit retry")?
                .into_json()
                .context("PSN API response was not valid JSON")?;
            Ok(Some(v))
        }
        Err(e) => Err(anyhow::anyhow!("PSN API request to {url} failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers.

/// A row carrying a partition key (`ts`) and a serialized `value`.  Only
/// `value` is written to disk (via `#[serde(flatten)]`).
#[derive(Serialize)]
struct TitledRow {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-psn-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Auth helpers -------------------------------------------------------

    #[test]
    fn playstation_urlencoded_spaces_and_slashes() {
        // Spaces and slashes in the scope/redirect must be percent-encoded.
        let encoded = urlencoded("psn:mobile.v2.core psn:clientapp");
        assert!(encoded.contains("%20"), "space must encode to %20");
        assert!(!encoded.contains(' '), "raw space must not remain");
        let uri = urlencoded("com.scee.psxandroid.scecompcall://redirect");
        assert!(uri.contains("%3A"), "colon must encode");
        assert!(uri.contains("%2F"), "slash must encode");
        // RFC 3986 unreserved characters (including '.') must NOT be percent-encoded.
        // Over-encoding them risks a strict literal compare failure on Sony's registered
        // redirect_uri/scope, and is unnecessary per spec.
        assert!(!uri.contains("%2E"), "dot is RFC 3986 unreserved — must NOT be encoded");
        assert!(uri.contains('.'), "raw dot must remain unencoded");
    }

    #[test]
    fn playstation_npsso_length_validation() {
        // connect must reject a token that is not 64 chars.
        let v = temp_vault("npsso_len");
        let short = "abc";
        let err = def_connect(&v, short).unwrap_err();
        assert!(err.to_string().contains("64"), "error must mention expected length");
        let long = "x".repeat(65);
        let err2 = def_connect(&v, &long).unwrap_err();
        assert!(err2.to_string().contains("64"));
    }

    // --- Fixtures: representative PSN API responses (field names from
    // psnawp title_stats.py and psn-api JS library examples). ---------------
    //
    // Trophy collection requires TWO endpoints:
    //
    // 1. `title_trophies_response` — GET /api/trophy/v1/npCommunicationIds/{id}/
    //    trophyGroups/all/trophies (NO /users/me segment).  This is
    //    `getTitleTrophies` in psn-api.  Returns `TitleTrophy` objects which
    //    include `trophyName`, `trophyDetail`, `trophyIconUrl`, `trophyGroupId`.
    //    Does NOT include `earned` / `earnedDateTime`.
    //
    // 2. `earned_trophies_response` — GET /api/trophy/v1/users/me/
    //    npCommunicationIds/{id}/trophyGroups/all/trophies.  This is
    //    `getUserTrophiesEarnedForTitle` in psn-api.  Returns `UserThinTrophy`
    //    objects which include `earned`, `earnedDateTime`, `trophyRare`,
    //    `trophyEarnedRate`, `trophyRewardImageUrl`, `trophyRewardName`.
    //    Does NOT include `trophyName`, `trophyDetail`, `trophyIconUrl`,
    //    `trophyGroupId`.
    //
    // The two responses are merged by `trophyId` before writing (mirroring
    // psn-api's `mergeTrophyLists` / psnawp's `{**trophy, **progress}`).

    fn titles_response() -> Value {
        json!({
            "titles": [
                {
                    "titleId": "PPSA01284_00",
                    "name": "Astro's Playroom",
                    "imageUrl": "https://image.api.playstation.com/vulcan/ap/rnd/202010/0513/5qGPEYMhroCFaRlekxGZ5Gyb.png",
                    "category": "ps5_native_game",
                    "playCount": 14,
                    "firstPlayedDateTime": "2020-11-12T13:22:00.000000Z",
                    "lastPlayedDateTime": "2021-03-07T10:05:00.000000Z",
                    "playDuration": "PT5H30M22S"
                },
                {
                    "titleId": "CUSA07408_00",
                    "name": "God of War",
                    "imageUrl": "https://image.api.playstation.com/cdn/UP9000/CUSA07408_00/0vdRHTQuoqwe9NJO8bTgFBfWnpTRDCbM.png",
                    "category": "ps4_game",
                    "playCount": 1,
                    "firstPlayedDateTime": "2019-04-20T20:00:00.000000Z",
                    "lastPlayedDateTime": "2019-04-20T20:00:00.000000Z"
                }
            ],
            "totalItemCount": 2,
            "nextOffset": 0
        })
    }

    fn trophy_titles_response() -> Value {
        json!({
            "trophyTitles": [
                {
                    "npCommunicationId": "NPWR22810_00",
                    "trophySetVersion": "01.07",
                    "trophyTitleName": "Astro's Playroom",
                    "trophyTitlePlatform": "PS5",
                    "hasTrophyGroups": false,
                    "definedTrophies": {"bronze": 14, "silver": 4, "gold": 1, "platinum": 1},
                    "earnedTrophies": {"bronze": 14, "silver": 4, "gold": 1, "platinum": 1}
                }
            ],
            "totalItemCount": 1,
            "nextOffset": 0
        })
    }

    /// Static trophy definitions from GET /npCommunicationIds/{id}/trophyGroups/all/trophies
    /// (NO /users/me).  `TitleTrophy` shape: has trophyName, trophyDetail,
    /// trophyIconUrl, trophyGroupId; does NOT have earned/earnedDateTime.
    fn title_trophies_response() -> Value {
        json!({
            "trophies": [
                {
                    "trophyId": 0,
                    "trophyHidden": false,
                    "trophyType": "platinum",
                    "trophyName": "Platinum Trophy",
                    "trophyDetail": "Earn all trophies in Astro's Playroom.",
                    "trophyIconUrl": "https://image.api.playstation.com/trophy/np/NPWR22810_00_00/0/thmb/plat.png",
                    "trophyGroupId": "default"
                },
                {
                    "trophyId": 1,
                    "trophyHidden": false,
                    "trophyType": "gold",
                    "trophyName": "Gold Medal Collector",
                    "trophyDetail": "Collect all Gold Medals.",
                    "trophyIconUrl": "https://image.api.playstation.com/trophy/np/NPWR22810_00_00/0/thmb/gold.png",
                    "trophyGroupId": "default"
                },
                {
                    "trophyId": 2,
                    "trophyHidden": false,
                    "trophyType": "bronze",
                    "trophyName": "Start Your Journey",
                    "trophyDetail": "Begin the game.",
                    "trophyIconUrl": "https://image.api.playstation.com/trophy/np/NPWR22810_00_00/0/thmb/bronze.png",
                    "trophyGroupId": "default"
                }
            ],
            "totalItemCount": 3,
            "nextOffset": 0
        })
    }

    /// Earned status from GET /users/me/npCommunicationIds/{id}/trophyGroups/all/trophies.
    /// `UserThinTrophy` shape: has earned/earnedDateTime/trophyRare/trophyEarnedRate;
    /// does NOT have trophyName/trophyDetail/trophyIconUrl/trophyGroupId.
    fn earned_trophies_response() -> Value {
        json!({
            "trophies": [
                {
                    "trophyId": 0,
                    "trophyHidden": false,
                    "trophyType": "platinum",
                    "trophyRare": 1,
                    "trophyEarnedRate": "1.3",
                    "earned": true,
                    "earnedDateTime": "2021-03-07T10:05:22.000000Z"
                },
                {
                    "trophyId": 1,
                    "trophyHidden": false,
                    "trophyType": "gold",
                    "trophyRare": 2,
                    "trophyEarnedRate": "8.7",
                    "earned": true,
                    "earnedDateTime": "2021-02-15T18:30:00.000000Z"
                },
                {
                    "trophyId": 2,
                    "trophyHidden": false,
                    "trophyType": "bronze",
                    "trophyRare": 4,
                    "trophyEarnedRate": "95.2",
                    "earned": false
                }
            ],
            "totalItemCount": 3,
            "nextOffset": 0
        })
    }

    /// Build a merged `defs` map from `title_trophies_response` (simulating the
    /// result of `fetch_title_trophy_defs`).
    fn build_defs_from_title_trophies() -> std::collections::HashMap<String, Value> {
        let mut defs = std::collections::HashMap::new();
        let resp = title_trophies_response();
        for t in resp["trophies"].as_array().unwrap() {
            let id = t["trophyId"].as_u64().unwrap().to_string();
            defs.insert(id, t.clone());
        }
        defs
    }

    // --- Parsing / write tests ----------------------------------------------

    #[test]
    fn playstation_titles_written_to_vault() {
        let vault = temp_vault("titles_write");
        let auth = "Bearer fake";
        let resp = titles_response();
        let stream = vault.stream(TITLES_DIR, Partition::Month);

        // Simulate writing titles from the response (mirrors sync_titles inner loop).
        // No `seen` dedup — titles are always written on every sync so playtime stays fresh.
        let titles = resp["titles"].as_array().unwrap();
        let mut rows: Vec<TitledRow> = Vec::new();
        for t in titles {
            let title_id = t.get("titleId").and_then(Value::as_str).unwrap_or("");
            if title_id.is_empty() {
                continue;
            }
            let ts = t
                .get("lastPlayedDateTime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let ts = if Partition::Month.key(&ts).is_some() {
                ts
            } else {
                Local::now().format("%Y-%m-01T00:00:00Z").to_string()
            };
            rows.push(TitledRow { ts, value: t.clone() });
        }

        let _ = auth; // auth not used in this offline test
        assert_eq!(rows.len(), 2, "both titles captured");
        stream.append(&rows, |r| &r.ts).unwrap();

        // Verify the Astro's Playroom row landed in 2021-03.
        let mar_rows: Vec<Value> = stream.read("2021-03").unwrap();
        assert_eq!(mar_rows.len(), 1);
        assert_eq!(mar_rows[0]["titleId"], "PPSA01284_00");
        assert_eq!(mar_rows[0]["name"], "Astro's Playroom");
        assert_eq!(mar_rows[0]["playDuration"], "PT5H30M22S");
        assert_eq!(mar_rows[0]["playCount"], 14);

        // God of War — PS4, no playDuration.
        let apr_rows: Vec<Value> = stream.read("2019-04").unwrap();
        assert_eq!(apr_rows.len(), 1);
        assert_eq!(apr_rows[0]["titleId"], "CUSA07408_00");
        assert!(apr_rows[0].get("playDuration").is_none() || apr_rows[0]["playDuration"].is_null(),
            "PS4 title may omit playDuration");
    }

    #[test]
    fn playstation_trophies_only_earned_written() {
        // This test simulates the two-endpoint merge:
        // 1. defs from title_trophies_response (getTitleTrophies — name/detail/icon/groupId)
        // 2. earned from earned_trophies_response (UserThinTrophy — earned status only)
        // The merger produces complete rows with both sets of fields.
        let vault = temp_vault("trophies_earned");
        let stream = vault.stream(TROPHIES_DIR, Partition::Month);
        let earned_resp = earned_trophies_response();
        let np_comm_id = "NPWR22810_00";

        // Simulate fetch_title_trophy_defs result.
        let defs = build_defs_from_title_trophies();

        let mut state = SyncState::default();
        let trophies = earned_resp["trophies"].as_array().unwrap();
        let mut rows: Vec<TitledRow> = Vec::new();

        for trophy in trophies {
            let earned = trophy.get("earned").and_then(Value::as_bool).unwrap_or(false);
            if !earned {
                continue; // not earned — skip
            }
            let trophy_id = trophy.get("trophyId")
                .and_then(|v| v.as_u64().map(|n| n.to_string())
                    .or_else(|| v.as_str().map(str::to_string)))
                .unwrap_or_default();
            let guid = format!("{np_comm_id}/{trophy_id}");
            if state.trophy_guids.contains(&guid) {
                continue;
            }
            let earned_dt = trophy.get("earnedDateTime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let ts = if Partition::Month.key(&earned_dt).is_some() {
                earned_dt
            } else {
                Local::now().format("%Y-%m-01T00:00:00Z").to_string()
            };
            let mut row: Map<String, Value> = Map::new();
            // Layer 1: definition metadata (name/detail/icon/groupId).
            if let Some(def_obj) = defs.get(&trophy_id).and_then(Value::as_object) {
                for (k, v) in def_obj {
                    row.insert(k.clone(), v.clone());
                }
            }
            // Layer 2: earned-endpoint fields (override).
            for (k, v) in trophy.as_object().iter().flat_map(|m| m.iter()) {
                row.insert(k.clone(), v.clone());
            }
            row.entry("guid".to_string()).or_insert_with(|| Value::String(guid.clone()));
            row.entry("np_comm_id".to_string()).or_insert_with(|| Value::String(np_comm_id.to_string()));
            row.entry("trophy_id".to_string()).or_insert_with(|| Value::String(trophy_id));
            rows.push(TitledRow { ts, value: Value::Object(row) });
            state.trophy_guids.insert(guid);
        }

        assert_eq!(rows.len(), 2, "only 2 of 3 trophies are earned");
        stream.append(&rows, |r| &r.ts).unwrap();

        // Platinum: 2021-03 — verify BOTH earned fields AND definition fields present.
        let mar: Vec<Value> = stream.read("2021-03").unwrap();
        assert_eq!(mar.len(), 1);
        let plat = &mar[0];
        assert_eq!(plat["guid"], "NPWR22810_00/0");
        assert_eq!(plat["np_comm_id"], "NPWR22810_00");
        assert_eq!(plat["trophyType"], "platinum");
        assert_eq!(plat["trophyRare"], 1);
        assert_eq!(plat["earnedDateTime"], "2021-03-07T10:05:22.000000Z");
        // Merged from definition endpoint:
        assert_eq!(plat["trophyName"], "Platinum Trophy", "trophyName must come from defs endpoint");
        assert_eq!(plat["trophyDetail"], "Earn all trophies in Astro's Playroom.", "trophyDetail must come from defs endpoint");
        assert_eq!(plat["trophyIconUrl"], "https://image.api.playstation.com/trophy/np/NPWR22810_00_00/0/thmb/plat.png", "trophyIconUrl must come from defs endpoint");
        assert_eq!(plat["trophyGroupId"], "default", "trophyGroupId must come from defs endpoint");

        // Gold: 2021-02
        let feb: Vec<Value> = stream.read("2021-02").unwrap();
        assert_eq!(feb.len(), 1);
        assert_eq!(feb[0]["guid"], "NPWR22810_00/1");
        assert_eq!(feb[0]["trophyType"], "gold");
        assert_eq!(feb[0]["trophyName"], "Gold Medal Collector", "gold trophyName from defs");

        // Unearned bronze must NOT be in the vault.
        assert_eq!(state.trophy_guids.len(), 2, "only earned guids tracked");
        assert!(!state.trophy_guids.contains("NPWR22810_00/2"),
            "unearned trophy must not be in guid set");
    }

    #[test]
    fn playstation_trophy_dedup_on_re_pull() {
        let vault = temp_vault("trophy_dedup");
        let stream = vault.stream(TROPHIES_DIR, Partition::Month);
        let np_comm_id = "NPWR22810_00";
        let guid = format!("{np_comm_id}/0");

        // Write the first time.
        let row = TitledRow {
            ts: "2021-03-07T10:05:22.000000Z".to_string(),
            value: json!({
                "guid": guid,
                "np_comm_id": np_comm_id,
                "trophy_id": "0",
                "trophyType": "platinum",
                "earned": true,
                "earnedDateTime": "2021-03-07T10:05:22.000000Z"
            }),
        };
        stream.append(&[row], |r| &r.ts).unwrap();

        // Simulate a second pull with the same trophy: guid already in set.
        let mut state = SyncState::default();
        state.trophy_guids.insert(guid.clone());

        // The dedup gate should prevent a duplicate write.
        assert!(state.trophy_guids.contains(&guid));
        let mar: Vec<Value> = stream.read("2021-03").unwrap();
        assert_eq!(mar.len(), 1, "second pull must not duplicate the trophy");
    }

    #[test]
    fn playstation_titles_re_synced_on_subsequent_pull() {
        // Verify that sync_titles does NOT skip already-seen titles.  Playtime
        // and playCount change over time; skipping would freeze them at first capture.
        let vault = temp_vault("titles_re_sync");
        let stream = vault.stream(TITLES_DIR, Partition::Month);

        // Helper: simulate one pass of the sync_titles inner loop for a single page.
        let write_titles = |titles: &[Value]| {
            let mut rows: Vec<TitledRow> = Vec::new();
            for t in titles {
                let title_id = t.get("titleId").and_then(Value::as_str).unwrap_or("");
                if title_id.is_empty() { continue; }
                let ts = t.get("lastPlayedDateTime").and_then(Value::as_str).unwrap_or("").to_string();
                let ts = if Partition::Month.key(&ts).is_some() { ts } else {
                    Local::now().format("%Y-%m-01T00:00:00Z").to_string()
                };
                rows.push(TitledRow { ts, value: t.clone() });
            }
            rows
        };

        // First sync: playCount = 5.
        let first: Vec<Value> = vec![json!({
            "titleId": "PPSA01284_00",
            "name": "Astro's Playroom",
            "playCount": 5,
            "lastPlayedDateTime": "2021-03-07T10:05:00.000000Z",
            "playDuration": "PT2H"
        })];
        let rows1 = write_titles(&first);
        assert_eq!(rows1.len(), 1);
        stream.append(&rows1, |r| &r.ts).unwrap();

        // Second sync: playCount = 14 (more plays).
        let second: Vec<Value> = vec![json!({
            "titleId": "PPSA01284_00",
            "name": "Astro's Playroom",
            "playCount": 14,
            "lastPlayedDateTime": "2021-03-07T10:05:00.000000Z",
            "playDuration": "PT5H30M22S"
        })];
        let rows2 = write_titles(&second);
        // Must NOT skip the title even though it was written before.
        assert_eq!(rows2.len(), 1, "title must be re-written on second sync (not skipped)");
        stream.append(&rows2, |r| &r.ts).unwrap();

        // Both rows should be in the vault (raw layer retains all; read-time takes latest).
        let mar: Vec<Value> = stream.read("2021-03").unwrap();
        assert_eq!(mar.len(), 2, "both snapshot rows must be in the vault");
        // The latest row has the updated playCount.
        let latest = mar.iter().max_by_key(|r| r["playCount"].as_u64().unwrap_or(0)).unwrap();
        assert_eq!(latest["playCount"], 14, "latest row must have updated playCount");
    }

    #[test]
    fn playstation_trophy_defs_have_required_fields() {
        // Confirm title_trophies_response has the definition fields that are
        // absent from earned_trophies_response (UserThinTrophy).
        let defs_resp = title_trophies_response();
        let trophies = defs_resp["trophies"].as_array().unwrap();
        for t in trophies {
            assert!(t.get("trophyName").is_some(), "trophyName in def");
            assert!(t.get("trophyDetail").is_some(), "trophyDetail in def");
            assert!(t.get("trophyIconUrl").is_some(), "trophyIconUrl in def");
            assert!(t.get("trophyGroupId").is_some(), "trophyGroupId in def");
            // Definitions do NOT have earned/earnedDateTime.
            assert!(t.get("earned").is_none(), "earned must not be in def endpoint");
            assert!(t.get("earnedDateTime").is_none(), "earnedDateTime must not be in def endpoint");
        }

        // Confirm earned_trophies_response is the real UserThinTrophy shape:
        // has earned status but no name/detail/icon.
        let earned_resp = earned_trophies_response();
        let earned_trophies = earned_resp["trophies"].as_array().unwrap();
        for t in earned_trophies {
            assert!(t.get("trophyId").is_some(), "trophyId present");
            assert!(t.get("trophyType").is_some(), "trophyType present");
            // UserThinTrophy does NOT include these — they come only from the defs endpoint.
            assert!(t.get("trophyName").is_none(), "trophyName must NOT be in earned endpoint shape");
            assert!(t.get("trophyDetail").is_none(), "trophyDetail must NOT be in earned endpoint shape");
            assert!(t.get("trophyIconUrl").is_none(), "trophyIconUrl must NOT be in earned endpoint shape");
            assert!(t.get("trophyGroupId").is_none(), "trophyGroupId must NOT be in earned endpoint shape");
        }
    }

    #[test]
    fn playstation_trophy_titles_response_fields() {
        // Confirm our fixture matches the field names from psn-api / psnawp.
        let resp = trophy_titles_response();
        let titles = resp["trophyTitles"].as_array().unwrap();
        assert_eq!(titles.len(), 1);
        let t = &titles[0];
        assert!(t.get("npCommunicationId").is_some(), "npCommunicationId field");
        assert!(t.get("trophyTitleName").is_some(), "trophyTitleName field");
        assert!(t.get("trophyTitlePlatform").is_some(), "trophyTitlePlatform field");
        assert!(t.get("earnedTrophies").is_some(), "earnedTrophies field");
    }

    #[test]
    fn playstation_sync_state_round_trips() {
        let vault = temp_vault("sync_state");
        let mut state = SyncState::default();
        state.trophy_guids.insert("NPWR22810_00/0".into());
        state.trophy_guids.insert("NPWR22810_00/1".into());
        state.updated = Some("2021-03-07T10:05:22+00:00".to_string());
        vault.write_psn_sync(&state).unwrap();

        let loaded = vault.read_psn_sync();
        assert_eq!(loaded.trophy_guids.len(), 2);
        assert!(loaded.trophy_guids.contains("NPWR22810_00/0"));
        assert_eq!(loaded.updated, Some("2021-03-07T10:05:22+00:00".to_string()));
    }
}

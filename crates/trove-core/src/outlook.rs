//! Microsoft Outlook / Microsoft 365 email via the Microsoft Graph API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/outlook.md.
//!
//! **"gmail.rs over Microsoft Graph."** A [`Behavior::Periodic`] cloud pull
//! of Microsoft email (Outlook.com / Hotmail / Microsoft 365) for every
//! connected Microsoft account. The OAuth side (a PUBLIC client + PKCE, the
//! per-account token store, refresh, reconnect flagging) lives in THIS module
//! because Microsoft personal accounts use a public client — there is no
//! client secret to bake — so the [`crate::sync::oauth`] machinery's
//! `default_client_secret: None` path applies (token POST carries client_id +
//! PKCE `code_verifier`, no secret). One [`CONNECTION`] ("microsoft") is
//! shared: future Teams / To Do / OneDrive defs will ride the same login.
//!
//! **Target.** Messages land in the unified correspondence stream
//! (`correspondence/email/YYYY-MM.jsonl`, see [`crate::correspondence`]) —
//! the *same* stream the `.mbox` importer ([`crate::email`]), the Gmail puller
//! ([`crate::gmail`]) and the IMAP puller ([`crate::imap`]) write — sharing
//! their Message-ID dedupe. So the SAME mailbox reached via Outlook + IMAP +
//! mbox never duplicates a message. This is a deliberate deviation from the
//! brief's stale `correspondence/outlook/` path: source is "email" (what
//! [`crate::email::email_to_message`] produces), guid is the Message-ID, and
//! `service` is the connected account address, so N accounts coexist.
//!
//! **Conversion** reuses the importer's `email_to_message`: Graph's
//! `/messages/{id}/$value` returns the verbatim RFC822 MIME, so the exact
//! same parse (text body preferred, attachment *metadata* only — the lean
//! default) applies. On top we layer the message's Outlook folder display
//! name as its single `label`.
//!
//! **Incremental via Graph delta**, per account, resumable (the gmail
//! reasoning): `GET /me/messages/delta` (no token on the first run) drains
//! `@odata.nextLink` pages, and the final `@odata.deltaLink` is persisted per
//! account in `.trove/outlook-sync.json` (a NON-secret cursor, beside the
//! vault's other `.trove/` indexes). The next pass resumes from that
//! deltaLink, so only changed/new messages come back. The cursor advances per
//! account ONLY after that account's batch is written (drain-don't-strand).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::correspondence::Message;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::write_json_atomic;
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

/// The unified email stream — shared with mbox import, Gmail, and IMAP (same
/// folder, same Message-ID dedupe).
const SOURCE: &str = "email";
/// Non-secret rebuildable cursor — *not* under `.trove/sync/` (that's for 0600
/// secrets); deleting it re-walks every account's full delta next sync.
const SYNC_FILE: &str = ".trove/outlook-sync.json";

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
/// Kept short so a hung connection can't stall the watcher owner loop (the
/// gmail/oura reasoning).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs in the watcher loop. Mail is time-sensitive enough to
/// want sub-hourly freshness, cheap enough (one delta call when nothing
/// changed) to afford it — the gmail/imap cadence.
pub const OUTLOOK_SYNC_SECS: u64 = 900;
/// `$select` for the delta call: just the fields we need to fetch + label each
/// message (the raw MIME comes from a second `/$value` call). Keeps the delta
/// pages light.
const DELTA_SELECT: &str = "id,internetMessageId,parentFolderId";

// ---------------------------------------------------------------------------
// Provider + connection.

/// The Microsoft OAuth provider — a PUBLIC client (no secret). `use_pkce` on
/// + `basic_auth` off + `default_client_secret: None` is exactly the public
/// client shape the shared [`oauth`] machinery handles: the token POST sends
/// client_id + the PKCE `code_verifier` and NO secret, which is what Microsoft
/// personal-account public clients require. `offline_access` → a refresh
/// token; `User.Read` → `/me` for the account identity. Redirect port 38577
/// (38573–38576 are taken by other connections).
///
/// `default_client_id` is BYO via the `TROVE_MICROSOFT_CLIENT_ID` env var at
/// build time (the ConnectSpec pattern); the baked default is empty, so a
/// build without it shows the bring-your-own-credentials form first.
pub static MICROSOFT: Provider = Provider {
    service: "microsoft",
    display_name: "Microsoft",
    auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    scopes: "offline_access Mail.Read Calendars.Read Notes.Read User.Read",
    redirect_port: 38577,
    use_pkce: true,
    basic_auth: false,
    // PUBLIC client: a client id (BYO via env), never a secret.
    default_client_id: option_env!("TROVE_MICROSOFT_CLIENT_ID"),
    default_client_secret: None,
    // select_account → let the user pick which account to add (multi-account,
    // the google reasoning). offline_access in `scopes` already requests the
    // refresh token, so no access_type param is needed (that's Google-only).
    extra_auth_params: &[("prompt", "select_account")],
};

/// Registered in [`crate::integrations::CONNECTIONS`]. Multi-account: each
/// connect run *adds* an account (keyed by the Microsoft `oid`/`sub`), and
/// re-running for an already-connected account is exactly "reconnect" — the
/// new token overwrites by key. Outlook auto-pulls right after a successful
/// connect (the backfill starts without a second click; the frontend still
/// consults the toggle). Shared login — future Teams / To Do / OneDrive defs
/// will reference this same connection.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "microsoft",
    display_name: "Microsoft",
    methods: &[ConnectMethod::OAuth {
        provider: &MICROSOFT,
        multi_account: true,
        run: connect_oauth,
    }],
    status: connect_status,
    disconnect: disconnect,
    auto_pull: &["outlook", "outlook-calendar", "onenote"],
    setup: &[
        "At entra.microsoft.com (Azure) → App registrations → New registration. Supported account types: \"Accounts in any organizational directory and personal Microsoft accounts\".",
        "Add a Redirect URI of platform type \"Mobile and desktop applications\": http://localhost:38577/callback — must match exactly.",
        "API permissions → add Microsoft Graph delegated permissions Mail.Read, Calendars.Read, Notes.Read, User.Read, and offline_access.",
        "Copy the Application (client) ID and paste it here. No client secret is needed — this is a public client. The ID is saved, so every future connect is just a login.",
    ],
};

/// [`ConnectMethod::OAuth`] adapter: forward to [`connect`] (which owns the
/// explicit → saved → compiled-in credential resolution) and drop the returned
/// account info — callers re-read state through the status hook.
fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

/// Map the stored accounts into the generic shape: one [`ConnectedAccount`]
/// per stored account, keyed by the Microsoft id (the disconnect key) and
/// labeled by email. `needs_reconnect` passes through — flagged accounts are
/// kept and surfaced, never silently dropped.
fn connect_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured =
        vault.load_sync_app(MICROSOFT.service)?.is_some() || MICROSOFT.default_credentials().is_some();
    let accounts = vault
        .outlook_accounts_raw()?
        .iter()
        .map(|a| {
            let mut extra = BTreeMap::new();
            if let Some(name) = &a.name {
                if !name.is_empty() {
                    extra.insert("name", name.clone());
                }
            }
            ConnectedAccount {
                key: a.id.clone(),
                label: a.email.clone(),
                connected_at: Some(a.connected_at.clone()),
                expires_at: a.token.expires_at,
                needs_reconnect: a.needs_reconnect,
                extra,
            }
        })
        .collect();
    Ok(ConnectStatus { configured, accounts })
}

/// Forget one account's token — the key *is* the Microsoft id. Other accounts
/// and the shared app credentials are untouched; synced data stays in the
/// vault.
fn disconnect(vault: &Vault, key: &str) -> Result<()> {
    vault.outlook_disconnect(key)
}

/// Interactive connect: opens the consent page, waits for the redirect,
/// resolves the account identity from `/me`, saves the account. Re-running for
/// an already-connected account is exactly "reconnect" — the new token
/// overwrites by id. Blocking — callers off the main thread only.
///
/// App credentials resolve in order: explicitly passed (first-time setup,
/// saved for next time) → previously saved → compiled-in default client id.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<OutlookAccountInfo> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(MICROSOFT.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(MICROSOFT.service)?
            .or_else(|| MICROSOFT.default_credentials())
            .context("no Microsoft app credentials — register an app and enter the client id once in the Integrations tab")?,
    };
    let flow = OauthFlow::start(&MICROSOFT, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    let identity = fetch_identity(&token.access_token)?;
    let account = StoredAccount {
        id: identity.id,
        email: identity.email,
        name: identity.name,
        connected_at: Local::now().to_rfc3339(),
        needs_reconnect: false,
        token,
    };
    vault.save_outlook_account(&account)?;
    Ok((&account).into())
}

// ---------------------------------------------------------------------------
// Identity resolution (`/me`) — the account's stable id + address.

/// Identity resolved from `/me` after a token exchange. The `id` is Microsoft
/// Graph's stable, opaque per-account id (filename-safe), used to key the
/// token store the way Google's `sub` is.
struct ResolvedIdentity {
    id: String,
    email: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct MeResponse {
    id: String,
    #[serde(default)]
    mail: Option<String>,
    #[serde(default)]
    #[serde(rename = "userPrincipalName")]
    user_principal_name: Option<String>,
    #[serde(default)]
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

/// Resolve an access token to a Microsoft account identity via `/me`. Personal
/// accounts often leave `mail` null and carry the address in
/// `userPrincipalName`, so fall back to it.
fn fetch_identity(access_token: &str) -> Result<ResolvedIdentity> {
    let me: MeResponse = ureq::get(&format!("{GRAPH_BASE}/me"))
        .set("Authorization", &format!("Bearer {access_token}"))
        .timeout(HTTP_TIMEOUT)
        .call()
        .map_err(oauth::describe_http_error)
        .context("fetching the Microsoft account identity (/me)")?
        .into_json()
        .context("parsing Microsoft /me")?;
    let email = me
        .mail
        .filter(|e| !e.is_empty())
        .or(me.user_principal_name)
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| me.id.clone());
    Ok(ResolvedIdentity {
        id: me.id,
        email,
        name: me.display_name,
    })
}

// ---------------------------------------------------------------------------
// Per-account token store (the google.rs precedent: per-account JSON under
// `.trove/sync/microsoft/{id}.json`, the shared app creds at the usual
// `.trove/sync/microsoft-app.json` slot via save_sync_app).

/// One connected account, on disk (holds the secret token — 0600).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAccount {
    /// Microsoft Graph's stable account id — the file key.
    id: String,
    email: String,
    #[serde(default)]
    name: Option<String>,
    /// RFC3339 local time the account was (re)connected.
    connected_at: String,
    /// A refresh failed — the account is kept so its identity still shows, but
    /// it needs a re-login.
    #[serde(default)]
    needs_reconnect: bool,
    token: TokenSet,
}

/// What the UI needs to render one account row — never the token itself.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct OutlookAccountInfo {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub connected_at: String,
    pub expires_at: Option<u64>,
    pub needs_reconnect: bool,
}

impl From<&StoredAccount> for OutlookAccountInfo {
    fn from(a: &StoredAccount) -> Self {
        OutlookAccountInfo {
            id: a.id.clone(),
            email: a.email.clone(),
            name: a.name.clone(),
            connected_at: a.connected_at.clone(),
            expires_at: a.token.expires_at,
            needs_reconnect: a.needs_reconnect,
        }
    }
}

impl Vault {
    /// `.trove/sync/microsoft`, created on demand.
    fn outlook_dir(&self) -> Result<PathBuf> {
        let dir = self.sync_dir()?.join("microsoft");
        fs::create_dir_all(&dir).context("creating .trove/sync/microsoft")?;
        Ok(dir)
    }

    fn outlook_account_path(&self, id: &str) -> Result<PathBuf> {
        check_account_key(id)?;
        Ok(self.outlook_dir()?.join(format!("{id}.json")))
    }

    fn save_outlook_account(&self, account: &StoredAccount) -> Result<()> {
        crate::sync::write_secret(&self.outlook_account_path(&account.id)?, account)
    }

    fn load_outlook_account(&self, id: &str) -> Result<Option<StoredAccount>> {
        crate::sync::read_secret(&self.outlook_account_path(id)?)
    }

    /// Every connected account, sorted by email for a stable UI order.
    fn outlook_accounts_raw(&self) -> Result<Vec<StoredAccount>> {
        let dir = self.outlook_dir()?;
        let mut accounts = Vec::new();
        for entry in fs::read_dir(&dir).context("reading .trove/sync/microsoft")? {
            let path = entry?.path();
            if path.extension().is_some_and(|x| x == "json") {
                if let Some(a) = crate::sync::read_secret::<StoredAccount>(&path)? {
                    accounts.push(a);
                }
            }
        }
        accounts.sort_by(|a, b| a.email.cmp(&b.email));
        Ok(accounts)
    }

    /// Disconnect one account: forget its token (other accounts and the shared
    /// app credentials are untouched).
    fn outlook_disconnect(&self, id: &str) -> Result<()> {
        let path = self.outlook_account_path(id)?;
        if path.exists() {
            fs::remove_file(&path).context("deleting Microsoft account")?;
        }
        Ok(())
    }
}

/// Account keys become file names; Graph ids are opaque alphanumerics, but
/// guard against anything that could escape the directory (the google.rs
/// guard).
fn check_account_key(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid Microsoft account key: {id}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared accessors for sibling collectors (outlook_calendar, etc.).

/// One connected Microsoft account in the minimal shape sibling collectors
/// need: id (the file key / token lookup key) + email (for display/logging).
#[derive(Debug, Clone)]
pub(crate) struct MicrosoftAccountInfo {
    pub id: String,
    pub email: String,
    pub needs_reconnect: bool,
}

impl From<&StoredAccount> for MicrosoftAccountInfo {
    fn from(a: &StoredAccount) -> Self {
        MicrosoftAccountInfo {
            id: a.id.clone(),
            email: a.email.clone(),
            needs_reconnect: a.needs_reconnect,
        }
    }
}

/// Every connected Microsoft account — id + email only (no token).
/// Used by sibling collectors (outlook_calendar) to iterate accounts.
pub(crate) fn microsoft_accounts(vault: &Vault) -> Result<Vec<MicrosoftAccountInfo>> {
    Ok(vault.outlook_accounts_raw()?.iter().map(MicrosoftAccountInfo::from).collect())
}

/// A live access token for one connected Microsoft account. Refreshes in
/// place if (about to be) expired; flags the account `needs_reconnect` on
/// a refresh failure rather than deleting it. Identical policy to the
/// private `fresh_token` used by the Outlook email pull.
pub(crate) fn microsoft_fresh_token(vault: &Vault, id: &str) -> Result<String> {
    fresh_token(vault, id).map(|t| t.access_token)
}

// ---------------------------------------------------------------------------
// Token freshness: refresh in place (Microsoft issues refresh tokens via
// offline_access), modeled on [`crate::sync::google::fresh_token`].

/// The live access token for one account, refreshed first if it is (about to
/// be) expired. Microsoft DOES issue refresh tokens (offline_access), so an
/// expired token is refreshed and re-saved in place; only a refresh *failure*
/// flags the account `needs_reconnect` (kept, not deleted, so the card shows a
/// known address) rather than forcing a silent reconnect.
fn fresh_token(vault: &Vault, id: &str) -> Result<TokenSet> {
    let mut account = vault
        .load_outlook_account(id)?
        .with_context(|| format!("Microsoft account {id} is not connected"))?;
    if !account.token.expired() {
        return Ok(account.token);
    }
    let creds = vault
        .load_sync_app(MICROSOFT.service)?
        .or_else(|| MICROSOFT.default_credentials())
        .context("no Microsoft app credentials — reconnect from the Integrations tab")?;
    match oauth::refresh_token(&MICROSOFT, &creds, &account.token) {
        Ok(new) => {
            account.token = new.clone();
            account.needs_reconnect = false;
            vault.save_outlook_account(&account)?;
            Ok(new)
        }
        Err(e) => {
            account.needs_reconnect = true;
            vault.save_outlook_account(&account)?;
            Err(e).with_context(|| {
                format!(
                    "Microsoft token refresh failed for {} — reconnect from the Integrations tab",
                    account.email
                )
            })
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — the base URL is injectable so the orchestration is testable
// against a local stub (the gmail/oura pattern). The PARSING is pure and
// fixture-tested without any network.

/// Status-level fetch errors needing distinct handling (the gmail set).
enum FetchError {
    RateLimited,
    Unauthorized,
    /// The delta token aged out (HTTP 410 Gone) — Graph's signal to restart the
    /// delta from scratch. Caught by [`sync_account`], which clears the stored
    /// `delta_link` so the account self-recovers with a full re-delta (the
    /// gmail [`HistoryGone`](crate::gmail) reset precedent). Dedupe absorbs the
    /// overlap, so no data is lost.
    DeltaExpired,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::DeltaExpired => write!(f, "delta token expired (HTTP 410)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// One page of a `/messages/delta` walk: the message stubs on this page and
/// whichever continuation link the response carried.
struct DeltaPage {
    items: Vec<DeltaItem>,
    /// `@odata.nextLink` — more pages of this same walk follow.
    next_link: Option<String>,
    /// `@odata.deltaLink` — the walk is complete; persist this as the cursor.
    delta_link: Option<String>,
}

/// One message stub off the delta feed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeltaItem {
    id: String,
    /// Graph's `internetMessageId` (the RFC822 Message-ID). Used to dedupe
    /// against the shared email stream BEFORE downloading the MIME body (Fix 2);
    /// empty when Graph omits it (then we fall back to the content-hash guid).
    internet_message_id: String,
    parent_folder_id: String,
}

/// Parse one delta response body into its message stubs + continuation links.
/// Graph delta items that are deletions carry `@removed` and no usable id —
/// skipped (we never delete from the append-only stream). PURE — fixture
/// tested.
fn parse_delta_page(v: &Value) -> DeltaPage {
    let items = v
        .get("value")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|it| it.get("@removed").is_none())
                .filter_map(|it| {
                    let id = it.get("id").and_then(Value::as_str)?.to_string();
                    if id.is_empty() {
                        return None;
                    }
                    Some(DeltaItem {
                        id,
                        internet_message_id: it
                            .get("internetMessageId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        parent_folder_id: it
                            .get("parentFolderId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    DeltaPage {
        items,
        next_link: v.get("@odata.nextLink").and_then(Value::as_str).map(str::to_string),
        delta_link: v.get("@odata.deltaLink").and_then(Value::as_str).map(str::to_string),
    }
}

/// Seconds to wait on a 429, honoring `Retry-After` when present (the gmail 429
/// reasoning). Capped so a hostile header can't park the watcher loop; a missing
/// or unparseable header falls back to a short fixed wait. PURE — unit-tested.
fn retry_after_secs(header: Option<&str>) -> u64 {
    header
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(2)
        .min(30)
}

/// Map a non-2xx ureq status (after the 429 retry has already been spent) to the
/// [`FetchError`] variants the sync logic special-cases. `context` labels the
/// `Other` body for the log. Shared by [`GraphClient::get_json`] and
/// [`GraphClient::get_mime`] so the 401/410/429 handling can't drift between
/// them.
fn classify_status(code: u16, resp: ureq::Response, context: &str) -> FetchError {
    match code {
        401 => FetchError::Unauthorized,
        // 410 Gone on a delta request = expired delta token → reset (Fix 1).
        410 => FetchError::DeltaExpired,
        429 => FetchError::RateLimited,
        _ => {
            let body = resp.into_string().unwrap_or_default();
            FetchError::Other(format!(
                "HTTP {code}{context}: {}",
                body.chars().take(300).collect::<String>()
            ))
        }
    }
}

/// Thin Graph client. The base URL is injected so the sync logic stays
/// testable against a local stub (the gmail/oura pattern).
struct GraphClient {
    base: String,
    token: String,
}

impl GraphClient {
    /// One authorized GET, retrying ONCE on a 429 after honoring `Retry-After`
    /// (the gmail 429 reasoning) — the shared retry seam for both `get_json` and
    /// `get_mime`, so the rate-limit handling can't drift between them (Fix 3).
    /// The closure turns a successful [`ureq::Response`] into `T`; non-2xx
    /// statuses go through [`classify_status`].
    fn get_with_retry<T>(
        &self,
        url: &str,
        context: &str,
        read: impl Fn(ureq::Response) -> Result<T, FetchError>,
    ) -> Result<T, FetchError> {
        let send = || {
            ureq::get(url)
                .set("Authorization", &format!("Bearer {}", self.token))
                .timeout(HTTP_TIMEOUT)
                .call()
        };
        match send() {
            Ok(resp) => read(resp),
            Err(ureq::Error::Status(429, resp)) => {
                // Honor Retry-After, then retry once; if it 429s again, bubble
                // up so the cursor doesn't advance.
                thread::sleep(Duration::from_secs(retry_after_secs(resp.header("Retry-After"))));
                match send() {
                    Ok(resp) => read(resp),
                    Err(ureq::Error::Status(code, resp)) => Err(classify_status(code, resp, context)),
                    Err(_) => Err(FetchError::RateLimited),
                }
            }
            Err(ureq::Error::Status(code, resp)) => Err(classify_status(code, resp, context)),
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }

    /// A GET that returns parsed JSON, mapping the statuses we special-case.
    /// `url` may be absolute (a nextLink/deltaLink Graph handed us) or a path
    /// relative to the base.
    fn get_json(&self, url: &str) -> Result<Value, FetchError> {
        let full = if url.starts_with("http") {
            url.to_string()
        } else {
            format!("{}{url}", self.base)
        };
        self.get_with_retry(&full, "", |resp| {
            resp.into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}")))
        })
    }

    /// The first delta page of a fresh walk (no stored cursor).
    fn delta_first(&self) -> Result<DeltaPage, FetchError> {
        self.get_json(&format!("/me/messages/delta?$select={DELTA_SELECT}"))
            .map(|v| parse_delta_page(&v))
    }

    /// Follow a nextLink / deltaLink Graph handed us verbatim.
    fn delta_follow(&self, link: &str) -> Result<DeltaPage, FetchError> {
        self.get_json(link).map(|v| parse_delta_page(&v))
    }

    /// One message's raw RFC822/MIME bytes (`/messages/{id}/$value`). Honors a
    /// 429 Retry-After + retries once via the shared seam (Fix 3), consistent
    /// with [`get_json`](Self::get_json).
    fn get_mime(&self, id: &str) -> Result<Vec<u8>, FetchError> {
        let full = format!("{}/me/messages/{id}/$value", self.base);
        let ctx = format!(" fetching MIME for {id}");
        self.get_with_retry(&full, &ctx, |resp| {
            let mut buf = Vec::new();
            resp.into_reader()
                .read_to_end(&mut buf)
                .map_err(|e| FetchError::Other(format!("reading MIME for {id}: {e}")))?;
            Ok(buf)
        })
    }

    /// All mail folders' `id → displayName`, for resolving each message's
    /// `parentFolderId` to a human label (cached per pass by the caller).
    /// Drains pagination defensively.
    fn folder_names(&self) -> Result<BTreeMap<String, String>, FetchError> {
        let mut map = BTreeMap::new();
        let mut url = "/me/mailFolders?$top=100&$select=id,displayName".to_string();
        loop {
            let v = self.get_json(&url)?;
            if let Some(arr) = v.get("value").and_then(Value::as_array) {
                for f in arr {
                    if let (Some(id), Some(name)) = (
                        f.get("id").and_then(Value::as_str),
                        f.get("displayName").and_then(Value::as_str),
                    ) {
                        map.insert(id.to_string(), name.to_string());
                    }
                }
            }
            match v.get("@odata.nextLink").and_then(Value::as_str) {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        Ok(map)
    }
}

// ---------------------------------------------------------------------------
// Cursor (non-secret): per-account deltaLink.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last sync attempt.
    #[serde(default)]
    updated: String,
    /// Per-account progress, keyed by the Microsoft id.
    #[serde(default)]
    accounts: BTreeMap<String, AccountCursor>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct AccountCursor {
    /// Display address (for the index; the map key is the Microsoft id).
    #[serde(default)]
    email: String,
    /// The incremental cursor: the `@odata.deltaLink` from the last completed
    /// walk. `None` → the next pass does a full delta from scratch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delta_link: Option<String>,
    /// Total messages ever written for this account (drives the index).
    #[serde(default)]
    messages: u64,
    /// Why this account's last pass failed, for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Vault {
    fn read_outlook_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_outlook_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// The pull.

/// Outcome of one Outlook sync pass, for logging / the UI notice.
#[derive(Debug, Clone, Default)]
struct OutlookSyncStats {
    /// Accounts that gained at least one message this pass.
    accounts: u32,
    /// Messages newly written to the email stream this pass.
    messages: u64,
}

/// One Outlook sync pass across every connected Microsoft account: refresh
/// each account's token, drain its delta feed from the stored cursor (or a
/// full walk on the first run), fetch + convert each new message, dedupe by
/// Message-ID against the shared email stream, append, and advance the
/// account's deltaLink ONLY after its batch is written. A silent no-op when no
/// Microsoft account is connected. Per-account failures are recorded in
/// `.trove/outlook-sync.json` and never abort the other accounts (the gmail
/// soft-fail).
fn collect(vault: &Vault) -> Result<OutlookSyncStats> {
    let accounts = vault.outlook_accounts_raw()?;
    let mut state = vault.read_outlook_sync();
    // Forget cursor state for accounts that have been disconnected.
    let live: HashSet<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
    state.accounts.retain(|id, _| live.contains(id.as_str()));

    if accounts.is_empty() {
        // Persist the pruning so a disconnected account's row doesn't linger.
        if vault.resolve(SYNC_FILE).map(|p| p.exists()).unwrap_or(false) {
            state.updated = Local::now().to_rfc3339();
            vault.write_outlook_sync(&state)?;
        }
        return Ok(OutlookSyncStats::default());
    }

    // The shared email dedupe set, loaded once and grown as we write — so
    // Outlook pulls coexist with Gmail/IMAP/mbox and re-runs never duplicate.
    let mut seen = vault.correspondence_guids(SOURCE)?;
    let mut stats = OutlookSyncStats::default();

    for acct in &accounts {
        // A flagged account can't refresh non-interactively; skip it (the card
        // surfaces the reconnect prompt).
        if acct.needs_reconnect {
            continue;
        }
        let before = state.accounts.get(&acct.id).map(|c| c.messages).unwrap_or(0);
        {
            let cur = state.accounts.entry(acct.id.clone()).or_default();
            cur.email = acct.email.clone();
            cur.error = None;
        }
        let token = match fresh_token(vault, &acct.id) {
            Ok(t) => t.access_token,
            Err(e) => {
                let cur = state.accounts.entry(acct.id.clone()).or_default();
                cur.error = Some(format!("{e:#}"));
                continue;
            }
        };
        let client = GraphClient { base: GRAPH_BASE.to_string(), token };
        if let Err(e) = sync_account(vault, &client, &acct.id, &acct.email, &mut state, &mut seen) {
            let cur = state.accounts.entry(acct.id.clone()).or_default();
            cur.error = Some(status_message(&acct.email, e));
        }
        let after = state.accounts.get(&acct.id).map(|c| c.messages).unwrap_or(before);
        if after > before {
            stats.accounts += 1;
            stats.messages += after - before;
        }
    }

    state.updated = Local::now().to_rfc3339();
    vault.write_outlook_sync(&state)?;
    Ok(stats)
}

/// Should the body download be skipped for this stub? YES when the stub carries
/// an `internetMessageId` we've ALREADY stored — the message is in the shared
/// email stream (this account before, or via Gmail/IMAP/mbox), so re-fetching
/// its MIME would be wasted bandwidth (Fix 2). On a delta reset (Fix 1) the
/// full re-delta re-enumerates the whole mailbox, so this skip is what keeps the
/// re-walk cheap. A stub with NO `internetMessageId` falls through to the MIME
/// fetch + content-hash-guid path (we can't dedupe what we can't key). PURE.
fn should_skip_fetch(stub: &DeltaItem, seen: &HashSet<String>) -> bool {
    !stub.internet_message_id.is_empty() && seen.contains(&stub.internet_message_id)
}

/// Clear one account's stored `delta_link` and persist, so the next request
/// starts a full delta from scratch. The 410-Gone reset seam (Fix 1) — Graph
/// returns 410 to say the delta token expired; mirrors gmail's HistoryGone
/// backfill reset. Counters/email are preserved; only the cursor is dropped.
fn reset_delta_cursor(vault: &Vault, state: &mut SyncState, id: &str, email: &str) -> Result<(), FetchError> {
    let cur = state.accounts.entry(id.to_string()).or_default();
    cur.email = email.to_string();
    cur.delta_link = None;
    vault.write_outlook_sync(state).map_err(soft)
}

/// Drain every page of one delta walk into its stubs + final deltaLink, starting
/// from `start_link` (a stored cursor) or a fresh full walk (`None`). Graph only
/// hands back the deltaLink on the final page, so we accumulate BEFORE the
/// caller advances the cursor.
fn drain_delta(client: &GraphClient, start_link: Option<&str>) -> Result<(Vec<DeltaItem>, Option<String>), FetchError> {
    let mut page = match start_link {
        Some(link) => client.delta_follow(link)?,
        None => client.delta_first()?,
    };
    let mut stubs: Vec<DeltaItem> = Vec::new();
    let mut delta_link: Option<String> = None;
    loop {
        stubs.extend(page.items);
        if let Some(d) = page.delta_link {
            delta_link = Some(d);
            break;
        }
        match page.next_link {
            Some(next) => page = client.delta_follow(&next)?,
            // No nextLink and no deltaLink (shouldn't happen) — stop to avoid a
            // loop; the cursor simply isn't advanced this pass.
            None => break,
        }
    }
    Ok((stubs, delta_link))
}

/// Drain one account's delta feed and write the new messages. Resolves the
/// folder-name map once per pass (lazily, only if there's anything to label).
fn sync_account(
    vault: &Vault,
    client: &GraphClient,
    id: &str,
    email: &str,
    state: &mut SyncState,
    seen: &mut HashSet<String>,
) -> Result<(), FetchError> {
    // Follow the stored deltaLink if we have one, else a full walk. A 410 Gone
    // means that delta token expired (Fix 1): clear the cursor and restart the
    // delta from scratch THIS pass, so the account self-recovers immediately.
    // Dedupe absorbs the full re-enumeration's overlap — no data is lost.
    let start_link = state.accounts.get(id).and_then(|c| c.delta_link.clone());
    let (stubs, delta_link) = match drain_delta(client, start_link.as_deref()) {
        Ok(out) => out,
        Err(FetchError::DeltaExpired) => {
            reset_delta_cursor(vault, state, id, email)?;
            // Restart from no token this pass; a second 410 (shouldn't happen on
            // a tokenless delta) just surfaces — next pass still has no cursor.
            drain_delta(client, None)?
        }
        Err(e) => return Err(e),
    };

    // Resolve folder display names only if there's anything to fetch.
    let folders = if stubs.is_empty() {
        BTreeMap::new()
    } else {
        client.folder_names().unwrap_or_default()
    };

    // Fetch + convert each new message, deduped by Message-ID. Skip the MIME
    // download up front for stubs whose Message-ID we've already stored (Fix 2)
    // — cheap on a delta reset / cross-source overlap. Stubs with no
    // Message-ID fall through to the fetch + content-hash-guid path below.
    let mut batch: Vec<Message> = Vec::new();
    for stub in &stubs {
        if should_skip_fetch(stub, seen) {
            continue;
        }
        let raw = client.get_mime(&stub.id)?;
        let Some(mut m) = crate::email::email_to_message(&raw, email) else {
            continue;
        };
        if let Some(name) = folders.get(&stub.parent_folder_id) {
            m.labels = vec![name.clone()];
        }
        if seen.insert(m.guid.clone()) {
            batch.push(m);
        }
    }

    let written = batch.len() as u64;
    if !batch.is_empty() {
        vault.append_messages(&batch).map_err(soft)?;
    }

    // Advance the cursor + counters ONLY after the batch is written.
    let cur = state.accounts.entry(id.to_string()).or_default();
    cur.email = email.to_string();
    cur.messages += written;
    if let Some(d) = delta_link {
        cur.delta_link = Some(d);
    }
    vault.write_outlook_sync(state).map_err(soft)?;
    Ok(())
}

/// A vault write error inside the API-fetch path → a soft [`FetchError`].
fn soft(e: anyhow::Error) -> FetchError {
    FetchError::Other(format!("{e:#}"))
}

/// Map a status-level error to a user-facing message for the sync log.
fn status_message(account: &str, e: FetchError) -> String {
    match e {
        FetchError::RateLimited => {
            format!("Microsoft Graph rate limited the sync ({account}) — it resumes next pass")
        }
        FetchError::Unauthorized => {
            format!("Microsoft rejected the token ({account}, 401) — reconnect from the Integrations tab")
        }
        // Normally caught + self-recovered in sync_account (the cursor resets);
        // only reaches here if a 410 surfaces on a non-delta request.
        FetchError::DeltaExpired => {
            format!("Microsoft delta token expired ({account}) — the next sync restarts from scratch")
        }
        FetchError::Other(m) => format!("outlook {account}: {m}"),
    }
}

/// Pull every connected account's Outlook mail into the vault.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let s = collect(vault)?;
    let errors = vault
        .read_outlook_sync()
        .accounts
        .values()
        .filter(|c| c.error.is_some())
        .count() as u64;
    let mut headline = if s.messages > 0 {
        format!("{} new messages across {} accounts", s.messages, s.accounts)
    } else {
        "no new messages".to_string()
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
            ("messages", s.messages),
            ("account_errors", errors),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join("correspondence/email"))
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// not-connected or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: chrono::DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    // A quiet no-op when no account is connected (don't even read the cursor).
    if vault.outlook_accounts_raw().map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(crate::registry::CollectOutcome::quiet());
    }
    match collect(vault) {
        Ok(s) => Ok(crate::registry::CollectOutcome::note_if(s.messages > 0, || {
            format!("outlook synced — {} messages across {} accounts", s.messages, s.accounts)
        })),
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "outlook sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    if vault.outlook_accounts_raw()?.is_empty() {
        bail!("no Microsoft account is connected");
    }
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (replaces the NotWired
/// stub).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "outlook",
        name: "Microsoft Outlook",
        kind: IntegrationKind::CloudSync,
        // Opt-in: pulling full message bodies is a privacy-sensitive choice.
        default_on: false,
        description: "Pulls your Outlook.com / Microsoft 365 email via the Microsoft \
                      Graph API into the same email stream as Gmail, IMAP, and mbox \
                      imports (deduped by Message-ID). Incremental via Graph delta. \
                      Shares one Microsoft login with future Teams, To Do, and OneDrive.",
        domain: "correspondence",
        vault_path: "correspondence/email/",
        toggleable: true,
        setup: &[
            "Connect a Microsoft account on the card above (register a free Azure app first — see the connect steps; no client secret needed).",
            "First sync walks your mailbox via Graph delta; later syncs fetch only what changed.",
        ],
        caveats: "Reads full message bodies from every connected account. Messages join the \
                  shared email stream and are deduped by Message-ID, so the same mailbox reached \
                  via Outlook and IMAP won't duplicate. Bodies and attachment metadata are stored; \
                  raw .eml archives and attachment files are not (a later opt-in). Uses OAuth via a \
                  public client (no secret) — Exchange Web Services (EWS) is retiring, Graph is the \
                  supported path.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(OUTLOOK_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("microsoft"),
    pull: Some(def_pull),
};

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-outlook-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn token(expires_at: Option<u64>, refresh: Option<&str>) -> TokenSet {
        TokenSet {
            access_token: "acc".into(),
            refresh_token: refresh.map(str::to_string),
            token_type: Some("Bearer".into()),
            scope: Some(MICROSOFT.scopes.into()),
            expires_at,
        }
    }

    fn stub_account(id: &str, email: &str, expires_at: Option<u64>, refresh: Option<&str>) -> StoredAccount {
        StoredAccount {
            id: id.into(),
            email: email.into(),
            name: Some("Test User".into()),
            connected_at: "2026-06-11T10:00:00-04:00".into(),
            needs_reconnect: false,
            token: token(expires_at, refresh),
        }
    }

    // -----------------------------------------------------------------------
    // OAuth: the public-client authorize URL (the trakt/oauth precedent), on a
    // TEST provider so the fixed real port 38577 is never bound here.

    static TEST_MS: Provider = Provider {
        service: "microsoft-test",
        display_name: "MicrosoftTest",
        auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        scopes: "offline_access Mail.Read User.Read",
        redirect_port: 38697,
        use_pkce: true,
        basic_auth: false,
        default_client_id: Some("test-client"),
        default_client_secret: None,
        extra_auth_params: &[("prompt", "select_account")],
    };

    #[test]
    fn oauth_is_a_public_client_with_pkce_and_no_secret() {
        // The real provider must be a PUBLIC client: PKCE on, basic_auth off,
        // NO baked secret, redirect port 38577.
        assert!(MICROSOFT.use_pkce, "PKCE required for a public client");
        assert!(!MICROSOFT.basic_auth, "no basic auth (no secret)");
        assert!(MICROSOFT.default_client_secret.is_none(), "public client: never a baked secret");
        assert_eq!(MICROSOFT.redirect_port, 38577);
        assert!(MICROSOFT.scopes.contains("offline_access"), "offline_access → refresh token");
        assert!(MICROSOFT.scopes.contains("Mail.Read"));
        assert!(MICROSOFT.scopes.contains("Calendars.Read"), "Calendars.Read → outlook_calendar");
        assert!(MICROSOFT.scopes.contains("Notes.Read"), "Notes.Read → onenote");
        assert!(MICROSOFT.scopes.contains("User.Read"));

        // The authorize URL carries PKCE, state, scopes, and the loopback
        // redirect — and crucially the public client sends NO secret.
        let creds = AppCredentials { client_id: "test-client".into(), client_secret: None };
        // Ephemeral bind: never bind a fixed test port under parallel `cargo test`.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let flow = OauthFlow::start_with_listener(&TEST_MS, &creds, listener).unwrap();
        let url = flow.authorize_url();
        assert!(url.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        assert!(url.contains("client_id=test-client"));
        assert!(url.contains("code_challenge_method=S256"), "PKCE challenge present");
        assert!(url.contains("state="));
        assert!(url.contains("scope=offline_access"));
        // The loopback redirect (url-encoded) carries the fixed port.
        assert!(url.contains("redirect_uri="));
        assert!(url.contains("38697"), "test redirect port present in the encoded redirect_uri");
        assert!(url.contains("prompt=select_account"));
    }

    #[test]
    fn connection_exposes_oauth_and_is_multi_account() {
        let m = CONNECTION.method("oauth").expect("OAuth method exposed");
        match m {
            ConnectMethod::OAuth { multi_account, provider, .. } => {
                assert!(multi_account, "Microsoft is multi-account");
                assert_eq!(provider.redirect_port, 38577);
            }
            _ => panic!("expected OAuth method"),
        }
        assert_eq!(CONNECTION.auto_pull, &["outlook", "outlook-calendar", "onenote"]);
    }

    // -----------------------------------------------------------------------
    // Delta-response parsing: canned JSON → message stubs + links, draining a
    // multi-page walk then capturing the deltaLink.

    #[test]
    fn parses_delta_page_stubs_links_and_skips_removed() {
        let body = serde_json::json!({
            "value": [
                {"id": "AAA", "internetMessageId": "<one@example.com>", "parentFolderId": "inbox-id"},
                {"id": "BBB", "internetMessageId": "<two@example.com>", "parentFolderId": "sent-id"},
                {"id": "CCC", "@removed": {"reason": "deleted"}}
            ],
            "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/messages/delta?$skiptoken=PAGE2"
        });
        let page = parse_delta_page(&body);
        assert_eq!(page.items.len(), 2, "the @removed deletion is skipped");
        assert_eq!(page.items[0].id, "AAA");
        assert_eq!(page.items[0].internet_message_id, "<one@example.com>");
        assert_eq!(page.items[0].parent_folder_id, "inbox-id");
        assert_eq!(page.items[1].id, "BBB");
        assert!(page.next_link.as_deref().unwrap().contains("PAGE2"));
        assert!(page.delta_link.is_none(), "no deltaLink mid-walk");

        // Final page: a deltaLink, no nextLink.
        let last = serde_json::json!({
            "value": [
                {"id": "DDD", "internetMessageId": "<three@example.com>", "parentFolderId": "inbox-id"}
            ],
            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/messages/delta?$deltatoken=FINAL"
        });
        let lp = parse_delta_page(&last);
        assert_eq!(lp.items.len(), 1);
        assert!(lp.next_link.is_none());
        assert!(lp.delta_link.as_deref().unwrap().contains("FINAL"), "deltaLink captured on the last page");
    }

    // -----------------------------------------------------------------------
    // MIME → correspondence/email/ with service=account, labels=[folder], and
    // a duplicate Message-ID (e.g. also pulled via Gmail/IMAP) is skipped.
    // Exercises the WRITE path directly (the conversion + dedupe the pull uses)
    // without a network: parse_delta_page + email_to_message + append_messages.

    const RAW_MIME: &[u8] = b"Message-ID: <shared@example.com>\r\n\
Date: Wed, 10 Jun 2026 09:00:00 -0700\r\n\
From: Alice Example <alice@example.com>\r\n\
To: me@outlook.com\r\n\
Subject: Hello from Outlook\r\n\
\r\n\
Body text.\r\n";

    #[test]
    fn mime_to_email_stream_with_folder_label_and_message_id_dedupe() {
        let v = temp_vault("mime-write");
        let account = "me@outlook.com";

        // Convert as the pull does, layering the folder label.
        let mut m = crate::email::email_to_message(RAW_MIME, account).unwrap();
        m.labels = vec!["Inbox".to_string()];
        assert_eq!(m.source, "email", "shared sink source");
        assert_eq!(m.guid, "<shared@example.com>", "guid = Message-ID");
        assert_eq!(m.service, account, "service = connected account");

        // First write lands in correspondence/email/<month>.jsonl.
        let mut seen = v.correspondence_guids(SOURCE).unwrap();
        assert!(seen.insert(m.guid.clone()));
        v.append_messages(&[m.clone()]).unwrap();

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].source, "email");
        assert_eq!(day[0].service, "me@outlook.com");
        assert_eq!(day[0].labels, vec!["Inbox"]);
        assert_eq!(day[0].sender, "alice@example.com");

        // The file is correspondence/email/, NOT correspondence/outlook/.
        assert!(v.root().join("correspondence/email/2026-06.jsonl").exists());
        assert!(!v.root().join("correspondence/outlook").exists());

        // A duplicate Message-ID (the same message also seen via Gmail/IMAP) is
        // skipped by the shared dedupe set — nothing re-written.
        let seen2 = v.correspondence_guids(SOURCE).unwrap();
        assert!(seen2.contains("<shared@example.com>"));
        let mut again = crate::email::email_to_message(RAW_MIME, account).unwrap();
        again.labels = vec!["Inbox".to_string()];
        let mut s = seen2;
        assert!(!s.insert(again.guid.clone()), "duplicate guid is not new");
        // (no append, mirroring the pull's `if seen.insert(...)` guard)
        assert_eq!(v.correspondence_timeline("2026-06-10").unwrap().len(), 1, "no duplicate row");
    }

    // -----------------------------------------------------------------------
    // Cursor: deltaLink persisted/loaded per account; first run (no deltaLink)
    // is a full delta, back-compat for an old/empty cursor line.

    #[test]
    fn cursor_round_trips_per_account_and_first_run_has_no_delta_link() {
        let v = temp_vault("cursor");
        assert!(v.read_outlook_sync().accounts.is_empty(), "no cursor → empty");

        let mut state = SyncState::default();
        state.accounts.insert(
            "oid-1".into(),
            AccountCursor {
                email: "a@outlook.com".into(),
                delta_link: Some("https://graph.microsoft.com/v1.0/me/messages/delta?$deltatoken=ABC".into()),
                messages: 42,
                error: None,
            },
        );
        state.updated = "2026-06-11T10:00:00-04:00".into();
        v.write_outlook_sync(&state).unwrap();

        let loaded = v.read_outlook_sync();
        let c = &loaded.accounts["oid-1"];
        assert_eq!(c.email, "a@outlook.com");
        assert!(c.delta_link.as_deref().unwrap().contains("deltatoken=ABC"));
        assert_eq!(c.messages, 42);

        // A second account with no deltaLink (a first-run account) round-trips
        // as None → the pull would do a full delta for it.
        let none: AccountCursor = serde_json::from_str(r#"{"email":"b@outlook.com"}"#).unwrap();
        assert!(none.delta_link.is_none(), "first run → full delta");

        // Back-compat: a bare `{}` cursor (fresh) deserializes.
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.accounts.is_empty() && empty.updated.is_empty());
    }

    // -----------------------------------------------------------------------
    // Token store: per-account save/load/list/disconnect, 0600, path-escape
    // guard (the google.rs precedent).

    #[test]
    fn account_store_roundtrip_list_and_disconnect() {
        let v = temp_vault("accounts");
        assert!(v.outlook_accounts_raw().unwrap().is_empty());

        v.save_outlook_account(&stub_account("id-b", "b@outlook.com", Some(1_900_000_000), Some("r")))
            .unwrap();
        v.save_outlook_account(&stub_account("id-a", "a@outlook.com", Some(1_900_000_000), Some("r")))
            .unwrap();

        let accts = v.outlook_accounts_raw().unwrap();
        // Sorted by email → a@ precedes b@.
        assert_eq!(accts.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["a@outlook.com", "b@outlook.com"]);

        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 2);
        assert_eq!(s.accounts[0].key, "id-a");
        assert_eq!(s.accounts[0].label, "a@outlook.com");
        assert_eq!(s.accounts[0].expires_at, Some(1_900_000_000));
        assert_eq!(s.accounts[0].extra.get("name").map(String::as_str), Some("Test User"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = v.outlook_account_path("id-a").unwrap();
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "account file must be 0600");
        }

        v.outlook_disconnect("id-b").unwrap();
        let accts = v.outlook_accounts_raw().unwrap();
        assert_eq!(accts.len(), 1);
        assert_eq!(accts[0].id, "id-a");
    }

    #[test]
    fn rejects_path_escape_account_keys() {
        let v = temp_vault("escape");
        assert!(v.outlook_account_path("../escape").is_err());
        assert!(v.outlook_account_path("a/b").is_err());
        assert!(v.outlook_account_path("").is_err());
        assert!(v.load_outlook_account("..").is_err());
        assert!(v.outlook_disconnect("a/b").is_err());
    }

    // -----------------------------------------------------------------------
    // Token refresh on expiry (the google/trakt refresh-decision precedent).

    #[test]
    fn fresh_token_returns_live_token_unchanged() {
        let v = temp_vault("fresh-live");
        v.save_outlook_account(&stub_account("id-1", "x@outlook.com", Some(1_900_000_000), Some("r")))
            .unwrap();
        let t = fresh_token(&v, "id-1").unwrap();
        assert_eq!(t.access_token, "acc", "live token passed through, no refresh");
    }

    #[test]
    fn expired_token_with_refresh_does_not_flag_reconnect_in_status() {
        // Microsoft issues refresh tokens, so an expired access token is
        // self-healing — status must NOT show reconnect until a refresh fails.
        let v = temp_vault("expired-status");
        v.save_outlook_account(&stub_account("id-1", "x@outlook.com", Some(1_000), Some("r")))
            .unwrap();
        let s = connect_status(&v).unwrap();
        assert!(!s.accounts[0].needs_reconnect, "refreshable expiry isn't a reconnect");
    }

    #[test]
    fn expired_token_refresh_failure_flags_reconnect_and_keeps_account() {
        // An EXPIRED token whose refresh fails (Microsoft rejects the dead
        // refresh token with invalid_grant): the failure path must flag
        // needs_reconnect and KEEP the account (so the card shows a known
        // address), mirroring google::fresh_token. App creds must be present
        // so the refresh is actually attempted (missing creds bail earlier,
        // also with a "reconnect" message — the google.rs precedent).
        let v = temp_vault("refresh-fail");
        v.save_sync_app(
            MICROSOFT.service,
            &AppCredentials { client_id: "test-client".into(), client_secret: None },
        )
        .unwrap();
        v.save_outlook_account(&stub_account("id-1", "x@outlook.com", Some(1_000), Some("dead")))
            .unwrap();
        let err = fresh_token(&v, "id-1").unwrap_err().to_string();
        assert!(err.contains("reconnect"), "refresh failure bails reconnect: {err}");
        // The account is kept and flagged (not deleted).
        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1, "account kept after a failed refresh");
        assert!(s.accounts[0].needs_reconnect, "flagged for re-login");
    }

    #[test]
    fn fresh_token_without_connection_is_a_clean_error() {
        let v = temp_vault("fresh-unconnected");
        let err = fresh_token(&v, "id-1").unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    // -----------------------------------------------------------------------
    // Pull / collect edge cases.

    #[test]
    fn collect_without_an_account_is_a_silent_noop() {
        let v = temp_vault("noaccount");
        let s = collect(&v).unwrap();
        assert_eq!(s.messages, 0);
        // No cursor file was written (nothing to prune).
        assert!(!v.root().join(SYNC_FILE).exists());
    }

    #[test]
    fn pull_without_an_account_is_a_clean_error() {
        let v = temp_vault("pull-noaccount");
        let err = def_pull(&v).unwrap_err().to_string();
        assert!(err.contains("no Microsoft account"), "{err}");
    }

    #[test]
    fn disconnected_account_cursor_is_pruned() {
        // A cursor row for an account no longer in the store is dropped on the
        // next collect (the gmail pruning).
        let v = temp_vault("prune");
        // Connect one account so collect doesn't short-circuit on "no accounts".
        v.save_outlook_account(&stub_account("id-live", "live@outlook.com", Some(1_900_000_000), Some("r")))
            .unwrap();
        let mut state = SyncState::default();
        state.accounts.insert("id-live".into(), AccountCursor { email: "live@outlook.com".into(), ..Default::default() });
        state.accounts.insert("id-gone".into(), AccountCursor { email: "gone@outlook.com".into(), ..Default::default() });
        v.write_outlook_sync(&state).unwrap();

        // collect will try to refresh id-live's (live) token, hit no network on
        // the delta call, and record an error — but the PRUNE of id-gone is
        // what we assert, and it happens before any network.
        let _ = collect(&v);
        let after = v.read_outlook_sync();
        assert!(after.accounts.contains_key("id-live"), "live account kept");
        assert!(!after.accounts.contains_key("id-gone"), "disconnected account pruned");
    }

    // -----------------------------------------------------------------------
    // FIX 1 — an expired deltaLink (HTTP 410 Gone) resets to a full delta.

    fn delta_item(id: &str, imid: &str, folder: &str) -> DeltaItem {
        DeltaItem {
            id: id.into(),
            internet_message_id: imid.into(),
            parent_folder_id: folder.into(),
        }
    }

    #[test]
    fn http_410_on_a_delta_request_maps_to_delta_expired() {
        // The HTTP-layer mapping: Graph's 410 Gone (expired delta token) must
        // become FetchError::DeltaExpired so sync_account can catch + reset it,
        // NOT FetchError::Other (which would propagate and re-410 forever). A
        // 401 still maps to Unauthorized; an unrelated 5xx stays Other.
        let resp410: ureq::Response = "HTTP/1.1 410 Gone\r\n\r\nexpired"
            .parse()
            .unwrap();
        assert!(
            matches!(classify_status(410, resp410, ""), FetchError::DeltaExpired),
            "410 → DeltaExpired (delta token reset signal)"
        );
        let resp401: ureq::Response = "HTTP/1.1 401 Unauthorized\r\n\r\nnope".parse().unwrap();
        assert!(matches!(classify_status(401, resp401, ""), FetchError::Unauthorized));
        let resp500: ureq::Response = "HTTP/1.1 500 Server Error\r\n\r\nboom".parse().unwrap();
        assert!(matches!(classify_status(500, resp500, " ctx"), FetchError::Other(_)));
    }

    #[test]
    fn expired_delta_link_410_resets_to_full_delta() {
        // Seed a cursor that HAS a deltaLink (a normal incremental account).
        let v = temp_vault("delta-410-reset");
        let mut state = SyncState::default();
        state.accounts.insert(
            "oid-1".into(),
            AccountCursor {
                email: "a@outlook.com".into(),
                delta_link: Some(
                    "https://graph.microsoft.com/v1.0/me/messages/delta?$deltatoken=STALE".into(),
                ),
                messages: 7,
                error: None,
            },
        );
        v.write_outlook_sync(&state).unwrap();

        // The 410→reset seam sync_account runs when delta_follow returns 410.
        assert!(
            reset_delta_cursor(&v, &mut state, "oid-1", "a@outlook.com").is_ok(),
            "cursor reset persists cleanly"
        );

        // The persisted cursor's delta_link is cleared → the NEXT pass does a
        // full delta from no token (self-recovery); counters are preserved so
        // the index isn't reset, and the cursor is durably written.
        assert!(state.accounts["oid-1"].delta_link.is_none(), "in-memory cursor cleared");
        assert_eq!(state.accounts["oid-1"].messages, 7, "message count preserved");
        let persisted = v.read_outlook_sync();
        assert!(
            persisted.accounts["oid-1"].delta_link.is_none(),
            "persisted deltaLink is None after 410 → next pass re-walks full delta"
        );
    }

    // -----------------------------------------------------------------------
    // FIX 2 — dedup on the stub's Message-ID BEFORE fetching the MIME body.

    #[test]
    fn already_seen_message_id_skips_mime_download() {
        // A stub whose internetMessageId is ALREADY stored skips the body fetch;
        // a NEW message-id does not; a stub with NO message-id falls through to
        // the fetch + content-hash path (we can't dedupe what we can't key).
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert("<already@example.com>".to_string());

        let seen_stub = delta_item("ID-A", "<already@example.com>", "inbox");
        assert!(should_skip_fetch(&seen_stub, &seen), "seen Message-ID → skip the MIME fetch");

        let new_stub = delta_item("ID-B", "<fresh@example.com>", "inbox");
        assert!(!should_skip_fetch(&new_stub, &seen), "new Message-ID → still fetch");

        let no_id_stub = delta_item("ID-C", "", "inbox");
        assert!(!should_skip_fetch(&no_id_stub, &seen), "no Message-ID → fall through to fetch");
    }

    #[test]
    fn seen_message_id_from_the_stored_stream_is_skipped_without_a_fetch() {
        // End-to-end against the REAL seen-set: a message already in
        // correspondence/email/ (e.g. pulled earlier or via Gmail/IMAP) is in
        // correspondence_guids, so its stub is skipped — proving no MIME fetch
        // is attempted (the loop short-circuits before get_mime). A stub for a
        // message-id NOT yet stored is NOT skipped.
        let v = temp_vault("seen-skip-stream");
        let account = "me@outlook.com";

        // Store one message so its Message-ID is in the seen-set.
        let m = crate::email::email_to_message(RAW_MIME, account).unwrap();
        assert_eq!(m.guid, "<shared@example.com>");
        v.append_messages(&[m]).unwrap();
        let seen = v.correspondence_guids(SOURCE).unwrap();
        assert!(seen.contains("<shared@example.com>"));

        // A stub for that same Message-ID is skipped (no get_mime call needed —
        // if it weren't skipped, the pull would hit the network for the body).
        let dup_stub = delta_item("graph-id-1", "<shared@example.com>", "inbox");
        assert!(should_skip_fetch(&dup_stub, &seen), "stored Message-ID skips the body download");

        // A brand-new Message-ID is not skipped → it would be fetched.
        let new_stub = delta_item("graph-id-2", "<brand-new@example.com>", "inbox");
        assert!(!should_skip_fetch(&new_stub, &seen), "unseen Message-ID is fetched");
    }

    // -----------------------------------------------------------------------
    // FIX 3 — get_mime honors 429 Retry-After like get_json (shared seam).

    #[test]
    fn retry_after_parses_header_and_caps_and_falls_back() {
        // The shared 429 backoff used by BOTH get_json and get_mime: a numeric
        // Retry-After is honored, a missing/garbage header falls back to a short
        // wait, and a hostile value is capped so it can't park the watcher loop.
        assert_eq!(retry_after_secs(Some("5")), 5, "numeric Retry-After honored");
        assert_eq!(retry_after_secs(Some("  7 ")), 7, "whitespace trimmed");
        assert_eq!(retry_after_secs(None), 2, "missing header → short fallback");
        assert_eq!(retry_after_secs(Some("not-a-number")), 2, "unparseable → fallback");
        assert_eq!(retry_after_secs(Some("99999")), 30, "capped at 30s");
    }

    #[test]
    fn classify_status_429_is_rate_limited_for_both_clients() {
        // Both get_json and get_mime route their post-retry non-2xx through
        // classify_status; a 429 (the retry was already spent) maps to
        // RateLimited so the cursor doesn't advance. The Retry-After header is
        // preserved on the parsed response (what get_with_retry reads).
        let resp: ureq::Response = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 12\r\n\r\nslow down"
            .parse()
            .unwrap();
        assert_eq!(resp.header("Retry-After"), Some("12"), "header preserved for the backoff");
        assert!(matches!(classify_status(429, resp, " fetching MIME for X"), FetchError::RateLimited));
    }
}

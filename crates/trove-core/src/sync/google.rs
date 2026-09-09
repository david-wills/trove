//! Google — one OAuth client, many services, many accounts.
//!
//! Unlike the single-account providers (TickTick, Oura), Google is one
//! integration *entity* spanning Gmail, Calendar, Contacts, Tasks, YouTube
//! and Books, and any number of Google accounts can be connected at once.
//! So tokens are keyed per account under `.trove/sync/google/{sub}.json`
//! (the `sub` is Google's stable, opaque account id from the OpenID
//! userinfo endpoint — filename-safe and immune to email changes), while
//! the shared OAuth *app* credentials live once at `.trove/sync/google-app.json`
//! (the usual [`Vault::save_sync_app`] store, keyed by the "google" service).
//!
//! This module owns the auth side only — connecting an account, listing
//! accounts, refreshing a token, disconnecting. The per-service pulls
//! (Gmail backfill, Calendar sync tokens, …) land in later steps and each
//! reuses [`fresh_token`] to get a live access token for a given account.
//!
//! **Scopes** are the full v1 bundle requested up front (one consent screen
//! rather than a re-consent per service): `openid email profile` to identify
//! the account, then read-only Gmail / Calendar / Contacts / Tasks / YouTube
//! / Books. **Refresh** uses Google's standard rotating-but-reusable refresh
//! tokens; `access_type=offline` + `prompt=consent` on the authorize URL
//! guarantee one is issued on every connect. While the OAuth consent screen
//! is in Testing mode, refresh tokens expire after 7 days — a failed refresh
//! flags the account [`needs_reconnect`](GoogleAccountInfo::needs_reconnect)
//! (kept, not deleted, so its identity still shows) for the UI to surface.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::registry::{ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef};
use crate::vault::Vault;

// The per-service pulls live in their own modules (the way Gmail's does):
// `google_calendar`, `google_contacts`, `google_tasks`, `youtube`,
// `google_books` — each with its own `DEF` registered in
// `crate::integrations::INTEGRATIONS`, each calling back here for tokens.

/// The full v1 scope set: identity, then each read-only service. Requested
/// together so connecting is a single consent; disabling a service is a
/// vault-side toggle, not a scope change.
const SCOPES: &str = "openid email profile \
https://www.googleapis.com/auth/gmail.readonly \
https://www.googleapis.com/auth/calendar.readonly \
https://www.googleapis.com/auth/contacts.readonly \
https://www.googleapis.com/auth/contacts.other.readonly \
https://www.googleapis.com/auth/tasks.readonly \
https://www.googleapis.com/auth/youtube.readonly \
https://www.googleapis.com/auth/books";

pub static GOOGLE: Provider = Provider {
    service: "google",
    display_name: "Google",
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: SCOPES,
    redirect_port: 38575,
    use_pkce: true,
    basic_auth: false,
    // Bake credentials in at build time for a zero-setup "just log in"
    // experience: TROVE_GOOGLE_CLIENT_ID / TROVE_GOOGLE_CLIENT_SECRET.
    default_client_id: option_env!("TROVE_GOOGLE_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_GOOGLE_CLIENT_SECRET"),
    // offline → issue a refresh token; consent → re-issue it on every
    // connect; select_account → let the user pick which account to add.
    extra_auth_params: &[
        ("access_type", "offline"),
        ("prompt", "consent select_account"),
    ],
};

/// Registered in [`crate::integrations::CONNECTIONS`]. Multi-account: each
/// connect run *adds* an account (keyed by `sub`), and re-running for an
/// already-connected account is exactly "reconnect" — the new token
/// overwrites by `sub`. Gmail auto-pulls right after a successful connect
/// (the backfill starts without a second click; the frontend still consults
/// the toggle).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "google",
    display_name: "Google",
    methods: &[ConnectMethod::OAuth {
        provider: &GOOGLE,
        multi_account: true,
        run: connect_oauth,
    }],
    status: connect_status,
    disconnect: disconnect,
    auto_pull: &["google-gmail"],
    setup: &[
        "At console.cloud.google.com create a project, then enable the Gmail, Calendar, People, Tasks, YouTube Data, and Books APIs.",
        "OAuth consent screen → External, Testing, and add your own Google addresses as test users.",
        "Create credentials → OAuth client ID → type Desktop app. (Desktop clients accept the loopback redirect http://localhost:38575/callback with no console-side registration.)",
        "Paste the Client ID and Client Secret here. They're saved and shared across every Google account you connect.",
    ],
};

/// [`ConnectMethod::OAuth`] adapter: forward to [`connect`] (which owns the
/// explicit → saved → compiled-in credential resolution) and drop the
/// returned [`GoogleAccountInfo`] — callers re-read state through the
/// status hook.
fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

/// Map [`Vault::google_status`] into the generic shape: one
/// [`ConnectedAccount`] per stored account, keyed by `sub` (the disconnect
/// key) and labeled by email, with the display name riding in `extra`.
/// `needs_reconnect` passes through — flagged accounts are kept and
/// surfaced, never silently dropped.
fn connect_status(vault: &Vault) -> Result<ConnectStatus> {
    let status = vault.google_status()?;
    let accounts = status
        .accounts
        .into_iter()
        .map(|a| {
            let mut extra = BTreeMap::new();
            if let Some(name) = a.name {
                extra.insert("name", name);
            }
            ConnectedAccount {
                key: a.sub,
                label: a.email,
                connected_at: Some(a.connected_at),
                expires_at: a.expires_at,
                needs_reconnect: a.needs_reconnect,
                extra,
            }
        })
        .collect();
    Ok(ConnectStatus { configured: status.configured, accounts })
}

/// Forget one account's token — the key *is* the `sub`. Other accounts and
/// the shared app credentials are untouched; synced data stays in the vault.
fn disconnect(vault: &Vault, sub: &str) -> Result<()> {
    vault.google_disconnect(sub)
}

/// Userinfo endpoint that resolves an access token to an account identity.
const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// One connected account, on disk (holds the secret token — 0600).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAccount {
    /// Google's stable account id — the file key.
    sub: String,
    email: String,
    #[serde(default)]
    name: Option<String>,
    /// RFC3339 local time the account was (re)connected.
    connected_at: String,
    /// A refresh failed (e.g. the 7-day Testing-mode expiry) — the account
    /// is kept so its identity still shows, but it needs a re-login.
    #[serde(default)]
    needs_reconnect: bool,
    token: TokenSet,
}

/// What the UI needs to render one account row — never the token itself.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GoogleAccountInfo {
    pub sub: String,
    pub email: String,
    pub name: Option<String>,
    pub connected_at: String,
    /// Access-token expiry, epoch seconds (refreshes transparently).
    pub expires_at: Option<u64>,
    /// Needs a re-login (refresh failed). Surface as a reconnect prompt.
    pub needs_reconnect: bool,
}

/// The Google card's whole state: are app credentials available, and which
/// accounts are connected.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GoogleStatus {
    /// App credentials saved or compiled in — connecting is just a login.
    pub configured: bool,
    pub accounts: Vec<GoogleAccountInfo>,
}

impl From<&StoredAccount> for GoogleAccountInfo {
    fn from(a: &StoredAccount) -> Self {
        GoogleAccountInfo {
            sub: a.sub.clone(),
            email: a.email.clone(),
            name: a.name.clone(),
            connected_at: a.connected_at.clone(),
            expires_at: a.token.expires_at,
            needs_reconnect: a.needs_reconnect,
        }
    }
}

/// Identity resolved from the userinfo endpoint after a token exchange.
struct ResolvedIdentity {
    sub: String,
    email: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct UserInfo {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Interactive connect: opens the consent page, waits for the redirect,
/// resolves the account identity, saves the account. Re-running for an
/// already-connected account is exactly "reconnect" — the new token
/// overwrites by `sub`. Blocking — callers off the main thread only.
///
/// App credentials resolve in order: explicitly passed (first-time setup,
/// saved for next time) → previously saved → compiled-in defaults.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<GoogleAccountInfo> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(GOOGLE.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(GOOGLE.service)?
            .or_else(|| GOOGLE.default_credentials())
            .context("no Google app credentials — enter them once in the Integrations tab")?,
    };
    let flow = OauthFlow::start(&GOOGLE, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    let identity = fetch_userinfo(&token.access_token)?;
    let account = StoredAccount {
        sub: identity.sub,
        email: identity.email,
        name: identity.name,
        connected_at: chrono::Local::now().to_rfc3339(),
        needs_reconnect: false,
        token,
    };
    vault.save_google_account(&account)?;
    Ok((&account).into())
}

/// The live access token for one account, refreshed first if it is (about
/// to be) expired. Called at the top of every per-service pull (steps 2-4).
///
/// On a refresh failure the account is flagged `needs_reconnect` and kept
/// (so the UI can show "reconnect" against a known email) rather than
/// silently vanishing.
pub fn fresh_token(vault: &Vault, sub: &str) -> Result<TokenSet> {
    let mut account = vault
        .load_google_account(sub)?
        .with_context(|| format!("Google account {sub} is not connected"))?;
    if !account.token.expired() {
        return Ok(account.token);
    }
    let creds = vault
        .load_sync_app(GOOGLE.service)?
        .or_else(|| GOOGLE.default_credentials())
        .context("no Google app credentials — reconnect from the Integrations tab")?;
    match oauth::refresh_token(&GOOGLE, &creds, &account.token) {
        Ok(new) => {
            account.token = new.clone();
            account.needs_reconnect = false;
            vault.save_google_account(&account)?;
            Ok(new)
        }
        Err(e) => {
            // Testing-mode refresh tokens die after 7 days → invalid_grant.
            account.needs_reconnect = true;
            vault.save_google_account(&account)?;
            Err(e).with_context(|| {
                format!(
                    "Google token refresh failed for {} — reconnect from the Integrations tab",
                    account.email
                )
            })
        }
    }
}

/// Resolve an access token to a Google account identity.
fn fetch_userinfo(access_token: &str) -> Result<ResolvedIdentity> {
    let info: UserInfo = ureq::get(USERINFO_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(oauth::describe_http_error)
        .context("fetching the Google account identity")?
        .into_json()
        .context("parsing Google userinfo")?;
    let email = info
        .email
        .filter(|e| !e.is_empty())
        .context("Google returned no email for this account — was the email scope granted?")?;
    Ok(ResolvedIdentity {
        sub: info.sub,
        email,
        name: info.name,
    })
}

impl Vault {
    /// `.trove/sync/google`, created on demand.
    fn google_dir(&self) -> Result<PathBuf> {
        let dir = self.sync_dir()?.join("google");
        fs::create_dir_all(&dir).context("creating .trove/sync/google")?;
        Ok(dir)
    }

    fn google_account_path(&self, sub: &str) -> Result<PathBuf> {
        check_account_key(sub)?;
        Ok(self.google_dir()?.join(format!("{sub}.json")))
    }

    fn save_google_account(&self, account: &StoredAccount) -> Result<()> {
        super::write_secret(&self.google_account_path(&account.sub)?, account)
    }

    fn load_google_account(&self, sub: &str) -> Result<Option<StoredAccount>> {
        super::read_secret(&self.google_account_path(sub)?)
    }

    /// Every connected account, sorted by email for a stable UI order.
    fn google_accounts_raw(&self) -> Result<Vec<StoredAccount>> {
        let dir = self.google_dir()?;
        let mut accounts = Vec::new();
        for entry in fs::read_dir(&dir).context("reading .trove/sync/google")? {
            let path = entry?.path();
            if path.extension().is_some_and(|x| x == "json") {
                if let Some(a) = super::read_secret::<StoredAccount>(&path)? {
                    accounts.push(a);
                }
            }
        }
        accounts.sort_by(|a, b| a.email.cmp(&b.email));
        Ok(accounts)
    }

    /// The Google card state: app-credential availability + account rows.
    pub fn google_status(&self) -> Result<GoogleStatus> {
        let configured =
            self.load_sync_app(GOOGLE.service)?.is_some() || GOOGLE.default_credentials().is_some();
        let accounts = self
            .google_accounts_raw()?
            .iter()
            .map(GoogleAccountInfo::from)
            .collect();
        Ok(GoogleStatus { configured, accounts })
    }

    /// Disconnect one account: forget its token (other accounts and the
    /// shared app credentials are untouched).
    pub fn google_disconnect(&self, sub: &str) -> Result<()> {
        let path = self.google_account_path(sub)?;
        if path.exists() {
            fs::remove_file(&path).context("deleting Google account")?;
        }
        Ok(())
    }
}

/// Account keys become file names; Google's `sub` is digits, but guard
/// against anything that could escape the directory.
fn check_account_key(sub: &str) -> Result<()> {
    if sub.is_empty()
        || !sub
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid Google account key: {sub}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-test-google-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn stub(sub: &str, email: &str) -> StoredAccount {
        StoredAccount {
            sub: sub.into(),
            email: email.into(),
            name: Some("Test User".into()),
            connected_at: "2026-06-11T10:00:00-04:00".into(),
            needs_reconnect: false,
            token: TokenSet {
                access_token: "atk".into(),
                refresh_token: Some("rtk".into()),
                token_type: Some("Bearer".into()),
                scope: Some(SCOPES.into()),
                expires_at: Some(1_900_000_000),
            },
        }
    }

    #[test]
    fn account_roundtrip_list_and_disconnect() {
        let v = temp_vault("roundtrip");
        assert!(v.google_status().unwrap().accounts.is_empty());

        v.save_google_account(&stub("111", "b@example.com")).unwrap();
        v.save_google_account(&stub("222", "a@example.com")).unwrap();

        let status = v.google_status().unwrap();
        // Sorted by email, so a@ precedes b@.
        let emails: Vec<_> = status.accounts.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(emails, ["a@example.com", "b@example.com"]);
        assert_eq!(status.accounts[0].sub, "222");
        assert_eq!(status.accounts[0].expires_at, Some(1_900_000_000));
        assert!(!status.accounts[0].needs_reconnect);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = v.google_account_path("111").unwrap();
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "account file must be 0600");
        }

        v.google_disconnect("111").unwrap();
        let status = v.google_status().unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].sub, "222");
    }

    #[test]
    fn rejects_path_escape_keys() {
        let v = temp_vault("escape");
        assert!(v.google_account_path("../escape").is_err());
        assert!(v.google_account_path("a/b").is_err());
        assert!(v.google_account_path("").is_err());
        assert!(v.load_google_account("../escape").is_err());
        assert!(v.google_disconnect("..").is_err());
    }

    #[test]
    fn connection_status_maps_accounts_into_generic_rows() {
        let v = temp_vault("connection-status");
        let mut flagged = stub("111", "b@example.com");
        flagged.needs_reconnect = true;
        v.save_google_account(&flagged).unwrap();
        v.save_google_account(&stub("222", "a@example.com")).unwrap();

        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 2);
        // Sorted by email, so a@ (sub 222) precedes b@ (sub 111).
        let a = &s.accounts[0];
        assert_eq!(a.key, "222");
        assert_eq!(a.label, "a@example.com");
        assert_eq!(a.connected_at.as_deref(), Some("2026-06-11T10:00:00-04:00"));
        assert_eq!(a.expires_at, Some(1_900_000_000));
        assert!(!a.needs_reconnect);
        assert_eq!(a.extra.get("name").map(String::as_str), Some("Test User"));
        // A flagged account rides through flagged — kept, never dropped.
        assert!(s.accounts[1].needs_reconnect);

        // The disconnect hook keys by sub; the other account survives.
        disconnect(&v, "222").unwrap();
        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        assert_eq!(s.accounts[0].key, "111");
    }

    #[test]
    fn connection_status_omits_extra_name_when_absent() {
        let v = temp_vault("connection-no-name");
        let mut a = stub("333", "c@example.com");
        a.name = None;
        v.save_google_account(&a).unwrap();
        let s = connect_status(&v).unwrap();
        assert!(s.accounts[0].extra.is_empty());
    }

    #[test]
    fn status_configured_reflects_app_credentials() {
        let v = temp_vault("configured");
        // No saved creds: `configured` only if a build baked them in.
        let baked = option_env!("TROVE_GOOGLE_CLIENT_ID").is_some();
        assert_eq!(v.google_status().unwrap().configured, baked);

        v.save_sync_app(
            GOOGLE.service,
            &AppCredentials {
                client_id: "id".into(),
                client_secret: Some("secret".into()),
            },
        )
        .unwrap();
        assert!(v.google_status().unwrap().configured);
    }
}

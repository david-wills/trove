//! Oura Ring — the second M5/OAuth source (health: sleep, readiness,
//! activity, heart rate, …).
//!
//! Two ways in, both landing in the same token store:
//!
//! - **OAuth** (preferred): authorization-code flow against
//!   `cloud.ouraring.com`, bring-your-own app (redirect URI
//!   `http://localhost:38574/callback`) or compiled-in credentials. Access
//!   tokens last ~30 days; Oura issues *single-use rotating* refresh tokens,
//!   so [`fresh_token`] serializes refreshes across processes (two app instances)
//!   with a file lock and persists the rotated token before returning.
//! - **Personal Access Token** (fallback): pasted from
//!   `cloud.ouraring.com/personal-access-tokens`, stored as a [`TokenSet`]
//!   with no expiry — `expired()` is then always false, so the refresh path
//!   never fires. Oura has deprecated PATs but existing ones keep working.
//!
//! This module owns the auth side only; the pull itself — every v2
//! collection, full-history backfill, keyed upserts — lives in
//! [`crate::oura`] (`Vault::collect_oura`), which the manual pull below and
//! the watcher loop's hourly sync both call, so there is exactly one writer
//! of `health/oura/`.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::registry::{ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef};
use crate::vault::Vault;

pub static OURA: Provider = Provider {
    service: "oura",
    display_name: "Oura Ring",
    auth_url: "https://cloud.ouraring.com/oauth/authorize",
    token_url: "https://api.ouraring.com/oauth/token",
    // Every scope Oura defines — the pull covers all collections.
    scopes: "email personal daily heartrate workout tag session spo2",
    redirect_port: 38574,
    use_pkce: false,
    basic_auth: true,
    // Bake credentials in at build time for a zero-setup "just log in"
    // experience: TROVE_OURA_CLIENT_ID / TROVE_OURA_CLIENT_SECRET.
    default_client_id: option_env!("TROVE_OURA_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_OURA_CLIENT_SECRET"),
    extra_auth_params: &[],
};

/// Interactive connect: opens the consent page in the browser, waits for
/// the redirect, saves the token. Blocking — callers off the main thread
/// only.
///
/// App credentials resolve in order: explicitly passed (first-time setup,
/// saved for next time) → previously saved → compiled-in defaults. After
/// the first connect, "reconnect" is therefore just a login.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(OURA.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(OURA.service)?
            .or_else(|| OURA.default_credentials())
            .context("no Oura app credentials — enter them once in the Integrations tab")?,
    };
    let flow = OauthFlow::start(&OURA, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(OURA.service, &token)?;
    Ok(token)
}

/// Connect with a pasted Personal Access Token: validated with one cheap
/// API call, then stored as a never-expiring [`TokenSet`] so everything
/// downstream of the token store is identical to the OAuth path.
pub fn connect_pat(vault: &Vault, pat: &str) -> Result<TokenSet> {
    let pat = pat.trim();
    if pat.is_empty() {
        bail!("empty token");
    }
    match ureq::get("https://api.ouraring.com/v2/usercollection/personal_info")
        .set("Authorization", &format!("Bearer {pat}"))
        .timeout(Duration::from_secs(10))
        .call()
    {
        Ok(_) => {}
        Err(ureq::Error::Status(401, _)) => {
            bail!("Oura rejected the token (401) — check it was copied whole")
        }
        Err(e) => {
            return Err(oauth::describe_http_error(e)).context("validating the token with Oura")
        }
    }
    let token = TokenSet {
        access_token: pat.to_string(),
        refresh_token: None,
        token_type: Some("Bearer".into()),
        scope: None,
        expires_at: None,
    };
    vault.save_sync_token(OURA.service, &token)?;
    Ok(token)
}

// -- connect phase: the registry face over the two ways in above ----------

// Thin signature adapters: the generic dispatch doesn't care about the
// TokenSet, only that the token store ends up populated.
fn def_connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_connect_pat(vault: &Vault, token: &str) -> Result<()> {
    connect_pat(vault, token).map(|_| ())
}

/// Single-account: `key` is always the service id, so disconnect just
/// forgets the one token (app credentials are kept — see
/// [`Vault::delete_sync_token`]).
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(OURA.service)
}

/// The token store mapped into the generic envelope. `configured` gates
/// only the bring-your-own-OAuth-credentials form — a pasted PAT needs no
/// app credentials and works either way.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(OURA.service)?.is_some()
        || OURA.default_credentials().is_some();
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(OURA.service)? {
        // The PAT path stores no refresh token and no expiry (see
        // [`connect_pat`]); an OAuth grant always carries an expiry. That
        // is the only distinction the stored TokenSet supports.
        let is_pat = token.refresh_token.is_none() && token.expires_at.is_none();
        // Honest reconnect flag: only when a refresh is *impossible* —
        // expired with no refresh token, or no app credentials to refresh
        // with. An expired token with both just refreshes on the next pull
        // (see [`fresh_token`]); a PAT never expires at all.
        let needs_reconnect =
            token.expired() && (token.refresh_token.is_none() || !configured);
        let mut extra = BTreeMap::new();
        extra.insert(
            "method",
            if is_pat { "personal access token" } else { "oauth" }.to_string(),
        );
        accounts.push(ConnectedAccount {
            key: OURA.service.to_string(),
            label: "Oura".to_string(),
            connected_at: None, // the token store doesn't record it
            expires_at: token.expires_at,
            needs_reconnect,
            extra,
        });
    }
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]; the dual-method
/// connection — OAuth or a pasted PAT, both landing in the same token
/// store. Referenced by the oura [`crate::registry::IntegrationDef`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "oura",
    display_name: "Oura",
    methods: &[
        ConnectMethod::OAuth { provider: &OURA, multi_account: false, run: def_connect_oauth },
        ConnectMethod::TokenPaste {
            label: "Personal access token",
            help: "Paste a Personal Access Token from cloud.ouraring.com (account → Personal Access Tokens). Oura has deprecated new PATs, but existing ones keep working.",
            placeholder: "Q5DKXATR3VJYWB2HM4UCZN6SD8FGL9EP",
            run: def_connect_pat,
        },
    ],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &[],
    setup: &[
        "Sign in at cloud.ouraring.com and create an app under OAuth applications.",
        "Set its redirect URI to http://localhost:38574/callback — must match exactly.",
        "Paste the app's Client ID and Client Secret here. They're saved, so every future connect is just a login.",
    ],
};

/// The current token, refreshed first if it is (about to be) expired.
/// Called at the top of every pull.
///
/// Oura's refresh tokens are single-use: refreshing rotates them, and a
/// second process refreshing with the old one gets `invalid_grant` and
/// would strand the chain. So the whole check-and-refresh runs under an
/// exclusive file lock shared by app and daemon, and the token is re-read
/// from disk *under the lock* in case the other process just rotated it.
pub fn fresh_token(vault: &Vault) -> Result<TokenSet> {
    // hand-rolled: this is a cross-process *lock file*, not a data write —
    // store.rs has no lock-file helper (JsonlStream's flock is per-append,
    // not held across a read-refresh-write critical section like this one).
    let lock_path = vault.resolve(".trove/sync/oura-refresh.lock")?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .context("opening oura-refresh.lock")?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            bail!("locking oura-refresh.lock failed");
        }
    }
    // Lock held until `lock` drops at the end of this function.
    let token = vault
        .load_sync_token(OURA.service)?
        .context("Oura is not connected")?;
    if !token.expired() {
        return Ok(token);
    }
    if token.refresh_token.is_none() {
        // A PAT never reaches here (no expiry); an OAuth token without a
        // refresh token can only be reconnected.
        vault.delete_sync_token(OURA.service)?;
        bail!("Oura token expired — reconnect from the Integrations tab");
    }
    let creds = vault
        .load_sync_app(OURA.service)?
        .or_else(|| OURA.default_credentials())
        .context("no Oura app credentials — reconnect from the Integrations tab")?;
    match oauth::refresh_token(&OURA, &creds, &token) {
        Ok(new) => {
            // Persist the rotated refresh token *before* using the new
            // access token — a crash after this line loses nothing.
            vault.save_sync_token(OURA.service, &new)?;
            Ok(new)
        }
        Err(e) => {
            vault.delete_sync_token(OURA.service)?;
            Err(e).context("refreshing the Oura token failed — reconnect from the Integrations tab")
        }
    }
}

impl Vault {
    /// Pull every Oura collection into the vault, unbudgeted (the manual
    /// "Sync now" / first-connect path — runs the full backfill to
    /// completion). Blocking (network). Delegates to [`crate::oura`], same
    /// operation as the watcher loop's hourly budgeted pass.
    pub fn oura_pull(&self) -> Result<crate::oura::OuraSyncStats> {
        if self.load_sync_token(OURA.service)?.is_none() {
            bail!("Oura is not connected");
        }
        self.collect_oura(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-oura-connect-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn pat_token() -> TokenSet {
        // Exactly what connect_pat stores: no refresh, no expiry.
        TokenSet {
            access_token: "pat".into(),
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: None,
            expires_at: None,
        }
    }

    #[test]
    fn status_without_a_token_has_no_accounts() {
        let v = temp_vault("noaccounts");
        let status = def_status(&v).unwrap();
        assert!(status.accounts.is_empty());
    }

    #[test]
    fn saved_app_credentials_mean_configured() {
        let v = temp_vault("configured");
        v.save_sync_app(
            OURA.service,
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        assert!(def_status(&v).unwrap().configured);
    }

    #[test]
    fn pat_account_is_labeled_and_never_needs_reconnect() {
        let v = temp_vault("pat");
        v.save_sync_token(OURA.service, &pat_token()).unwrap();
        let status = def_status(&v).unwrap();
        let acct = &status.accounts[0];
        assert_eq!(acct.key, "oura");
        assert_eq!(acct.extra["method"], "personal access token");
        assert_eq!(acct.expires_at, None);
        assert!(!acct.needs_reconnect);
    }

    #[test]
    fn oauth_account_reports_method_and_expiry() {
        let v = temp_vault("oauth");
        // App credentials saved → a live refresh token could rotate, so an
        // unexpired OAuth token must not be flagged.
        v.save_sync_app(
            OURA.service,
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        let expires = now_epoch() + 3600;
        v.save_sync_token(
            OURA.service,
            &TokenSet {
                access_token: "at".into(),
                refresh_token: Some("rt".into()),
                token_type: Some("Bearer".into()),
                scope: None,
                expires_at: Some(expires),
            },
        )
        .unwrap();
        let acct = &def_status(&v).unwrap().accounts[0];
        assert_eq!(acct.extra["method"], "oauth");
        assert_eq!(acct.expires_at, Some(expires));
        assert!(!acct.needs_reconnect);
    }

    #[test]
    fn expired_token_without_refresh_needs_reconnect() {
        let v = temp_vault("expired");
        // An OAuth token past expiry with no refresh token: fresh_token can
        // only fail, so status must flag it (the account is kept, not
        // dropped — the registry contract).
        v.save_sync_token(
            OURA.service,
            &TokenSet {
                access_token: "at".into(),
                refresh_token: None,
                token_type: Some("Bearer".into()),
                scope: None,
                expires_at: Some(now_epoch() - 1),
            },
        )
        .unwrap();
        let acct = &def_status(&v).unwrap().accounts[0];
        assert!(acct.needs_reconnect);
    }

    #[test]
    fn disconnect_forgets_the_token_but_keeps_app_credentials() {
        let v = temp_vault("disconnect");
        v.save_sync_app(
            OURA.service,
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        v.save_sync_token(OURA.service, &pat_token()).unwrap();
        def_disconnect(&v, "oura").unwrap();
        let status = def_status(&v).unwrap();
        assert!(status.accounts.is_empty());
        assert!(status.configured, "disconnect must not forget app credentials");
    }

    #[test]
    fn connection_exposes_both_methods() {
        assert!(CONNECTION.method("oauth").is_some());
        assert!(CONNECTION.method("token-paste").is_some());
    }
}

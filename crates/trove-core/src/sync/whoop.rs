//! WHOOP — the OAuth/token side.
//!
//! The pull itself (every v2 API collection, keyed upserts, cursor) lives in
//! [`crate::whoop`] (`Vault::collect_whoop`); this module owns only the auth:
//! connect flow, token store, refresh.
//!
//! WHOOP issues *rotating* refresh tokens (like Oura): a second process
//! refreshing with the old token gets `invalid_grant`. So the whole
//! check-and-refresh runs under a file lock, and the token is re-read from
//! disk *under the lock* in case the other process just rotated it.
//!
//! Registration: developer.whoop.com (requires an active WHOOP membership).
//! Redirect URI: `http://localhost:38669/callback` (production port 38669).
//! Scopes: `offline read:cycles read:recovery read:sleep read:workout read:profile read:body_measurement`
//! The `offline` scope is required for refresh tokens.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::registry::{ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef};
use crate::vault::Vault;

pub static WHOOP: Provider = Provider {
    service: "whoop",
    display_name: "WHOOP",
    auth_url: "https://api.prod.whoop.com/oauth/oauth2/auth",
    token_url: "https://api.prod.whoop.com/oauth/oauth2/token",
    // `offline` is required for refresh tokens; the rest are the data scopes.
    scopes: "offline read:cycles read:recovery read:sleep read:workout read:profile read:body_measurement",
    // Production redirect port 38669 — must be registered verbatim in the
    // WHOOP developer dashboard: http://localhost:38669/callback
    redirect_port: 38669,
    use_pkce: false,
    // WHOOP uses HTTP Basic auth for the client secret on token requests
    // (standard confidential client pattern, same as Oura).
    basic_auth: true,
    // Bake credentials in at build time: TROVE_WHOOP_CLIENT_ID /
    // TROVE_WHOOP_CLIENT_SECRET. Defaults to None → bring-your-own.
    default_client_id: option_env!("TROVE_WHOOP_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_WHOOP_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// -- connect phase ----------------------------------------------------------

fn def_connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(WHOOP.service)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(WHOOP.service)?.is_some()
        || WHOOP.default_credentials().is_some();
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(WHOOP.service)? {
        let needs_reconnect =
            token.expired() && (token.refresh_token.is_none() || !configured);
        accounts.push(ConnectedAccount {
            key: WHOOP.service.to_string(),
            label: "WHOOP".to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            needs_reconnect,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "whoop",
    display_name: "WHOOP",
    methods: &[ConnectMethod::OAuth {
        provider: &WHOOP,
        multi_account: false,
        run: def_connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &[],
    setup: &[
        "Register an app at developer.whoop.com (requires an active WHOOP membership).",
        "Set its redirect URI to http://localhost:38669/callback — must match exactly.",
        "Paste the app's Client ID and Client Secret here. They're saved so every future connect is just a login.",
    ],
};

/// Interactive connect: opens the consent page in the browser, waits for the
/// redirect, saves the token. Blocking — callers off the main thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(WHOOP.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(WHOOP.service)?
            .or_else(|| WHOOP.default_credentials())
            .context("no WHOOP app credentials — enter them once in the Integrations tab")?,
    };
    let flow = OauthFlow::start(&WHOOP, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(WHOOP.service, &token)?;
    Ok(token)
}

/// The current token, refreshed first if it is (about to be) expired.
/// Called at the top of every pull.
///
/// WHOOP's refresh tokens are rotating (single-use): refreshing rotates them,
/// and a second process refreshing with the old one gets `invalid_grant`.
/// So the whole check-and-refresh runs under an exclusive file lock shared
/// by app and daemon, and the token is re-read from disk *under the lock*
/// in case the other process just rotated it.
pub fn fresh_token(vault: &Vault) -> Result<TokenSet> {
    let lock_path = vault.resolve(".trove/sync/whoop-refresh.lock")?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .context("opening whoop-refresh.lock")?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            bail!("locking whoop-refresh.lock failed");
        }
    }
    // Re-read under the lock — the other process may have just rotated.
    let token = vault
        .load_sync_token(WHOOP.service)?
        .context("WHOOP is not connected")?;
    if !token.expired() {
        return Ok(token);
    }
    if token.refresh_token.is_none() {
        vault.delete_sync_token(WHOOP.service)?;
        bail!("WHOOP token expired — reconnect from the Integrations tab");
    }
    let creds = vault
        .load_sync_app(WHOOP.service)?
        .or_else(|| WHOOP.default_credentials())
        .context("no WHOOP app credentials — reconnect from the Integrations tab")?;
    match oauth::refresh_token(&WHOOP, &creds, &token) {
        Ok(new) => {
            vault.save_sync_token(WHOOP.service, &new)?;
            Ok(new)
        }
        Err(e) => {
            vault.delete_sync_token(WHOOP.service)?;
            Err(e).context("refreshing the WHOOP token failed — reconnect from the Integrations tab")
        }
    }
}

impl Vault {
    /// Pull all WHOOP collections into the vault, unbudgeted (the manual
    /// "Sync now" / first-connect path). Delegates to [`crate::whoop`].
    pub fn whoop_pull(&self) -> Result<crate::whoop::WhoopSyncStats> {
        if self.load_sync_token(WHOOP.service)?.is_none() {
            bail!("WHOOP is not connected");
        }
        self.collect_whoop(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::oauth::TokenSet;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-whoop-conn-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn token(expires_at: Option<u64>, refresh: bool) -> TokenSet {
        TokenSet {
            access_token: "tok".into(),
            refresh_token: if refresh { Some("rt".into()) } else { None },
            token_type: Some("bearer".into()),
            scope: None,
            expires_at,
        }
    }

    #[test]
    fn status_without_token_has_no_accounts() {
        let v = temp_vault("noaccounts");
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
    }

    #[test]
    fn connected_account_maps_into_status() {
        let v = temp_vault("connected");
        let exp = now_epoch() + 3600;
        v.save_sync_token("whoop", &token(Some(exp), true)).unwrap();
        let s = def_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "whoop");
        assert_eq!(a.label, "WHOOP");
        assert_eq!(a.expires_at, Some(exp));
        assert!(!a.needs_reconnect);
    }

    #[test]
    fn expired_token_without_refresh_needs_reconnect() {
        let v = temp_vault("expired");
        v.save_sync_token("whoop", &token(Some(1_000), false)).unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts[0].needs_reconnect);
    }

    #[test]
    fn expired_token_with_refresh_and_credentials_does_not_need_reconnect() {
        let v = temp_vault("refreshable");
        v.save_sync_app(
            "whoop",
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        v.save_sync_token("whoop", &token(Some(1_000), true)).unwrap();
        let s = def_status(&v).unwrap();
        assert!(!s.accounts[0].needs_reconnect, "live refresh token + creds → no reconnect needed");
    }

    #[test]
    fn disconnect_forgets_token_only() {
        let v = temp_vault("disconnect");
        v.save_sync_app(
            "whoop",
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        v.save_sync_token("whoop", &token(Some(now_epoch() + 3600), true)).unwrap();
        def_disconnect(&v, "whoop").unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        assert!(s.configured, "disconnect must not forget app credentials");
    }

    #[test]
    fn connection_exposes_oauth_method() {
        assert!(CONNECTION.method("oauth").is_some());
    }

    #[test]
    fn provider_uses_correct_port() {
        assert_eq!(WHOOP.redirect_port, 38669);
    }

    #[test]
    fn fresh_token_without_any_token_errors() {
        let v = temp_vault("notoken");
        let err = fresh_token(&v).unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[test]
    fn whoop_pull_without_token_is_clean_error() {
        let v = temp_vault("pull-notoken");
        let err = v.whoop_pull().unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }
}

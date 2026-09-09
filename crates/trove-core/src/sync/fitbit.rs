//! Fitbit / Google Health API — OAuth connection side.
//!
//! The Google Health API (`health.googleapis.com`) is the successor to the
//! legacy Fitbit Web API (shutdown September 2026). This module owns the auth
//! side only: OAuth connect flow, token store, refresh. The pull itself lives
//! in [`crate::fitbit`] (`Vault::collect_fitbit`).
//!
//! Setup is bring-your-own Google Cloud project: enable the Health API, create
//! an OAuth 2.0 Desktop client, and paste the client id/secret here. The app
//! requires a CASA security review above 100 authorized users — fine at personal
//! scale, a scale gate for wider distribution.
//!
//! Tokens: Google issues a refresh token (with `access_type=offline` and
//! `prompt=consent`), so tokens rotate automatically. A failed refresh flags
//! `needs_reconnect`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::registry::{ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef};
use crate::vault::Vault;

/// All scopes required for the activity, sleep, heart rate, and biometric
/// streams we pull. We request them all up front so connecting is a single
/// consent screen.
const SCOPES: &str = "\
https://www.googleapis.com/auth/googlehealth.activity_and_fitness.readonly \
https://www.googleapis.com/auth/googlehealth.health_metrics_and_measurements.readonly \
https://www.googleapis.com/auth/googlehealth.sleep.readonly";

pub static FITBIT: Provider = Provider {
    service: "fitbit",
    display_name: "Fitbit",
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: SCOPES,
    // Unique production redirect port (38653). Must be registered verbatim in
    // the Google Cloud OAuth client as: http://localhost:38653/callback
    redirect_port: 38653,
    use_pkce: true,
    basic_auth: false,
    // Bake credentials in at build time: TROVE_FITBIT_CLIENT_ID /
    // TROVE_FITBIT_CLIENT_SECRET. Defaults to empty so a build without
    // baked creds falls back to the user pasting their own.
    default_client_id: option_env!("TROVE_FITBIT_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_FITBIT_CLIENT_SECRET"),
    // offline -> issue a refresh token; consent -> re-issue it on every
    // connect (required by Google to get a refresh token on every auth).
    extra_auth_params: &[
        ("access_type", "offline"),
        ("prompt", "consent"),
    ],
};

/// Registered in [`crate::integrations::CONNECTIONS`]. Single-account: the
/// user connects their Google/Fitbit account once; re-running replaces the
/// saved token.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "fitbit",
    display_name: "Fitbit",
    methods: &[ConnectMethod::OAuth {
        provider: &FITBIT,
        multi_account: false,
        run: connect_oauth,
    }],
    status: connect_status,
    disconnect: disconnect,
    auto_pull: &[],
    setup: &[
        "At console.cloud.google.com, create a project and enable the Google Health API.",
        "OAuth consent screen → External, Testing; add your Google account as a test user.",
        "Create credentials → OAuth client ID → type Desktop app. (Desktop clients accept \
         the loopback redirect http://localhost:38653/callback with no console registration.)",
        "Paste the Client ID and Client Secret here. They're saved so every future connect \
         is just a login.",
        "Note: above 100 authorized users, Google requires a CASA security review — fine for \
         personal use.",
    ],
};

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn connect_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(FITBIT.service)?.is_some()
        || FITBIT.default_credentials().is_some();
    let accounts = match vault.load_sync_token(FITBIT.service)? {
        Some(token) => {
            let needs_reconnect = token.expired() && token.refresh_token.is_none();
            vec![ConnectedAccount {
                key: FITBIT.service.to_string(),
                label: "Fitbit".to_string(),
                connected_at: None,
                expires_at: token.expires_at,
                needs_reconnect,
                extra: BTreeMap::new(),
            }]
        }
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

fn disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(FITBIT.service)
}

/// Interactive connect: opens the consent page in the browser, waits for the
/// redirect, saves the token. Blocking — callers off the main thread only.
///
/// Credentials resolve: explicit → previously saved → compiled-in defaults.
/// After the first connect, "reconnect" is just a login (creds are saved).
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(FITBIT.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(FITBIT.service)?
            .or_else(|| FITBIT.default_credentials())
            .context("no Fitbit app credentials — enter them once in the Integrations tab")?,
    };
    let flow = OauthFlow::start(&FITBIT, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(FITBIT.service, &token)?;
    Ok(token)
}

/// The current token, refreshed first if expired. Called at the top of every
/// pull.
pub fn fresh_token(vault: &Vault) -> Result<TokenSet> {
    let token = vault
        .load_sync_token(FITBIT.service)?
        .context("Fitbit is not connected")?;
    if !token.expired() {
        return Ok(token);
    }
    let Some(ref rt) = token.refresh_token else {
        vault.delete_sync_token(FITBIT.service)?;
        bail!("Fitbit token expired — reconnect from the Integrations tab");
    };
    let creds = vault
        .load_sync_app(FITBIT.service)?
        .or_else(|| FITBIT.default_credentials())
        .context("no Fitbit app credentials — reconnect from the Integrations tab")?;
    match oauth::refresh_token(&FITBIT, &creds, &token) {
        Ok(new) => {
            vault.save_sync_token(FITBIT.service, &new)?;
            Ok(new)
        }
        Err(e) => {
            let _ = rt; // silence unused warning — used above via token.refresh_token
            vault.delete_sync_token(FITBIT.service)?;
            Err(e)
                .context("refreshing the Fitbit token failed — reconnect from the Integrations tab")
        }
    }
}

impl Vault {
    /// Pull all Fitbit streams into the vault, unbudgeted (the manual
    /// "Sync now" / first-connect path). Delegates to [`crate::fitbit`].
    pub fn fitbit_pull(&self) -> Result<crate::fitbit::FitbitSyncStats> {
        if self.load_sync_token(FITBIT.service)?.is_none() {
            bail!("Fitbit is not connected");
        }
        self.collect_fitbit(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::oauth::TokenSet;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-fitbit-conn-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
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
    fn status_with_no_token_has_no_accounts() {
        let v = temp_vault("empty");
        let s = connect_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        let baked = option_env!("TROVE_FITBIT_CLIENT_ID").is_some();
        assert_eq!(s.configured, baked);
    }

    #[test]
    fn status_maps_token_into_one_account() {
        let v = temp_vault("connected");
        // Far future: ~2030
        v.save_sync_token("fitbit", &token(Some(1_900_000_000), true)).unwrap();
        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "fitbit");
        assert_eq!(a.label, "Fitbit");
        assert_eq!(a.expires_at, Some(1_900_000_000));
        assert!(!a.needs_reconnect);
    }

    #[test]
    fn expired_token_without_refresh_needs_reconnect() {
        let v = temp_vault("expired");
        v.save_sync_token("fitbit", &token(Some(1_000), false)).unwrap();
        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        assert!(s.accounts[0].needs_reconnect);
    }

    #[test]
    fn expired_token_with_refresh_does_not_need_reconnect_at_status() {
        // A live refresh token means fresh_token() can renew it — status
        // should not flag reconnect just because the access token is stale.
        let v = temp_vault("refreshable");
        v.save_sync_token("fitbit", &token(Some(1_000), true)).unwrap();
        let s = connect_status(&v).unwrap();
        assert!(!s.accounts[0].needs_reconnect);
    }

    #[test]
    fn disconnect_forgets_token_only() {
        let v = temp_vault("disconnect");
        v.save_sync_app(
            "fitbit",
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        v.save_sync_token("fitbit", &token(Some(1_900_000_000), true)).unwrap();
        disconnect(&v, "fitbit").unwrap();
        let s = connect_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        assert!(s.configured, "app credentials survive a disconnect");
    }

    #[test]
    fn provider_uses_correct_port() {
        assert_eq!(FITBIT.redirect_port, 38653);
    }

    #[test]
    fn fresh_token_without_any_token_errors() {
        let v = temp_vault("notoken");
        let err = fresh_token(&v).unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[test]
    fn fitbit_pull_without_token_is_clean_error() {
        let v = temp_vault("pull-notoken");
        let err = v.fitbit_pull().unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }
}

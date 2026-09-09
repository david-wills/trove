//! Withings — OAuth connection side (scales, sleep mat, BP monitors, ScanWatch).
//!
//! This module owns the auth side only: OAuth connect flow, token store,
//! refresh. The pull itself — measures, sleep, activity, heart collections,
//! full-history backfill, keyed upserts — lives in [`crate::withings`]
//! (`Vault::collect_withings`), which the manual pull and the watcher loop's
//! hourly sync both call.
//!
//! ## OAuth details
//!
//! Withings uses the standard OAuth 2.0 authorization-code flow at
//! `account.withings.com` (a single sign-on distinct from the data host
//! `wbsapi.withings.net`). Refresh tokens are long-lived (valid for 1 year);
//! the pull silently refreshes when the access token is within the expiry
//! window, and surfaces a reconnect hint if the refresh token itself lapses.
//!
//! App credentials: bring-your-own from developer.withings.com (free Public
//! API tier) or compiled-in via `TROVE_WITHINGS_CLIENT_ID` /
//! `TROVE_WITHINGS_CLIENT_SECRET`. Confidential client (client_secret in the
//! token exchange body, not Basic auth). PKCE is NOT used — the Withings
//! token endpoint does not support it on the Public API.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::registry::{ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef};
use crate::vault::Vault;

/// The Withings OAuth 2.0 provider. Data lives at `wbsapi.withings.net`;
/// authorization is via `account.withings.com`.
///
/// Scopes requested cover every collection the pull uses:
/// - `user.info`         — account-level profile (used to validate the token)
/// - `user.metrics`      — body measures (scale/BP monitor readings)
/// - `user.activity`     — activity / step data
/// - `user.sleepevents`  — sleep summaries and stage series
/// - `user.heartrate`    — continuous heart rate + ECG recordings
pub static WITHINGS: Provider = Provider {
    service: "withings",
    display_name: "Withings",
    auth_url: "https://account.withings.com/oauth2_user/authorize2",
    token_url: "https://wbsapi.withings.net/v2/oauth2",
    scopes: "user.info,user.metrics,user.activity,user.sleepevents,user.heartrate",
    // Unique assigned production redirect port for this integration.
    // The ports 38573–38579 and 38647/38653/38660/38663/38666 are all taken;
    // 38670 is assigned to withings.
    redirect_port: 38670,
    // Withings is NOT a public/PKCE client — it requires a client_secret.
    use_pkce: false,
    // The Withings token endpoint wants client_id + client_secret as *body*
    // parameters (not Basic auth). The oauth module's `basic_auth = false`
    // path sends them in the form body.
    basic_auth: false,
    // Compiled-in baked credentials for a zero-setup "just log in" experience:
    // TROVE_WITHINGS_CLIENT_ID / TROVE_WITHINGS_CLIENT_SECRET.
    default_client_id: option_env!("TROVE_WITHINGS_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_WITHINGS_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// -- registry adapters -------------------------------------------------------

fn def_connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(WITHINGS.service)
}

/// Status for the connection card: configured when app credentials are saved
/// or compiled in; one account when a token is stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(WITHINGS.service)?.is_some()
        || WITHINGS.default_credentials().is_some();
    let accounts = match vault.load_sync_token(WITHINGS.service)? {
        Some(token) => {
            let needs_reconnect =
                token.expired() && token.refresh_token.is_none();
            let mut extra = BTreeMap::new();
            extra.insert("method", "oauth".to_string());
            vec![ConnectedAccount {
                key: WITHINGS.service.to_string(),
                label: "Withings".to_string(),
                connected_at: None,
                expires_at: token.expires_at,
                needs_reconnect,
                extra,
            }]
        }
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. The integrator must
/// add `&crate::withings::CONNECTION,` to the CONNECTIONS slice.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "withings",
    display_name: "Withings",
    methods: &[ConnectMethod::OAuth {
        provider: &WITHINGS,
        multi_account: false,
        run: def_connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &[],
    setup: &[
        "Go to developer.withings.com and create a free Public API application.",
        "Set its Redirect URI to http://localhost:38670/callback — must match exactly.",
        "Paste the app's Client ID and Client Secret here. \
         They're saved so every future connect is just a login.",
        "Refresh tokens are valid for 1 year; you'll get a hint to reconnect \
         before they lapse.",
    ],
};

// -- connect + token refresh -------------------------------------------------

/// Interactive connect: opens the Withings consent page, waits for the
/// redirect, saves the token. Blocking — callers off the main thread only.
///
/// Credentials resolve: explicitly passed → previously saved → compiled-in.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(WITHINGS.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(WITHINGS.service)?
            .or_else(|| WITHINGS.default_credentials())
            .context(
                "no Withings app credentials — enter them once in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&WITHINGS, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(WITHINGS.service, &token)?;
    Ok(token)
}

/// Return a fresh token, refreshing if the access token is expired.
/// Called at the top of every pull so the collect/pull path is identical.
///
/// A token with a live refresh token is silently refreshed and the rotated
/// token persisted. A token with no refresh token and past expiry bails with
/// a reconnect message.
pub fn fresh_token(vault: &Vault) -> Result<TokenSet> {
    let token = vault
        .load_sync_token(WITHINGS.service)?
        .context("Withings is not connected")?;
    if !token.expired() {
        return Ok(token);
    }
    let Some(_) = token.refresh_token.as_ref() else {
        vault.delete_sync_token(WITHINGS.service)?;
        bail!("Withings token expired — reconnect from the Integrations tab");
    };
    let creds = vault
        .load_sync_app(WITHINGS.service)?
        .or_else(|| WITHINGS.default_credentials())
        .context(
            "no Withings app credentials — reconnect from the Integrations tab",
        )?;
    match oauth::refresh_token(&WITHINGS, &creds, &token) {
        Ok(new) => {
            vault.save_sync_token(WITHINGS.service, &new)?;
            Ok(new)
        }
        Err(e) => {
            vault.delete_sync_token(WITHINGS.service)?;
            Err(e).context(
                "refreshing the Withings token failed — reconnect from the Integrations tab",
            )
        }
    }
}

impl Vault {
    /// Pull all Withings collections into the vault, unbudgeted (the manual
    /// "Sync now" / first-connect path). Delegates to [`crate::withings`].
    pub fn withings_pull(&self) -> Result<crate::withings::WithingsSyncStats> {
        if self.load_sync_token(WITHINGS.service)?.is_none() {
            bail!("Withings is not connected");
        }
        self.collect_withings(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::oauth::TokenSet;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-withings-conn-{}-{name}", std::process::id()));
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
            token_type: Some("Bearer".into()),
            scope: None,
            expires_at,
        }
    }

    #[test]
    fn status_with_no_token_has_no_accounts() {
        let v = temp_vault("empty");
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        let baked = option_env!("TROVE_WITHINGS_CLIENT_ID").is_some();
        assert_eq!(s.configured, baked);
    }

    #[test]
    fn status_maps_token_into_one_account() {
        let v = temp_vault("connected");
        // Far future: ~2030
        v.save_sync_token("withings", &token(Some(1_900_000_000), true)).unwrap();
        let s = def_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "withings");
        assert_eq!(a.label, "Withings");
        assert_eq!(a.expires_at, Some(1_900_000_000));
        assert!(!a.needs_reconnect);
        assert_eq!(a.extra["method"], "oauth");
    }

    #[test]
    fn expired_token_without_refresh_needs_reconnect() {
        let v = temp_vault("expired");
        v.save_sync_token("withings", &token(Some(1_000), false)).unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts[0].needs_reconnect);
    }

    #[test]
    fn expired_token_with_refresh_does_not_need_reconnect_at_status() {
        // A live refresh token means fresh_token() can renew it at pull time.
        let v = temp_vault("refreshable");
        v.save_sync_token("withings", &token(Some(1_000), true)).unwrap();
        let s = def_status(&v).unwrap();
        assert!(!s.accounts[0].needs_reconnect);
    }

    #[test]
    fn disconnect_forgets_token_keeps_app_credentials() {
        let v = temp_vault("disconnect");
        v.save_sync_app(
            "withings",
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        v.save_sync_token("withings", &token(Some(1_900_000_000), true)).unwrap();
        def_disconnect(&v, "withings").unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        assert!(s.configured, "app credentials survive a disconnect");
    }

    #[test]
    fn provider_uses_assigned_redirect_port() {
        assert_eq!(WITHINGS.redirect_port, 38670);
        assert_eq!(WITHINGS.redirect_uri(), "http://localhost:38670/callback");
    }

    #[test]
    fn fresh_token_without_any_token_errors() {
        let v = temp_vault("notoken");
        let err = fresh_token(&v).unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[test]
    fn withings_pull_without_token_is_clean_error() {
        let v = temp_vault("pull-notoken");
        let err = v.withings_pull().unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[test]
    fn fresh_token_returns_unexpired_token_as_is() {
        let v = temp_vault("live");
        let expires = now_epoch() + 3600;
        v.save_sync_token(
            "withings",
            &TokenSet {
                access_token: "live_token".into(),
                refresh_token: Some("rt".into()),
                token_type: Some("Bearer".into()),
                scope: None,
                expires_at: Some(expires),
            },
        )
        .unwrap();
        let tok = fresh_token(&v).unwrap();
        assert_eq!(tok.access_token, "live_token");
    }

    #[test]
    fn connection_exposes_oauth_method() {
        assert!(CONNECTION.method("oauth").is_some());
        assert_eq!(CONNECTION.id, "withings");
    }
}

//! TickTick — the first M5/OAuth source (tasks; the Open API does not expose
//! habits or focus records, those need a different path later).
//!
//! Setup is bring-your-own-app: register an app at
//! <https://developer.ticktick.com> with redirect URI
//! `http://localhost:38573/callback`, then paste the client id/secret into
//! Trove. TickTick access tokens last ~180 days and the API issues no
//! refresh token, so an expired token means reconnecting.
//!
//! This module owns the OAuth side only: app credentials, the interactive
//! connect flow, and the token store. The pull itself — normalized snapshot,
//! diff-based completion events, markdown regeneration — lives in
//! [`crate::tasks`] (`Vault::collect_tasks`), which both the Sync tab's
//! manual pull (below) and the watcher loop's 15-minute background sync
//! call, so there is exactly one writer of `tasks/ticktick/`.
//!
//! Note: the Open API's project list excludes the built-in Inbox; only real
//! projects are pulled.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};

use super::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use super::SyncReport;
use crate::registry::{
    ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef, PullOutcome,
};
use crate::vault::Vault;

pub static TICKTICK: Provider = Provider {
    service: "ticktick",
    display_name: "TickTick",
    auth_url: "https://ticktick.com/oauth/authorize",
    token_url: "https://ticktick.com/oauth/token",
    scopes: "tasks:read",
    redirect_port: 38573,
    use_pkce: false,
    basic_auth: true,
    // Bake credentials in at build time for a zero-setup "just log in"
    // experience: TROVE_TICKTICK_CLIENT_ID / TROVE_TICKTICK_CLIENT_SECRET.
    default_client_id: option_env!("TROVE_TICKTICK_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_TICKTICK_CLIENT_SECRET"),
    extra_auth_params: &[],
};

/// Registered in [`crate::integrations::CONNECTIONS`]. Single-login (no
/// per-account keying): re-connecting replaces the saved token.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "ticktick",
    display_name: "TickTick",
    methods: &[ConnectMethod::OAuth {
        provider: &TICKTICK,
        multi_account: false,
        run: connect_oauth,
    }],
    status: connect_status,
    disconnect: disconnect,
    auto_pull: &[],
    setup: &[
        "Sign in at developer.ticktick.com and create an app (any name).",
        "Set its OAuth redirect URL to http://localhost:38573/callback — must match exactly.",
        "Paste the app's Client ID and Client Secret here. They're saved, so every future connect is just a login.",
    ],
};

/// [`ConnectMethod::OAuth`] adapter: forward to [`connect`] (which owns the
/// explicit → saved → compiled-in credential resolution) and drop the
/// returned token — callers re-read state through the status hook.
fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

/// `configured` mirrors [`Vault::sync_status`]: app credentials saved or
/// compiled in, so connecting is just a login. At most one account; a saved
/// but expired token flags `needs_reconnect` rather than vanishing —
/// TickTick issues no refresh token (~180-day expiry), so expiry always
/// means logging in again.
fn connect_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(TICKTICK.service)?.is_some()
        || TICKTICK.default_credentials().is_some();
    let accounts = match vault.load_sync_token(TICKTICK.service)? {
        Some(token) => {
            let mut extra = BTreeMap::new();
            if let Some(last_sync) = vault
                .sync_status()?
                .into_iter()
                .find(|s| s.service == TICKTICK.service)
                .and_then(|s| s.last_sync)
            {
                extra.insert("last_sync", last_sync);
            }
            vec![ConnectedAccount {
                key: TICKTICK.service.to_string(),
                label: TICKTICK.display_name.to_string(),
                // Not stored anywhere — the token file's mtime would lie
                // after a re-save.
                connected_at: None,
                expires_at: token.expires_at,
                needs_reconnect: token.expired(),
                extra,
            }]
        }
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

/// Forget the token; app credentials are kept so reconnecting is just a
/// login. Single-account, so the key is ignored.
fn disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(TICKTICK.service)
}

/// [`crate::registry::IntegrationDef::pull`] adapter for
/// [`crate::tasks::TICKTICK_DEF`]: the same diff-based pull, mapped into the
/// generic outcome shape. [`SyncReport`] carries snapshot totals (not
/// per-pass deltas), so the headline reports state, not churn.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let report = vault.ticktick_pull()?;
    Ok(PullOutcome {
        headline: format!(
            "{} open tasks across {} projects",
            report.tasks, report.projects
        ),
        counts: BTreeMap::from([
            ("projects", report.projects as u64),
            ("tasks", report.tasks as u64),
        ]),
    })
}

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
            vault.save_sync_app(TICKTICK.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(TICKTICK.service)?
            .or_else(|| TICKTICK.default_credentials())
            .context("no TickTick app credentials — enter them once in the Sync tab")?,
    };
    let flow = OauthFlow::start(&TICKTICK, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(TICKTICK.service, &token)?;
    Ok(token)
}

impl Vault {
    /// Pull all projects + open tasks into the vault. Blocking (network).
    /// Delegates to the diff-based sync in [`crate::tasks`] — a manual pull
    /// and the background 15-minute sync are the same operation.
    pub fn ticktick_pull(&self) -> Result<SyncReport> {
        let token = self
            .load_sync_token(TICKTICK.service)?
            .context("TickTick is not connected")?;
        if token.expired() {
            // No refresh tokens from TickTick — force a clean reconnect.
            self.delete_sync_token(TICKTICK.service)?;
            anyhow::bail!("TickTick token expired — reconnect from the Sync tab");
        }
        let stats = self.collect_tasks()?;
        Ok(SyncReport {
            projects: stats.projects as usize,
            tasks: stats.open as usize,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-ticktick-conn-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn token(expires_at: Option<u64>) -> TokenSet {
        TokenSet {
            access_token: "tok".into(),
            refresh_token: None,
            token_type: Some("bearer".into()),
            scope: Some("tasks:read".into()),
            expires_at,
        }
    }

    #[test]
    fn status_with_no_token_has_no_accounts() {
        let v = temp_vault("empty");
        let s = connect_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        // Build-dependent, same as sync_status: compiled-in defaults count.
        let baked = option_env!("TROVE_TICKTICK_CLIENT_ID").is_some();
        assert_eq!(s.configured, baked);

        v.save_sync_app(
            "ticktick",
            &AppCredentials {
                client_id: "id".into(),
                client_secret: Some("secret".into()),
            },
        )
        .unwrap();
        assert!(connect_status(&v).unwrap().configured);
    }

    #[test]
    fn status_maps_token_into_one_account() {
        let v = temp_vault("connected");
        // Far future: ~2030.
        v.save_sync_token("ticktick", &token(Some(1_900_000_000))).unwrap();
        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "ticktick");
        assert_eq!(a.label, "TickTick");
        assert_eq!(a.connected_at, None);
        assert_eq!(a.expires_at, Some(1_900_000_000));
        assert!(!a.needs_reconnect);
        assert!(a.extra.is_empty(), "no sync recorded yet");

        v.record_sync("ticktick", &SyncReport { projects: 2, tasks: 9 }).unwrap();
        let s = connect_status(&v).unwrap();
        assert!(s.accounts[0].extra.contains_key("last_sync"));
    }

    #[test]
    fn expired_token_flags_reconnect_but_keeps_the_account() {
        // No refresh token from TickTick: expiry must surface as a flagged
        // account, never a silently dropped one.
        let v = temp_vault("expired");
        v.save_sync_token("ticktick", &token(Some(1_000))).unwrap();
        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        assert!(s.accounts[0].needs_reconnect);
    }

    #[test]
    fn disconnect_forgets_token_only() {
        let v = temp_vault("disconnect");
        v.save_sync_app(
            "ticktick",
            &AppCredentials { client_id: "id".into(), client_secret: None },
        )
        .unwrap();
        v.save_sync_token("ticktick", &token(Some(1_900_000_000))).unwrap();

        disconnect(&v, "ticktick").unwrap();
        let s = connect_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        assert!(s.configured, "app credentials survive a disconnect");
    }

    #[test]
    fn pull_without_token_is_a_clean_error() {
        let v = temp_vault("pull-unconnected");
        let err = pull(&v).unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[test]
    fn pull_with_expired_token_demands_reconnect() {
        let v = temp_vault("pull-expired");
        v.save_sync_token("ticktick", &token(Some(1_000))).unwrap();
        let err = pull(&v).unwrap_err();
        assert!(err.to_string().contains("reconnect"), "{err}");
        // ticktick_pull deletes the dead token to force a clean reconnect.
        assert!(v.load_sync_token("ticktick").unwrap().is_none());
    }
}

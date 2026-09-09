//! Generic connect/pull dispatch: the four commands that replace the
//! per-service connect surface.
//!
//! Everything here routes through the two registries —
//! [`crate::integrations::CONNECTIONS`] for logins,
//! [`crate::integrations::INTEGRATIONS`] for pull hooks — so a new
//! login-bearing integration ships its whole connect UI from its module's
//! statics, with no new Tauri command or frontend branch.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};

use crate::integrations::{CONNECTIONS, INTEGRATIONS};
use crate::registry::{ConnectMethod, ConnectStatus, ConnectionDef, ConnectionStatusRow, PullOutcome};
use crate::sync::oauth::AppCredentials;
use crate::vault::Vault;

fn find_connection(id: &str) -> Result<&'static ConnectionDef> {
    CONNECTIONS
        .iter()
        .find(|c| c.id == id)
        .copied()
        .with_context(|| format!("unknown connection: {id}"))
}

impl Vault {
    /// Run one connect method. Blocking (OAuth waits on the loopback
    /// redirect for up to five minutes) — callers off the main thread only.
    ///
    /// `params` for OAuth: optional `client_id`/`client_secret` (first-time
    /// bring-your-own-credentials setup; persisted by the method's own
    /// resolution order). For token-paste: `token`.
    pub fn connect_run(
        &self,
        connection: &str,
        method: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<ConnectStatus> {
        let def = find_connection(connection)?;
        let m = def
            .method(method)
            .with_context(|| format!("{connection} has no {method} method"))?;
        match m {
            ConnectMethod::OAuth { run, .. } => {
                let creds = params.get("client_id").map(|client_id| AppCredentials {
                    client_id: client_id.clone(),
                    client_secret: params.get("client_secret").cloned(),
                });
                run(self, creds)?;
            }
            ConnectMethod::TokenPaste { run, .. } => {
                let token = params
                    .get("token")
                    .map(|t| t.trim())
                    .filter(|t| !t.is_empty())
                    .context("paste the token first")?;
                run(self, token)?;
            }
        }
        (def.status)(self)
    }

    /// Every connection's renderable state, one call for the whole hub.
    /// Cheap to poll — status hooks are file reads.
    pub fn connect_status_all(&self) -> Result<Vec<ConnectionStatusRow>> {
        CONNECTIONS
            .iter()
            .map(|c| {
                Ok(ConnectionStatusRow {
                    id: c.id,
                    display_name: c.display_name,
                    methods: c.methods.iter().map(ConnectMethod::info).collect(),
                    auto_pull: c.auto_pull,
                    setup: c.setup,
                    status: (c.status)(self)?,
                })
            })
            .collect()
    }

    /// Forget one account (`key` from [`ConnectStatus`]'s rows). Synced
    /// data stays in the vault.
    pub fn connect_disconnect(&self, connection: &str, key: &str) -> Result<ConnectStatus> {
        let def = find_connection(connection)?;
        (def.disconnect)(self, key)?;
        (def.status)(self)
    }

    /// Manual "Sync now" for one integration, off the def's `pull` hook.
    /// Blocking (network) — callers off the main thread only.
    pub fn integration_pull(&self, id: &str) -> Result<PullOutcome> {
        let def = INTEGRATIONS
            .iter()
            .find(|d| d.id == id)
            .with_context(|| format!("unknown integration: {id}"))?;
        let Some(pull) = def.pull else {
            bail!("{id} has no manual pull");
        };
        pull(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-connect-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    #[test]
    fn unknown_ids_are_rejected() {
        let v = temp_vault("unknown");
        assert!(v.connect_run("not-a-thing", "oauth", &BTreeMap::new()).is_err());
        assert!(v.connect_disconnect("not-a-thing", "x").is_err());
        assert!(v.integration_pull("not-a-thing").is_err());
        // A real integration without a pull hook is a clean error too.
        assert!(v.integration_pull("letterboxd").is_err());
    }

    #[test]
    fn status_all_covers_every_connection() {
        let v = temp_vault("status");
        let rows = v.connect_status_all().unwrap();
        assert_eq!(rows.len(), CONNECTIONS.len());
    }
}

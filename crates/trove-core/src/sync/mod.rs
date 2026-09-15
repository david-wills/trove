//! Cloud API pull ("M5" in docs/data-sources.md): OAuth/token pull of the
//! user's own data from a service, explicit opt-in per source.
//!
//! Secrets (app credentials, tokens) live under `.trove/sync/` with 0600
//! permissions — never inside the data folders, so the vault's data files
//! stay safe to share or commit. Pulled data is written to normal vault
//! folders (e.g. `tasks/ticktick/`) and each pull replaces the previous
//! snapshot, mirroring how health re-imports replace.
pub mod google;
pub mod oauth;
pub mod oura;
pub mod ticktick;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::vault::Vault;
use oauth::{AppCredentials, Provider, TokenSet};

/// Every service Trove knows how to sync. Order is display order.
static PROVIDERS: [&Provider; 2] = [&ticktick::TICKTICK, &oura::OURA];

pub fn providers() -> &'static [&'static Provider] {
    &PROVIDERS
}

/// What the UI needs to render a service row.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SyncStatus {
    pub service: String,
    pub display_name: String,
    /// App credentials (client id/secret) saved.
    pub configured: bool,
    /// OAuth token saved.
    pub connected: bool,
    /// Token expiry, seconds since the Unix epoch (None = unknown/none).
    pub expires_at: Option<u64>,
    /// RFC3339 local time of the last successful pull.
    pub last_sync: Option<String>,
    /// Item counts from the last successful pull.
    pub last_report: Option<SyncReport>,
}

/// Outcome of one pull.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SyncReport {
    pub projects: usize,
    pub tasks: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncRecord {
    last_sync: String,
    #[serde(flatten)]
    report: SyncReport,
}

impl Vault {
    /// `.trove/sync`, created on demand.
    pub(crate) fn sync_dir(&self) -> Result<PathBuf> {
        let dir = self.root().join(".trove").join("sync");
        fs::create_dir_all(&dir).context("creating .trove/sync")?;
        Ok(dir)
    }

    fn sync_secret_path(&self, service: &str, kind: &str) -> Result<PathBuf> {
        check_service_name(service)?;
        Ok(self.sync_dir()?.join(format!("{service}-{kind}.json")))
    }

    /// Save the user's own OAuth app credentials for `service` (0600).
    pub fn save_sync_app(&self, service: &str, creds: &AppCredentials) -> Result<()> {
        write_secret(&self.sync_secret_path(service, "app")?, creds)
    }

    pub fn load_sync_app(&self, service: &str) -> Result<Option<AppCredentials>> {
        read_secret(&self.sync_secret_path(service, "app")?)
    }

    /// Save the OAuth token for `service` (0600).
    pub fn save_sync_token(&self, service: &str, token: &TokenSet) -> Result<()> {
        write_secret(&self.sync_secret_path(service, "token")?, token)
    }

    pub fn load_sync_token(&self, service: &str) -> Result<Option<TokenSet>> {
        read_secret(&self.sync_secret_path(service, "token")?)
    }

    /// Disconnect: forget the token (app credentials are kept).
    pub fn delete_sync_token(&self, service: &str) -> Result<()> {
        let path = self.sync_secret_path(service, "token")?;
        if path.exists() {
            fs::remove_file(&path).context("deleting token")?;
        }
        Ok(())
    }

    /// One row per known service, for the UI.
    pub fn sync_status(&self) -> Result<Vec<SyncStatus>> {
        let records = self.read_sync_records()?;
        providers()
            .iter()
            .map(|p| {
                let token = self.load_sync_token(p.service)?;
                let record = records.get(p.service);
                Ok(SyncStatus {
                    service: p.service.to_string(),
                    display_name: p.display_name.to_string(),
                    configured: self.load_sync_app(p.service)?.is_some()
                        || p.default_credentials().is_some(),
                    connected: token.is_some(),
                    expires_at: token.and_then(|t| t.expires_at),
                    last_sync: record.map(|r| r.last_sync.clone()),
                    last_report: record.map(|r| r.report.clone()),
                })
            })
            .collect()
    }

    /// Record a successful pull (drives `last_sync` in the UI).
    pub(crate) fn record_sync(&self, service: &str, report: &SyncReport) -> Result<()> {
        check_service_name(service)?;
        let mut records = self.read_sync_records()?;
        records.insert(
            service.to_string(),
            SyncRecord {
                last_sync: chrono::Local::now().to_rfc3339(),
                report: report.clone(),
            },
        );
        let path = self.sync_dir()?.join("state.json");
        fs::write(&path, serde_json::to_string_pretty(&records)?)
            .context("writing sync state.json")
    }

    fn read_sync_records(&self) -> Result<BTreeMap<String, SyncRecord>> {
        let path = self.sync_dir()?.join("state.json");
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let raw = fs::read_to_string(&path).context("reading sync state.json")?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }
}

/// Service names become file names; keep them boring.
fn check_service_name(service: &str) -> Result<()> {
    if service.is_empty()
        || !service
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("invalid service name: {service}");
    }
    Ok(())
}

pub(crate) fn write_secret<T: Serialize>(path: &PathBuf, value: &T) -> Result<()> {
    // Atomic + 0600-before-rename so a crash or concurrent reader can never
    // see a torn or briefly world-readable secret. This matters most for
    // rotated refresh tokens (Oura's are single-use): a torn token file
    // would strand the account until the user reconnects.
    crate::store::write_atomic_secret(path, serde_json::to_string_pretty(value)?.as_bytes())
}

pub(crate) fn read_secret<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(serde_json::from_str(&raw).with_context(|| {
        format!("parsing {}", path.display())
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-test-sync-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    #[test]
    fn token_roundtrip_and_disconnect() {
        let v = temp_vault("token");
        assert!(v.load_sync_token("ticktick").unwrap().is_none());

        let token = TokenSet {
            access_token: "abc".into(),
            refresh_token: Some("def".into()),
            token_type: Some("bearer".into()),
            scope: Some("tasks:read".into()),
            expires_at: Some(1_900_000_000),
        };
        v.save_sync_token("ticktick", &token).unwrap();
        let loaded = v.load_sync_token("ticktick").unwrap().unwrap();
        assert_eq!(loaded.access_token, "abc");
        assert_eq!(loaded.expires_at, Some(1_900_000_000));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = v.sync_secret_path("ticktick", "token").unwrap();
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be 0600");
        }

        v.delete_sync_token("ticktick").unwrap();
        assert!(v.load_sync_token("ticktick").unwrap().is_none());
    }

    #[test]
    fn rejects_sketchy_service_names() {
        let v = temp_vault("names");
        assert!(v.load_sync_token("../escape").is_err());
        assert!(v.load_sync_token("Tick Tick").is_err());
        assert!(v.load_sync_token("").is_err());
    }

    #[test]
    fn status_reflects_state() {
        let v = temp_vault("status");
        let status = v.sync_status().unwrap();
        let tt = status.iter().find(|s| s.service == "ticktick").unwrap();
        // `configured` is build-dependent here: TROVE_TICKTICK_* env vars at
        // compile time bake in default credentials (option_env!, e.g. from
        // David's ~/.zshrc in interactive shells), so a fresh vault reports
        // configured on a credentialed build.
        let baked = option_env!("TROVE_TICKTICK_CLIENT_ID").is_some();
        assert_eq!(tt.configured, baked);
        assert!(!tt.connected && tt.last_sync.is_none());

        v.save_sync_app(
            "ticktick",
            &AppCredentials {
                client_id: "id".into(),
                client_secret: Some("secret".into()),
            },
        )
        .unwrap();
        v.record_sync("ticktick", &SyncReport { projects: 2, tasks: 9 }).unwrap();

        let status = v.sync_status().unwrap();
        let tt = status.iter().find(|s| s.service == "ticktick").unwrap();
        assert!(tt.configured && !tt.connected);
        assert!(tt.last_sync.is_some());
        assert_eq!(tt.last_report.as_ref().unwrap().tasks, 9);
    }
}

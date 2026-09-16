//! The integrations registry: every way data gets into the vault, as data.
//!
//! Each integration is defined by one [`crate::registry::IntegrationDef`]
//! static living in the module that owns its collection logic — metadata and
//! behavior together — and registered here with a single line in
//! [`INTEGRATIONS`]. User choices live in `.trove/integrations.json` as a
//! *disabled set* — anything not listed is enabled, so new integrations
//! default on and the file stays tiny. The sync loop in `runner.rs` (and the
//! external `trove-collector`, which re-reads the same file) consult
//! [`Vault::integration_enabled`] each pass, so toggling off actually stops
//! collection rather than just hiding a card.
//!
//! [`Vault::integrations_status`] is the one read the hub UI needs: the
//! catalog joined with the enabled flag, a permission preflight, and a
//! cheap "when did data last land" probe per integration.

use std::collections::BTreeSet;
use std::fs;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::registry::{ConnectionDef, IntegrationDef};
use crate::vault::Vault;

const SETTINGS_FILE: &str = ".trove/integrations.json";

/// How an integration runs — drives which controls the hub shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum IntegrationKind {
    /// Always-on capture by the external collector process; the stream only
    /// exists while it runs.
    Live,
    /// Periodic read of another local app's own store (copy-then-read).
    LocalSync,
    /// Periodic pull from a cloud API (OAuth / token).
    CloudSync,
    /// User-triggered file import; nothing runs in the background.
    Import,
}

/// macOS permission an integration depends on, with its preflight result.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PermissionInfo {
    /// "screen-recording" | "full-disk-access" | "media-library"
    pub kind: &'static str,
    /// Preflight from *this* process — `None` when it can't be checked
    /// cheaply (Media Library only fails at read time). The grant is
    /// per-binary; this reflects the app.
    pub granted: Option<bool>,
    /// `false` = the integration works without it, just with less detail
    /// (e.g. activity runs app-level without Screen Recording).
    pub required: bool,
}

/// A catalog entry: everything static about one integration. Owned by the
/// integration's module as the `meta` of its `IntegrationDef`.
pub struct Integration {
    /// Stable id; also the settings key. Lowercase/digits/dash only.
    pub id: &'static str,
    pub name: &'static str,
    pub kind: IntegrationKind,
    /// Whether the integration runs before the user touches anything.
    /// `false` = deliberate opt-in (stored in the settings' enabled set);
    /// reserve it for collectors that duplicate another source by design
    /// (e.g. the this-Mac Screen Time backup behind the live watcher).
    pub default_on: bool,
    /// One- or two-sentence card copy.
    pub description: &'static str,
    /// Taxonomy domain token that drives hub grouping: the FIRST path
    /// segment of the integration's vault folder per the taxonomy table in
    /// `docs/integrations/README.md` (`health/nutrition/` → "health",
    /// `browser/searches/` → "browser", `media/plays/` → "media").
    /// Grandfathered paths use the domain the table catalogues them under
    /// (`weather/` → "environment", `youtube/` → "media").
    pub domain: &'static str,
    /// Where its data lands, vault-relative, for display.
    pub vault_path: &'static str,
    /// Imports have nothing to toggle — they only run when invoked.
    pub toggleable: bool,
    /// Step-by-step instructions to get the integration working, one step
    /// per entry. Empty = works out of the box. The hub renders these as a
    /// collapsible numbered list — this is the home for setup knowledge, not
    /// docs or code comments.
    pub setup: &'static [&'static str],
    /// What to know before relying on this source: retention limits, API
    /// gaps, manual-grant quirks, what can't be backfilled. Empty = none.
    pub caveats: &'static str,
}

/// Every integration Trove knows, one `&DEF` per line — this list is the
/// whole registration. Order is display order; the hub groups by `kind`.
/// Each def lives in the module that owns the integration's logic.
pub static INTEGRATIONS: &[&IntegrationDef] = &[
    &crate::activity::DEF,
    &crate::music::SCROBBLER_DEF,
    &crate::browser_ext::DEF,
    &crate::ads::DEF,
    &crate::ads::IDENTIFY_DEF,
    &crate::browser::CHROME_DEF,
    &crate::browser::SAFARI_DEF,
    &crate::imessage::DEF,
    &crate::calls::DEF,
    &crate::music_library::DEF,
    &crate::podcasts::DEF,
    &crate::books::DEF,
    &crate::screen_time::SCREEN_TIME_DEF,
    &crate::screen_time::NOWPLAYING_DEF,
    &crate::screen_time::THIS_MAC_DEF,
    &crate::calendar::DEF,
    &crate::tasks::REMINDERS_DEF,
    &crate::tasks::TICKTICK_DEF,
    &crate::finance::BANK_SYNC_DEF,
    &crate::finance::CSV_IMPORT_DEF,
    &crate::oura::DEF,
    &crate::gmail::DEF,
    &crate::google_calendar::DEF,
    &crate::google_contacts::DEF,
    &crate::google_tasks::DEF,
    &crate::youtube::DEF,
    &crate::google_books::DEF,
    &crate::weather::DEF,
    &crate::health::DEF,
    &crate::email::DEF,
    &crate::slack::DEF,
    &crate::letterboxd::DEF,
    // Pruned 2026-09-14 (docs/roadmap.md): the full briefed catalog lives in
    // docs/integrations/INDEX.md; 📦 rows are restorable per its header.
    &crate::macos_screenshots::DEF,
    &crate::apple_mail::DEF,
    &crate::dropbox::DEF,
    &crate::google_drive::DEF,
    &crate::icloud_drive::DEF,
    &crate::macos_downloads::DEF,
    &crate::fathom::DEF,
];

/// Every login Trove knows, one `&CONNECTION` per line — the connect-phase
/// counterpart of [`INTEGRATIONS`]. Each def lives in the module that owns
/// the service's auth logic; integrations reference one by id via
/// [`IntegrationDef::connection`].
pub static CONNECTIONS: &[&ConnectionDef] = &[
    &crate::sync::ticktick::CONNECTION,
    &crate::sync::oura::CONNECTION,
    &crate::sync::google::CONNECTION,
    &crate::finance::simplefin::CONNECTION,
    &crate::fathom::CONNECTION,
];

/// The connection a def references, resolved. `None` = no login needed.
pub fn connection_of(def: &IntegrationDef) -> Option<&'static ConnectionDef> {
    let id = def.connection?;
    CONNECTIONS.iter().find(|c| c.id == id).copied()
}

/// User choices, stored as two small sets so the file stays
/// self-explanatory: `disabled` opts out of default-on integrations (so new
/// collectors default on), `enabled` opts *in* to the few default-off ones.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntegrationSettings {
    #[serde(default)]
    pub disabled: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub enabled: BTreeSet<String>,
}

/// Is `id` on under these settings? Shared by [`Vault::integration_enabled`]
/// and the runner's per-tick closure so the default-off rule can't drift.
pub(crate) fn enabled_in(settings: &IntegrationSettings, id: &str) -> bool {
    match INTEGRATIONS.iter().find(|i| i.id == id) {
        Some(i) if i.unavailable_reason().is_some() => false,
        Some(i) if !i.default_on => settings.enabled.contains(id),
        _ => !settings.disabled.contains(id),
    }
}

/// What the hub renders: one row per catalog entry plus live state.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct IntegrationStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: IntegrationKind,
    pub description: &'static str,
    /// Taxonomy domain token the hub groups by (see [`Integration::domain`]).
    pub domain: &'static str,
    pub vault_path: &'static str,
    pub toggleable: bool,
    /// Catalogued but not built yet ([`crate::registry::Behavior::NotWired`]):
    /// the hub shows a "planned" badge and no controls — nothing runs until
    /// a collector is wired.
    pub planned: bool,
    pub setup: &'static [&'static str],
    pub caveats: &'static str,
    pub enabled: bool,
    /// Why this source can't be built, for catalogued-but-unavailable
    /// entries ([`crate::registry::Behavior::Unavailable`]). `Some` = the
    /// hub greys the card, hides every control, and shows this copy.
    pub unavailable_reason: Option<&'static str>,
    /// Permission this integration depends on, if any.
    pub permission: Option<PermissionInfo>,
    /// When data last landed (or last sync pass ran): RFC3339 local time or
    /// a bare date, whichever the cheap probe yields. `None` = no data yet.
    pub last_data: Option<String>,
    /// Present for file imports: what the hub's generic import box needs
    /// (accepted extensions + param fields), straight off the def.
    pub import: Option<crate::registry::ImportInfo>,
    /// The connection (login) this integration needs, by id — the hub joins
    /// it against `connect_status_all`. `None` = no login.
    pub connection: Option<&'static str>,
    /// Has a manual "Sync now" hook (the generic `integration_pull`).
    pub pullable: bool,
}

impl Vault {
    /// The persisted enable/disable choices (missing file = all enabled).
    pub fn integration_settings(&self) -> IntegrationSettings {
        let Ok(path) = self.resolve(SETTINGS_FILE) else {
            return IntegrationSettings::default();
        };
        let mut settings: IntegrationSettings = fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        // "browser-history" predates the per-browser split — honor an old
        // disable by mapping it onto both halves (normalized on every read;
        // the file rewrites itself on the next toggle).
        if settings.disabled.remove("browser-history") {
            settings.disabled.insert("chrome-history".into());
            settings.disabled.insert("safari-history".into());
        }
        settings
    }

    /// Is `id` enabled? Cheap (one tiny file read) — the collector loops call
    /// this every pass so toggles take effect without a restart.
    pub fn integration_enabled(&self, id: &str) -> bool {
        enabled_in(&self.integration_settings(), id)
    }

    /// Persist a toggle. Unknown ids are rejected so typos can't silently
    /// store a no-op choice.
    pub fn set_integration_enabled(&self, id: &str, enabled: bool) -> Result<IntegrationSettings> {
        let Some(integration) = INTEGRATIONS.iter().find(|i| i.id == id) else {
            bail!("unknown integration: {id}");
        };
        if let Some(reason) = integration.unavailable_reason() {
            bail!("{id} is not available: {reason}");
        }
        let mut settings = self.integration_settings();
        if integration.default_on {
            if enabled {
                settings.disabled.remove(id);
            } else {
                settings.disabled.insert(id.to_string());
            }
        } else if enabled {
            settings.enabled.insert(id.to_string());
        } else {
            settings.enabled.remove(id);
        }
        let path = self.resolve(SETTINGS_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&settings)?)
            .context("writing integrations settings")?;
        fs::rename(&tmp, &path).context("publishing integrations settings")?;
        Ok(settings)
    }

    /// One row per known integration: catalog + enabled flag + permission
    /// preflight + last-data probe (both straight off the def's hooks).
    /// Everything here is cheap (file stats and open-for-read checks) — safe
    /// for the UI to poll.
    pub fn integrations_status(&self) -> Vec<IntegrationStatus> {
        let settings = self.integration_settings();
        INTEGRATIONS
            .iter()
            .map(|i| IntegrationStatus {
                id: i.id,
                name: i.name,
                kind: i.kind,
                description: i.description,
                domain: i.domain,
                vault_path: i.vault_path,
                toggleable: i.toggleable,
                planned: matches!(i.behavior, crate::registry::Behavior::NotWired),
                setup: i.setup,
                caveats: i.caveats,
                enabled: enabled_in(&settings, i.id),
                unavailable_reason: i.unavailable_reason(),
                permission: i.permission.map(|f| f()),
                last_data: i.last_data.and_then(|f| f(self)),
                import: i.import_spec().map(|s| crate::registry::ImportInfo {
                    accepts: s.accepts,
                    params: s.params,
                }),
                connection: i.connection,
                pullable: i.pull.is_some(),
            })
            .collect()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-integrations-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    #[test]
    fn everything_enabled_by_default_except_opt_ins() {
        let v = temp_vault("default");
        for i in INTEGRATIONS {
            assert_eq!(
                v.integration_enabled(i.id),
                i.default_on,
                "{} default state",
                i.id
            );
        }
        let status = v.integrations_status();
        assert!(status
            .iter()
            .all(|s| s.enabled == INTEGRATIONS.iter().find(|i| i.id == s.id).unwrap().default_on));
    }

    #[test]
    fn default_off_integration_is_a_real_opt_in() {
        let v = temp_vault("optin");
        assert!(!v.integration_enabled("screen-time-this-mac"));
        v.set_integration_enabled("screen-time-this-mac", true).unwrap();
        assert!(v.integration_enabled("screen-time-this-mac"));
        // The opt-in lands in the enabled set, not as a disabled-set removal.
        assert!(v.integration_settings().enabled.contains("screen-time-this-mac"));
        v.set_integration_enabled("screen-time-this-mac", false).unwrap();
        assert!(!v.integration_enabled("screen-time-this-mac"));
        assert!(v.integration_settings().enabled.is_empty());
        // Default-on neighbors are untouched throughout.
        assert!(v.integration_enabled("screen-time"));
    }

    #[test]
    fn toggle_round_trip() {
        let v = temp_vault("toggle");
        v.set_integration_enabled("podcasts", false).unwrap();
        assert!(!v.integration_enabled("podcasts"));
        assert!(v.integration_enabled("books"), "other ids unaffected");

        let status = v.integrations_status();
        let podcasts = status.iter().find(|s| s.id == "podcasts").unwrap();
        assert!(!podcasts.enabled);

        v.set_integration_enabled("podcasts", true).unwrap();
        assert!(v.integration_enabled("podcasts"));
        assert!(v.integration_settings().disabled.is_empty());
    }

    #[test]
    fn rejects_unknown_ids() {
        let v = temp_vault("unknown");
        assert!(v.set_integration_enabled("not-a-thing", false).is_err());
    }


    #[test]
    fn settings_survive_unknown_entries() {
        // A settings file naming a since-removed integration must not break
        // parsing (forward/backward compatibility across versions).
        let v = temp_vault("forward");
        v.set_integration_enabled("books", false).unwrap();
        let path = v.root().join(SETTINGS_FILE);
        let raw = fs::read_to_string(&path).unwrap();
        let patched = raw.replace("\"books\"", "\"books\", \"retired-source\"");
        fs::write(&path, patched).unwrap();
        assert!(!v.integration_enabled("books"));
        assert!(!v.integration_enabled("retired-source"));
        // Toggling still works and keeps the unknown entry.
        v.set_integration_enabled("books", true).unwrap();
        assert!(v.integration_settings().disabled.contains("retired-source"));
    }

    #[test]
    fn legacy_browser_history_disable_maps_to_both_browsers() {
        let v = temp_vault("legacy-browser");
        v.set_integration_enabled("books", false).unwrap();
        let path = v.root().join(SETTINGS_FILE);
        let raw = fs::read_to_string(&path).unwrap();
        fs::write(&path, raw.replace("\"books\"", "\"books\", \"browser-history\"")).unwrap();
        assert!(!v.integration_enabled("chrome-history"));
        assert!(!v.integration_enabled("safari-history"));
        // Re-enabling one half persists the migrated form and keeps the other off.
        v.set_integration_enabled("chrome-history", true).unwrap();
        assert!(v.integration_enabled("chrome-history"));
        assert!(!v.integration_enabled("safari-history"));
        assert!(!v.integration_settings().disabled.contains("browser-history"));
    }

    #[test]
    fn catalog_ids_are_unique_and_well_formed() {
        // The catalog is meant to grow a lot — guard the invariants every
        // entry must hold (ids are settings keys and future filenames).
        let mut seen = BTreeSet::new();
        for i in INTEGRATIONS {
            assert!(
                !i.id.is_empty()
                    && i.id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "bad id: {}",
                i.id
            );
            assert!(seen.insert(i.id), "duplicate id: {}", i.id);
            // Out-of-scope sources (Behavior::Unavailable) get no folder —
            // an empty vault_path is legal for them only.
            if i.vault_path.is_empty() {
                assert!(
                    i.unavailable_reason().is_some(),
                    "{}: only unavailable entries may omit a vault_path",
                    i.id
                );
            } else {
                assert!(i.vault_path.ends_with('/'), "{}: vault_path should be a dir", i.id);
            }
        }
    }

    #[test]
    fn registry_behaviors_match_kinds() {
        use crate::registry::Behavior;
        // The type system already makes nonsense hook combinations
        // unrepresentable; what's left is the kind label (a display grouping)
        // agreeing with the behavior shape, covered-by edges resolving, and
        // cadences the poll can actually serve.
        let shape = |b: &Behavior| match b {
            Behavior::Periodic { .. } => "Periodic",
            Behavior::CoveredBy(_) => "CoveredBy",
            Behavior::External { .. } => "External",
            Behavior::Import(_) => "Import",
            Behavior::NotWired => "NotWired",
            Behavior::Unavailable { .. } => "Unavailable",
        };
        for d in INTEGRATIONS {
            match (d.kind, &d.behavior) {
                // Unavailable is honest under any kind (the kind says what
                // it *would* be); nothing runs, so nothing is toggleable.
                (_, Behavior::Unavailable { reason }) => {
                    assert!(!reason.is_empty(), "{}: unavailable needs a reason", d.id);
                    assert!(
                        !d.toggleable && !d.default_on,
                        "{}: unavailable entries can't be toggled or default on",
                        d.id
                    );
                }
                // NotWired is honest under any kind too (the kind says what
                // it *would* be); nothing runs until a collector is wired,
                // so there's nothing to toggle and nothing defaults on.
                (_, Behavior::NotWired) => {
                    assert!(
                        !d.toggleable && !d.default_on,
                        "{}: not-wired entries can't be toggled or default on",
                        d.id
                    );
                }
                (IntegrationKind::Import, Behavior::Import(_)) => assert!(
                    !d.toggleable,
                    "{}: imports have nothing to toggle",
                    d.id
                ),
                (IntegrationKind::Live, Behavior::External { collector }) => {
                    assert!(!collector.is_empty(), "{}: external needs a collector id", d.id)
                }
                (
                    IntegrationKind::LocalSync,
                    Behavior::Periodic { .. } | Behavior::CoveredBy(_),
                ) => {}
                (
                    IntegrationKind::CloudSync,
                    Behavior::Periodic { .. } | Behavior::CoveredBy(_),
                ) => {}
                (kind, b) => {
                    panic!("{}: kind {kind:?} can't carry behavior {}", d.id, shape(b))
                }
            }
            if let Behavior::CoveredBy(owner) = d.behavior {
                let o = INTEGRATIONS
                    .iter()
                    .find(|o| o.id == owner)
                    .unwrap_or_else(|| {
                        panic!("{}: covered-by target {owner} is not a catalog id", d.id)
                    });
                assert!(
                    matches!(o.behavior, Behavior::Periodic { .. }),
                    "{}: covered-by target {owner} has no periodic pass",
                    d.id
                );
            }
            if let Behavior::Periodic { cadence, .. } = d.behavior {
                assert!(
                    cadence.every_secs >= crate::runner::POLL_SECS,
                    "{}: cadence faster than the runner poll",
                    d.id
                );
            }
        }
    }

    #[test]
    fn connections_are_well_formed() {
        // The connect-phase counterpart of the catalog invariants: ids
        // unique and well-formed, every method renderable, every reference
        // between the two registries resolving.
        let mut seen = BTreeSet::new();
        for c in CONNECTIONS {
            assert!(
                !c.id.is_empty()
                    && c.id
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
                "bad connection id: {}",
                c.id
            );
            assert!(seen.insert(c.id), "duplicate connection id: {}", c.id);
            assert!(!c.display_name.is_empty(), "{}: empty display name", c.id);
            assert!(!c.methods.is_empty(), "{}: a connection needs at least one method", c.id);
            let mut kinds = BTreeSet::new();
            for m in c.methods {
                assert!(
                    kinds.insert(m.key()),
                    "{}: duplicate {} method — the connect command selects by kind",
                    c.id,
                    m.key()
                );
                if let crate::registry::ConnectMethod::TokenPaste { label, help, placeholder, .. } = m {
                    assert!(
                        !label.is_empty() && !help.is_empty() && !placeholder.is_empty(),
                        "{}: token-paste method needs label, help, and placeholder copy",
                        c.id
                    );
                }
            }
            // A connection nobody references is dead weight.
            assert!(
                INTEGRATIONS.iter().any(|d| d.connection == Some(c.id)),
                "{}: no integration references this connection",
                c.id
            );
            for id in c.auto_pull {
                let d = INTEGRATIONS
                    .iter()
                    .find(|d| d.id == *id)
                    .unwrap_or_else(|| panic!("{}: auto_pull id {id} is not a catalog id", c.id));
                assert_eq!(
                    d.connection,
                    Some(c.id),
                    "{}: auto_pull target {id} references a different connection",
                    c.id
                );
                assert!(d.pull.is_some(), "{}: auto_pull target {id} has no pull hook", c.id);
            }
        }
        for d in INTEGRATIONS {
            if let Some(conn) = d.connection {
                assert!(
                    CONNECTIONS.iter().any(|c| c.id == conn),
                    "{}: connection {conn} is not registered",
                    d.id
                );
                assert_eq!(
                    d.kind,
                    IntegrationKind::CloudSync,
                    "{}: only cloud syncs carry a connection",
                    d.id
                );
            }
        }
    }

    #[test]
    fn status_covers_the_whole_catalog() {
        let v = temp_vault("coverage");
        let status = v.integrations_status();
        assert_eq!(status.len(), INTEGRATIONS.len());
        // Imports never show a toggle.
        for s in &status {
            if matches!(s.kind, IntegrationKind::Import) {
                assert!(!s.toggleable, "{} should not be toggleable", s.id);
            }
        }
    }

    #[test]
    fn last_data_probes_the_vault() {
        let v = temp_vault("lastdata");
        let none = v.integrations_status();
        assert!(none.iter().find(|s| s.id == "activity").unwrap().last_data.is_none());

        fs::create_dir_all(v.root().join("activity")).unwrap();
        fs::write(v.root().join("activity/2026-06-10.jsonl"), "{}\n").unwrap();
        fs::write(v.root().join("activity/2026-06-11.jsonl"), "{}\n").unwrap();
        let status = v.integrations_status();
        let activity = status.iter().find(|s| s.id == "activity").unwrap();
        assert_eq!(activity.last_data.as_deref(), Some("2026-06-11"));
    }
}

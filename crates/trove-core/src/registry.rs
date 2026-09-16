//! The integration definition: metadata plus behavior, one static per module.
//!
//! Every integration is described by one [`IntegrationDef`] living in the
//! module that owns its collection logic, and registered with a single line
//! in [`crate::integrations::INTEGRATIONS`]. The def carries the static
//! catalog metadata ([`Integration`]), the one [`Behavior`] shape that says
//! how it runs — a periodic collect pass with its [`Cadence`], coverage by
//! another def's pass, an external collector process, a file
//! [`ImportSpec`], or nothing yet — plus the hooks that apply
//! to any shape: a permission preflight and a cheap "when did data last
//! land" probe. One shape per def means an impossible combination (an import
//! with a cadence, a live collector with a collect hook) simply doesn't
//! compile.
//!
//! Hooks are plain `fn` pointers (not closures — `static` initializers need
//! named, capture-free functions): all I/O state rides on `&Vault` and
//! gating state lives in the runner's per-id job state.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Serialize;

use crate::health::ImportProgress;
use crate::integrations::{Integration, PermissionInfo};
use crate::vault::Vault;

/// When the runner's timer for a due-but-skipped pass advances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// The window is consumed whenever it comes due, even while the toggle is
    /// off — re-enabling waits out the rest of the current window. (The
    /// historical browser/tasks-block semantics.)
    Due,
    /// The timer advances only when the pass actually runs — re-enabling
    /// fires within one poll. (The historical oura/gmail semantics.)
    Run,
}

/// An extra condition on top of the periodic timer. Gates commit their state
/// only on a *successful* pass, so a failed read (e.g. a TCC denial) retries
/// on the next due tick rather than waiting out the gate.
#[derive(Clone, Copy)]
pub enum Gate {
    /// Run on every due tick (the collector may still self-gate internally).
    Always,
    /// Only the first successful pass of each local day.
    LocalDay,
    /// Only when the probed source mtime differs from the last successful
    /// pass's. Probed *before* collecting, so a write racing the copy
    /// re-triggers next tick.
    SourceMtime(fn() -> Option<SystemTime>),
}

/// How often (and under what gate) a collect hook runs.
#[derive(Clone, Copy)]
pub struct Cadence {
    /// Minimum seconds between attempts (the runner polls every
    /// [`crate::runner::POLL_SECS`]).
    pub every_secs: u64,
    pub advance: Advance,
    pub gate: Gate,
}

impl Cadence {
    /// Plain periodic pass; timer advances on due (window consumed even
    /// while disabled).
    pub const fn every(secs: u64) -> Self {
        Cadence { every_secs: secs, advance: Advance::Due, gate: Gate::Always }
    }

    /// Periodic pass whose timer advances only on a real run (re-enable
    /// fires within one poll).
    pub const fn every_on_run(secs: u64) -> Self {
        Cadence { every_secs: secs, advance: Advance::Run, gate: Gate::Always }
    }

    /// Checked every `secs`, but runs at most once per local day.
    pub const fn daily(secs: u64) -> Self {
        Cadence { every_secs: secs, advance: Advance::Due, gate: Gate::LocalDay }
    }

    /// Checked every `secs`, but runs only when the probed source changed.
    pub const fn on_change(secs: u64, probe: fn() -> Option<SystemTime>) -> Self {
        Cadence { every_secs: secs, advance: Advance::Due, gate: Gate::SourceMtime(probe) }
    }
}

/// Outcome of one collect pass: at most one log line for the watcher log.
/// `None` = nothing noteworthy happened (stay silent, like today's
/// `Ok(_) => {}` arms).
pub struct CollectOutcome {
    pub summary: Option<String>,
}

impl CollectOutcome {
    pub fn quiet() -> Self {
        CollectOutcome { summary: None }
    }

    pub fn note(s: impl Into<String>) -> Self {
        CollectOutcome { summary: Some(s.into()) }
    }

    pub fn note_if(interesting: bool, f: impl FnOnce() -> String) -> Self {
        CollectOutcome { summary: interesting.then(f) }
    }
}

/// One user-supplied field of an import (account address, handle, …),
/// rendered by the hub as a text input — so a new import ships its whole
/// setup UI from the catalog, with zero frontend code.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ImportParam {
    /// Key in the params map handed to [`ImportSpec::run`].
    pub key: &'static str,
    pub label: &'static str,
    pub placeholder: &'static str,
    pub required: bool,
}

/// A static header signature that lets a compiled importer *claim* a dropped
/// file's shape before the normalizer's contract heuristics run — the drop
/// surface (`normalizer::detect`) checks these first and, on a hit, offers to
/// route the file to the built importer (an offer, never an auto-run).
///
/// Match semantics (see `normalizer::importer_route`): every header in
/// `required` must be present among the file's headers, and none of `absent`
/// may be — compared with the normalizer's header normalization (trim,
/// collapse internal whitespace, lowercase), order-independent, extra columns
/// ignored. `required`/`absent` are characteristic *marker* subsets, not the
/// full column list, so a signature tolerates reordered or newly-added columns
/// the same way name-based importers (e.g. `imdb.rs`) already do.
#[derive(Debug, Clone, Copy)]
pub struct ImportSignature {
    /// Human label of the shape this signature recognizes, shown in the route
    /// offer (e.g. "IMDb ratings", "IMDb watchlist / custom list").
    pub label: &'static str,
    /// Headers that must ALL be present for this signature to claim the file.
    pub required: &'static [&'static str],
    /// Headers that must NOT be present — disambiguates sibling shapes that
    /// share a base column set (e.g. IMDb ratings vs. a list export).
    pub absent: &'static [&'static str],
}

/// A user-triggered file import, declared on the integration's def and run
/// through the one generic `run_import` command — no per-import Tauri
/// command, API binding, or hub wiring.
pub struct ImportSpec {
    /// Accepted file extensions (lowercase, no dot) for the picker.
    pub accepts: &'static [&'static str],
    pub params: &'static [ImportParam],
    /// Static header signatures that let this importer claim a dropped file's
    /// shape before contract heuristics run (Step 2a). Empty `&[]` = this
    /// importer advertises no drop-recognizable shape (the common case — most
    /// imports are zips or account-specific CSVs with no fixed header).
    pub signatures: &'static [ImportSignature],
    /// The import itself: parse `path`, write the vault, report progress.
    /// Runs on a blocking thread with its own `Vault` handle.
    pub run: fn(
        &Vault,
        &Path,
        &BTreeMap<String, String>,
        &mut dyn FnMut(ImportProgress),
    ) -> Result<ImportOutcome>,
}

/// Generic outcome of one import, rendered by the hub: a human headline
/// plus named counts. The importer's typed stats stay internal.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ImportOutcome {
    /// e.g. "1,204 messages imported, 320 duplicates skipped".
    pub headline: String,
    pub counts: BTreeMap<&'static str, u64>,
}

/// The serializable face of an [`ImportSpec`] (what the hub needs to render
/// the picker and param fields), carried on `IntegrationStatus`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ImportInfo {
    pub accepts: &'static [&'static str],
    pub params: &'static [ImportParam],
}

// ---------------------------------------------------------------------------
// The connect phase: a connection is a login, declared as data — the
// `ImportSpec` move applied to OAuth/token setup. A [`ConnectionDef`] lives
// in the module that owns the auth logic and is registered with one line in
// [`crate::integrations::CONNECTIONS`]; integrations reference the
// connection they need via [`IntegrationDef::connection`]. One login may
// serve many integrations (Google: six defs, one connection).

/// One way to establish a connection. A connection declares one or more
/// (Oura: OAuth *or* a pasted personal access token); the hub renders each
/// from [`ConnectMethod::info`] with zero per-service frontend code.
pub enum ConnectMethod {
    /// Browser OAuth via [`crate::sync::oauth::OauthFlow`] (PKCE +
    /// loopback). `run` blocks until the redirect lands or times out —
    /// callers off the main thread only. Credentials resolve inside `run`
    /// (explicit → saved → compiled-in defaults).
    OAuth {
        provider: &'static crate::sync::oauth::Provider,
        /// Re-running connect adds another account (keyed per-service, e.g.
        /// Google's OpenID `sub`) instead of replacing the login.
        multi_account: bool,
        run: fn(&Vault, Option<crate::sync::oauth::AppCredentials>) -> Result<()>,
    },
    /// A pasted token/claim-URL exchanged or stored directly (SimpleFIN's
    /// setup token, Oura's PAT).
    TokenPaste {
        label: &'static str,
        /// One or two sentences of where to get the token, shown with the
        /// input — the affordance lives in the catalog, not the frontend.
        help: &'static str,
        placeholder: &'static str,
        run: fn(&Vault, &str) -> Result<()>,
    },
}

impl ConnectMethod {
    /// Stable key the generic `connect_run` command selects a method by;
    /// matches the serde tag of [`ConnectMethodInfo`].
    pub fn key(&self) -> &'static str {
        match self {
            ConnectMethod::OAuth { .. } => "oauth",
            ConnectMethod::TokenPaste { .. } => "token-paste",
        }
    }

    /// The serializable face the hub renders from.
    pub fn info(&self) -> ConnectMethodInfo {
        match *self {
            ConnectMethod::OAuth { multi_account, .. } => {
                ConnectMethodInfo::Oauth { multi_account }
            }
            ConnectMethod::TokenPaste { label, help, placeholder, .. } => {
                ConnectMethodInfo::TokenPaste { label, help, placeholder }
            }
        }
    }
}

/// What the hub needs to render one connect method, straight off the def.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum ConnectMethodInfo {
    Oauth { multi_account: bool },
    TokenPaste {
        label: &'static str,
        help: &'static str,
        placeholder: &'static str,
    },
}

/// One login, declared as data. Registered in
/// [`crate::integrations::CONNECTIONS`].
pub struct ConnectionDef {
    /// Stable id integrations reference ("google", "ticktick", …).
    /// Lowercase/digits/dash.
    pub id: &'static str,
    pub display_name: &'static str,
    pub methods: &'static [ConnectMethod],
    /// Current state: are app credentials available, which accounts are
    /// connected. Cheap — the hub polls it.
    pub status: fn(&Vault) -> Result<ConnectStatus>,
    /// Forget one account. `key` is [`ConnectedAccount::key`] (an account
    /// sub for multi-account, the service id otherwise). Synced data stays
    /// in the vault.
    pub disconnect: fn(&Vault, key: &str) -> Result<()>,
    /// Integration ids to pull right after a successful connect, if their
    /// toggle is on (gmail's auto-backfill). Each must reference this
    /// connection and carry a `pull` hook.
    pub auto_pull: &'static [&'static str],
    /// Step-by-step bring-your-own-credentials instructions (register the
    /// OAuth app, the exact redirect URI, where the token lives), one step
    /// per entry — the connect-phase counterpart of [`Integration::setup`].
    /// This is the home for connect setup knowledge, not frontend JSX.
    pub setup: &'static [&'static str],
}

impl ConnectionDef {
    pub fn method(&self, key: &str) -> Option<&'static ConnectMethod> {
        self.methods.iter().find(|m| m.key() == key)
    }
}

/// Generic envelope for one connected account/login — what any service's
/// status hook maps its typed account into (the `MediaItem` trick). Never
/// carries the token itself. No `skip_serializing_if` here on purpose: the
/// generated TS bindings stay honest.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ConnectedAccount {
    /// Disconnect key: account sub for multi-account services, the service
    /// id otherwise.
    pub key: String,
    /// What the row shows: email or display name.
    pub label: String,
    /// RFC3339 local time of the connect, when known.
    pub connected_at: Option<String>,
    /// Access-token expiry, epoch seconds (None = unknown/none). For
    /// no-refresh services this is when a reconnect will be needed.
    pub expires_at: Option<u64>,
    /// Re-login required (refresh failed / token expired). The account is
    /// kept and flagged, never silently dropped.
    pub needs_reconnect: bool,
    /// Service-specific details worth showing, named.
    pub extra: BTreeMap<&'static str, String>,
}

/// A connection's whole state, off [`ConnectionDef::status`].
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ConnectStatus {
    /// App credentials saved or compiled in — connecting is just a login.
    /// `false` = the hub shows the bring-your-own-credentials form first.
    pub configured: bool,
    pub accounts: Vec<ConnectedAccount>,
}

/// One row of the generic `connect_status_all` command: everything the hub
/// needs to render one connection's card section.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ConnectionStatusRow {
    pub id: &'static str,
    pub display_name: &'static str,
    pub methods: Vec<ConnectMethodInfo>,
    pub auto_pull: &'static [&'static str],
    pub setup: &'static [&'static str],
    pub status: ConnectStatus,
}

/// Generic outcome of one manual pull ("Sync now"), mirroring
/// [`ImportOutcome`]: a human headline plus named counts. The service's
/// typed stats stay internal.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PullOutcome {
    pub headline: String,
    pub counts: BTreeMap<&'static str, u64>,
}

/// How an integration runs — exactly one shape per def, so an impossible
/// combination (an import with a cadence, an external stream with a collect
/// hook) is unrepresentable.
#[derive(Clone, Copy)]
pub enum Behavior {
    /// The runner calls `collect` on a schedule. `now` is the tick's single
    /// timestamp (so a mid-tick midnight can't split snapshot stamps from
    /// the daily gate that admitted them).
    Periodic {
        cadence: Cadence,
        collect: fn(&Vault, DateTime<Local>) -> Result<CollectOutcome>,
    },
    /// No hook of its own: the named def's periodic pass serves this toggle
    /// too (one screen-time pass serves three toggles; one browser pass
    /// serves both browsers). Enabling a covered entry keeps the owner's
    /// pass running; per-arm opt-ins are consulted inside the collector.
    CoveredBy(&'static str),
    /// Written by a separate, always-on collector program (named by its
    /// launchd/binary id, e.g. "trove-collector") that follows the vault
    /// spec. Nothing runs in this process: the hub keeps the toggle (the
    /// collector re-reads `.trove/integrations.json`) and shows the
    /// collector's heartbeat; the app only reads the stream.
    External { collector: &'static str },
    /// User-triggered file import only, run via the generic `run_import`
    /// command; nothing runs in the background.
    Import(&'static ImportSpec),
    /// Catalog card exists but no collector is wired yet.
    NotWired,
    /// Catalogued but cannot be built (hard-blocked source): nothing ever
    /// runs, nothing can be toggled. The hub renders the card greyed with
    /// this reason — the app itself answers "why isn't X available?", so
    /// nobody re-scours the web for a path that doesn't exist.
    Unavailable { reason: &'static str },
}

/// One integration: catalog metadata plus its behavior shape and the hooks
/// that apply to any shape. Declare a `pub static DEF: IntegrationDef` in
/// the owning module and add one line to
/// [`crate::integrations::INTEGRATIONS`] — that's the whole registration.
pub struct IntegrationDef {
    /// The static catalog metadata (id, copy, setup steps…).
    pub meta: Integration,
    /// How it runs: pick one [`Behavior`] shape.
    pub behavior: Behavior,
    /// Cheap TCC preflight run in this process. `None` = no permission.
    pub permission: Option<fn() -> PermissionInfo>,
    /// Cheap "when did data last land" probe for the hub UI.
    pub last_data: Option<fn(&Vault) -> Option<String>>,
    /// The [`ConnectionDef`] this integration needs, by id — `None` for
    /// sources with no login. Many defs may share one connection.
    pub connection: Option<&'static str>,
    /// Manual "Sync now", run via the one generic `integration_pull`
    /// command. Blocking (network) — callers off the main thread only. The
    /// scheduled path stays [`Behavior::Periodic`]; this is the
    /// user-triggered one.
    pub pull: Option<fn(&Vault) -> Result<PullOutcome>>,
}

impl IntegrationDef {
    /// The import spec, for the [`Behavior::Import`] shape.
    pub fn import_spec(&self) -> Option<&'static ImportSpec> {
        match self.behavior {
            Behavior::Import(spec) => Some(spec),
            _ => None,
        }
    }

    /// The reason this integration can't be built, for the
    /// [`Behavior::Unavailable`] shape. `Some` = greyed catalog card.
    pub fn unavailable_reason(&self) -> Option<&'static str> {
        match self.behavior {
            Behavior::Unavailable { reason } => Some(reason),
            _ => None,
        }
    }
}

impl std::ops::Deref for IntegrationDef {
    type Target = Integration;

    fn deref(&self) -> &Integration {
        &self.meta
    }
}

// ---------------------------------------------------------------------------
// Shared probe helpers for `last_data` hooks.

/// Newest file stem in a directory of date-named JSONL files (YYYY-MM-DD /
/// YYYY-MM sort lexically). `None` when the directory is missing or empty.
pub(crate) fn newest_stem(dir: &Path) -> Option<String> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
        .max()
}

/// Newest modification time of any file directly in `dir`, RFC3339 local.
pub(crate) fn newest_mtime(dir: &Path) -> Option<String> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
        .max()
        .map(|t| DateTime::<Local>::from(t).to_rfc3339())
}

/// Newest modification time of any `.jsonl` file anywhere under `dir`
/// (recursively), RFC3339 local. For sources whose vault layout nests files
/// in subdirectories (e.g. `health/garmin/activities/YYYY-MM/…`). `None` when
/// the tree has no such file.
pub(crate) fn newest_mtime_recursive(dir: &Path) -> Option<String> {
    fn walk(dir: &Path, best: &mut Option<SystemTime>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, best);
            } else if path.extension().is_some_and(|x| x == "jsonl") {
                if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                    if best.is_none_or(|b| t > b) {
                        *best = Some(t);
                    }
                }
            }
        }
    }
    let mut best = None;
    walk(dir, &mut best);
    best.map(|t| DateTime::<Local>::from(t).to_rfc3339())
}

pub(crate) fn file_mtime(path: &Path) -> Option<String> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| DateTime::<Local>::from(t).to_rfc3339())
}

/// Can this process open `path` for reading? The cheap FDA preflight.
pub(crate) fn readable(path: &Path) -> bool {
    fs::File::open(path).is_ok()
}

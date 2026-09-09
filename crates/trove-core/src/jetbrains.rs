//! JetBrains IDE activity — recent-projects list from `recentProjects.xml`.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/jetbrains.md.
//!
//! A **Periodic** local filesystem collector (the [`crate::local_git`] shape:
//! a bounded read under `~/Library/Application Support/JetBrains/`, no network
//! and no connection). Every installed JetBrains IDE (IntelliJ IDEA, WebStorm,
//! PyCharm, GoLand, …) writes one `options/recentProjects.xml` per IDE+version
//! directory; this collector globs all of them and extracts the per-project
//! last-activated timestamp from each.
//!
//! **`developer/` is raw-only** (taxonomy decision): no media-plays/domain
//! contract, no normalized struct, no spec validation — this module owns its
//! row shape.
//!
//! ## XML format (JetBrains state persistence)
//!
//! The `recentProjects.xml` file is a standard JetBrains component state file:
//!
//! ```xml
//! <application>
//!   <component name="RecentProjectsManager">
//!     <option name="additionalInfo">
//!       <map>
//!         <entry key="$USER_HOME$/path/to/project">
//!           <value>
//!             <RecentProjectMetaInfo opened="true" displayName="MyApp">
//!               <option name="activationTimestamp" value="1687788000000" />
//!               <option name="projectOpenTimestamp" value="1687786000000" />
//!               <option name="build" value="IU-241.18034.62" />
//!               <option name="productionCode" value="IU" />
//!             </RecentProjectMetaInfo>
//!           </value>
//!         </entry>
//!       </map>
//!     </option>
//!     <!-- legacy (IDEA < 2019.2): only paths, no timestamps -->
//!     <option name="recentPaths">
//!       <list>
//!         <option value="$USER_HOME$/path/to/project" />
//!       </list>
//!     </option>
//!   </component>
//! </application>
//! ```
//!
//! The `activationTimestamp` is milliseconds since Unix epoch (set when the
//! project is opened/activated). Legacy paths with no `additionalInfo` entry
//! get a synthetic timestamp of 0 and are only written when the path is not
//! already covered by the richer `additionalInfo` block.
//!
//! ## Cursor / incremental / dedupe
//!
//! `.trove/jetbrains-sync.json` maps each `recentProjects.xml` path (as a
//! home-relative display string) → the file's last-seen mtime (unix ms). A
//! file is (re)processed only when its mtime advances. Within each file, ALL
//! entries are re-emitted on every re-read; dedup against previously written
//! rows is done by `guid` via an upsert (read-replace-rewrite the month
//! partition). Guid = `sha256(ide | project_path | activation_ts_ms)[..16]`,
//! which is stable as long as the triple is stable.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Hourly, like the other always-on local collectors.
pub const JETBRAINS_SYNC_SECS: u64 = 3600;

/// Contract-free raw stream: one row per (project, opened-at) observation.
const DIR: &str = "developer/jetbrains";
/// Rebuildable cursor: xml file display name → mtime (unix ms).
const SYNC_FILE: &str = ".trove/jetbrains-sync.json";
/// The `$USER_HOME$` macro JetBrains embeds in project paths.
const USER_HOME_MACRO: &str = "$USER_HOME$";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_jetbrains()?;
    Ok(CollectOutcome::note_if(s.rows > 0, || {
        format!(
            "jetbrains synced — {} project observations from {} IDE dirs",
            s.rows, s.ide_dirs
        )
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_jetbrains()?;
    let headline = if s.rows == 0 {
        format!(
            "JetBrains is up to date — no changes ({} IDE dirs scanned)",
            s.ide_dirs
        )
    } else {
        format!(
            "JetBrains synced — {} project observations from {} IDE dirs",
            s.rows, s.ide_dirs
        )
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("rows", s.rows), ("ide_dirs", s.ide_dirs)]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "jetbrains",
        name: "JetBrains IDEs",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Recently opened projects from JetBrains IDEs (IntelliJ IDEA, WebStorm, \
             PyCharm, GoLand, and others), with last-activated timestamps. \
             Reads the recentProjects.xml that each IDE version writes to \
             ~/Library/Application Support/JetBrains/.",
        domain: "developer",
        vault_path: "developer/jetbrains/",
        toggleable: true,
        setup: &[
            "Reads the recent-projects files JetBrains IDEs already write under ~/Library \
             — nothing to install or connect.",
            "Works with all JetBrains IDEs (IntelliJ IDEA, WebStorm, PyCharm, GoLand, \
             CLion, and others) installed via Toolbox or standalone.",
            "Set TROVE_HOME to scan a different home directory for testing.",
        ],
        caveats:
            "In-editor local history (VCS → Local History) uses a proprietary binary \
             format and is not collected. Detailed time-in-code comes from WakaTime \
             instead. Only projects opened since the IDE started recording \
             recentProjects.xml appear.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(JETBRAINS_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Row shape (raw — this module's own schema, no contract).

/// One row in `developer/jetbrains/YYYY-MM.jsonl`: a single (project, ts)
/// observation parsed from a JetBrains `recentProjects.xml`. `guid` is the
/// upsert/dedupe key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ProjectRow {
    /// RFC3339 local time of the project's last activation (when the IDE was
    /// opened to that project). `0` activation timestamps from legacy paths
    /// without metadata yield `1970-01-01T00:00:00+00:00`.
    pub ts: String,
    /// Absolute path to the project directory, with `$USER_HOME$` expanded.
    pub project_path: String,
    /// Human project name: the last path component of `project_path`.
    pub project_name: String,
    /// JetBrains IDE identifier, e.g. `IntelliJIdea`, `WebStorm`, `PyCharm`,
    /// `GoLand`, `CLion`, `DataGrip`, `Rider`, `PhpStorm`, `RubyMine`.
    /// Extracted from the config-dir name (`<IDEName><Version>`).
    pub ide: String,
    /// Version string, e.g. `2024.1`, `2023.3.4`. Extracted from the
    /// config-dir name.
    pub ide_version: String,
    /// Display name set by the user inside the IDE, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// JetBrains build ID, e.g. `IU-241.18034.62`, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Stable dedupe key: hex-prefix of sha256(ide | project_path | ts_ms).
    pub guid: String,
}

// ---------------------------------------------------------------------------
// Stats.

/// Result of one scan pass.
#[derive(Debug, Clone, Default)]
pub struct JetBrainsStats {
    /// Project rows written (new or updated by guid) this pass.
    pub rows: u64,
    /// IDE config directories that contained a `recentProjects.xml`.
    pub ide_dirs: u64,
}

// ---------------------------------------------------------------------------
// Scan root.

/// The base for JetBrains config dirs: `TROVE_HOME/Library/Application
/// Support/JetBrains/` when `TROVE_HOME` is set, else `~/Library/Application
/// Support/JetBrains/`. Returns `None` when the home dir isn't known.
fn jetbrains_base(home: &Path) -> PathBuf {
    home.join("Library").join("Application Support").join("JetBrains")
}

fn home_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// Expand `$USER_HOME$` in a JetBrains path to the given home directory.
fn expand_home(path: &str, home: &Path) -> String {
    if let Some(rest) = path.strip_prefix(USER_HOME_MACRO) {
        format!("{}{}", home.display(), rest)
    } else {
        path.to_string()
    }
}

/// The last path component of a path string (the project name).
fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// Split an IDE config dir name like `IntelliJIdea2024.1` or `WebStorm2023.3`
/// into `(ide, version)`. The version is the trailing `NNNN.N[.N]` segment;
/// everything before it is the IDE name. Returns `(full, "")` when no version
/// segment is found.
fn split_ide_version(dir_name: &str) -> (String, String) {
    // Find the first digit that starts a version suffix (e.g. "2024.1").
    // JetBrains dir names are: one or more uppercase/lowercase ASCII letters,
    // then one or more digits, then optional `.minor` parts.
    let mut version_start = None;
    for (i, c) in dir_name.char_indices() {
        if c.is_ascii_digit() {
            version_start = Some(i);
            break;
        }
    }
    match version_start {
        Some(i) if i > 0 => (dir_name[..i].to_string(), dir_name[i..].to_string()),
        _ => (dir_name.to_string(), String::new()),
    }
}

// ---------------------------------------------------------------------------
// Stable guid.

fn stable_guid(ide: &str, project_path: &str, ts_ms: i64) -> String {
    use std::collections::hash_map::DefaultHasher;
    let mut h = DefaultHasher::new();
    ide.hash(&mut h);
    project_path.hash(&mut h);
    ts_ms.hash(&mut h);
    // Mix a second time for better distribution.
    let mut h2 = DefaultHasher::new();
    h.finish().hash(&mut h2);
    ide.len().hash(&mut h2);
    project_path.len().hash(&mut h2);
    format!("jb-{:016x}", h2.finish())
}

// ---------------------------------------------------------------------------
// Cursor.

/// Incremental-sync state, persisted in `.trove/jetbrains-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JetBrainsSyncState {
    /// RFC3339 local time of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// xml-file display key → last-seen mtime (unix ms).
    #[serde(default)]
    pub mtimes: BTreeMap<String, i64>,
}

/// File mtime in unix milliseconds.
fn file_mtime_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

// ---------------------------------------------------------------------------
// XML parse (pure, tested against fixture files).

/// One project entry parsed from a `recentProjects.xml`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedEntry {
    /// Raw project path (may contain `$USER_HOME$`).
    path: String,
    /// `activationTimestamp` in millis (0 if absent / legacy-only path).
    activation_ts_ms: i64,
    /// `projectOpenTimestamp` in millis (0 if absent).
    open_ts_ms: i64,
    /// `displayName` attribute from `<RecentProjectMetaInfo>`, when set.
    display_name: Option<String>,
    /// `build` option from `<RecentProjectMetaInfo>`, when set.
    build: Option<String>,
}

/// Parse all project entries from the body of a `recentProjects.xml`.
/// Tolerant: unknown elements, missing attributes, and unexpected structure
/// are silently skipped — a partial parse is returned, never an error.
///
/// Strategy: lightweight state-machine over the quick-xml event stream.
/// We track which `<option name="...">` block we're inside (`InsideMap` or
/// `InsideList`) and collect `<entry key="...">` / `<option value="...">` /
/// `<RecentProjectMetaInfo ...>` events accordingly.
pub(crate) fn parse_recent_projects(xml: &str) -> Vec<ParsedEntry> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    // State machine.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum State {
        Root,
        InsideAdditionalInfo, // inside <option name="additionalInfo"><map>
        InsideEntry,          // inside <entry key="...">
        InsideValue,          // inside <entry><value>
        InsideMeta,           // inside <RecentProjectMetaInfo ...>
        InsideRecentPaths,    // inside <option name="recentPaths"><list>
    }

    let mut state = State::Root;
    let mut entries: BTreeMap<String, ParsedEntry> = BTreeMap::new();
    let mut legacy_paths: Vec<String> = Vec::new();

    // Current entry being built while InsideMeta.
    let mut cur_path = String::new();
    let mut cur_activation_ts_ms: i64 = 0;
    let mut cur_open_ts_ms: i64 = 0;
    let mut cur_display_name: Option<String> = None;
    let mut cur_build: Option<String> = None;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();

                let attr = |key: &str| -> Option<String> {
                    for a in e.attributes().flatten() {
                        let k = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                        if k == key {
                            return a
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .ok()
                                .map(|v| v.into_owned());
                        }
                    }
                    None
                };

                match state {
                    State::Root => {
                        if name == "option" {
                            if attr("name").as_deref() == Some("additionalInfo") {
                                state = State::InsideAdditionalInfo;
                            } else if attr("name").as_deref() == Some("recentPaths") {
                                state = State::InsideRecentPaths;
                            }
                        }
                    }
                    State::InsideAdditionalInfo => {
                        if name == "entry" {
                            if let Some(k) = attr("key") {
                                cur_path = k;
                                cur_activation_ts_ms = 0;
                                cur_open_ts_ms = 0;
                                cur_display_name = None;
                                cur_build = None;
                                state = State::InsideEntry;
                            }
                        }
                    }
                    State::InsideEntry => {
                        if name == "value" {
                            state = State::InsideValue;
                        }
                    }
                    State::InsideValue => {
                        if name == "RecentProjectMetaInfo" {
                            cur_display_name = attr("displayName")
                                .filter(|s| !s.is_empty());
                            state = State::InsideMeta;
                        }
                    }
                    State::InsideMeta => {
                        if name == "option" {
                            match attr("name").as_deref() {
                                Some("activationTimestamp") => {
                                    cur_activation_ts_ms = attr("value")
                                        .and_then(|v| v.parse::<i64>().ok())
                                        .unwrap_or(0);
                                }
                                Some("projectOpenTimestamp") => {
                                    cur_open_ts_ms = attr("value")
                                        .and_then(|v| v.parse::<i64>().ok())
                                        .unwrap_or(0);
                                }
                                Some("build") => {
                                    cur_build = attr("value").filter(|s| !s.is_empty());
                                }
                                _ => {}
                            }
                        }
                    }
                    State::InsideRecentPaths => {
                        if name == "option" {
                            if let Some(v) = attr("value") {
                                if !v.is_empty() {
                                    legacy_paths.push(v);
                                }
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                match state {
                    State::InsideMeta if name == "RecentProjectMetaInfo" => {
                        if !cur_path.is_empty() {
                            entries.insert(
                                cur_path.clone(),
                                ParsedEntry {
                                    path: cur_path.clone(),
                                    activation_ts_ms: cur_activation_ts_ms,
                                    open_ts_ms: cur_open_ts_ms,
                                    display_name: cur_display_name.clone(),
                                    build: cur_build.clone(),
                                },
                            );
                        }
                        state = State::InsideValue;
                    }
                    State::InsideValue if name == "value" => {
                        state = State::InsideEntry;
                    }
                    State::InsideEntry if name == "entry" => {
                        state = State::InsideAdditionalInfo;
                    }
                    State::InsideAdditionalInfo if name == "option" => {
                        state = State::Root;
                    }
                    State::InsideRecentPaths if name == "option" => {
                        state = State::Root;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // malformed XML — stop, return partial results
            _ => {}
        }
        buf.clear();
    }

    // Merge legacy paths that aren't already covered by the richer additionalInfo map.
    for p in legacy_paths {
        if !entries.contains_key(&p) {
            entries.insert(
                p.clone(),
                ParsedEntry {
                    path: p,
                    activation_ts_ms: 0,
                    open_ts_ms: 0,
                    display_name: None,
                    build: None,
                },
            );
        }
    }

    entries.into_values().collect()
}

// ---------------------------------------------------------------------------
// Vault impl.

impl Vault {
    fn read_jetbrains_sync(&self) -> JetBrainsSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_jetbrains_sync(&self, state: &JetBrainsSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    /// Upsert one [`ProjectRow`] into `developer/jetbrains/YYYY-MM.jsonl`
    /// (keyed by the row's month). Replaces an existing row with the same
    /// `guid`; appends when new. Partition is rewritten atomically.
    fn upsert_jetbrains_row(&self, row: &ProjectRow) -> Result<()> {
        let key = Partition::Month
            .key(&row.ts)
            .with_context(|| format!("jetbrains: unpartitionable ts {:?}", row.ts))?;
        let stream = self.stream(DIR, Partition::Month);
        let mut rows: Vec<ProjectRow> = stream.read(key)?;
        match rows.iter_mut().find(|r| r.guid == row.guid) {
            Some(existing) => *existing = row.clone(),
            None => rows.push(row.clone()),
        }
        self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &rows)
    }

    /// One incremental scan pass over JetBrains `recentProjects.xml` files.
    /// Silently a no-op when `~/Library/Application Support/JetBrains/` is
    /// missing (JetBrains not installed).
    pub fn collect_jetbrains(&self) -> Result<JetBrainsStats> {
        let Some(home) = home_dir() else {
            return Ok(JetBrainsStats::default());
        };
        self.collect_jetbrains_from(&home)
    }

    /// The pass itself, home-dir-injected for tests.
    pub(crate) fn collect_jetbrains_from(&self, home: &Path) -> Result<JetBrainsStats> {
        let base = jetbrains_base(home);
        if !base.is_dir() {
            return Ok(JetBrainsStats::default());
        }

        let mut stats = JetBrainsStats::default();
        let mut sync = self.read_jetbrains_sync();
        let mut changed = false;

        // Enumerate `<base>/<IDEName><Version>/options/recentProjects.xml`.
        let ide_entries = match fs::read_dir(&base) {
            Ok(e) => e,
            Err(_) => return Ok(stats),
        };

        for ide_entry in ide_entries.flatten() {
            let ide_dir = ide_entry.path();
            if !ide_dir.is_dir() {
                continue;
            }
            let ide_dir_name = ide_entry.file_name().to_string_lossy().into_owned();
            let xml_path = ide_dir.join("options").join("recentProjects.xml");
            if !xml_path.is_file() {
                continue;
            }

            stats.ide_dirs += 1;

            // Display key for the cursor (stable across runs).
            let display_key = format!("{ide_dir_name}/options/recentProjects.xml");

            let Some(mtime) = file_mtime_ms(&xml_path) else {
                continue;
            };
            // mtime-gated: only re-process when the file changed.
            if sync.mtimes.get(&display_key) == Some(&mtime) {
                continue;
            }

            let Ok(body) = fs::read_to_string(&xml_path) else {
                continue;
            };

            let (ide, ide_version) = split_ide_version(&ide_dir_name);
            let entries = parse_recent_projects(&body);

            for entry in entries {
                let project_path = expand_home(&entry.path, home);
                let project_name = basename(&project_path);

                // Convert activation_ts_ms to RFC3339.
                let ts = DateTime::from_timestamp_millis(entry.activation_ts_ms)
                    .unwrap_or_else(|| DateTime::from_timestamp_millis(0).unwrap())
                    .with_timezone(&Local)
                    .to_rfc3339();

                let guid = stable_guid(&ide, &project_path, entry.activation_ts_ms);

                let row = ProjectRow {
                    ts,
                    project_path,
                    project_name,
                    ide: ide.clone(),
                    ide_version: ide_version.clone(),
                    display_name: entry.display_name,
                    build: entry.build,
                    guid,
                };

                if let Err(e) = self.upsert_jetbrains_row(&row) {
                    eprintln!(
                        "trove jetbrains: upsert for {} / {} failed: {e:#}",
                        ide_dir_name, row.project_path
                    );
                    continue;
                }
                stats.rows += 1;
            }

            sync.mtimes.insert(display_key, mtime);
            changed = true;
        }

        if changed {
            sync.updated = Local::now().to_rfc3339();
            self.write_jetbrains_sync(&sync)?;
        }
        Ok(stats)
    }

    /// All project rows for one month (`YYYY-MM`). The hub's Recent-data view
    /// reads this.
    pub fn jetbrains_projects(&self, month: &str) -> Result<Vec<ProjectRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -----------------------------------------------------------------------
    // Helpers.

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-jb-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("trove-jbhome-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a synthetic `recentProjects.xml` under
    /// `<home>/Library/Application Support/JetBrains/<ide_dir>/options/`.
    fn write_xml(home: &Path, ide_dir: &str, content: &str) -> PathBuf {
        let dir = home
            .join("Library")
            .join("Application Support")
            .join("JetBrains")
            .join(ide_dir)
            .join("options");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("recentProjects.xml");
        fs::write(&p, content).unwrap();
        p
    }

    // -----------------------------------------------------------------------
    // Unit tests for the XML parser.

    // Fixture: full recentProjects.xml with additionalInfo + legacy recentPaths.
    const FIXTURE_FULL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<application>
  <component name="RecentProjectsManager">
    <option name="additionalInfo">
      <map>
        <entry key="$USER_HOME$/Projects/alpha">
          <value>
            <RecentProjectMetaInfo opened="true" displayName="Alpha App">
              <option name="activationTimestamp" value="1687788000000" />
              <option name="projectOpenTimestamp" value="1687786000000" />
              <option name="build" value="IU-241.18034.62" />
              <option name="productionCode" value="IU" />
            </RecentProjectMetaInfo>
          </value>
        </entry>
        <entry key="$USER_HOME$/Projects/beta">
          <value>
            <RecentProjectMetaInfo opened="false">
              <option name="activationTimestamp" value="1687700000000" />
              <option name="build" value="IU-241.18034.62" />
            </RecentProjectMetaInfo>
          </value>
        </entry>
      </map>
    </option>
    <option name="recentPaths">
      <list>
        <option value="$USER_HOME$/Projects/alpha" />
        <option value="$USER_HOME$/Projects/gamma" />
      </list>
    </option>
    <option name="lastOpenedProject" value="$USER_HOME$/Projects/alpha" />
  </component>
</application>"#;

    // Fixture: legacy-only (IDEA < 2019.2) — only recentPaths, no additionalInfo.
    const FIXTURE_LEGACY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<application>
  <component name="RecentProjectsManager">
    <option name="recentPaths">
      <list>
        <option value="$USER_HOME$/OldProject" />
        <option value="/absolute/path/project" />
      </list>
    </option>
  </component>
</application>"#;

    // Fixture: empty map (no projects).
    const FIXTURE_EMPTY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<application>
  <component name="RecentProjectsManager">
    <option name="additionalInfo">
      <map />
    </option>
  </component>
</application>"#;

    // Fixture: WebStorm 2023.3 style (no displayName, numeric version).
    const FIXTURE_WEBSTORM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<application>
  <component name="RecentProjectsManager">
    <option name="additionalInfo">
      <map>
        <entry key="$USER_HOME$/code/my-site">
          <value>
            <RecentProjectMetaInfo opened="true">
              <option name="activationTimestamp" value="1695000000000" />
              <option name="build" value="WS-233.11799.241" />
              <option name="productionCode" value="WS" />
            </RecentProjectMetaInfo>
          </value>
        </entry>
      </map>
    </option>
  </component>
</application>"#;

    #[test]
    fn parse_full_fixture() {
        let entries = parse_recent_projects(FIXTURE_FULL);
        // alpha + beta from additionalInfo, gamma from legacy (alpha already covered)
        assert_eq!(entries.len(), 3, "expected alpha, beta, gamma");

        let alpha = entries.iter().find(|e| e.path.ends_with("/alpha")).unwrap();
        assert_eq!(alpha.activation_ts_ms, 1_687_788_000_000);
        assert_eq!(alpha.open_ts_ms, 1_687_786_000_000);
        assert_eq!(alpha.display_name.as_deref(), Some("Alpha App"));
        assert_eq!(alpha.build.as_deref(), Some("IU-241.18034.62"));

        let beta = entries.iter().find(|e| e.path.ends_with("/beta")).unwrap();
        assert_eq!(beta.activation_ts_ms, 1_687_700_000_000);
        assert!(beta.display_name.is_none(), "beta has no displayName");

        let gamma = entries.iter().find(|e| e.path.ends_with("/gamma")).unwrap();
        assert_eq!(gamma.activation_ts_ms, 0, "legacy path has ts=0");
        assert!(gamma.build.is_none());
    }

    #[test]
    fn parse_legacy_fixture() {
        let entries = parse_recent_projects(FIXTURE_LEGACY);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.path.ends_with("/OldProject")));
        assert!(entries.iter().any(|e| e.path == "/absolute/path/project"));
        for e in &entries {
            assert_eq!(e.activation_ts_ms, 0);
        }
    }

    #[test]
    fn parse_empty_fixture() {
        let entries = parse_recent_projects(FIXTURE_EMPTY);
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_webstorm_fixture() {
        let entries = parse_recent_projects(FIXTURE_WEBSTORM);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.activation_ts_ms, 1_695_000_000_000);
        assert_eq!(e.build.as_deref(), Some("WS-233.11799.241"));
        assert!(e.display_name.is_none());
    }

    #[test]
    fn split_ide_version_various() {
        assert_eq!(
            split_ide_version("IntelliJIdea2024.1"),
            ("IntelliJIdea".to_string(), "2024.1".to_string())
        );
        assert_eq!(
            split_ide_version("WebStorm2023.3"),
            ("WebStorm".to_string(), "2023.3".to_string())
        );
        assert_eq!(
            split_ide_version("PyCharm2023.3.4"),
            ("PyCharm".to_string(), "2023.3.4".to_string())
        );
        assert_eq!(
            split_ide_version("CLion2024.1"),
            ("CLion".to_string(), "2024.1".to_string())
        );
        // No version — keep full name.
        let (ide, ver) = split_ide_version("SomeIDE");
        assert_eq!(ide, "SomeIDE");
        assert_eq!(ver, "");
    }

    #[test]
    fn expand_home_macro() {
        let home = PathBuf::from("/Users/alice");
        assert_eq!(
            expand_home("$USER_HOME$/Projects/foo", &home),
            "/Users/alice/Projects/foo"
        );
        assert_eq!(
            expand_home("/absolute/path", &home),
            "/absolute/path"
        );
    }

    // -----------------------------------------------------------------------
    // Integration tests: full vault scan.

    #[test]
    fn collect_empty_when_no_jetbrains_dir() {
        let vault = temp_vault("no_jb");
        let home = temp_home("no_jb_home");
        // No JetBrains dir under home — should be a no-op.
        let stats = vault.collect_jetbrains_from(&home).unwrap();
        assert_eq!(stats.rows, 0);
        assert_eq!(stats.ide_dirs, 0);
    }

    #[test]
    fn collect_full_fixture_two_projects() {
        let vault = temp_vault("full");
        let home = temp_home("full_home");
        write_xml(&home, "IntelliJIdea2024.1", FIXTURE_FULL);

        let stats = vault.collect_jetbrains_from(&home).unwrap();
        // alpha + beta from additionalInfo, gamma from legacy
        assert_eq!(stats.ide_dirs, 1);
        assert_eq!(stats.rows, 3);

        // Verify the month partition was written.
        // alpha activation = 1687788000000 ms → 2023-06
        let month_rows = vault.jetbrains_projects("2023-06").unwrap();
        let alpha = month_rows.iter().find(|r| r.project_name == "alpha").unwrap();
        assert_eq!(alpha.ide, "IntelliJIdea");
        assert_eq!(alpha.ide_version, "2024.1");
        assert_eq!(alpha.display_name.as_deref(), Some("Alpha App"));
        assert_eq!(alpha.build.as_deref(), Some("IU-241.18034.62"));
        // guid is stable
        assert!(alpha.guid.starts_with("jb-"));
    }

    #[test]
    fn collect_idempotent_no_mtime_change() {
        let vault = temp_vault("idem");
        let home = temp_home("idem_home");
        write_xml(&home, "WebStorm2023.3", FIXTURE_WEBSTORM);

        let first = vault.collect_jetbrains_from(&home).unwrap();
        assert_eq!(first.rows, 1);

        // Second pass: mtime unchanged → no-op.
        let second = vault.collect_jetbrains_from(&home).unwrap();
        assert_eq!(second.rows, 0);
    }

    #[test]
    fn collect_two_ide_dirs() {
        let vault = temp_vault("two_ides");
        let home = temp_home("two_ides_home");
        write_xml(&home, "IntelliJIdea2024.1", FIXTURE_FULL);
        write_xml(&home, "WebStorm2023.3", FIXTURE_WEBSTORM);

        let stats = vault.collect_jetbrains_from(&home).unwrap();
        assert_eq!(stats.ide_dirs, 2);
        // 3 from IntelliJ + 1 from WebStorm
        assert_eq!(stats.rows, 4);
    }

    #[test]
    fn collect_legacy_format() {
        let vault = temp_vault("legacy");
        let home = temp_home("legacy_home");
        write_xml(&home, "PyCharm2019.1", FIXTURE_LEGACY);

        let stats = vault.collect_jetbrains_from(&home).unwrap();
        assert_eq!(stats.ide_dirs, 1);
        assert_eq!(stats.rows, 2);

        // Legacy paths get ts = epoch 0; the local-time partition may be
        // 1970-01 (UTC or east of UTC) or 1969-12 (west of UTC, e.g. UTC-7).
        // Scan both and verify the rows land somewhere.
        let rows_jan = vault.jetbrains_projects("1970-01").unwrap();
        let rows_dec = vault.jetbrains_projects("1969-12").unwrap();
        let rows: Vec<_> = rows_jan.iter().chain(rows_dec.iter()).collect();
        assert_eq!(rows.len(), 2, "expected 2 rows across epoch partitions");
        assert!(rows.iter().any(|r| r.project_name == "OldProject"));
    }

    #[test]
    fn guid_is_stable() {
        let g1 = stable_guid("IntelliJIdea", "/home/user/Projects/alpha", 1_687_788_000_000);
        let g2 = stable_guid("IntelliJIdea", "/home/user/Projects/alpha", 1_687_788_000_000);
        assert_eq!(g1, g2);

        let g3 = stable_guid("WebStorm", "/home/user/Projects/alpha", 1_687_788_000_000);
        assert_ne!(g1, g3, "different IDE → different guid");
    }
}

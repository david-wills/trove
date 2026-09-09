//! OmniFocus — local task collector reading the `.ofocus` transaction-log
//! format on disk. A **Periodic** pass (hourly) replays ZIP+XML deltas into
//! the already-bound [`crate::tasks`] contract.
//!
//! ## Format
//!
//! `~/Library/Containers/com.omnigroup.OmniFocus4/Data/Library/Application
//! Support/OmniFocus/OmniFocus.ofocus` is a directory of ZIP bundles.
//! Each bundle contains a `contents.xml` of transaction deltas. The chain is:
//!
//! ```text
//! 00000000000000={rand}+{A}.zip   — master (full snapshot)
//! {date}={A}+{B}.zip              — transaction 1
//! {date}={B}+{C}.zip              — transaction 2
//! …
//! ```
//!
//! Reading strategy: enumerate all `.zip` files, sort lexicographically (the
//! file-name date prefix keeps them in order), replay all transactions to build
//! the current in-memory state, then diff against the stored snapshot.
//! Merge-point files (`{date}={X}+{Y}+{Z}.zip`) contain no data — they are
//! silently skipped.
//!
//! ## XML element names (source: `tomzx/ofocus-format`)
//!
//! `<task id="…">` — an action or project node. Children:
//! - `<name>` — task title
//! - `<note><text>` — HTML note body
//! - `<added>` — creation ISO-8601 timestamp
//! - `<modified>` — last-modified ISO-8601 timestamp
//! - `<due>` — due datetime (ISO-8601 with ms, UTC)
//! - `<start>` — defer datetime (ISO-8601 with ms, UTC)
//! - `<completed>` — completion datetime (ISO-8601 with ms, UTC)
//! - `<flagged>` — "true"/"false"
//! - `<project>` child — present when the task is a project root
//!   - `<folder idref="…">` — parent folder id
//!   - `<status>` — "active","inactive","done","dropped"
//! - `<task idref="…">` child — present when the task is a subtask (references
//!   the parent task id)
//! - `<context idref="…">` — assigned context/tag id
//! - `<inbox>` child — task is in the inbox (no parent)
//! - `<repetition-rule>` — RRULE-style string (FREQ=WEEKLY, etc.)
//!
//! `<context id="…">` — a context/tag:
//! - `<name>` — display name
//! - `<context idref="…">` — parent context id (if nested)
//!
//! `<folder id="…">` — a folder:
//! - `<name>` — display name
//! - `<folder idref="…">` — parent folder id
//!
//! `op` attribute on any element: "add" (default), "update", "delete",
//! "reference" (informational; not a write operation).
//!
//! ## Cursor
//!
//! `.trove/omnifocus-sync.json` stores the mtime (Unix seconds) of the most
//! recently read ZIP. On subsequent runs we rebuild only from new ZIPs added
//! since the cursor.  **But**: to keep state correct after compaction (when the
//! master ZIP is replaced with a new one), we detect a missing or older master
//! ZIP and do a full replay from scratch. The cursor is only advanced after a
//! successful full write.
//!
//! ## Access
//!
//! Full Disk Access is required to read the sandbox container path. The pull
//! degrades gracefully when the container does not exist.
//!
//! **IMPORTANT — parser status:** This parser is built from the community
//! schema (`tomzx/ofocus-format`) and has not been validated against a real
//! `.ofocus` container. A `Needs-sample` gate is in effect. The pull returns a
//! clear message if the container is absent rather than an error.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::tasks::{ProjectInfo, Task, TaskFate};
use crate::vault::Vault;

/// Hourly — reading local files, zero network cost.
pub const OMNIFOCUS_SYNC_SECS: u64 = 3600;

const SYNC_FILE: &str = ".trove/omnifocus-sync.json";
const RAW_DIR: &str = "tasks/omnifocus/raw";
const SOURCE: &str = "omnifocus";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "OmniFocus synced — {} open, {} completed, {} deleted",
                    c("open"),
                    c("completed"),
                    c("deleted"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "OmniFocus sync skipped: {e}"
        ))),
    }
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(omnifocus_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::tasks::source_last_data(vault, SOURCE)
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "omnifocus",
        name: "OmniFocus",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads your tasks, projects, and contexts directly from OmniFocus's local \
                      database. No login required — Full Disk Access lets Trove read the app's \
                      data folder.",
        domain: "tasks",
        vault_path: "tasks/omnifocus/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the \
             troved binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
            "OmniFocus 4 (or 3) must have been run at least once to create the database.",
        ],
        caveats: "The .ofocus format is a community-documented ZIP + XML transaction log with \
                 no official schema guarantee. A TaskPaper or JSON export can be imported as \
                 a fallback via the Import option.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(OMNIFOCUS_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Container location.

/// Env-override root for tests; otherwise the real home directory.
fn home_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// OmniFocus 4 container path.
const OF4_CONTAINER: &str =
    "Library/Containers/com.omnigroup.OmniFocus4/Data/Library/Application Support/OmniFocus";
/// OmniFocus 3 container path.
const OF3_CONTAINER: &str =
    "Library/Containers/com.omnigroup.OmniFocus3/Data/Library/Application Support/OmniFocus";

/// Locate the `OmniFocus.ofocus` directory. Prefers OF4, falls back to OF3.
pub(crate) fn ofocus_path() -> Option<PathBuf> {
    let home = home_root()?;
    for rel in [OF4_CONTAINER, OF3_CONTAINER] {
        let p = home.join(rel).join("OmniFocus.ofocus");
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// Returns true when the container is visible to this process (Full Disk
/// Access granted and OmniFocus has been run at least once).
pub fn omnifocus_permission_ok() -> bool {
    ofocus_path().is_some()
}

// ---------------------------------------------------------------------------
// Sync cursor.

/// Persisted in `.trove/omnifocus-sync.json`. Not a secret; rebuildable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct OmniFocusSyncState {
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// mtime (Unix seconds) of the youngest ZIP seen in the last full replay.
    /// When the master ZIP changes (compaction), we detect it and re-replay.
    #[serde(default)]
    pub last_zip_mtime: u64,
    /// Name of the master ZIP from the last replay (to detect compaction).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub master_zip_name: String,
    /// Names of all ZIPs seen in the last full replay. A new ZIP name that
    /// does not appear here triggers a full replay regardless of mtime
    /// (guards against same-second writes on fast SSDs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seen_zip_names: Vec<String>,
}

impl Vault {
    pub(crate) fn read_omnifocus_sync(&self) -> OmniFocusSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write_omnifocus_sync(&self, state: &OmniFocusSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// .ofocus reader — ZIP enumeration + XML transaction replay.

/// One ZIP entry in the `.ofocus` directory with its sort key and mtime.
#[derive(Debug, Clone)]
struct ZipEntry {
    /// Full path to the .zip file.
    path: PathBuf,
    /// File name (without directory), used for sorting and master detection.
    name: String,
    /// mtime as Unix seconds.
    mtime: u64,
}

/// Enumerate all `.zip` files in the `.ofocus` directory, sorted by name
/// (lexicographic = chronological for the filename scheme the format uses).
fn enumerate_zips(dir: &Path) -> Result<Vec<ZipEntry>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading .ofocus directory {}", dir.display()))?;
    let mut zips: Vec<ZipEntry> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.ends_with(".zip") {
                return None;
            }
            let mtime = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Some(ZipEntry { path, name, mtime })
        })
        .collect();
    zips.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(zips)
}

/// Returns the contents.xml string from a .zip bundle. Returns `None` if
/// the zip has no contents.xml (e.g. merge-point files with no data).
fn read_contents_xml(zip_path: &Path) -> Result<Option<String>> {
    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", zip_path.display()))?;
    // Extract to a String before dropping the ZipFile borrow.
    let body = match archive.by_name("contents.xml") {
        Ok(mut entry) => {
            let mut s = String::new();
            entry.read_to_string(&mut s).with_context(|| {
                format!("reading contents.xml from {}", zip_path.display())
            })?;
            s
        }
        Err(_) => return Ok(None), // merge-point ZIP or empty bundle
    };
    Ok(Some(body))
}

// ---------------------------------------------------------------------------
// In-memory entity state (built by replaying all transactions).

/// The combined in-memory state after replaying the transaction log.
#[derive(Debug, Default)]
struct OFState {
    /// task id → reconstructed task node
    tasks: HashMap<String, OFTask>,
    /// context/tag id → name
    contexts: HashMap<String, String>,
    /// folder id → (name, parent folder id)
    folders: HashMap<String, (String, Option<String>)>,
}

/// One OmniFocus task/action, reconstructed from the transaction log.
#[derive(Debug, Clone, Default)]
pub(crate) struct OFTask {
    pub id: String,
    /// The task title (from `<name>`).
    pub name: String,
    /// HTML note body (from `<note><text>`).
    pub note: String,
    /// ISO-8601 creation time (from `<added>`).
    pub added: String,
    /// ISO-8601 modification time (from `<modified>`).
    pub modified: String,
    /// ISO-8601 due datetime (from `<due>`).
    pub due: Option<String>,
    /// ISO-8601 defer/start datetime (from `<start>`).
    pub start: Option<String>,
    /// ISO-8601 completion time (from `<completed>`).
    pub completed: Option<String>,
    /// Flagged flag (from `<flagged>`).
    pub flagged: bool,
    /// `true` when this task node has a `<project>` child — it is a project
    /// root, not an action.
    pub is_project: bool,
    /// Project status when `is_project` (active/inactive/done/dropped).
    pub project_status: String,
    /// Parent folder id (from `<project><folder idref>`) for projects.
    pub folder_id: Option<String>,
    /// Parent task id (from `<task idref>`) for subtasks.
    pub parent_task_id: Option<String>,
    /// Context/tag ids (from `<context idref>` children). OF3 allows one
    /// context; OF4 introduced multiple tags per task — we collect all of them.
    /// NOTE: the exact element name for OF4 multi-tag references is unconfirmed
    /// without a real sample (the community spec predates OF3 multi-tag). Both
    /// `<context idref>` (OF3 spec) and `<tag idref>` (plausible OF4 element
    /// name) are collected here. Needs-sample to confirm.
    pub context_ids: Vec<String>,
    /// `true` when `<inbox>` child is present (inbox task with no project).
    pub in_inbox: bool,
    /// RRULE-style repeat string (from `<repetition-rule>`).
    pub repetition_rule: Option<String>,
    /// `true` when a `delete` transaction removed this task.
    pub deleted: bool,
}

// ---------------------------------------------------------------------------
// XML parser — processes one `contents.xml` string into state mutations.

/// Parse one `contents.xml` and apply its operations to `state`.
/// Operations: op absent/"add" or "update" → upsert; op="delete" → mark
/// deleted; op="reference" → included for context/folder defs but not tasks.
///
/// Uses an explicit element-name stack to track path without depth arithmetic.
pub(crate) fn apply_xml(state: &mut OFState, xml: &str) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    // Element-name stack. Index 0 = outermost non-omnifocus element.
    // path[0] = top-level entity ("task"/"context"/"folder"/…)
    // path[1] = direct child of the entity ("name","project","note",…)
    // path[2] = grandchild ("text" inside note, "folder"/"status" inside project)
    let mut path: Vec<String> = Vec::new();

    // State for the current top-level entity.
    let mut current_task: Option<OFTask> = None;
    let mut current_ctx_id: Option<String> = None;
    let mut current_ctx_name = String::new();
    let mut current_fld_id: Option<String> = None;
    let mut current_fld_name = String::new();
    let mut current_fld_parent: Option<String> = None;
    // True when we are inside <project> child of a task (path[1]=="project").
    let mut in_project_child = false;
    // True when we should skip the current top-level entity.
    let mut skip = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();

                if path.is_empty() {
                    // The root <omnifocus> element — push but don't act.
                    path.push(local);
                    continue;
                }

                if path.len() == 1 {
                    // A top-level entity inside <omnifocus>.
                    skip = false;
                    in_project_child = false;
                    current_task = None;
                    current_ctx_id = None;
                    current_fld_id = None;
                    current_ctx_name.clear();
                    current_fld_name.clear();
                    current_fld_parent = None;

                    let id_attr = attr_val(&e, b"id");
                    let op = attr_val(&e, b"op").unwrap_or_default();

                    match op.as_str() {
                        "delete" => {
                            // op="delete" with only an id attr (no children usually).
                            match local.as_str() {
                                "task" => {
                                    if let Some(id) = id_attr {
                                        let t = state.tasks.entry(id.clone()).or_insert_with(|| {
                                            OFTask { id: id.clone(), ..Default::default() }
                                        });
                                        t.deleted = true;
                                    }
                                }
                                "context" => {
                                    if let Some(id) = id_attr {
                                        state.contexts.remove(&id);
                                    }
                                }
                                "folder" => {
                                    if let Some(id) = id_attr {
                                        state.folders.remove(&id);
                                    }
                                }
                                _ => {}
                            }
                            skip = true;
                        }
                        "reference" => {
                            // Reference copies: populate contexts/folders (they help
                            // resolve cross-refs in transaction files), but DO NOT
                            // overwrite tasks already in state.
                            match local.as_str() {
                                "task" => skip = true, // don't overwrite real task state
                                "context" => {
                                    if let Some(id) = id_attr {
                                        current_ctx_id = Some(id);
                                    } else {
                                        skip = true;
                                    }
                                }
                                "folder" => {
                                    if let Some(id) = id_attr {
                                        current_fld_id = Some(id);
                                    } else {
                                        skip = true;
                                    }
                                }
                                _ => skip = true,
                            }
                        }
                        _ => {
                            // "add", "update", or no op (implicit add).
                            match local.as_str() {
                                "task" => {
                                    if let Some(id) = id_attr {
                                        current_task = Some(OFTask {
                                            id: id.clone(),
                                            ..Default::default()
                                        });
                                    } else {
                                        skip = true;
                                    }
                                }
                                "context" => {
                                    if let Some(id) = id_attr {
                                        current_ctx_id = Some(id);
                                    } else {
                                        skip = true;
                                    }
                                }
                                "folder" => {
                                    if let Some(id) = id_attr {
                                        current_fld_id = Some(id);
                                    } else {
                                        skip = true;
                                    }
                                }
                                _ => skip = true, // setting, perspective — not needed
                            }
                        }
                    }
                    path.push(local);
                    continue;
                }

                // Deeper elements — skip if the parent entity was skipped.
                if skip {
                    path.push(local);
                    continue;
                }

                if path.len() == 2 {
                    // Direct child of the top-level entity.
                    match local.as_str() {
                        "project" => {
                            if let Some(t) = current_task.as_mut() {
                                t.is_project = true;
                            }
                            in_project_child = true;
                        }
                        "note" => {} // container for <text>
                        "inbox" => {
                            if let Some(t) = current_task.as_mut() {
                                t.in_inbox = true;
                            }
                        }
                        "task" => {
                            // <task idref="…"> — parent task reference
                            if let Some(t) = current_task.as_mut() {
                                if let Some(idref) = attr_val(&e, b"idref") {
                                    t.parent_task_id = Some(idref);
                                }
                            }
                        }
                        "context" | "tag" => {
                            // <context idref="…"> or <tag idref="…"> inside a task.
                            // OF3 uses <context>; OF4 may use <tag> for multi-tag.
                            // Collect all of them — Needs-sample to confirm OF4 name.
                            if let Some(t) = current_task.as_mut() {
                                if let Some(idref) = attr_val(&e, b"idref") {
                                    t.context_ids.push(idref);
                                }
                            }
                        }
                        "folder" => {
                            // <folder idref="…"> inside a folder entity = parent ref
                            if current_fld_id.is_some() {
                                if let Some(idref) = attr_val(&e, b"idref") {
                                    current_fld_parent = Some(idref);
                                }
                            }
                        }
                        _ => {}
                    }
                } else if path.len() == 3 && in_project_child {
                    // Grandchild inside <project>: <folder idref>, <status>, …
                    match local.as_str() {
                        "folder" => {
                            if let Some(t) = current_task.as_mut() {
                                if let Some(idref) = attr_val(&e, b"idref") {
                                    t.folder_id = Some(idref);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                path.push(local);
            }

            Ok(Event::Empty(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();

                // A self-closing element at the top level (e.g., <task id="…" op="delete"/>).
                if path.len() == 1 {
                    let op = attr_val(&e, b"op").unwrap_or_default();
                    if op == "delete" {
                        match local.as_str() {
                            "task" => {
                                if let Some(id) = attr_val(&e, b"id") {
                                    let t = state.tasks.entry(id.clone()).or_insert_with(|| {
                                        OFTask { id: id.clone(), ..Default::default() }
                                    });
                                    t.deleted = true;
                                }
                            }
                            // Self-closing context/folder deletes (e.g. <context id="…" op="delete"/>)
                            "context" => {
                                if let Some(id) = attr_val(&e, b"id") {
                                    state.contexts.remove(&id);
                                }
                            }
                            "folder" => {
                                if let Some(id) = attr_val(&e, b"id") {
                                    state.folders.remove(&id);
                                }
                            }
                            _ => {}
                        }
                    }
                    continue;
                }

                if skip {
                    continue;
                }

                if path.len() == 2 {
                    // Self-closing child of the top-level entity.
                    match local.as_str() {
                        "inbox" => {
                            if let Some(t) = current_task.as_mut() {
                                t.in_inbox = true;
                            }
                        }
                        "task" => {
                            if let Some(t) = current_task.as_mut() {
                                if let Some(idref) = attr_val(&e, b"idref") {
                                    t.parent_task_id = Some(idref);
                                }
                            }
                        }
                        "context" | "tag" => {
                            // Self-closing <context idref="…"/> or <tag idref="…"/> inside a task.
                            if let Some(t) = current_task.as_mut() {
                                if let Some(idref) = attr_val(&e, b"idref") {
                                    t.context_ids.push(idref);
                                }
                            }
                        }
                        "folder" => {
                            if current_fld_id.is_some() {
                                if let Some(idref) = attr_val(&e, b"idref") {
                                    current_fld_parent = Some(idref);
                                }
                            }
                        }
                        _ => {}
                    }
                } else if path.len() == 3 && in_project_child {
                    // Self-closing grandchild inside <project>.
                    if local == "folder" {
                        if let Some(t) = current_task.as_mut() {
                            if let Some(idref) = attr_val(&e, b"idref") {
                                t.folder_id = Some(idref);
                            }
                        }
                    }
                }
            }

            Ok(Event::Text(e)) => {
                if skip {
                    continue;
                }
                let text = match e.decode() {
                    Ok(decoded) => match quick_xml::escape::unescape(&decoded) {
                        Ok(unescaped) => unescaped.into_owned(),
                        Err(_) => decoded.into_owned(),
                    },
                    Err(_) => String::new(),
                };
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }

                // Determine what element we are the text content of.
                // path.last() is the current open element.
                let current_el = path.last().map(|s| s.as_str()).unwrap_or("");
                let parent_el = path.iter().rev().nth(1).map(|s| s.as_str()).unwrap_or("");

                // path = ["omnifocus", entity, child, ...]
                // path.len() == 3 → we are inside a direct child of the top entity
                // path.len() == 4 → we are inside a grandchild (e.g., <note><text>, <project><status>)
                match path.len() {
                    3 => {
                        // Direct child of top-level entity: current_el is the child name.
                        // e.g. path = ["omnifocus", "folder", "name"]
                        if let Some(t) = current_task.as_mut() {
                            match current_el {
                                "name" => t.name = text,
                                "added" => t.added = text,
                                "modified" => t.modified = text,
                                "due" => t.due = Some(text),
                                "start" => t.start = Some(text),
                                "completed" => t.completed = Some(text),
                                "flagged" => t.flagged = text.eq_ignore_ascii_case("true"),
                                "repetition-rule" | "repeat" => {
                                    t.repetition_rule = Some(text)
                                }
                                _ => {}
                            }
                        } else if current_ctx_id.is_some() && current_el == "name" {
                            current_ctx_name = text;
                        } else if current_fld_id.is_some() && current_el == "name" {
                            current_fld_name = text;
                        }
                    }
                    4 => {
                        // Grandchild: path = ["omnifocus", entity, intermediate, leaf]
                        match parent_el {
                            "note" => {
                                // <note><text>…</text>
                                if current_el == "text" {
                                    if let Some(t) = current_task.as_mut() {
                                        t.note = text;
                                    }
                                }
                            }
                            "project" => {
                                // <project><status>…</status>
                                if current_el == "status" {
                                    if let Some(t) = current_task.as_mut() {
                                        t.project_status = text;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }

            Ok(Event::End(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();

                if path.len() == 2 && path[1] == local {
                    // Closing the top-level entity.
                    if !skip {
                        match local.as_str() {
                            "task" => {
                                if let Some(t) = current_task.take() {
                                    if !t.id.is_empty() {
                                        let entry = state
                                            .tasks
                                            .entry(t.id.clone())
                                            .or_insert_with(|| OFTask {
                                                id: t.id.clone(),
                                                ..Default::default()
                                            });
                                        merge_task(entry, &t);
                                    }
                                }
                            }
                            "context" => {
                                if let Some(id) = current_ctx_id.take() {
                                    if !current_ctx_name.is_empty() {
                                        state.contexts.insert(id, current_ctx_name.clone());
                                    }
                                }
                                current_ctx_name.clear();
                            }
                            "folder" => {
                                if let Some(id) = current_fld_id.take() {
                                    if !current_fld_name.is_empty() {
                                        state.folders.insert(
                                            id,
                                            (current_fld_name.clone(), current_fld_parent.take()),
                                        );
                                    } else {
                                        // Name is empty — still clear the parent.
                                        current_fld_parent = None;
                                    }
                                } else {
                                    current_fld_parent = None;
                                }
                                current_fld_name.clear();
                            }
                            _ => {}
                        }
                    }
                    skip = false;
                    in_project_child = false;
                } else if path.len() == 3 && path[2] == local && local == "project" {
                    // Closing </project> at depth 3 (child of task entity).
                    in_project_child = false;
                }

                path.pop();
            }

            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Merge `update` into `base`: for each field in `update`, if it is non-empty
/// (or explicitly set), overwrite the corresponding field in `base`.
/// This handles partial-update transactions that set only a changed field.
fn merge_task(base: &mut OFTask, update: &OFTask) {
    if !update.name.is_empty() {
        base.name.clone_from(&update.name);
    }
    if !update.note.is_empty() {
        base.note.clone_from(&update.note);
    }
    if !update.added.is_empty() {
        base.added.clone_from(&update.added);
    }
    if !update.modified.is_empty() {
        base.modified.clone_from(&update.modified);
    }
    if update.due.is_some() {
        base.due.clone_from(&update.due);
    }
    if update.start.is_some() {
        base.start.clone_from(&update.start);
    }
    if update.completed.is_some() {
        base.completed.clone_from(&update.completed);
    }
    if update.flagged {
        base.flagged = true;
    }
    if update.is_project {
        base.is_project = true;
    }
    if !update.project_status.is_empty() {
        base.project_status.clone_from(&update.project_status);
    }
    if update.folder_id.is_some() {
        base.folder_id.clone_from(&update.folder_id);
    }
    if update.parent_task_id.is_some() {
        base.parent_task_id.clone_from(&update.parent_task_id);
    }
    if !update.context_ids.is_empty() {
        // Merge context/tag ids from update into base (union, no duplicates).
        for cid in &update.context_ids {
            if !base.context_ids.contains(cid) {
                base.context_ids.push(cid.clone());
            }
        }
    }
    if update.in_inbox {
        base.in_inbox = true;
    }
    if update.repetition_rule.is_some() {
        base.repetition_rule.clone_from(&update.repetition_rule);
    }
    if update.deleted {
        base.deleted = true;
    }
}

/// Extract an attribute value as a String from a quick-xml BytesStart event.
fn attr_val(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

// ---------------------------------------------------------------------------
// ISO-8601 → RFC3339 local.

/// OmniFocus timestamps are ISO-8601 with ms, UTC, e.g. `2014-11-03T10:14:09.123Z`.
/// Convert to RFC3339 local (the tasks contract uses local time).
fn of_time(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .or_else(|_| {
            // Some OF timestamps have ms: "2014-11-03T10:14:09.123Z" — parse with %f
            DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3fZ")
                .map(|t| t.into())
                .or_else(|_| DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ"))
        })
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

// ---------------------------------------------------------------------------
// State → contract + raw rows.

/// Build a folder path string "Folder A > Folder B" by walking parent links.
fn folder_path(folder_id: &str, folders: &HashMap<String, (String, Option<String>)>) -> String {
    let mut parts = Vec::new();
    let mut current = folder_id.to_string();
    let mut guard = 0u32;
    loop {
        guard += 1;
        if guard > 20 {
            break; // cycle guard
        }
        match folders.get(&current) {
            Some((name, parent)) => {
                parts.push(name.clone());
                match parent {
                    Some(pid) => current = pid.clone(),
                    None => break,
                }
            }
            None => break,
        }
    }
    parts.reverse();
    parts.join(" > ")
}

/// Map an [`OFTask`] → a normalized [`Task`] for the tasks contract. Returns
/// `None` for tasks without a name or id, deleted tasks, project nodes, and
/// completed tasks (they go into the completed fate map, not the snapshot).
///
/// `all_tasks` is the full task map; it is used to walk the parent chain for
/// nested subtasks to find the containing project.
pub(crate) fn of_task_to_task(
    t: &OFTask,
    contexts: &HashMap<String, String>,
    folders: &HashMap<String, (String, Option<String>)>,
    project_name_by_id: &HashMap<String, String>,
    all_tasks: &HashMap<String, OFTask>,
) -> Option<Task> {
    if t.deleted || t.name.is_empty() || t.id.is_empty() {
        return None;
    }
    if t.is_project {
        return None; // project nodes are represented in ProjectInfo, not as tasks
    }
    if t.completed.is_some() {
        return None; // completed tasks are handled by the fate closure
    }

    // Project: walk the parent_task_id chain upward until we hit an is_project
    // node. This correctly resolves nested subtasks (OF power users nest deeply).
    // The `project_name_by_id` lookup only contains project-root nodes, so a
    // direct action child resolves in one step; a subtask under another action
    // takes more. Cycle guard prevents infinite loops.
    let project = if t.in_inbox {
        "Inbox".to_string()
    } else {
        let mut proj_name = String::new();
        if let Some(first_pid) = &t.parent_task_id {
            // Fast path: direct child of a project.
            if let Some(name) = project_name_by_id.get(first_pid) {
                proj_name = name.clone();
            } else {
                // Slow path: walk ancestors until we find a project root.
                let mut current_pid = first_pid.clone();
                let mut guard = 0u32;
                loop {
                    guard += 1;
                    if guard > 50 {
                        break; // cycle guard
                    }
                    if let Some(parent_task) = all_tasks.get(&current_pid) {
                        if parent_task.is_project && !parent_task.name.is_empty() {
                            proj_name = parent_task.name.clone();
                            break;
                        }
                        match &parent_task.parent_task_id {
                            Some(pid) => current_pid = pid.clone(),
                            None => break,
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        proj_name
    };

    // Tags: resolve all context/tag ids to names. OF3 stores one context per
    // task; OF4 allows multiple tags. Both <context idref> and <tag idref>
    // children are collected into context_ids during parsing. Names are resolved
    // from the contexts map (populated from <context id> entity declarations).
    // NOTE: the actual OF4 element name for multi-tag references is unconfirmed
    // without a real sample — Needs-sample to verify.
    let tags: Vec<String> = t
        .context_ids
        .iter()
        .filter_map(|cid| contexts.get(cid))
        .cloned()
        .collect();

    let mut extra: Map<String, Value> = Map::new();
    if t.flagged {
        extra.insert("flagged".into(), Value::Bool(true));
    }
    if let Some(folder_id) = &t.folder_id {
        let fp = folder_path(folder_id, folders);
        if !fp.is_empty() {
            extra.insert("folder".into(), Value::String(fp));
        }
    }
    if !t.context_ids.is_empty() {
        // Store all context/tag ids for full-fidelity round-trip.
        let ids: Vec<Value> = t.context_ids.iter().map(|s| Value::String(s.clone())).collect();
        extra.insert("context_ids".into(), Value::Array(ids));
    }

    Some(Task {
        source: SOURCE.into(),
        id: t.id.clone(),
        title: t.name.clone(),
        project,
        notes: strip_html(&t.note),
        status: "open".into(),
        priority: if t.flagged { 5 } else { 0 }, // flagged = high priority
        due: t.due.as_deref().map(of_time),
        start: t.start.as_deref().map(of_time),
        // Needs-sample: whether OmniFocus due/defer dates are always time-anchored
        // (UTC timestamps with a time component) or sometimes date-only (local
        // end-of-day midnight) is unconfirmed without a real .ofocus container.
        // The community spec types them as "datetime" strings. Default to false
        // (timed) — verify against a real container once available.
        all_day: false,
        recurrence: t.repetition_rule.clone(),
        tags,
        subtasks: Vec::new(),
        created: if t.added.is_empty() { None } else { Some(of_time(&t.added)) },
        modified: if t.modified.is_empty() { None } else { Some(of_time(&t.modified)) },
        completed: None,
        extra,
    })
}

/// Minimal HTML stripper: removes tags, unescapes `&amp;`/`&lt;`/`&gt;`.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&#xA;", "\n")
}

// ---------------------------------------------------------------------------
// Raw row (full-fidelity flat representation).

/// One raw row in `tasks/omnifocus/raw/YYYY-MM.jsonl`, upserted by id,
/// partitioned by `added` month.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct RawOFTask {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub added: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub modified: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<String>,
    #[serde(default)]
    pub flagged: bool,
    #[serde(default)]
    pub is_project: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    /// All context/tag ids assigned to this task. OF3 allows one; OF4 allows
    /// multiple. Old rows with a single `context_id` string field deserialize
    /// as an empty Vec (serde default) — the field was renamed for OF4 support.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_ids: Vec<String>,
    #[serde(default)]
    pub in_inbox: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetition_rule: Option<String>,
}

impl RawOFTask {
    fn from_of_task(t: &OFTask) -> Self {
        RawOFTask {
            id: t.id.clone(),
            name: t.name.clone(),
            note: t.note.clone(),
            added: t.added.clone(),
            modified: t.modified.clone(),
            due: t.due.clone(),
            start: t.start.clone(),
            completed: t.completed.clone(),
            flagged: t.flagged,
            is_project: t.is_project,
            project_status: t.project_status.clone(),
            folder_id: t.folder_id.clone(),
            parent_task_id: t.parent_task_id.clone(),
            context_ids: t.context_ids.clone(),
            in_inbox: t.in_inbox,
            repetition_rule: t.repetition_rule.clone(),
        }
    }

    fn partition_key(&self) -> Option<String> {
        // Partition by the year-month of the creation date.
        let ts = if self.added.is_empty() { return None } else { &self.added };
        // Extract YYYY-MM from the ISO-8601 timestamp.
        if ts.len() >= 7 { Some(ts[..7].to_string()) } else { None }
    }
}

/// Upsert raw rows into `tasks/omnifocus/raw/YYYY-MM.jsonl` partitioned by
/// creation month; returns count of net-new rows.
fn upsert_raw(vault: &Vault, rows: Vec<RawOFTask>) -> Result<u64> {
    use crate::store::Partition;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawOFTask>> = BTreeMap::new();
    for r in rows {
        // Fall back to a fixed key for tasks with no added date.
        let key = r.partition_key().unwrap_or_else(|| "unknown".to_string());
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        // Skip the "unknown" partition — these are incomplete records that
        // can't be reliably keyed.
        if month == "unknown" {
            continue;
        }
        let mut existing: Vec<RawOFTask> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> =
            existing.iter().enumerate().map(|(i, r)| (r.id.clone(), i)).collect();
        for r in fresh {
            match idx.get(&r.id).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(r.id.clone(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| a.added.cmp(&b.added).then_with(|| a.id.cmp(&b.id)));
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Entry point: pull the current OmniFocus state into the vault.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let Some(ofocus_dir) = ofocus_path() else {
        return Ok(PullOutcome {
            headline: "OmniFocus database not found — grant Full Disk Access or install \
                       OmniFocus"
                .into(),
            counts: BTreeMap::from([("open", 0u64)]),
        });
    };
    pull_with(vault, &ofocus_dir)
}

/// The testable pull body over an injected `.ofocus` path.
pub(crate) fn pull_with(vault: &Vault, ofocus_dir: &Path) -> Result<PullOutcome> {
    let state = vault.read_omnifocus_sync();

    let zips = enumerate_zips(ofocus_dir)
        .context("OmniFocus: could not read .ofocus directory")?;
    if zips.is_empty() {
        return Ok(PullOutcome {
            headline: "OmniFocus database is empty — no data yet".into(),
            counts: BTreeMap::from([("open", 0u64)]),
        });
    }

    // Detect the master ZIP (starts with "00000000000000=").
    let master = zips.iter().find(|z| z.name.starts_with("00000000000000="));

    // Decide whether a full replay is needed:
    // - No cursor yet (first run).
    // - Master ZIP has changed (compaction happened).
    // - Any new ZIP appeared by mtime since the cursor.
    // - A ZIP name is not in the seen-names set (guards against same-second
    //   writes on fast SSDs where mtime comparison would miss the new ZIP).
    let master_name = master.map(|m| m.name.as_str()).unwrap_or("");
    let seen: std::collections::HashSet<&str> =
        state.seen_zip_names.iter().map(|s| s.as_str()).collect();
    let needs_full = state.last_zip_mtime == 0
        || state.master_zip_name != master_name
        || zips.iter().any(|z| z.mtime > state.last_zip_mtime)
        || zips.iter().any(|z| !seen.contains(z.name.as_str()));

    if !needs_full {
        return Ok(PullOutcome {
            headline: "OmniFocus: no new data since last sync".into(),
            counts: BTreeMap::from([("open", 0u64)]),
        });
    }

    // Replay all ZIPs to build current state.
    let mut of_state = OFState::default();
    let mut parsed_any_content = false;
    for zip in &zips {
        match read_contents_xml(&zip.path) {
            Ok(Some(xml)) => {
                parsed_any_content = true;
                if let Err(e) = apply_xml(&mut of_state, &xml) {
                    // Non-fatal: log and continue with remaining ZIPs.
                    let _ = e; // tolerate per-bundle parse errors
                }
            }
            Ok(None) => {} // merge-point zip with no contents.xml
            Err(_) => {}   // unreadable zip — skip
        }
    }

    // Zero-parse guard (blocking defect fix): if we got content from the ZIPs
    // but ended up with zero tasks in state, treat this as a parse failure and
    // do NOT advance the cursor. This prevents the silent zero-collection bug
    // where every field name is wrong but the cursor still advances, making
    // subsequent syncs skip the same ZIP set forever.
    //
    // Rationale: a legitimate empty database has no ZIPs with task XML; if we
    // had parseable content but zero tasks, something is structurally wrong with
    // the parser's element-name assumptions. Needs-sample to confirm real shape.
    if parsed_any_content && of_state.tasks.is_empty() {
        return Ok(PullOutcome {
            headline: "OmniFocus: parsed ZIP content but found zero tasks — parser may \
                       not match this container's XML schema. Cursor NOT advanced. \
                       A real .ofocus sample is needed to confirm element names. \
                       (Needs-sample)"
                .into(),
            counts: BTreeMap::from([("open", 0u64)]),
        });
    }

    // Build project-id → name index (projects are is_project=true, non-deleted).
    let project_name_by_id: HashMap<String, String> = of_state
        .tasks
        .values()
        .filter(|t| t.is_project && !t.deleted && !t.name.is_empty())
        .map(|t| (t.id.clone(), t.name.clone()))
        .collect();

    // Build ProjectInfo list for apply_tasks_sync.
    let projects: Vec<ProjectInfo> = of_state
        .tasks
        .values()
        .filter(|t| t.is_project && !t.deleted)
        .map(|t| ProjectInfo { id: t.id.clone(), name: t.name.clone() })
        .collect();

    // Raw firehose: all non-deleted tasks (including completed, including projects).
    let raw_rows: Vec<RawOFTask> = of_state
        .tasks
        .values()
        .filter(|t| !t.deleted && !t.id.is_empty() && !t.added.is_empty())
        .map(|t| RawOFTask::from_of_task(t))
        .collect();
    let raw_new = upsert_raw(vault, raw_rows)?;

    // Fresh open task list (no completed, no deleted, no projects).
    // Pass `&of_state.tasks` so the parent-chain walker can resolve nested subtasks.
    let fresh: Vec<Task> = of_state
        .tasks
        .values()
        .filter(|t| !t.deleted && !t.is_project)
        .filter_map(|t| {
            of_task_to_task(
                t,
                &of_state.contexts,
                &of_state.folders,
                &project_name_by_id,
                &of_state.tasks,
            )
        })
        .collect();

    // Completed tasks by id → completion time (for the fate closure).
    let completed_by_id: HashMap<String, String> = of_state
        .tasks
        .values()
        .filter(|t| t.completed.is_some() && !t.deleted)
        .map(|t| (t.id.clone(), of_time(t.completed.as_deref().unwrap_or(""))))
        .collect();

    let deleted_ids: std::collections::HashSet<String> = of_state
        .tasks
        .values()
        .filter(|t| t.deleted)
        .map(|t| t.id.clone())
        .collect();

    let stats = vault
        .apply_tasks_sync(SOURCE, &projects, fresh, |t| {
            if let Some(when) = completed_by_id.get(&t.id) {
                TaskFate::Completed(Some(when.clone()))
            } else if deleted_ids.contains(&t.id) {
                TaskFate::Deleted
            } else {
                TaskFate::Unknown
            }
        })
        .context("omnifocus: applying task sync")?;

    // Advance the cursor — only reached when tasks were found (zero-parse guard
    // above prevents cursor advance on empty-state replays).
    let new_mtime = zips.iter().map(|z| z.mtime).max().unwrap_or(0);
    let new_seen: Vec<String> = zips.iter().map(|z| z.name.clone()).collect();
    let new_state = OmniFocusSyncState {
        updated: Local::now().to_rfc3339(),
        last_zip_mtime: new_mtime,
        master_zip_name: master_name.to_string(),
        seen_zip_names: new_seen,
    };
    vault.write_omnifocus_sync(&new_state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("open", stats.open);
    counts.insert("completed", stats.completed);
    counts.insert("deleted", stats.deleted);
    counts.insert("created", stats.created);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!("{} open OmniFocus tasks", stats.open),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-omnifocus-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Minimal XML fixtures (based on the tomzx/ofocus-format spec).

    /// Master XML: two tasks (one action, one project) + one context + one folder.
    const MASTER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<omnifocus app-id="com.omnigroup.OmniFocus4" xmlns="http://www.omnigroup.com/namespace/OmniFocus/v2">
<folder id="fld001">
    <added>2026-01-01T00:00:00.000Z</added>
    <modified>2026-01-01T00:00:00.000Z</modified>
    <name>Work</name>
    <rank>0</rank>
</folder>
<context id="ctx001">
    <added>2026-01-01T00:00:00.000Z</added>
    <modified>2026-01-01T00:00:00.000Z</modified>
    <name>Email</name>
    <rank>0</rank>
</context>
<task id="proj001">
    <project>
        <folder idref="fld001"/>
        <last-review>2026-06-01T00:00:00.000Z</last-review>
        <review-interval>@1w</review-interval>
        <status>active</status>
    </project>
    <added>2026-01-10T09:00:00.000Z</added>
    <modified>2026-06-01T10:00:00.000Z</modified>
    <name>Launch v1</name>
    <rank>0</rank>
</task>
<task id="task001">
    <task idref="proj001"/>
    <context idref="ctx001"/>
    <added>2026-06-01T08:00:00.000Z</added>
    <modified>2026-06-10T09:00:00.000Z</modified>
    <name>Write release notes</name>
    <due>2026-06-15T17:00:00.000Z</due>
    <flagged>true</flagged>
    <note><text>Make it clear and concise</text></note>
    <rank>0</rank>
</task>
</omnifocus>"#;

    /// Transaction XML: update task001 (add a defer date), add task002, delete task003.
    const TRANSACTION_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<omnifocus app-id="com.omnigroup.OmniFocus4" xmlns="http://www.omnigroup.com/namespace/OmniFocus/v2">
<task id="proj001" op="reference">
    <added>2026-01-10T09:00:00.000Z</added>
    <modified>2026-01-10T09:00:00.000Z</modified>
    <name>Launch v1</name>
    <rank>0</rank>
</task>
<task id="task001" op="update">
    <start>2026-06-12T09:00:00.000Z</start>
    <modified>2026-06-12T09:00:00.000Z</modified>
</task>
<task id="task002">
    <task idref="proj001"/>
    <added>2026-06-12T10:00:00.000Z</added>
    <modified>2026-06-12T10:00:00.000Z</modified>
    <name>Tag the release</name>
    <rank>1</rank>
</task>
<task id="task003" op="delete"/>
</omnifocus>"#;

    /// Transaction XML: complete task001.
    const COMPLETE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<omnifocus app-id="com.omnigroup.OmniFocus4" xmlns="http://www.omnigroup.com/namespace/OmniFocus/v2">
<task id="task001" op="update">
    <completed>2026-06-14T16:30:00.000Z</completed>
    <modified>2026-06-14T16:30:00.000Z</modified>
</task>
</omnifocus>"#;

    // -----------------------------------------------------------------------
    // apply_xml unit tests.

    #[test]
    fn parses_master_xml_into_tasks_context_folder() {
        let mut state = OFState::default();
        apply_xml(&mut state, MASTER_XML).unwrap();

        // Folder parsed.
        assert!(state.folders.contains_key("fld001"), "folder fld001 parsed");
        assert_eq!(state.folders["fld001"].0, "Work");

        // Context parsed.
        assert!(state.contexts.contains_key("ctx001"), "context ctx001 parsed");
        assert_eq!(state.contexts["ctx001"], "Email");

        // Project task parsed.
        let proj = &state.tasks["proj001"];
        assert!(proj.is_project, "proj001 is a project");
        assert_eq!(proj.name, "Launch v1");
        assert_eq!(proj.folder_id.as_deref(), Some("fld001"));
        assert!(!proj.deleted);

        // Action task parsed.
        let t = &state.tasks["task001"];
        assert_eq!(t.name, "Write release notes");
        assert_eq!(t.parent_task_id.as_deref(), Some("proj001"));
        assert!(t.context_ids.contains(&"ctx001".to_string()), "context ctx001 in context_ids");
        assert_eq!(t.due.as_deref(), Some("2026-06-15T17:00:00.000Z"));
        assert!(t.flagged);
        assert_eq!(t.note, "Make it clear and concise");
        assert!(!t.deleted);
    }

    #[test]
    fn transaction_update_merges_start_and_new_task_and_delete() {
        let mut state = OFState::default();
        apply_xml(&mut state, MASTER_XML).unwrap();
        apply_xml(&mut state, TRANSACTION_XML).unwrap();

        // task001: start date was added by update, name preserved from master.
        let t1 = &state.tasks["task001"];
        assert_eq!(t1.name, "Write release notes", "update must not clear name");
        assert_eq!(t1.start.as_deref(), Some("2026-06-12T09:00:00.000Z"));

        // task002: new task added by transaction.
        assert!(state.tasks.contains_key("task002"), "task002 added by transaction");
        assert_eq!(state.tasks["task002"].name, "Tag the release");

        // task003: deleted by transaction.
        assert!(state.tasks["task003"].deleted, "task003 deleted");

        // reference task (proj001) does NOT overwrite the existing task.
        assert_eq!(state.tasks["proj001"].name, "Launch v1");
    }

    #[test]
    fn completion_transaction_marks_task_completed() {
        let mut state = OFState::default();
        apply_xml(&mut state, MASTER_XML).unwrap();
        apply_xml(&mut state, COMPLETE_XML).unwrap();

        let t = &state.tasks["task001"];
        assert_eq!(t.completed.as_deref(), Some("2026-06-14T16:30:00.000Z"));
    }

    // -----------------------------------------------------------------------
    // of_task_to_task mapping.

    #[test]
    fn maps_open_task_to_contract_with_project_tags_due_priority() {
        let mut state = OFState::default();
        apply_xml(&mut state, MASTER_XML).unwrap();
        let project_names: HashMap<String, String> =
            [("proj001".to_string(), "Launch v1".to_string())].into_iter().collect();
        let t = of_task_to_task(
            &state.tasks["task001"],
            &state.contexts,
            &state.folders,
            &project_names,
            &state.tasks,
        )
        .unwrap();

        assert_eq!(t.source, "omnifocus");
        assert_eq!(t.id, "task001");
        assert_eq!(t.title, "Write release notes");
        assert_eq!(t.project, "Launch v1");
        assert_eq!(t.notes, "Make it clear and concise");
        assert_eq!(t.status, "open");
        assert_eq!(t.priority, 5, "flagged → priority 5 (high)");
        assert!(t.tags.contains(&"Email".to_string()), "context resolved to tag name");
        assert!(t.due.is_some(), "due date present");
        assert_eq!(t.extra.get("flagged").and_then(Value::as_bool), Some(true));
        // context_ids stored as JSON array in extra.
        assert!(
            t.extra
                .get("context_ids")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().any(|x| x.as_str() == Some("ctx001")))
                .unwrap_or(false),
            "context_ids contains ctx001"
        );
    }

    #[test]
    fn project_nodes_are_excluded_from_task_output() {
        let mut state = OFState::default();
        apply_xml(&mut state, MASTER_XML).unwrap();
        let project_names = HashMap::new();
        // proj001 is is_project=true and should not map to a Task.
        let result = of_task_to_task(
            &state.tasks["proj001"],
            &state.contexts,
            &state.folders,
            &project_names,
            &state.tasks,
        );
        assert!(result.is_none(), "project node must not map to a task");
    }

    #[test]
    fn completed_tasks_are_excluded_from_task_output() {
        let mut state = OFState::default();
        apply_xml(&mut state, MASTER_XML).unwrap();
        apply_xml(&mut state, COMPLETE_XML).unwrap();
        let project_names = HashMap::new();
        // task001 now has completed set → excluded from Task list.
        let result = of_task_to_task(
            &state.tasks["task001"],
            &state.contexts,
            &state.folders,
            &project_names,
            &state.tasks,
        );
        assert!(result.is_none(), "completed task must not appear in open list");
    }

    // -----------------------------------------------------------------------
    // of_time timestamp conversion.

    #[test]
    fn of_time_parses_utc_with_milliseconds() {
        let converted = of_time("2026-06-15T17:00:00.000Z");
        // Must be a valid RFC3339 local timestamp.
        assert!(
            DateTime::parse_from_rfc3339(&converted).is_ok(),
            "converted time must be valid RFC3339: {converted}"
        );
        // The UTC instant must be preserved — verify by re-parsing and comparing
        // to the original timestamp parsed directly.
        let ts_converted = DateTime::parse_from_rfc3339(&converted).unwrap().timestamp();
        let ts_original =
            DateTime::parse_from_rfc3339("2026-06-15T17:00:00Z").unwrap().timestamp();
        assert_eq!(ts_converted, ts_original, "UTC instant must be preserved after local conversion");
    }

    // -----------------------------------------------------------------------
    // strip_html.

    #[test]
    fn strip_html_removes_tags_and_unescapes_entities() {
        assert_eq!(strip_html("<b>Hello</b> &amp; World"), "Hello & World");
        assert_eq!(strip_html("Plain text"), "Plain text");
        assert_eq!(strip_html("<p>Line 1</p><p>Line 2</p>"), "Line 1Line 2");
    }

    // -----------------------------------------------------------------------
    // folder_path.

    #[test]
    fn folder_path_resolves_nested_folders() {
        let mut folders: HashMap<String, (String, Option<String>)> = HashMap::new();
        folders.insert("f1".into(), ("Root".into(), None));
        folders.insert("f2".into(), ("Child".into(), Some("f1".into())));
        folders.insert("f3".into(), ("GrandChild".into(), Some("f2".into())));
        assert_eq!(folder_path("f3", &folders), "Root > Child > GrandChild");
        assert_eq!(folder_path("f1", &folders), "Root");
    }

    // -----------------------------------------------------------------------
    // Full pull_with test using a synthetic .ofocus directory.

    fn make_ofocus_dir(dir: &Path, bundles: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        for (zip_name, xml_content) in bundles {
            let zip_path = dir.join(zip_name);
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("contents.xml", opts).unwrap();
            std::io::Write::write_all(&mut zw, xml_content.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
    }

    #[test]
    fn full_pull_writes_contract_and_raw_and_advances_cursor() {
        let base = std::env::temp_dir()
            .join(format!("trove-of-fullpull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ofocus_dir = base.join("OmniFocus.ofocus");
        make_ofocus_dir(&ofocus_dir, &[("00000000000000=AAA+BBB.zip", MASTER_XML)]);

        let vault_dir = base.join("vault");
        let v = Vault::open_or_create(vault_dir).unwrap();

        let out = pull_with(&v, &ofocus_dir).unwrap();
        // Two open tasks: task001 (action), task002 was not added yet.
        // Only task001 is in the master (task002 is in the transaction).
        assert_eq!(out.counts.get("open"), Some(&1), "1 open action from master");
        assert_eq!(out.counts.get("created"), Some(&1));
        // Raw: task001 + proj001 (project) = 2 tasks with added date.
        assert!(out.counts.get("raw").copied().unwrap_or(0) >= 1);

        // Snapshot file exists.
        assert!(v.root().join("tasks/omnifocus/tasks.jsonl").exists());
        // Raw file exists.
        assert!(v.root().join("tasks/omnifocus/raw/2026-06.jsonl").exists());

        // Cursor advanced.
        let state = v.read_omnifocus_sync();
        assert!(!state.updated.is_empty());
        assert!(state.last_zip_mtime > 0);
        assert_eq!(state.master_zip_name, "00000000000000=AAA+BBB.zip");
    }

    #[test]
    fn completed_task_triggers_completion_event() {
        let base = std::env::temp_dir()
            .join(format!("trove-of-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ofocus_dir = base.join("OmniFocus.ofocus");
        let vault_dir = base.join("vault");
        let v = Vault::open_or_create(vault_dir).unwrap();

        // First pull: master only (task001 is open).
        make_ofocus_dir(&ofocus_dir, &[("00000000000000=AAA+BBB.zip", MASTER_XML)]);
        pull_with(&v, &ofocus_dir).unwrap();
        assert_eq!(v.load_tasks_snapshot("omnifocus").unwrap().len(), 1, "one open task after first pull");

        // Rewind the cursor to 0 so the second pull always does a full replay.
        // This simulates "new ZIPs appeared since last sync" without filesystem
        // mtime precision issues.
        v.write_omnifocus_sync(&OmniFocusSyncState {
            updated: chrono::Local::now().to_rfc3339(),
            last_zip_mtime: 0, // force full replay on next pull
            master_zip_name: "00000000000000=AAA+BBB.zip".into(),
            seen_zip_names: Vec::new(),
        })
        .unwrap();

        // Add a transaction ZIP with the completion (only the new zip needed).
        make_ofocus_dir(
            &ofocus_dir,
            &[
                ("00000000000000=AAA+BBB.zip", MASTER_XML),
                ("20260614T163000=BBB+CCC.zip", COMPLETE_XML),
            ],
        );

        // Second pull: task001 is now completed via the transaction zip.
        let out = pull_with(&v, &ofocus_dir).unwrap();
        assert_eq!(out.counts.get("completed").copied().unwrap_or(0), 1, "completion detected");
        assert_eq!(out.counts.get("open").copied().unwrap_or(0), 0);

        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        let completions: Vec<_> = events.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completions.len(), 1, "one completion event logged");
        assert_eq!(completions[0].task.id, "task001");
    }

    #[test]
    fn no_new_zips_skips_work_without_advancing_cursor() {
        let base = std::env::temp_dir()
            .join(format!("trove-of-noop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ofocus_dir = base.join("OmniFocus.ofocus");
        make_ofocus_dir(&ofocus_dir, &[("00000000000000=AAA+BBB.zip", MASTER_XML)]);

        let vault_dir = base.join("vault");
        let v = Vault::open_or_create(vault_dir).unwrap();
        pull_with(&v, &ofocus_dir).unwrap();

        // Get the cursor mtime after first pull.
        let state_after_first = v.read_omnifocus_sync();

        // Second pull with no new files — same mtime → early return.
        let out = pull_with(&v, &ofocus_dir).unwrap();
        assert_eq!(out.counts.get("open").copied().unwrap_or(0), 0, "no work done");

        let state_after_second = v.read_omnifocus_sync();
        assert_eq!(
            state_after_first.last_zip_mtime, state_after_second.last_zip_mtime,
            "cursor unchanged when no new zips"
        );
    }

    #[test]
    fn cursor_back_compat_empty_deserializes() {
        let empty: OmniFocusSyncState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.last_zip_mtime, 0);
        assert!(empty.master_zip_name.is_empty());
        assert!(empty.updated.is_empty());
    }

    #[test]
    fn raw_task_roundtrips() {
        let raw = RawOFTask {
            id: "task001".into(),
            name: "Write release notes".into(),
            note: "A note".into(),
            added: "2026-06-01T08:00:00.000Z".into(),
            modified: "2026-06-10T09:00:00.000Z".into(),
            due: Some("2026-06-15T17:00:00.000Z".into()),
            start: None,
            completed: None,
            flagged: true,
            is_project: false,
            project_status: String::new(),
            folder_id: None,
            parent_task_id: Some("proj001".into()),
            context_ids: vec!["ctx001".into()],
            in_inbox: false,
            repetition_rule: None,
        };
        let line = serde_json::to_string(&raw).unwrap();
        let back: RawOFTask = serde_json::from_str(&line).unwrap();
        assert_eq!(back.id, "task001");
        assert_eq!(back.flagged, true);
        assert_eq!(back.due.as_deref(), Some("2026-06-15T17:00:00.000Z"));
        assert_eq!(back.context_ids, vec!["ctx001"]);
    }

    #[test]
    fn raw_task_back_compat_missing_context_ids_deserializes() {
        // Old rows written before the Vec rename have no context_ids field;
        // they must still deserialize without error (serde default = empty Vec).
        let old_line = r#"{"id":"t1","name":"Old task","added":"2026-01-01T00:00:00.000Z"}"#;
        let back: RawOFTask = serde_json::from_str(old_line).unwrap();
        assert_eq!(back.id, "t1");
        assert!(back.context_ids.is_empty(), "missing field defaults to empty Vec");
    }

    #[test]
    fn nested_subtask_resolves_project_via_parent_chain() {
        // task001 (action) → task_mid (intermediate action) → proj001 (project)
        // The project lookup should walk up two levels.
        const NESTED_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<omnifocus xmlns="http://www.omnigroup.com/namespace/OmniFocus/v2">
<task id="proj001">
    <project><status>active</status></project>
    <added>2026-01-01T00:00:00.000Z</added>
    <modified>2026-01-01T00:00:00.000Z</modified>
    <name>Big Project</name>
</task>
<task id="task_mid">
    <task idref="proj001"/>
    <added>2026-01-02T00:00:00.000Z</added>
    <modified>2026-01-02T00:00:00.000Z</modified>
    <name>Middle action</name>
</task>
<task id="task_leaf">
    <task idref="task_mid"/>
    <added>2026-01-03T00:00:00.000Z</added>
    <modified>2026-01-03T00:00:00.000Z</modified>
    <name>Leaf subtask</name>
</task>
</omnifocus>"#;
        let mut state = OFState::default();
        apply_xml(&mut state, NESTED_XML).unwrap();
        let project_names: HashMap<String, String> =
            [("proj001".to_string(), "Big Project".to_string())].into_iter().collect();
        let leaf = of_task_to_task(
            &state.tasks["task_leaf"],
            &state.contexts,
            &state.folders,
            &project_names,
            &state.tasks,
        )
        .unwrap();
        assert_eq!(leaf.project, "Big Project", "nested subtask resolves to root project");
    }

    #[test]
    fn multi_tag_context_ids_all_collected() {
        // Two <context idref> children on one task — both should appear in context_ids.
        const MULTI_CTX_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<omnifocus xmlns="http://www.omnigroup.com/namespace/OmniFocus/v2">
<context id="ctx001"><name>Email</name></context>
<context id="ctx002"><name>Home</name></context>
<task id="task001">
    <context idref="ctx001"/>
    <context idref="ctx002"/>
    <added>2026-01-01T00:00:00.000Z</added>
    <modified>2026-01-01T00:00:00.000Z</modified>
    <name>Multi-tag task</name>
</task>
</omnifocus>"#;
        let mut state = OFState::default();
        apply_xml(&mut state, MULTI_CTX_XML).unwrap();
        let t = &state.tasks["task001"];
        assert!(t.context_ids.contains(&"ctx001".to_string()), "ctx001 collected");
        assert!(t.context_ids.contains(&"ctx002".to_string()), "ctx002 collected");
        let project_names: HashMap<String, String> = HashMap::new();
        let mapped = of_task_to_task(
            t,
            &state.contexts,
            &state.folders,
            &project_names,
            &state.tasks,
        )
        .unwrap();
        assert!(mapped.tags.contains(&"Email".to_string()), "Email tag resolved");
        assert!(mapped.tags.contains(&"Home".to_string()), "Home tag resolved");
    }

    #[test]
    fn self_closing_context_delete_removes_from_map() {
        // <context id="ctx001" op="delete"/> should remove ctx001 from state.contexts.
        const DEL_CTX_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<omnifocus xmlns="http://www.omnigroup.com/namespace/OmniFocus/v2">
<context id="ctx001"><name>Email</name></context>
<context id="ctx001" op="delete"/>
</omnifocus>"#;
        let mut state = OFState::default();
        apply_xml(&mut state, DEL_CTX_XML).unwrap();
        assert!(!state.contexts.contains_key("ctx001"), "ctx001 removed by self-closing delete");
    }

    #[test]
    fn zero_parse_guard_does_not_advance_cursor() {
        // A ZIP that produces zero tasks (e.g. malformed content XML that
        // parses without error but has no <task> elements) must NOT advance
        // the cursor.
        const NO_TASKS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<omnifocus xmlns="http://www.omnigroup.com/namespace/OmniFocus/v2">
<context id="ctx001"><name>Email</name></context>
</omnifocus>"#;
        let base = std::env::temp_dir()
            .join(format!("trove-of-zerguard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ofocus_dir = base.join("OmniFocus.ofocus");
        make_ofocus_dir(&ofocus_dir, &[("00000000000000=AAA+BBB.zip", NO_TASKS_XML)]);
        let vault_dir = base.join("vault");
        let v = Vault::open_or_create(vault_dir).unwrap();
        let out = pull_with(&v, &ofocus_dir).unwrap();
        // Headline must warn about Needs-sample / parser mismatch.
        assert!(
            out.headline.contains("Needs-sample") || out.headline.contains("zero tasks"),
            "headline warns about zero parse: {}", out.headline
        );
        // Cursor must NOT have advanced.
        let state = v.read_omnifocus_sync();
        assert_eq!(state.last_zip_mtime, 0, "cursor not advanced when zero tasks parsed");
    }

    #[test]
    fn def_behavior_is_periodic() {
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }), "Behavior::Periodic");
    }

    #[test]
    fn def_has_full_disk_permission_gate() {
        assert!(DEF.permission.is_some(), "permission gate declared");
    }
}

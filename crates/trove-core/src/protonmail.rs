//! ProtonMail export importer — accepts the folder produced by Proton's
//! official open-source export tool (`proton-mail-export`, free-plan
//! compatible, macOS CLI/GUI).
//!
//! ## What the export tool produces
//!
//! Running `proton-mail-export` decrypts the mailbox locally and writes one
//! pair of files per message into a flat output folder:
//!
//! ```text
//! <export-folder>/
//!   <ID>.eml                  — decrypted RFC 822 message
//!   <ID>.metadata.json        — versioned JSON with Proton-native metadata
//!   labels.json               — maps Proton label/folder IDs → names
//! ```
//!
//! The metadata JSON shape:
//! ```json
//! {
//!   "Version": 1,
//!   "Payload": {
//!     "ID":          "<proton-internal-id>",
//!     "ExternalID":  "<rfc822-message-id>",
//!     "LabelIDs":    ["<label-id>", …],
//!     "Subject":     "…",
//!     "Sender":      { "Name": "…", "Address": "…" },
//!     "ToList":      [{ "Name": "…", "Address": "…" }],
//!     "CCList":      [],
//!     "BCCList":     [],
//!     "Time":        1718000000,
//!     "Flags":       1,
//!     "Unread":      0,
//!     …
//!   }
//! }
//! ```
//!
//! The `labels.json` is versioned JSON:
//! ```json
//! { "Version": 1, "Payload": [{ "ID": "…", "Name": "…", "Path": "…", … }, …] }
//! ```
//! **Note:** the export tool filters out system labels (Inbox, Sent, Drafts,
//! Trash, Spam, Archive, Starred — integer IDs 0–10) from `labels.json`.
//! Trove seeds the label map with a built-in table for those IDs before
//! overlaying any user-defined labels from the file.
//!
//! ## Import strategy
//!
//! The user points Trove at the export folder (path may also be a single
//! `.eml` file — both are handled). For each `.eml` file Trove:
//!
//! 1. Reads the matching `<ID>.metadata.json` when present, extracts
//!    `ExternalID` (the RFC-822 Message-ID, the dedupe key) and `LabelIDs`.
//! 2. Parses the `.eml` through the shared `email_to_message` function.
//! 3. Overrides `source` to `"protonmail"`, sets `labels` from the metadata,
//!    overwrites `guid` with the `ExternalID` when the metadata is present.
//! 4. Appends to `correspondence/protonmail/YYYY-MM.jsonl` via the shared
//!    `append_messages` sink.
//!
//! Re-imports skip guids already stored — the same dedupe scheme as every
//! other email source. Multiple exports or overlapping folders never produce
//! duplicate rows.
//!
//! ## Privacy note
//!
//! Message bodies (full text) enter the vault when the user runs this import.
//! The import card shows an explicit acknowledgement before the first run.
//! Trove never holds Proton credentials — the export tool handles auth
//! entirely.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::correspondence::Message;
use crate::email::email_to_message;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Proton metadata.json deserialization

/// `<ID>.metadata.json` — versioned wrapper.
#[derive(Debug, Deserialize)]
struct MetadataFile {
    #[serde(rename = "Payload")]
    payload: MessagePayload,
}

/// The inner payload; only the fields we use are named.
#[derive(Debug, Deserialize)]
struct MessagePayload {
    /// RFC-822 Message-ID — authoritative dedupe key (same scheme as gmail /
    /// email / fastmail). May be absent for very old messages; fall through
    /// to mail-parser's value or content hash.
    #[serde(rename = "ExternalID", default)]
    external_id: String,
    /// Proton label / folder IDs (system and user).
    #[serde(rename = "LabelIDs", default)]
    label_ids: Vec<String>,
}

/// `labels.json` — one entry in the Payload array.
#[derive(Debug, Deserialize)]
struct LabelEntry {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Path", default)]
    path: String,
}

/// `labels.json` — the versioned wrapper the export tool writes.
/// Shape: `{ "Version": 1, "Payload": [ { "ID": "…", "Name": "…", … }, … ] }`
#[derive(Debug, Deserialize)]
struct LabelsFile {
    #[serde(rename = "Payload")]
    payload: Vec<LabelEntry>,
}

/// Proton's well-known system-label IDs.  The export tool filters these OUT
/// of `labels.json`, but each message's `LabelIDs` array still includes them.
/// Seed the map so e.g. ID "0" → "Inbox", "1" → "All Drafts", etc.
fn system_label_map() -> HashMap<String, String> {
    [
        ("0", "Inbox"),
        ("1", "All Drafts"),
        ("2", "All Sent"),
        ("3", "Trash"),
        ("4", "Spam"),
        ("5", "All Mail"),
        ("6", "Archive"),
        ("7", "Sent"),
        ("8", "Drafts"),
        ("10", "Starred"),
    ]
    .iter()
    .map(|(id, name)| (id.to_string(), name.to_string()))
    .collect()
}

/// Load `labels.json` from the export folder; gracefully returns a map seeded
/// with system labels when the file is absent or unparseable.
///
/// The export tool writes a versioned wrapper:
/// `{ "Version": 1, "Payload": [ { "ID": "…", "Name": "…", "Path": "…" }, … ] }`
/// System-label IDs (0–10) are filtered out by the tool, so we seed those
/// ourselves before overlaying user labels from the file.
fn load_label_map(folder: &Path) -> HashMap<String, String> {
    // Start with the built-in system label table.
    let mut map = system_label_map();

    let path = folder.join("labels.json");
    let body = match fs::read_to_string(&path) {
        Ok(b) => b,
        Err(_) => return map,
    };

    // Primary: versioned wrapper { "Version": N, "Payload": [{…}, …] }
    if let Ok(lf) = serde_json::from_str::<LabelsFile>(&body) {
        for e in lf.payload {
            if !e.id.is_empty() {
                let display = if e.path.is_empty() { e.name } else { e.path };
                map.insert(e.id, display);
            }
        }
        return map;
    }

    // Fallback: bare array [ { "ID": "…", "Name": "…" }, … ]
    // (older exports or hand-crafted test fixtures)
    #[derive(Deserialize)]
    struct LabelItem {
        #[serde(rename = "ID", default)]
        id: String,
        #[serde(rename = "Name", default)]
        name: String,
        #[serde(rename = "Path", default)]
        path: String,
    }
    if let Ok(arr) = serde_json::from_str::<Vec<LabelItem>>(&body) {
        for e in arr {
            if !e.id.is_empty() {
                let display = if e.path.is_empty() { e.name } else { e.path };
                map.insert(e.id, display);
            }
        }
        return map;
    }

    // Fallback: object map { "<id>": { "Name": "…", … } }
    // (may appear in older or third-party export variants)
    #[derive(Deserialize)]
    struct LabelObj {
        #[serde(rename = "Name", default)]
        name: String,
        #[serde(rename = "Path", default)]
        path: String,
    }
    if let Ok(obj) = serde_json::from_str::<HashMap<String, LabelObj>>(&body) {
        for (id, e) in obj {
            let display = if e.path.is_empty() { e.name } else { e.path };
            map.insert(id, display);
        }
        return map;
    }

    map
}

/// Parse `<ID>.metadata.json` next to the given `.eml` file.
/// Returns `None` when the sidecar is absent or unparseable.
fn load_metadata(eml_path: &Path) -> Option<MessagePayload> {
    // The tool writes "<ID>.eml" → sidecar is "<ID>.metadata.json".
    // with_extension replaces the last extension, so "abc.eml" →
    // "abc.metadata" (wrong). Build the path from the stem explicitly.
    let stem = eml_path.file_stem()?;
    let meta_path = eml_path.with_file_name(format!("{}.metadata.json", stem.to_string_lossy()));
    let body = fs::read_to_string(&meta_path).ok()?;
    let mf: MetadataFile = serde_json::from_str(&body).ok()?;
    Some(mf.payload)
}

// ---------------------------------------------------------------------------
// Core import logic (shared by the Vault extension and tests)

pub(crate) struct ProtonStats {
    pub(crate) imported: u64,
    pub(crate) duplicates: u64,
    pub(crate) failed: u64,
}

/// Import one `.eml` file, optionally enriched by its sidecar metadata.
/// Returns `None` when the EML is unparseable or undateable.
fn import_one(
    raw: &[u8],
    eml_path: &Path,
    account: &str,
    label_map: &HashMap<String, String>,
) -> Option<Message> {
    let meta = load_metadata(eml_path);

    // Parse the raw EML bytes through the shared function.
    let mut m = email_to_message(raw, account)?;

    // Re-tag as protonmail so this source has its own stream.
    m.source = "protonmail".to_string();

    // Override guid with the RFC-822 ExternalID from the sidecar when present
    // and non-empty — it is always the canonical Message-ID, so it dedupes
    // cleanly across re-exports. Fall through to mail-parser's value or the
    // content hash otherwise.
    if let Some(ref p) = meta {
        if !p.external_id.is_empty() {
            let raw_id = p.external_id.trim();
            // Normalise to "<…>" form, same as email_to_message.
            m.guid = if raw_id.starts_with('<') {
                raw_id.to_string()
            } else {
                format!("<{raw_id}>")
            };
        }
        // Attach label names (falling back to raw IDs when the map is empty).
        if !p.label_ids.is_empty() {
            m.labels = p
                .label_ids
                .iter()
                .map(|id| {
                    label_map
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
        }
    }

    Some(m)
}

impl Vault {
    /// Import every `.eml` file found in `folder` (or the single file at
    /// `folder` when it is a `.eml` file itself). Re-runnable: already-stored
    /// guids are skipped. `account` is the Proton address whose mailbox was
    /// exported — it decides `service` and `from_me`.
    pub(crate) fn import_protonmail_folder<F>(
        &self,
        folder: &Path,
        account: &str,
        mut progress: F,
    ) -> Result<ProtonStats>
    where
        F: FnMut(ImportProgress),
    {
        // Collect all EML paths to import.
        let eml_paths: Vec<std::path::PathBuf> = if folder.is_dir() {
            let mut paths: Vec<_> = fs::read_dir(folder)
                .with_context(|| format!("reading folder {}", folder.display()))?
                .flatten()
                .filter(|e| {
                    e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.eq_ignore_ascii_case("eml"))
                        .unwrap_or(false)
                })
                .map(|e| e.path())
                .collect();
            paths.sort(); // deterministic order
            paths
        } else if folder
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.eq_ignore_ascii_case("eml"))
            .unwrap_or(false)
        {
            vec![folder.to_path_buf()]
        } else {
            anyhow::bail!(
                "expected a folder of .eml files or a single .eml file; got: {}",
                folder.display()
            );
        };

        if eml_paths.is_empty() {
            return Ok(ProtonStats { imported: 0, duplicates: 0, failed: 0 });
        }

        let label_map = if folder.is_dir() {
            load_label_map(folder)
        } else {
            folder
                .parent()
                .map(load_label_map)
                .unwrap_or_default()
        };

        let total = eml_paths.len() as f32;
        let mut seen = self.correspondence_guids("protonmail")?;
        let mut stats = ProtonStats { imported: 0, duplicates: 0, failed: 0 };
        let mut batch: Vec<Message> = Vec::new();
        let mut done = 0u32;

        for path in &eml_paths {
            let raw = match fs::read(path) {
                Ok(b) => b,
                Err(_) => {
                    stats.failed += 1;
                    done += 1;
                    continue;
                }
            };
            match import_one(&raw, path, account, &label_map) {
                Some(m) => {
                    if seen.insert(m.guid.clone()) {
                        batch.push(m);
                        stats.imported += 1;
                    } else {
                        stats.duplicates += 1;
                    }
                }
                None => stats.failed += 1,
            }
            done += 1;
            if batch.len() >= 2000 {
                self.append_messages(&batch)?;
                batch.clear();
            }
            if done % 250 == 0 {
                progress(ImportProgress {
                    records: stats.imported,
                    percent: (done as f32 / total) * 100.0,
                });
            }
        }
        self.append_messages(&batch)?;
        progress(ImportProgress {
            records: stats.imported,
            percent: 100.0,
        });
        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// Registry wiring

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/protonmail"))
}

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let account = params
        .get("account")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if account.is_empty() {
        anyhow::bail!(
            "the Proton Mail address is required (it decides which messages \
             count as sent by you and is stored as the account identifier)"
        );
    }
    let s = vault.import_protonmail_folder(path, &account, |p| progress(p))?;
    Ok(ImportOutcome {
        headline: format!(
            "{} messages imported, {} duplicates skipped",
            s.imported, s.duplicates
        ),
        counts: [
            ("imported", s.imported),
            ("duplicates", s.duplicates),
            ("failed", s.failed),
        ]
        .into(),
    })
}

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // The tool emits a *folder* of .eml files; "eml" signals the file type to
    // the hub (the user may also drop a single .eml for a quick test). In
    // practice the user drops the export folder: the run fn accepts both.
    accepts: &["eml"],
    params: &[crate::registry::ImportParam {
        key: "account",
        label: "Your ProtonMail address",
        placeholder: "you@proton.me",
        required: true,
    }],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "protonmail",
        name: "ProtonMail",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports email exported from ProtonMail using Proton's \
                      open-source export tool (proton-mail-export), which \
                      produces .eml files that Trove's email importer reads \
                      directly. Message bodies enter the vault — no Proton \
                      credentials are ever shared with Trove.",
        domain: "correspondence",
        vault_path: "correspondence/protonmail/",
        toggleable: false,
        setup: &[
            "Download and run the proton-mail-export tool from github.com/ProtonMail/proton-mail-export (free plan compatible). It will ask for your Proton login + 2FA and write a folder of .eml files.",
            "Drop the export folder (or any .eml file from it) onto this import box, enter your ProtonMail address, and click Import.",
            "Re-imports are safe: messages already in the vault are skipped automatically.",
        ],
        caveats: "Full message bodies are stored in the vault. The Proton Mail Bridge \
                 (paid plans, running app required) is not supported — use the official \
                 export tool instead.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-protonmail-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("trove-protonmail-export-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -----------------------------------------------------------------------
    // Fixture EML — a minimal RFC 822 message that mail-parser will accept.

    const EML_A: &[u8] = b"\
Message-ID: <abc123@proton.me>\r\n\
Date: Mon, 10 Jun 2026 08:00:00 +0000\r\n\
From: Alice <alice@example.com>\r\n\
To: me@proton.me\r\n\
Subject: Hello from Alice\r\n\
\r\n\
Hi there!\r\n";

    const EML_B: &[u8] = b"\
Message-ID: <def456@proton.me>\r\n\
Date: Mon, 10 Jun 2026 09:00:00 +0000\r\n\
From: me@proton.me\r\n\
To: alice@example.com\r\n\
Subject: Re: Hello from Alice\r\n\
\r\n\
Hey Alice!\r\n";

    // metadata.json sidecar for EML_A — uses ExternalID as the dedupe key.
    // LabelIDs: "0" = Inbox (system label, not in labels.json), "pl1" = user
    // label "Personal" (defined in labels.json below).
    const META_A: &str = r#"{
  "Version": 1,
  "Payload": {
    "ID": "proton-internal-id-001",
    "ExternalID": "abc123@proton.me",
    "LabelIDs": ["0", "pl1"],
    "Subject": "Hello from Alice",
    "Sender": { "Name": "Alice", "Address": "alice@example.com" },
    "ToList": [{ "Name": "Me", "Address": "me@proton.me" }],
    "CCList": [],
    "BCCList": [],
    "Time": 1749542400,
    "Flags": 1,
    "Unread": 1,
    "IsReplied": 0,
    "IsRepliedAll": 0,
    "IsForwarded": 0,
    "NumAttachments": 0,
    "Attachments": []
  }
}"#;

    // Real shape the proton-mail-export tool writes:
    // { "Version": 1, "Payload": [ { "ID": "…", "Name": "…", "Path": "…" }, … ] }
    // System-label IDs (0–10) are NOT included by the tool — they come from the
    // built-in seed table in load_label_map. Only user-defined labels appear here.
    const LABELS_JSON: &str = r#"{
  "Version": 1,
  "Payload": [
    { "ID": "pl1", "Name": "Personal", "Path": "Personal", "Color": "", "Type": 1 },
    { "ID": "pl2", "Name": "Work",     "Path": "Work",     "Color": "", "Type": 1 }
  ]
}"#;

    // -----------------------------------------------------------------------

    #[test]
    fn folder_import_parses_and_dedupes() {
        let vault = temp_vault("basic");
        let export = temp_dir("basic");

        // Write EML A with sidecar metadata.
        fs::write(export.join("proton-internal-id-001.eml"), EML_A).unwrap();
        fs::write(export.join("proton-internal-id-001.metadata.json"), META_A).unwrap();
        // EML B has no sidecar — guid comes from the Message-ID header.
        fs::write(export.join("proton-internal-id-002.eml"), EML_B).unwrap();
        // labels.json in the root.
        fs::write(export.join("labels.json"), LABELS_JSON).unwrap();

        let stats = vault
            .import_protonmail_folder(&export, "me@proton.me", |_| {})
            .unwrap();
        assert_eq!(stats.imported, 2);
        assert_eq!(stats.duplicates, 0);
        assert_eq!(stats.failed, 0);

        // Verify records written to correspondence/protonmail/.
        let day = vault
            .correspondence_timeline("2026-06-10")
            .unwrap()
            .into_iter()
            .filter(|m| m.source == "protonmail")
            .collect::<Vec<_>>();
        assert_eq!(day.len(), 2);

        let a = day.iter().find(|m| m.subject.contains("Hello from Alice")).unwrap();
        assert_eq!(a.source, "protonmail");
        assert!(!a.from_me);
        assert_eq!(a.sender, "alice@example.com");
        // guid comes from ExternalID in sidecar.
        assert_eq!(a.guid, "<abc123@proton.me>");
        // "0" resolves via the built-in system-label table (not in labels.json).
        assert!(a.labels.contains(&"Inbox".to_string()), "labels: {:?}", a.labels);
        // "pl1" resolves via labels.json user-label entry.
        assert!(a.labels.contains(&"Personal".to_string()), "labels: {:?}", a.labels);

        let b = day.iter().find(|m| m.subject.contains("Re: Hello")).unwrap();
        assert_eq!(b.source, "protonmail");
        assert!(b.from_me);
        // No sidecar — guid from Message-ID header.
        assert_eq!(b.guid, "<def456@proton.me>");
        assert!(b.labels.is_empty(), "no sidecar → no labels");

        // Re-import: all duplicates, nothing new.
        let again = vault
            .import_protonmail_folder(&export, "me@proton.me", |_| {})
            .unwrap();
        assert_eq!(again.imported, 0);
        assert_eq!(again.duplicates, 2);
        assert_eq!(again.failed, 0);
    }

    #[test]
    fn single_eml_file_import() {
        let vault = temp_vault("single");
        let dir = temp_dir("single");
        let path = dir.join("msg.eml");
        fs::write(&path, EML_A).unwrap();

        let stats = vault
            .import_protonmail_folder(&path, "me@proton.me", |_| {})
            .unwrap();
        assert_eq!(stats.imported, 1);
        assert_eq!(stats.duplicates, 0);
    }

    #[test]
    fn missing_metadata_sidecar_falls_back_to_eml_headers() {
        let vault = temp_vault("nosidecar");
        let export = temp_dir("nosidecar");
        // EML with no sidecar — guid comes from Message-ID header.
        fs::write(export.join("xyz.eml"), EML_A).unwrap();

        let stats = vault
            .import_protonmail_folder(&export, "other@example.com", |_| {})
            .unwrap();
        assert_eq!(stats.imported, 1);
        let day = vault
            .correspondence_timeline("2026-06-10")
            .unwrap()
            .into_iter()
            .filter(|m| m.source == "protonmail")
            .collect::<Vec<_>>();
        assert_eq!(day.len(), 1);
        // guid from the Message-ID header, not ExternalID.
        assert_eq!(day[0].guid, "<abc123@proton.me>");
        assert!(day[0].labels.is_empty());
    }

    #[test]
    fn label_map_loaded_from_versioned_wrapper() {
        // Primary format: { "Version": 1, "Payload": [ { "ID": "…", … }, … ] }
        // This is the real shape proton-mail-export writes.
        let dir = temp_dir("labels");
        fs::write(dir.join("labels.json"), LABELS_JSON).unwrap();
        let map = load_label_map(&dir);
        // User labels from the versioned Payload array.
        assert_eq!(map.get("pl1").map(String::as_str), Some("Personal"));
        assert_eq!(map.get("pl2").map(String::as_str), Some("Work"));
        // System labels come from the built-in seed table, not labels.json.
        assert_eq!(map.get("0").map(String::as_str), Some("Inbox"));
        assert_eq!(map.get("7").map(String::as_str), Some("Sent"));
        assert_eq!(map.get("10").map(String::as_str), Some("Starred"));
    }

    #[test]
    fn label_map_system_labels_seeded_without_file() {
        // Even when labels.json is absent, system label IDs resolve correctly.
        let dir = temp_dir("labels-nosystemfile");
        // Intentionally do NOT write labels.json.
        let map = load_label_map(&dir);
        assert_eq!(map.get("0").map(String::as_str), Some("Inbox"));
        assert_eq!(map.get("3").map(String::as_str), Some("Trash"));
        assert_eq!(map.get("6").map(String::as_str), Some("Archive"));
    }

    #[test]
    fn label_map_fallback_bare_array_form() {
        // Bare-array fallback: [ { "ID": "…", … }, … ] (older or hand-crafted files).
        let dir = temp_dir("labels-arr");
        let json = r#"[
  { "ID": "u1", "Name": "Newsletter", "Path": "Newsletter" },
  { "ID": "u2", "Name": "Work",       "Path": "Work"       }
]"#;
        fs::write(dir.join("labels.json"), json).unwrap();
        let map = load_label_map(&dir);
        assert_eq!(map.get("u1").map(String::as_str), Some("Newsletter"));
        assert_eq!(map.get("u2").map(String::as_str), Some("Work"));
        // System labels still seeded.
        assert_eq!(map.get("0").map(String::as_str), Some("Inbox"));
    }

    #[test]
    fn label_map_fallback_object_form() {
        // Object-map fallback: { "<id>": { "Name": "…", … } } (third-party variants).
        let dir = temp_dir("labels-obj");
        let json = r#"{"u9":{"Name":"Finance","Path":"Finance","Color":"","Type":1}}"#;
        fs::write(dir.join("labels.json"), json).unwrap();
        let map = load_label_map(&dir);
        assert_eq!(map.get("u9").map(String::as_str), Some("Finance"));
        // System labels still seeded.
        assert_eq!(map.get("7").map(String::as_str), Some("Sent"));
    }

    #[test]
    fn empty_export_folder_returns_zero_stats() {
        let vault = temp_vault("empty");
        let export = temp_dir("empty");
        let stats = vault
            .import_protonmail_folder(&export, "me@proton.me", |_| {})
            .unwrap();
        assert_eq!(stats.imported, 0);
        assert_eq!(stats.duplicates, 0);
        assert_eq!(stats.failed, 0);
    }

    #[test]
    fn def_metadata_sanity() {
        assert_eq!(DEF.id, "protonmail");
        assert_eq!(DEF.domain, "correspondence");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.connection, None);
    }
}

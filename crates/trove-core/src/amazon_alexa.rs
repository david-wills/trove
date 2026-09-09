//! Amazon Alexa — voice-interaction history via the Amazon privacy data export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/amazon-alexa.md.
//!
//! ## Vault layout
//!
//! - **Raw layer (unconditional):**
//!   `home/amazon-alexa/raw/<datestamp>-voice_history.json` — the verbatim
//!   export file, preserved at full fidelity on every import.
//!
//! - **Contract layer (deferred):**
//!   `home/amazon-alexa/events/YYYY-MM.jsonl` per the `home.event` shape
//!   (`docs/vault-spec/domains/home.md`). Each voice interaction is one
//!   `event:"command"` row (`device`, `detail` = the transcribed utterance,
//!   `extra.response` = Alexa's reply). **This layer is parked** — the
//!   `voice_history.json` field names are community-reverse-engineered and
//!   have not been confirmed against a real export. Once a real sample pins
//!   the exact field names, only [`interactions_from_export`] needs filling;
//!   the raw-store and the contract shape are already designed.
//!
//! ## Access model
//!
//! No API. Manual export: amazon.com/privacy → Request My Data → Alexa
//! interaction history. The export arrives in 24–72 hours as an emailed
//! download link. Trove never logs in; the user hands the file to this importer.
//!
//! ## Coverage caveat
//!
//! Amazon retains approximately 18 months of voice history. Users should
//! re-import periodically; older interactions are gone from the portal once
//! the window rolls. Audio recordings are never included — transcripts only.
//!
//! ## Parser parked — Needs-sample (evidence rule)
//!
//! The `voice_history.json` layout is community-described and not officially
//! documented. Per the project evidence rule we do **not** parse blind against
//! an assumed field shape (a green test over a fabricated fixture is false
//! confidence). The raw layer stores the export verbatim; the event layer is
//! parked until a real sample pins the exact field names. See [`PARKED_MSG`].

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::write_atomic;
use crate::vault::Vault;

/// Raw-layer directory for verbatim export files.
const RAW_DIR: &str = "home/amazon-alexa/raw";

/// Events layer directory (deferred — home.event contract, unbound draft).
/// Created by this module once the parser is unparked.
#[allow(dead_code)]
const EVENTS_DIR: &str = "home/amazon-alexa/events";

/// Shown when the import parser is invoked before a real export sample exists.
///
/// The `voice_history.json` field layout is community-reverse-engineered and
/// not confirmed. When a real sample lands, populate [`interactions_from_export`]
/// field-for-field — the raw layer, the event schema, and this import scaffold
/// are all in place.
const PARKED_MSG: &str = "Amazon Alexa event import is parked pending a real data-export sample. \
The voice_history.json field names in the Amazon privacy export are community-described \
and not officially documented; Trove does not parse export files against a guessed field shape \
(the evidence rule). The export file has been stored verbatim in home/amazon-alexa/raw/. \
Once a real voice_history.json sample is provided, the field mapping is wired in \
amazon_alexa::interactions_from_export — the raw layer and home.event schema are already designed.";

fn def_last_data(vault: &Vault) -> Option<String> {
    // Raw snapshots are JSON files (not JSONL), so we use mtime to find the
    // most recently imported export, regardless of extension.
    crate::registry::newest_mtime(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
/// The line is already present; this build upgrades the def from `NotWired` to `Import`.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "amazon-alexa",
        name: "Amazon Alexa",
        kind: IntegrationKind::Import,
        // Voice command transcripts are message-body-equivalent content — ships opt-in.
        default_on: false,
        description: "Import your Alexa voice-interaction history from an Amazon privacy \
                      data export — every voice command and Alexa's reply, timestamped by \
                      device. Re-importable: newer exports accumulate history across the \
                      18-month rolling window.",
        domain: "home",
        vault_path: "home/amazon-alexa/",
        toggleable: false,
        setup: &[
            "Open amazon.com/privacy → Request My Data → select Alexa interaction history.",
            "Amazon emails a download link in 24–72 hours. Import the voice_history.json \
             (or the ZIP containing it) here.",
            "Re-import periodically — Amazon retains only ~18 months; older interactions \
             are gone from the portal once the window rolls.",
        ],
        caveats: "Voice command transcripts are conversation-equivalent content — this \
                  integration ships opt-in and requires explicit acknowledgement. No live API \
                  exists; the privacy export is the only path. Amazon retains approximately \
                  18 months of history — re-import each time a new export arrives to avoid \
                  losing older interactions. Audio recordings are never included, only transcriptions.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // The export arrives as a ZIP or as a bare voice_history.json.
    accepts: &["zip", "json"],
    params: &[],
    run: run_import,
};

/// One Alexa voice interaction, normalized to the small set of fields the
/// community schema describes. This is the seam the parser will fill and the
/// home.event mapping will consume, so the two concerns stay independent:
/// [`interactions_from_export`] (parked) produces `Interaction`s;
/// `interaction_to_event` (designed but deferred) will map each one to a
/// home.event line when the home.event contract is ratified and the parser
/// is unparked.
///
/// Fields are intentionally generic to survive any field-name variance in the
/// export; the parser that fills them must be grounded in a real sample.
#[derive(Debug, Clone, Default)]
pub struct Interaction {
    /// RFC3339 timestamp of the interaction (local time).
    pub ts: String,
    /// The Echo/Alexa device name or id that heard the command.
    pub device: String,
    /// The transcribed voice command the user spoke.
    pub command: String,
    /// Alexa's textual reply (may be empty if unavailable).
    pub response: String,
    /// A source-unique id for the interaction, for deduplication.
    /// When the export supplies a stable id it goes here; otherwise the
    /// caller synthesizes `device|ts` as the stable natural key.
    pub guid: String,
}

/// Parse an Alexa export file (ZIP or bare `voice_history.json`) into
/// normalized [`Interaction`]s.
///
/// **Parked — Needs-sample.** The `voice_history.json` field names and
/// nesting are community-described and unconfirmed. This is the *only* piece
/// waiting on a real sample: the raw-store, the home.event schema, and the
/// import scaffold are already in place. When a sample lands, implement the
/// parse here (extract from ZIP if needed, walk the top-level array, fill
/// each `Interaction`) — nothing downstream changes.
#[allow(dead_code)]
fn interactions_from_export(_path: &Path) -> Result<Vec<Interaction>> {
    anyhow::bail!("{PARKED_MSG}")
}

/// Read `voice_history.json` bytes from a path that is either a bare .json
/// or a .zip containing an `alexa/voice_history.json` at its expected location.
/// Returns the raw bytes for verbatim storage and (once unparked) the body
/// text for parsing.
///
/// **Parked — not called until the parser is wired.** Here for completeness
/// so the zip-extraction path is designed alongside the parser design.
#[allow(dead_code)]
fn voice_history_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        use std::io::Read;
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading zip {}", path.display()))?;
        // Community schema locates the file at alexa/voice_history.json inside
        // the Amazon privacy export ZIP. Some exports may place it at a
        // different depth — check both. When a real sample is available,
        // confirm the exact path and adjust the try-list accordingly.
        let candidates = ["alexa/voice_history.json", "voice_history.json"];
        for candidate in candidates {
            if let Ok(mut entry) = archive.by_name(candidate) {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                return Ok(buf);
            }
        }
        anyhow::bail!(
            "no voice_history.json found in the ZIP — expected at alexa/voice_history.json \
             (is this an Amazon Alexa interaction history export?)"
        )
    } else {
        std::fs::read(path).with_context(|| format!("reading {}", path.display()))
    }
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Raw layer: store the verbatim export file regardless of the parser state.
    // This preserves full fidelity even while the per-interaction parser is parked.
    let raw_bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;

    // Name the raw snapshot by the import timestamp so re-imports accumulate
    // rather than overwrite (each import is a distinct snapshot).
    let stamp = Local::now().format("%Y%m%dT%H%M%S").to_string();
    let orig_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("voice_history.json");
    let raw_rel = format!("{RAW_DIR}/{stamp}-{orig_name}");
    let raw_path = vault.resolve(&raw_rel)?;
    write_atomic(&raw_path, &raw_bytes)?;
    progress(ImportProgress { records: 1, percent: 50.0 });

    // Event layer: parked — the per-interaction parser waits for a real sample.
    // Return a clear informational outcome (not an error) so the user sees the
    // file was stored safely. The parser's parked message appears in the caveat.
    progress(ImportProgress { records: 1, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "Export stored verbatim in home/amazon-alexa/raw/ — \
             per-interaction import parked pending a real sample (Needs-sample). \
             Raw file: {stamp}-{orig_name}"
        ),
        counts: BTreeMap::from([("raw_files", 1u64), ("interactions", 0u64)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-amazon-alexa-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // A plausible voice_history.json shape based on community descriptions.
    // The real field names are unconfirmed — this fixture is deliberately NOT
    // used to drive a parser (parser_parked_needs_sample = true). It represents
    // the kind of JSON the export is expected to contain, stored verbatim.
    const SAMPLE_JSON: &str = r#"[
  {
    "timestamp": "2026-05-15T08:14:32Z",
    "device": "Kitchen Echo",
    "utterance": "Alexa, set a timer for ten minutes",
    "response": "Ten minutes, starting now."
  },
  {
    "timestamp": "2026-05-16T18:22:10Z",
    "device": "Bedroom Echo Dot",
    "utterance": "Alexa, what's the weather today?",
    "response": "Currently 68 degrees and sunny in Portland."
  }
]"#;

    #[test]
    fn import_stores_raw_verbatim_and_returns_parked_outcome() {
        let v = temp_vault("raw-store");
        let export_path = v.root().join("voice_history.json");
        fs::write(&export_path, SAMPLE_JSON).unwrap();

        let outcome =
            (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        // The outcome reports raw storage and zero interactions (parked).
        assert_eq!(outcome.counts.get("raw_files"), Some(&1));
        assert_eq!(outcome.counts.get("interactions"), Some(&0));
        assert!(
            outcome.headline.contains("raw"),
            "headline should mention raw storage: {}",
            outcome.headline
        );
        assert!(
            outcome.headline.contains("parked") || outcome.headline.contains("Needs-sample"),
            "headline should note the parked parser: {}",
            outcome.headline
        );

        // The raw directory now contains the snapshot.
        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1, "one raw snapshot written");

        // The raw file is byte-identical to what was imported (full fidelity).
        let raw_body = fs::read_to_string(entries[0].path()).unwrap();
        assert_eq!(raw_body, SAMPLE_JSON, "raw snapshot is verbatim");
    }

    #[test]
    fn import_twice_accumulates_two_snapshots() {
        // Re-importing a new export (the 18-month rolling window) accumulates
        // raw snapshots rather than overwriting — each import gets its own
        // timestamped filename.
        let v = temp_vault("accumulate");
        let path1 = v.root().join("voice_history.json");
        fs::write(&path1, SAMPLE_JSON).unwrap();
        (IMPORT.run)(&v, &path1, &BTreeMap::new(), &mut |_| {}).unwrap();

        // Simulate re-import a week later with updated content.
        let updated = r#"[{"timestamp":"2026-06-01T10:00:00Z","device":"Living Room Echo","utterance":"Alexa, play jazz","response":"Playing jazz from Amazon Music."}]"#;
        let path2 = v.root().join("voice_history_v2.json");
        fs::write(&path2, updated).unwrap();
        (IMPORT.run)(&v, &path2, &BTreeMap::new(), &mut |_| {}).unwrap();

        // Two raw snapshots accumulated.
        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(entries.len(), 2, "each import leaves its own raw snapshot");
    }

    #[test]
    fn def_is_import_behavior_and_not_wired_for_connections() {
        // The DEF must be an Import behavior — not NotWired (the stub was replaced).
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        // No connection: the user downloads the export themselves.
        assert!(DEF.connection.is_none());
        // Import box accepts JSON and ZIP.
        let spec = DEF.import_spec().expect("Import behavior has a spec");
        assert!(spec.accepts.contains(&"json"));
        assert!(spec.accepts.contains(&"zip"));
        // Ships opt-in (voice transcripts are sensitive).
        assert!(!DEF.meta.default_on);
        // There is a last_data hook (for the hub card).
        assert!(DEF.last_data.is_some());
    }

    #[test]
    fn last_data_returns_none_when_no_raw_files_exist() {
        // Hub card shows "no data yet" when nothing has been imported.
        let v = temp_vault("empty");
        let result = (DEF.last_data.unwrap())(&v);
        assert!(result.is_none(), "empty vault → no last_data");
    }

    #[test]
    fn last_data_returns_a_datestamp_after_import() {
        let v = temp_vault("has-data");
        let export_path = v.root().join("voice_history.json");
        fs::write(&export_path, SAMPLE_JSON).unwrap();
        (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        let last = (DEF.last_data.unwrap())(&v);
        assert!(last.is_some(), "after import, last_data is set");
    }

    #[test]
    fn parked_message_is_well_formed() {
        // The parked message must name the source, state the reason (Needs-sample),
        // and point to the function to fill in when the sample lands.
        assert!(PARKED_MSG.contains("voice_history.json"), "names the file");
        assert!(
            PARKED_MSG.contains("interactions_from_export"),
            "points to the function"
        );
        assert!(PARKED_MSG.contains("raw"), "confirms raw storage happened");
    }
}

//! Twitch — streaming platform, file [`Behavior::Import`].
//! Catalogued in the Phase 2 pass; brief: docs/integrations/twitch.md.
//!
//! ## What it is
//!
//! Live-streaming platform. Official account data download available via
//! Settings → Security and Privacy → Download Your Data. The export package
//! contains a **Viewing and Chat History** category including a
//! `_minutes_watched.csv` file (minutes watched per channel). Several other
//! file names are also publicly known from community research: `_follow_unfollow.csv`
//! (follow/unfollow events) and `_chat_cheer_sub_notif.csv` (chat, cheers,
//! subscriptions). The **Helix API** does NOT expose viewer watch history —
//! that distinction is important: absence from the API ≠ absence from the export.
//!
//! ## Vault layout
//!
//! - **Raw layer (unconditional):**
//!   `social/twitch/raw/<datestamp>-<filename>` — the verbatim download
//!   package, preserved at full fidelity on every import (accumulating,
//!   not overwriting). Whatever the package turns out to contain is kept.
//!
//! - **Contract layer (parked — Needs-sample):**
//!   The exact column layout of each CSV is not confirmed from a real sample.
//!   Once a sample lands, the parsing splits by data type:
//!   - `_minutes_watched.csv` → media-plays contract (`media/twitch/YYYY-MM.jsonl`,
//!     `kind:"play"`) — time spent watching per channel is media-plays-shaped.
//!   - `_follow_unfollow.csv` + `_chat_cheer_sub_notif.csv` → social contract
//!     (`social/twitch/YYYY-MM.jsonl`, `kind:"post"/"comment"`) — correspondence-shaped.
//!   **Parked** — per the evidence rule, we do not parse against an assumed
//!   column layout. The raw layer is already in place; only [`posts_from_export`]
//!   needs filling once a sample confirms column names.
//!
//! ## Parser parked — Needs-sample (evidence rule)
//!
//! The file inventory is publicly known (`_minutes_watched.csv`,
//! `_follow_unfollow.csv`, `_chat_cheer_sub_notif.csv`), but the exact column
//! names inside each CSV are not confirmed from an official schema or a real
//! sample on disk. Per the project evidence rule we do **not** parse blind
//! against an assumed column layout (a green test over a fabricated fixture is
//! false confidence — cf. the raindrop `_id` bug). The raw layer stores the
//! export verbatim; the per-item parse layer is parked until a real sample pins
//! the exact field names. See [`PARKED_MSG`].
//!
//! ## Viewer watch history note
//!
//! The **Helix API** does not expose viewer watch history. The **data export**,
//! however, DOES include a `Viewing and Chat History` category with a
//! `_minutes_watched.csv` file. The hub card distinguishes these two paths
//! clearly (see `caveats`).
//!
//! ## Future: Helix API (separate build, out of scope here)
//!
//! A future Periodic def using the Helix API (GET /channels, /clips, /videos,
//! /subscriptions) is possible for streamers with OAuth. That build would add
//! a `twitch` OAuth `ConnectionDef` + a Periodic def. Not in scope here —
//! the Import path makes no network calls, standalone-clean.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::write_atomic;
use crate::vault::Vault;

/// Raw-layer directory for verbatim export snapshots.
const RAW_DIR: &str = "social/twitch/raw";

/// Contract-layer directory: authored-content rows per the `social` contract.
/// Written once [`posts_from_export`] is unparked and a sample pins the format.
#[allow(dead_code)]
const DIR: &str = "social/twitch";

/// Shown when the import is invoked before a real export sample exists to pin
/// the exact column layout of the Twitch data download package's CSV files.
/// The raw layer stores the export verbatim; the per-item parse layer is the
/// only piece waiting on a real sample.
///
/// Note on what IS known: the file inventory is publicly documented via
/// community research — `_minutes_watched.csv` (viewing history, media-plays-shaped),
/// `_follow_unfollow.csv` (social), `_chat_cheer_sub_notif.csv` (social/chat).
/// What is NOT confirmed from an official schema: the exact column names inside
/// each CSV. The evidence rule bars parsing against an assumed column layout.
const PARKED_MSG: &str = "Twitch per-item import is parked pending a real data-download sample. \
The export package's file inventory is known (_minutes_watched.csv, _follow_unfollow.csv, \
_chat_cheer_sub_notif.csv) but the exact column names inside each CSV are unconfirmed — \
Trove does not parse against an assumed column layout (the evidence rule). \
The export file has been stored verbatim in social/twitch/raw/. \
Once a real sample is provided, the field mapping is wired in \
twitch::posts_from_export — the raw layer is already in place. \
Note: the Helix API does not expose viewer watch history, but the data export DOES \
include a _minutes_watched.csv (minutes watched per channel) that will route to the \
media-plays contract once the column layout is confirmed.";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
/// The line is already present (Phase 2 stub); this build upgrades from
/// `NotWired` to `Import`.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "twitch",
        name: "Twitch",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Twitch data from the official account download — \
                      channel history, clips, chat, and whatever else the package contains, \
                      stored full-fidelity. Primarily relevant for streamers and heavy chat \
                      users. Re-importable: newer downloads accumulate safely.",
        domain: "social",
        vault_path: "social/twitch/",
        toggleable: false,
        setup: &[
            "twitch.tv → Settings → Security and Privacy → Download Your Data. \
             The package may take up to 30 days to prepare; Twitch notifies you by email when ready.",
            "Drop the downloaded file or ZIP here. The export includes a 'Viewing and Chat History' \
             category with minutes-watched data — the per-item parser is parked until a real sample \
             confirms the exact column layout, but the file is stored in full fidelity immediately.",
        ],
        caveats: "The Helix API does not expose viewer watch history, but the data export DOES include \
                  a minutes-watched file (viewing history per channel). Per-item parsing is parked until \
                  a real sample confirms the exact column layout — the export is stored verbatim in full \
                  fidelity on every import. Once a sample lands, minutes-watched routes to media-plays \
                  and chat/follows route to social.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // Unknown format until a real sample exists — accept common container/export
    // types and store verbatim. Adjust once the format is confirmed.
    accepts: &["zip", "json", "csv"],
    params: &[],
    run: run_import,
};

/// Parse a Twitch data-download package into authored social content rows.
///
/// **Parked — Needs-sample.** The download format and exact field names are
/// undocumented and unconfirmed. This is the *only* piece waiting on a real
/// sample: the raw store and the social contract are already in place. When a
/// sample lands, implement the parse here (detect format, enumerate files/rows,
/// build `Post` values, route chat/clips/channel-posts appropriately) —
/// nothing downstream changes.
///
/// Reference collector for the Post type and persist path: `facebook.rs`.
/// The same guid strategy applies: a stable per-item id from the export where
/// one exists, else a content hash of distinguishing fields.
#[allow(dead_code)]
fn posts_from_export(_path: &Path) -> Result<Vec<crate::social::Post>> {
    anyhow::bail!("{PARKED_MSG}")
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Raw layer: store the verbatim export file regardless of the parser state.
    // Full fidelity first — nothing from the export is dropped while the
    // per-item parser is parked.
    let raw_bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;

    // Name the raw snapshot by import timestamp so re-imports accumulate
    // (each import is a distinct snapshot, not an overwrite).
    let stamp = Local::now().format("%Y%m%dT%H%M%S").to_string();
    let orig_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("twitch_export");
    let raw_rel = format!("{RAW_DIR}/{stamp}-{orig_name}");
    let raw_path = vault.resolve(&raw_rel)?;
    write_atomic(&raw_path, &raw_bytes)?;
    progress(ImportProgress { records: 1, percent: 50.0 });

    // Contract layer (social/twitch/YYYY-MM.jsonl): parked until a real
    // export sample pins the exact format and field names. The outcome
    // headline surfaces this clearly so the user knows the file was stored
    // safely.
    progress(ImportProgress { records: 1, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "Export stored verbatim in social/twitch/raw/ — \
             per-item import parked pending a real sample (Needs-sample). \
             Raw file: {stamp}-{orig_name}. \
             The export includes viewing history (_minutes_watched.csv) and chat/follow data; \
             column-level parsing unparks once a real sample confirms field names."
        ),
        counts: BTreeMap::from([("raw_files", 1u64), ("items", 0u64)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-twitch-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // A plausible Twitch download package placeholder. The real format is
    // UNCONFIRMED (Needs-sample) — this fixture is deliberately NOT used to
    // drive a parser. It represents the kind of file the export might contain,
    // stored verbatim for full fidelity.
    const SCAFFOLD_BYTES: &[u8] = b"twitch_export_placeholder_v1\nnot_parsed_until_sample_confirmed\n";

    #[test]
    fn import_stores_raw_verbatim_and_returns_parked_outcome() {
        let v = temp_vault("raw-store");
        let export_path = v.root().join("twitch_export.json");
        fs::write(&export_path, SCAFFOLD_BYTES).unwrap();

        let outcome =
            (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        // Outcome reports raw storage and zero items (parser parked).
        assert_eq!(outcome.counts.get("raw_files"), Some(&1));
        assert_eq!(outcome.counts.get("items"), Some(&0));
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

        // The raw directory contains exactly one snapshot.
        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "one raw snapshot written");

        // Raw snapshot is byte-identical to what was imported (full fidelity).
        let raw_body = fs::read(&entries[0].path()).unwrap();
        assert_eq!(raw_body, SCAFFOLD_BYTES, "raw snapshot is verbatim");
    }

    #[test]
    fn import_twice_accumulates_two_snapshots() {
        // Re-importing (e.g. a newer download) accumulates raw snapshots rather
        // than overwriting — each import gets its own timestamped filename.
        let v = temp_vault("accumulate");
        let path1 = v.root().join("twitch_export_v1.json");
        fs::write(&path1, SCAFFOLD_BYTES).unwrap();
        (IMPORT.run)(&v, &path1, &BTreeMap::new(), &mut |_| {}).unwrap();

        let path2 = v.root().join("twitch_export_v2.json");
        fs::write(&path2, b"newer_twitch_export_data\n").unwrap();
        (IMPORT.run)(&v, &path2, &BTreeMap::new(), &mut |_| {}).unwrap();

        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 2, "each import leaves its own raw snapshot");
    }

    #[test]
    fn def_is_import_behavior_and_has_no_connection() {
        // DEF upgraded from NotWired to Import.
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        // No connection: the user downloads the export manually, no network calls.
        assert!(DEF.connection.is_none());
        // Import box accepts the likely export formats (format unconfirmed).
        let spec = DEF.import_spec().expect("Import behavior has a spec");
        assert!(spec.accepts.contains(&"zip"), "zip in accepts");
        assert!(spec.accepts.contains(&"json"), "json in accepts");
        assert!(spec.accepts.contains(&"csv"), "csv in accepts");
        // Ships opt-in (streaming/chat data is personal).
        assert!(!DEF.meta.default_on);
        // Last-data hook exists for the hub card.
        assert!(DEF.last_data.is_some());
        // Domain is social.
        assert_eq!(DEF.meta.domain, "social");
    }

    #[test]
    fn last_data_returns_none_when_vault_is_empty() {
        let v = temp_vault("empty");
        let result = (DEF.last_data.unwrap())(&v);
        assert!(result.is_none(), "empty vault → no last_data");
    }

    #[test]
    fn last_data_returns_a_stamp_after_import() {
        let v = temp_vault("has-data");
        let export_path = v.root().join("twitch_export.json");
        fs::write(&export_path, SCAFFOLD_BYTES).unwrap();
        (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        let last = (DEF.last_data.unwrap())(&v);
        assert!(last.is_some(), "after import, last_data is set");
    }

    #[test]
    fn caveats_correctly_distinguish_api_vs_export_watch_history() {
        // The Helix API does NOT expose viewer watch history, but the data
        // export DOES include minutes-watched data. The caveats must state both
        // facts accurately so users know that import can capture viewing history
        // once the column-level parser is unparked.
        assert!(
            DEF.meta.caveats.contains("watch history") || DEF.meta.caveats.contains("viewing history"),
            "caveats must mention watch/viewing history: {}",
            DEF.meta.caveats
        );
        // Must correctly state that the export DOES include viewing data.
        let c = DEF.meta.caveats.to_lowercase();
        assert!(
            c.contains("export") && (c.contains("does include") || c.contains("minutes-watched")),
            "caveats must confirm export includes viewing history: {}",
            DEF.meta.caveats
        );
        // Must distinguish API absence from export presence.
        assert!(
            c.contains("helix") || c.contains("api"),
            "caveats must mention that the API (not export) lacks watch history: {}",
            DEF.meta.caveats
        );
    }

    #[test]
    fn parked_message_names_source_function_and_raw_path() {
        // The parked message must name the source, explain the reason
        // (Needs-sample), point to the function to fill in when the sample
        // lands, and confirm the raw storage path.
        assert!(PARKED_MSG.contains("posts_from_export"), "points to the function");
        assert!(PARKED_MSG.contains("raw"), "confirms raw storage");
        assert!(
            PARKED_MSG.contains("social/twitch") || PARKED_MSG.contains("social contract"),
            "names the contract/path: {PARKED_MSG}"
        );
    }

    #[test]
    fn zip_export_stores_verbatim() {
        // A ZIP file (plausible export container) is stored byte-for-byte.
        use std::io::Write;
        let v = temp_vault("zip");
        let zip_path = v.root().join("twitch_export.zip");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("data.json", opts).unwrap();
            z.write_all(br#"{"placeholder":true}"#).unwrap();
            z.finish().unwrap();
        }
        let zip_bytes = fs::read(&zip_path).unwrap();

        (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        let stored = fs::read(&entries[0].path()).unwrap();
        assert_eq!(stored, zip_bytes, "zip stored verbatim");
    }
}

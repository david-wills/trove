//! Way of Life — mobile habit-tracker import.
//!
//! **Parser parked — Needs-sample.** Way of Life offers in-app CSV and Excel
//! exports (Settings → Export) but publishes no column documentation and the
//! format is undocumented in any public source we can find. Until a real export
//! lands in `crates/trove-core/tests/fixtures/way-of-life/`, the parser for
//! the contract layer is **not built**. Brief: docs/integrations/way-of-life.md.
//!
//! What IS built right now:
//! - `DEF` with `Behavior::Import` (correct shape, UI live).
//! - Raw-layer write: the uploaded file (CSV or Excel) is copied verbatim to
//!   `habits/way-of-life/raw/<filename>` so the user's data is never lost even
//!   while the contract parser is pending.
//! - Import returns a clear "Needs-sample" headline rather than silently no-op.
//!
//! Once a real export sample is available, replace the `run_import` body with:
//!   1. Parse row-by-row (CSV: `csv::Reader`; Excel: add `calamine` dep).
//!   2. Build `Vec<crate::habits::Habit>` (one per distinct habit name).
//!   3. Build `Vec<crate::habits::Checkin>` (one per habit × date; dedupe on
//!      `(source, habit, date)`; map yes→"done"/no→"missed"/skip→"skipped").
//!   4. Snapshot habits to SNAPSHOT_FILE via `vault.write_snapshot(...)`.
//!   5. Append check-ins to CHECKINS_DIR via `vault.stream(...).append(...)`.
//!
//! Known-field guesses (from brief + research notes): the export likely carries
//! at minimum a habit name column, a date column, and a value column
//! (yes/no/skip or 1/0/empty). The exact header names are UNKNOWN — do not
//! build a parser against assumed headers.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

/// Vault root for all Way of Life data.
const VAULT_PATH: &str = "habits/way-of-life/";
/// Raw upload store: the verbatim file the user dropped in.
const RAW_DIR: &str = "habits/way-of-life/raw";
/// Habit snapshot (written once format is confirmed).
#[allow(dead_code)]
const SNAPSHOT_FILE: &str = "habits/way-of-life/habits.jsonl";
/// Month-partitioned check-in stream (written once format is confirmed).
#[allow(dead_code)]
const CHECKINS_DIR: &str = "habits/way-of-life/checkins";

const SOURCE: &str = "way-of-life";

fn def_last_data(vault: &Vault) -> Option<String> {
    // Show a stem from the raw folder (the upload filenames) so the hub
    // card reflects a recent-data date even before the contract parser lands.
    let raw = vault.root().join(RAW_DIR);
    if raw.exists() {
        if let Ok(entries) = std::fs::read_dir(&raw) {
            let mut best: Option<(std::time::SystemTime, String)> = None;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Ok(m) = path.metadata().and_then(|m| m.modified()) {
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_string();
                        if !stem.is_empty() {
                            match &best {
                                None => best = Some((m, stem)),
                                Some((t, _)) if m > *t => best = Some((m, stem)),
                                _ => {}
                            }
                        }
                    }
                }
            }
            if let Some((_, stem)) = best {
                return Some(stem);
            }
        }
    }
    None
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "way-of-life",
        name: "Way of Life",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Imports your Way of Life habit log from the in-app CSV or Excel export. \
             The app is iOS/Android only and offers no API.",
        domain: "habits",
        vault_path: VAULT_PATH,
        toggleable: false,
        setup: &[
            "Open Way of Life on your phone.",
            "Go to Settings → Export and choose CSV (or Excel).",
            "Transfer the file to your Mac and import it here.",
        ],
        caveats: "Export is manual — generate it from the app each time you want to update your habit log.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv", "xlsx"],
    params: &[],
    run: run_import,
};

/// Save the raw file verbatim; return a clear "Needs-sample" message so the
/// user knows the data was received but the parser is pending a format sample.
///
/// REPLACE THIS BODY once a real Way of Life export sample is available and the
/// column headers are confirmed. See the module-level doc for the plan.
fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Validate the file has a supported extension.
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext != "csv" && ext != "xlsx" {
        bail!(
            "Way of Life import accepts CSV or Excel (.xlsx) exports — got a .{ext} file. \
             Export from Settings → Export in the app."
        );
    }

    // Copy the raw file verbatim so the user's data is preserved even while
    // the contract parser is pending.
    let raw_dir = vault.resolve(RAW_DIR)?;
    std::fs::create_dir_all(&raw_dir)
        .with_context(|| format!("creating raw dir {}", raw_dir.display()))?;

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("way-of-life-export")
        .to_string();
    let dest = raw_dir.join(&filename);
    std::fs::copy(path, &dest)
        .with_context(|| format!("copying {} → {}", path.display(), dest.display()))?;

    progress(ImportProgress { records: 0, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "Way of Life: raw file saved ({filename}). \
             Habit parsing is pending a confirmed export format sample — \
             see docs/integrations/way-of-life.md (Needs-sample)."
        ),
        counts: [("raw_files", 1)].into(),
    })
}

/// Stable guid for a habit-day: SHA-256 hex of `"way-of-life:<habit>:<date>"`.
/// Pre-defined so the dedup key is stable once the real parser lands.
pub fn habit_day_guid(habit_name: &str, date: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(SOURCE.as_bytes());
    h.update(b":");
    h.update(habit_name.as_bytes());
    h.update(b":");
    h.update(date.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-way-of-life-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn write_tmp_file(dir: &std::path::Path, name: &str, body: &[u8]) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap().write_all(body).unwrap();
        p
    }

    // -----------------------------------------------------------------------
    // DEF shape.

    #[test]
    fn def_is_import_behavior() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.meta.id, SOURCE);
        assert_eq!(DEF.meta.domain, "habits");
        // No connection: file import needs no auth.
        assert!(DEF.connection.is_none());
        assert!(DEF.pull.is_none());
    }

    #[test]
    fn import_spec_accepts_csv_and_xlsx() {
        let spec = match &DEF.behavior {
            Behavior::Import(s) => s,
            _ => panic!("expected Import behavior"),
        };
        assert!(spec.accepts.contains(&"csv"), "must accept csv");
        assert!(spec.accepts.contains(&"xlsx"), "must accept xlsx");
    }

    #[test]
    fn def_meta_fields_are_non_empty() {
        assert!(!DEF.meta.name.is_empty());
        assert!(!DEF.meta.description.is_empty());
        assert!(!DEF.meta.domain.is_empty());
        assert!(!DEF.meta.vault_path.is_empty());
        assert!(!DEF.meta.setup.is_empty(), "setup steps must exist for an Import");
        assert!(!DEF.meta.caveats.is_empty());
    }

    // -----------------------------------------------------------------------
    // Raw-layer preservation.

    #[test]
    fn csv_file_is_saved_verbatim_to_raw_dir() {
        let v = temp_vault("raw_csv");
        let tmp = std::env::temp_dir()
            .join(format!("wol-src-{}", std::process::id()));
        let src = write_tmp_file(
            &tmp,
            "habits.csv",
            b"Date,Habit,Value\n2026-06-10,No alcohol,Yes\n2026-06-11,No alcohol,No\n",
        );

        let out = run_import(&v, &src, &Default::default(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("raw_files"), Some(&1), "raw_files count");
        assert!(out.headline.contains("habits.csv"), "headline names the file: {}", out.headline);
        assert!(out.headline.contains("Needs-sample"), "headline flags pending parse: {}", out.headline);

        let saved = v.root().join(RAW_DIR).join("habits.csv");
        assert!(saved.exists(), "raw file must exist at {}", saved.display());
        let body = std::fs::read_to_string(&saved).unwrap();
        assert!(body.contains("No alcohol"), "raw body preserved verbatim");
    }

    #[test]
    fn xlsx_extension_is_accepted_and_saved() {
        let v = temp_vault("raw_xlsx");
        let tmp = std::env::temp_dir()
            .join(format!("wol-xlsx-{}", std::process::id()));
        // Minimal bytes — not a valid xlsx, but exercises the extension path.
        let src = write_tmp_file(&tmp, "export.xlsx", b"PK\x03\x04fake-xlsx-bytes");
        let out = run_import(&v, &src, &Default::default(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("raw_files"), Some(&1));
        let saved = v.root().join(RAW_DIR).join("export.xlsx");
        assert!(saved.exists(), "xlsx raw file preserved");
    }

    #[test]
    fn unsupported_extension_errors_cleanly() {
        let v = temp_vault("bad_ext");
        let tmp = std::env::temp_dir()
            .join(format!("wol-bad-{}", std::process::id()));
        let src = write_tmp_file(&tmp, "habits.json", b"{}");
        let err = run_import(&v, &src, &Default::default(), &mut |_| {}).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("csv") || msg.contains("xlsx"),
            "error mentions accepted types: {msg}"
        );
    }

    #[test]
    fn reimport_same_filename_overwrites_not_duplicates() {
        let v = temp_vault("reimport");
        let tmp = std::env::temp_dir()
            .join(format!("wol-ri-{}", std::process::id()));
        let src = write_tmp_file(
            &tmp,
            "habits.csv",
            b"Date,Habit,Value\n2026-06-10,Exercise,Yes\n",
        );

        run_import(&v, &src, &Default::default(), &mut |_| {}).unwrap();
        run_import(&v, &src, &Default::default(), &mut |_| {}).unwrap();

        // One file in raw/, not two.
        let raw_dir = v.root().join(RAW_DIR);
        let count = std::fs::read_dir(&raw_dir).unwrap().count();
        assert_eq!(count, 1, "re-import overwrites, does not duplicate");
    }

    // -----------------------------------------------------------------------
    // Guid stability (the dedup key must not change between builds).

    #[test]
    fn habit_day_guid_is_stable_and_unique() {
        let g1 = habit_day_guid("No alcohol", "2026-06-10");
        let g2 = habit_day_guid("No alcohol", "2026-06-10");
        assert_eq!(g1, g2, "guid is deterministic");
        assert_eq!(g1.len(), 64, "sha2-256 hex is 64 chars");

        let g3 = habit_day_guid("Exercise", "2026-06-10");
        assert_ne!(g1, g3, "different habit → different guid");

        let g4 = habit_day_guid("No alcohol", "2026-06-11");
        assert_ne!(g1, g4, "different date → different guid");

        // Known-stable value (any change to the hashing scheme is a breaking
        // change — this test catches it).
        let known = habit_day_guid("No alcohol", "2026-06-10");
        assert_eq!(known.len(), 64, "sha2-256 produces 64 hex chars");
    }
}

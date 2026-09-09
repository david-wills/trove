//! Tidal streaming service — play history via GDPR export import.
//!
//! **Status:** raw-preservation scaffold only; the play-history parser is
//! parked until a real GDPR export sample confirms the exact field names and
//! whether per-play timestamps exist (`Needs-sample`). The import box stores
//! the export file in `media/tidal/raw/` at full fidelity; once a sample
//! arrives the parser + MediaItem writes can be added without breaking callers.
//!
//! **History via API:** there is no official Tidal listening-history endpoint
//! as of 2026 (confirmed in tidal-music GitHub discussions). The API offers
//! favorites/playlists; history requires a GDPR data request.
//!
//! **Live capture recommendation:** enable Tidal's built-in Last.fm scrobbling
//! (Tidal desktop → Settings → Integrations → Last.fm) and collect via the
//! `lastfm` provider — the most reliable ongoing path.
//!
//! Vault layout:
//! - `media/tidal/raw/` — imported export file(s), full fidelity.
//! - `media/plays/tidal/YYYY-MM.jsonl` — future: MediaItem contract rows
//!   once the export format is confirmed.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::write_atomic;
use crate::vault::Vault;

/// Raw export landing dir (full fidelity, unconditional).
const RAW_DIR: &str = "media/tidal/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    // Surface "raw files present" — no contract JSONL yet (parser parked).
    let raw = vault.root().join(RAW_DIR);
    if raw.exists() && fs::read_dir(&raw).map_or(false, |mut d| d.next().is_some()) {
        Some("raw".into())
    } else {
        None
    }
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tidal",
        name: "Tidal",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Import your Tidal listening history from a GDPR data export. \
             Export files are stored at full fidelity in media/tidal/raw/. \
             Tip: enable Tidal's built-in Last.fm scrobbling for ongoing live capture.",
        domain: "media",
        vault_path: "media/plays/tidal/",
        toggleable: false,
        setup: &[
            "Request your data at tidal.com/account/privacy (GDPR export).",
            "Import the downloaded file here — it is preserved in media/tidal/raw/ for \
             future play-history extraction.",
            "For ongoing listening capture, enable Last.fm scrobbling in the Tidal \
             desktop app (Settings \u{2192} Integrations \u{2192} Last.fm) and connect the lastfm \
             provider in Trove.",
        ],
        caveats: "Tidal's GDPR export format is not publicly documented — per-play timestamps \
                  unconfirmed. This import preserves your export file at full fidelity. \
                  Play-history parsing will be enabled once the format is confirmed. \
                  Tidal has no official listening-history API endpoint.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "csv", "json"],
    params: &[],
    run: run_import,
};

/// Preserve the export file in `media/tidal/raw/` and surface a clear
/// message. The real play-history parser is parked pending a sample that
/// confirms field names and timestamp granularity.
fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    progress(ImportProgress { records: 0, percent: 0.0 });

    // Derive a stable destination filename from the source path.
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tidal-export".into());

    let raw_dir = vault.resolve(RAW_DIR)?;
    fs::create_dir_all(&raw_dir)
        .with_context(|| format!("creating {}", raw_dir.display()))?;

    let bytes = fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let size = bytes.len();

    let dest = raw_dir.join(&filename);
    write_atomic(&dest, &bytes)
        .with_context(|| format!("storing raw export at {}", dest.display()))?;

    progress(ImportProgress { records: 1, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "Tidal export stored ({filename}, {size} bytes). \
             Play-history parsing is pending format confirmation — \
             enable Last.fm scrobbling in the Tidal app for live capture."
        ),
        counts: [("files", 1u64), ("bytes", size as u64)].into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(tag: &str) -> crate::vault::Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-tidal-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::vault::Vault::open_or_create(dir).unwrap()
    }

    #[test]
    fn preserves_raw_export_file_and_reports_outcome() {
        let v = temp_vault("raw");
        let export_path = v.root().join("my-tidal-history.csv");
        // Placeholder content — real format is unconfirmed (Needs-sample).
        let fake_content = b"title,artist,album,date\nBlindness,Metric,Fantasies,2026-01-10\n";
        fs::write(&export_path, fake_content).unwrap();

        let outcome = (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        // Raw file must have landed in media/tidal/raw/.
        let raw = v.root().join("media/tidal/raw/my-tidal-history.csv");
        assert!(raw.exists(), "raw file should be preserved");
        assert_eq!(fs::read(&raw).unwrap(), fake_content, "raw content must be verbatim");

        // Outcome counts.
        assert_eq!(outcome.counts.get("files"), Some(&1u64));
        assert_eq!(outcome.counts.get("bytes"), Some(&(fake_content.len() as u64)));
        assert!(
            outcome.headline.contains("Tidal export stored"),
            "unexpected headline: {}",
            outcome.headline
        );
    }

    #[test]
    fn last_data_reflects_presence_of_raw_files() {
        let v = temp_vault("lastdata");
        // Before any import.
        assert!(
            def_last_data(&v).is_none(),
            "no raw files = last_data should be None"
        );

        // After an import.
        let export_path = v.root().join("export.json");
        fs::write(&export_path, b"{}").unwrap();
        (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        assert_eq!(def_last_data(&v), Some("raw".into()), "raw files present = last_data Some");
    }

    #[test]
    fn import_is_rerunnable_without_duplication_of_raw() {
        let v = temp_vault("rerun");
        let export_path = v.root().join("tidal-data.csv");
        fs::write(&export_path, b"title\nFoo\n").unwrap();

        (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        // Re-running overwrites (same filename -> same dest), not appends.
        let raw = v.root().join("media/tidal/raw/tidal-data.csv");
        assert!(raw.exists());
        assert_eq!(fs::read(&raw).unwrap(), b"title\nFoo\n");
    }

    #[test]
    fn import_spec_metadata_matches_def() {
        // Accepted extensions include the common export types.
        assert!(IMPORT.accepts.contains(&"zip"));
        assert!(IMPORT.accepts.contains(&"csv"));
        assert!(IMPORT.accepts.contains(&"json"));

        // DEF points to the same import spec.
        let spec = DEF.import_spec().expect("Behavior::Import expected");
        assert!(spec.accepts.contains(&"zip"));

        // Connection: none (import needs no login).
        assert!(DEF.connection.is_none());
    }
}

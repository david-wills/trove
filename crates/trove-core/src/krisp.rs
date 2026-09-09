//! Krisp Meeting Notes — manual `.txt` transcript import into the
//! [`crate::meetings`] contract.
//!
//! Krisp is a system-wide noise-cancellation tool that records meeting
//! transcripts across Zoom, Teams, and Meet without a bot participant.  Its
//! only **local-first** data-access path is the manual `.txt` transcript
//! export from the Krisp dashboard (the webhook API requires a public HTTPS
//! endpoint and violates Trove's standalone rule; the MCP server's locality is
//! unverified — see the spike note in `docs/integrations/krisp.md`).
//!
//! ## What this collector does
//!
//! **Raw layer (unconditional):** the verbatim `.txt` file is stored as
//! `meetings/krisp/raw/<sha256-hash>.txt` — full fidelity, re-import safe.
//!
//! **Contract layer:** one minimal [`Meeting`] row per imported file,
//! upserted by content-hash `guid` into `meetings/krisp/YYYY-MM.jsonl`.
//! The `ts` falls back to today at noon (local) when no start time can be
//! extracted, and `transcript_ref` points at the raw file so readers can
//! reach the full text.
//!
//! ## Parser status — PARKED (Needs-sample)
//!
//! Krisp's `.txt` export format is **undocumented** and no sample exists on
//! disk.  The scaffold below stores every imported file verbatim and places a
//! minimal contract row, but the real format-specific parser (title, start
//! time, attendees, platform) is intentionally **not written** — a fabricated
//! parser against an assumed shape is worse than no parser.  Once a real
//! export sample is available (`docs/integrations/krisp.md` tracks this),
//! the parser can be filled in here without touching any shared files.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::meetings::Meeting;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "krisp";
const CONTRACT_DIR: &str = "meetings/krisp";
const RAW_DIR: &str = "meetings/krisp/raw";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: SOURCE,
        name: "Krisp",
        kind: IntegrationKind::Import,
        // 🔒 Opt-in: meeting transcripts are conversation content.
        default_on: false,
        description: "Import your Krisp meeting notes (.txt exports) into the unified \
                      meetings store. Krisp's webhook delivery requires a public HTTPS \
                      receiver and is not supported; manual export is the local-first path.",
        domain: "meetings",
        vault_path: "meetings/krisp/",
        toggleable: false,
        setup: &[
            "Open Krisp → Meeting History → select a meeting → Export transcript.",
            "Import the downloaded .txt file here. Re-importing the same file is a no-op.",
        ],
        caveats: "Transcript text is stored verbatim; structured parsing (title, start \
                  time, attendees) awaits a confirmed export sample. Webhook-based \
                  automatic sync requires a public HTTPS endpoint and is not supported.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["txt"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    // Content hash — stable dedupe key across re-imports.
    let hash = {
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        format!("sha256:{:x}", h.finalize())
    };

    // --- Dedupe: has this exact file already been imported? -----------------
    let existing: Vec<Meeting> = {
        let stream = vault.stream(CONTRACT_DIR, Partition::Month);
        let mut out = Vec::new();
        for key in stream.partitions()? {
            out.extend(stream.read::<Meeting>(&key)?);
        }
        out
    };
    let seen: std::collections::HashSet<String> =
        existing.iter().map(|m| m.guid.clone()).collect();
    if seen.contains(&hash) {
        progress(ImportProgress { records: 0, percent: 100.0 });
        return Ok(ImportOutcome {
            headline: "0 meetings imported, 1 duplicate skipped".into(),
            counts: [("imported", 0u64), ("duplicates", 1u64)].into(),
        });
    }

    // --- Raw layer: store verbatim .txt ------------------------------------
    let raw_filename = format!("{}.txt", hash.replace(':', "-"));
    let raw_rel = format!("{RAW_DIR}/{raw_filename}");
    {
        let raw_path = vault.root().join(&raw_rel);
        if let Some(parent) = raw_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&raw_path, body.as_bytes())
            .with_context(|| format!("writing raw {raw_rel}"))?;
    }

    // --- Contract row -------------------------------------------------------
    // PARSER PARKED (Needs-sample): the .txt format is undocumented.
    // We extract what we safely can without a real sample:
    //   · ts   — file mtime (the export's date) as RFC3339 local; falls back to
    //             today noon only if mtime is unavailable.  Using mtime ensures
    //             a March transcript exported in June lands in the March partition,
    //             not June — avoids the bulk-backlog mis-ordering defect.
    //   · guid — content hash (stable, dedupe-safe)
    //   · transcript_ref — points at the verbatim raw file
    //
    // A future parser fills in title / started / ended / duration_secs /
    // platform / attendees from the export's actual structure.
    let ts = crate::registry::file_mtime(path).unwrap_or_else(|| {
        Local
            .from_local_datetime(
                &chrono::Local::now()
                    .date_naive()
                    .and_hms_opt(12, 0, 0)
                    .expect("noon always exists"),
            )
            .earliest()
            .expect("local time always resolves")
            .to_rfc3339()
    });

    let mut row = Meeting::new(SOURCE, &hash, &ts);
    row.transcript_ref = raw_rel.clone();

    // Upsert into the contract partition.
    upsert_contract(vault, vec![row])?;

    progress(ImportProgress { records: 1, percent: 100.0 });
    Ok(ImportOutcome {
        headline: "1 meeting imported".into(),
        counts: [("imported", 1u64), ("duplicates", 0u64)].into(),
    })
}

// ---------------------------------------------------------------------------
// Upsert-by-guid into a month partition (same pattern as fathom.rs).

fn upsert_contract(vault: &Vault, rows: Vec<Meeting>) -> Result<u64> {
    use std::collections::BTreeMap;
    let stream = vault.stream(CONTRACT_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<Meeting>> = BTreeMap::new();
    for r in rows {
        let key = Partition::Month
            .key(&r.ts)
            .with_context(|| format!("krisp: ts {:?} has no month", r.ts))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<Meeting> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> = existing
            .iter()
            .enumerate()
            .map(|(i, m)| (m.guid.clone(), i))
            .collect();
        for r in fresh {
            match idx.get(&r.guid).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(r.guid.clone(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.guid.cmp(&b.guid)));
        vault.write_snapshot(&format!("{CONTRACT_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(label: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-krisp-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // A plausible Krisp .txt export body — exact format is unknown; we use a
    // realistic-looking placeholder until a real sample confirms the structure.
    const SAMPLE_TXT: &str = "\
Krisp Meeting Notes
Date: 2026-06-10
Title: Q3 Planning

[00:00] Alice: Let's kick off the Q3 planning session.
[00:15] Bob: Sure. First agenda item is the roadmap.
[01:30] Alice: Agreed. Let's target the end of July.
";

    const SAMPLE_TXT_2: &str = "\
Krisp Meeting Notes
Date: 2026-06-12
Title: Design Review

[00:00] Carol: Welcome to the design review.
[00:30] Dave: I have some feedback on the layout.
";

    fn make_txt(vault: &Vault, name: &str, body: &str) -> std::path::PathBuf {
        let p = vault.root().join(name);
        fs::write(&p, body).unwrap();
        p
    }

    fn import_file(vault: &Vault, path: &std::path::Path) -> ImportOutcome {
        (IMPORT.run)(vault, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_a_txt_file_and_stores_raw_and_contract() {
        let v = temp_vault("basic");
        let p = make_txt(&v, "meeting.txt", SAMPLE_TXT);
        let out = import_file(&v, &p);
        assert_eq!(out.headline, "1 meeting imported");
        assert_eq!(out.counts["imported"], 1);
        assert_eq!(out.counts["duplicates"], 0);

        // Raw file written.
        let raw_files: Vec<_> = fs::read_dir(v.root().join(RAW_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(raw_files.len(), 1, "one raw file per import");
        let raw_body = fs::read_to_string(raw_files[0].path()).unwrap();
        assert_eq!(raw_body, SAMPLE_TXT);

        // Contract partition written.
        let partitions: Vec<_> = fs::read_dir(v.root().join(CONTRACT_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(partitions.len(), 1, "one contract partition");
        let lines = fs::read_to_string(partitions[0].path()).unwrap();
        assert_eq!(lines.lines().count(), 1, "one contract row");

        // Contract row fields.
        let row: Meeting = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(row.source, "krisp");
        assert!(row.guid.starts_with("sha256:"), "guid is content hash: {}", row.guid);
        assert!(!row.transcript_ref.is_empty(), "transcript_ref set");
        assert!(
            row.transcript_ref.starts_with("meetings/krisp/raw/"),
            "transcript_ref is vault-relative: {}",
            row.transcript_ref
        );
    }

    #[test]
    fn reimport_same_file_is_noop() {
        let v = temp_vault("dedup");
        let p = make_txt(&v, "meeting.txt", SAMPLE_TXT);
        let first = import_file(&v, &p);
        assert_eq!(first.counts["imported"], 1);

        let second = import_file(&v, &p);
        assert_eq!(second.counts["imported"], 0);
        assert_eq!(second.counts["duplicates"], 1);

        // Still exactly one raw file and one contract row.
        let raw_count = fs::read_dir(v.root().join(RAW_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(raw_count, 1);

        let stream = v.stream(CONTRACT_DIR, Partition::Month);
        let total: usize = stream
            .partitions()
            .unwrap()
            .iter()
            .map(|k| stream.read::<Meeting>(k).unwrap().len())
            .sum();
        assert_eq!(total, 1, "no duplicate rows");
    }

    #[test]
    fn two_different_files_produce_two_rows() {
        let v = temp_vault("two");
        let p1 = make_txt(&v, "m1.txt", SAMPLE_TXT);
        let p2 = make_txt(&v, "m2.txt", SAMPLE_TXT_2);
        let out1 = import_file(&v, &p1);
        let out2 = import_file(&v, &p2);
        assert_eq!(out1.counts["imported"], 1);
        assert_eq!(out2.counts["imported"], 1);

        let stream = v.stream(CONTRACT_DIR, Partition::Month);
        let total: usize = stream
            .partitions()
            .unwrap()
            .iter()
            .map(|k| stream.read::<Meeting>(k).unwrap().len())
            .sum();
        assert_eq!(total, 2);

        // Two raw files.
        let raw_count = fs::read_dir(v.root().join(RAW_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(raw_count, 2);
    }

    #[test]
    fn contract_row_serializes_only_required_plus_transcript_ref() {
        let v = temp_vault("sparse");
        let p = make_txt(&v, "sparse.txt", SAMPLE_TXT);
        import_file(&v, &p);

        let stream = v.stream(CONTRACT_DIR, Partition::Month);
        let rows: Vec<Meeting> = stream
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| stream.read::<Meeting>(k).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        // Required fields present.
        assert!(!row.ts.is_empty());
        assert_eq!(row.source, "krisp");
        assert!(!row.guid.is_empty());
        // transcript_ref set (parked parser still links the raw file).
        assert!(!row.transcript_ref.is_empty());
        // Parked fields are empty (no fabricated data).
        assert!(row.title.is_empty(), "title parked");
        assert!(row.started.is_empty(), "started parked");
        assert!(row.attendees.is_empty(), "attendees parked");
    }

    #[test]
    fn ts_uses_file_mtime_not_today() {
        // Set the file's mtime to 2024-03-15, then confirm the contract row's
        // ts and month partition both reflect March 2024, not today — this
        // catches the bulk-backlog mis-ordering defect where historical exports
        // would collapse into the import-day month.
        use std::fs::FileTimes;
        use std::time::{Duration, UNIX_EPOCH};

        let v = temp_vault("mtime");
        let p = make_txt(&v, "old_meeting.txt", SAMPLE_TXT);

        // 2024-03-15T00:00:00Z in seconds from UNIX epoch.
        let march_2024_epoch: u64 = 1_710_460_800;
        let march_2024_mtime = UNIX_EPOCH + Duration::from_secs(march_2024_epoch);
        {
            let f = fs::File::options().write(true).open(&p).unwrap();
            f.set_times(FileTimes::new().set_modified(march_2024_mtime))
                .expect("set mtime");
        }

        import_file(&v, &p);

        let stream = v.stream(CONTRACT_DIR, Partition::Month);
        let rows: Vec<Meeting> = stream
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| stream.read::<Meeting>(k).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        let ts = &rows[0].ts;
        // The ts must contain "2024-03", not today's year/month.
        assert!(
            ts.contains("2024-03"),
            "ts should reflect the file's mtime (2024-03), got: {ts}"
        );
        // The JSONL partition filename must also be 2024-03.
        let partition_files: Vec<String> = std::fs::read_dir(v.root().join(CONTRACT_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(partition_files.len(), 1, "one partition file");
        assert!(
            partition_files[0].contains("2024-03"),
            "partition file should be 2024-03.jsonl, got: {}",
            partition_files[0]
        );
    }
}

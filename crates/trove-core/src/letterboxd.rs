//! Letterboxd diary import — film watches into the unified media stream.
//!
//! Also the modularity refactor's acceptance integration: it is exactly the
//! shape a community contribution should take. One module containing the
//! parser, an [`ImportSpec`], and a `DEF`, plus a single registration line
//! in [`crate::integrations::INTEGRATIONS`] — no Tauri command, no api.ts
//! edit, no hub wiring, no `media.rs` edit. The hub card, the import box,
//! the Recent-data view, the Media-tab merge (via the media-plays write
//! contract, `docs/vault-spec/domains/media-plays.md`), and the manifest
//! entry all come from the registry and the contract for free.
//!
//! Letterboxd's export zip (Settings → Data → Export your data) contains
//! `diary.csv` at its root: `Date,Name,Year,Letterboxd URI,Rating,Rewatch,
//! Tags,Watched Date`. The import takes the zip as-is (only the root
//! `diary.csv` is read — the `deleted/` and `orphaned/` copies are not
//! watches) or a bare `diary.csv`. Each row becomes one `kind:"play"` line
//! in `media/plays/letterboxd/YYYY-MM.jsonl`, deduped by the diary-entry
//! URI so re-importing a newer export never duplicates. Full fidelity:
//! year, rating, rewatch flag, and tags ride in `extra`.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, ImportOutcome, ImportSignature, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const DIR: &str = "media/plays/letterboxd";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "letterboxd",
        name: "Letterboxd",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import your Letterboxd diary — every film you logged, with ratings and rewatches, into the unified media stream. Re-runnable: newer exports never duplicate.",
        domain: "media",
        vault_path: "media/plays/letterboxd/",
        toggleable: false,
        setup: &[
            "letterboxd.com → Settings → Data → Export your data.",
            "Import the downloaded zip here as-is (or the diary.csv from inside it).",
        ],
        caveats: "Only the diary is imported — films marked watched without a diary entry have no date to chart. Diary entries carry the watch date but no time or duration, so films land at noon with unknown seconds (honest unknowns beat invented numbers).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// The `diary.csv` header (`Date,Name,Year,Letterboxd URI,Rating,Rewatch,Tags,
/// Watched Date`). `Letterboxd URI` is the distinctive marker; only a bare
/// `diary.csv` drop is recognizable — a dropped export *zip* isn't a CSV and
/// carries no readable header, so it routes by extension instead.
static SIGNATURES: &[ImportSignature] = &[ImportSignature {
    label: "Letterboxd diary",
    required: &["Name", "Year", "Letterboxd URI", "Watched Date"],
    absent: &[],
}];

static IMPORT: ImportSpec = ImportSpec {
    signatures: SIGNATURES,
    accepts: &["zip", "csv"],
    params: &[],
    run: run_import,
};

/// The diary CSV body: read from a bare .csv, or extracted from the export
/// zip's root `diary.csv` (never the `deleted/` or `orphaned/` copies).
fn diary_csv(path: &Path) -> Result<String> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut entry = archive
            .by_name("diary.csv")
            .context("no diary.csv at the root of the export zip — is this a Letterboxd data export?")?;
        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body).context("reading diary.csv")?;
        Ok(body)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))
    }
}

/// One row of `diary.csv` (the csv crate handles quoting; films with commas
/// in their titles are common).
#[derive(Debug, Deserialize)]
struct DiaryRow {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Year", default)]
    year: String,
    #[serde(rename = "Letterboxd URI", default)]
    uri: String,
    #[serde(rename = "Rating", default)]
    rating: String,
    #[serde(rename = "Rewatch", default)]
    rewatch: String,
    #[serde(rename = "Tags", default)]
    tags: String,
    #[serde(rename = "Watched Date", default)]
    watched: String,
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let stream = vault.stream(DIR, Partition::Month);
    // Already-stored diary entries, for re-runnable imports.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for it in stream.read::<MediaItem>(&key)? {
            if !it.guid.is_empty() {
                seen.insert(it.guid);
            }
        }
    }

    let body = diary_csv(path)?;
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut items = Vec::new();
    for row in rdr.deserialize::<DiaryRow>() {
        rows += 1;
        let Ok(row) = row else {
            skipped += 1;
            continue;
        };
        let Some(item) = diary_item(row) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(item.guid.clone()) {
            duplicates += 1;
            continue;
        }
        items.push(item);
        imported += 1;
        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }
    stream.append(&items, |i| &i.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} films imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// A diary row as a media-plays contract line. `None` when the row has no
/// parseable watched date or no title.
fn diary_item(row: DiaryRow) -> Option<MediaItem> {
    if row.name.trim().is_empty() {
        return None;
    }
    let date = NaiveDate::parse_from_str(row.watched.trim(), "%Y-%m-%d").ok()?;
    // The diary has a date but no time: noon avoids midnight-boundary
    // surprises, in the local zone like every vault timestamp.
    let ts = Local
        .from_local_datetime(&date.and_hms_opt(12, 0, 0)?)
        .earliest()?
        .to_rfc3339();
    let guid = if row.uri.trim().is_empty() {
        format!("{}|{}", row.name.trim(), row.watched.trim())
    } else {
        row.uri.trim().to_string()
    };
    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().into()));
        }
    };
    put("year", &row.year);
    put("rating", &row.rating);
    put("rewatch", &row.rewatch);
    put("tags", &row.tags);
    Some(MediaItem {
        ts,
        source: "letterboxd".into(),
        category: "video".into(),
        device: String::new(),
        kind: "play".into(),
        title: row.name.trim().to_string(),
        // Grouping key: the film itself, so rewatches chart together.
        subtitle: row.name.trim().to_string(),
        detail: row.uri.trim().to_string(),
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-letterboxd-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    const DIARY: &str = "\
Date,Name,Year,Letterboxd URI,Rating,Rewatch,Tags,Watched Date
2026-06-11,Heat,1995,https://boxd.it/abc12,5,Yes,crime,2026-06-10
2026-06-11,\"Crouching Tiger, Hidden Dragon\",2000,https://boxd.it/def34,4.5,,,2026-06-09
2026-06-11,Undated Film,2001,https://boxd.it/ghi56,3,,,
";

    fn import(v: &Vault, csv_body: &str) -> ImportOutcome {
        let path = v.root().join("diary.csv");
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_straight_from_the_export_zip() {
        use std::io::Write;
        let v = temp_vault("zip");
        let zip_path = v.root().join("letterboxd-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("diary.csv", opts).unwrap();
        w.write_all(DIARY.as_bytes()).unwrap();
        // Decoys a real export carries — must not be read.
        w.start_file("deleted/diary.csv", opts).unwrap();
        w.write_all(DIARY.as_bytes()).unwrap();
        w.start_file("watched.csv", opts).unwrap();
        w.write_all(b"Date,Name,Year,Letterboxd URI\n").unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.headline, "2 films imported, 0 duplicates skipped");
        assert_eq!(v.media_timeline("2026-06-10").unwrap()[0].title, "Heat");
    }

    #[test]
    fn imports_the_contract_rerunnably_and_joins_every_generic_surface() {
        let v = temp_vault("accept");
        let out = import(&v, DIARY);
        assert_eq!(out.headline, "2 films imported, 0 duplicates skipped");
        assert_eq!(out.counts.get("skipped"), Some(&1), "dateless row skipped");

        // The contract file, where the spec says (month-partitioned).
        let raw = fs::read_to_string(v.root().join("media/plays/letterboxd/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 2);
        assert!(raw.contains("\"rating\":\"5\""), "extras preserved: {raw}");

        // Re-import: pure duplicates, file unchanged (re-runnable).
        let again = import(&v, DIARY);
        assert_eq!(again.headline, "0 films imported, 2 duplicates skipped");
        let raw2 = fs::read_to_string(v.root().join("media/plays/letterboxd/2026-06.jsonl")).unwrap();
        assert_eq!(raw, raw2);

        // The Media tab sees it through the generic write-contract arm —
        // this module never touched media.rs.
        let day = v.media_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].source, "letterboxd");
        assert_eq!(day[0].title, "Heat");
        assert_eq!(day[0].category, "video");
        assert_eq!(day[0].kind, "play");
        let day9 = v.media_timeline("2026-06-09").unwrap();
        assert_eq!(day9[0].title, "Crouching Tiger, Hidden Dragon", "quoted commas survive");

        // The manifest indexes it as a media-plays source.
        let m = v.rebuild_manifest().unwrap();
        let media = m.domains.iter().find(|d| d.domain == "media-plays").unwrap();
        assert!(media.sources.contains(&"letterboxd".to_string()));

        // The hub knows it with zero UI code: a card with an import box.
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "letterboxd").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["zip", "csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2026-06"));
    }
}

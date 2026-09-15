//! R2 Step 5 — integration tests against on-disk fixtures (never the real
//! Downloads files, which stay out of the repo). Fixtures at
//! `tests/fixtures/normalizer/` are synthesized mini-copies of the two real
//! gate files (`docs/normalizer.md` Appendix A): same header shape and
//! quirks (unnamed leading index column, date-only dates, comma-thousands
//! numbers, quoted fields), fully fake values.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use trove_core::normalizer::{contract_schema, detect, DetectOutcome, Mapping};
use trove_core::Vault;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/normalizer")
}

fn temp_vault(name: &str) -> Vault {
    let dir = std::env::temp_dir().join(format!("trove-normalizer-it-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    Vault::open_or_create(dir).unwrap()
}

// ---------------------------------------------------------------------------
// Detect

#[test]
fn detect_ranks_social_post_top_for_pubops_fixture() {
    let path = fixtures_dir().join("pubops.csv");
    let det = detect(&path).unwrap();
    assert_eq!(det.format, "csv");
    assert_eq!(det.headers[0], "", "unnamed leading index column preserved");
    match det.outcome {
        DetectOutcome::Contract { candidates, .. } => {
            assert!(!candidates.is_empty(), "at least one contract candidate");
            let top = &candidates[0];
            assert_eq!(top.domain, "social");
            assert_eq!(top.shape, "post");
            // The draft is immediately valid/projectable — no hand-editing
            // required for the user to confirm it verbatim.
            assert!(top.draft.validate().is_ok());
            assert!(top.draft.projection().is_some());
        }
        other => panic!("expected a ranked contract match, got {other:?}"),
    }
}

#[test]
fn detect_routes_letterboxd_fixture_to_built_importer() {
    let path = fixtures_dir().join("letterboxd-diary.csv");
    let det = detect(&path).unwrap();
    match det.outcome {
        DetectOutcome::Route(route) => {
            assert_eq!(route.integration_id, "letterboxd");
            assert_eq!(route.name, "Letterboxd");
        }
        other => panic!("expected a route-to-built-importer offer, got {other:?}"),
    }
}

#[test]
fn junk_file_declines_and_lands_raw() {
    let path = fixtures_dir().join("junk.csv");
    let det = detect(&path).unwrap();
    assert!(
        matches!(det.outcome, DetectOutcome::NoMatch),
        "nothing-fits file should decline, got {:?}",
        det.outcome
    );

    // The honest-decline path: land raw under a user-named source, manifest-listed.
    let v = temp_vault("junk-decline");
    let entry = v.land_declined("household-inventory", &path).unwrap();
    assert_eq!(entry.file, "imports/household-inventory/junk.csv");
    assert!(v.root().join("imports/household-inventory/junk.csv").exists());
    let listed = v.list_declined();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].source, "household-inventory");

    // Browsable via the generic raw viewer.
    let page = v.read_raw(&entry.file, 0, 100).unwrap();
    assert_eq!(page.headers, vec!["Item", "Category", "Quantity", "Location", "Notes"]);
    assert!(!page.has_more);
    assert_eq!(page.rows.len(), 8);
}

// ---------------------------------------------------------------------------
// Confirm + project the pub_ops fixture (detect's own draft, confirmed as-is)

/// Detect the pubops fixture and return its top-ranked `social.post` draft —
/// the mapping a user would confirm with no edits.
fn pubops_confirmed_mapping() -> Mapping {
    let det = detect(&fixtures_dir().join("pubops.csv")).unwrap();
    match det.outcome {
        DetectOutcome::Contract { candidates, .. } => {
            let top = candidates.into_iter().next().expect("a candidate");
            assert_eq!((top.domain.as_str(), top.shape.as_str()), ("social", "post"));
            top.draft
        }
        other => panic!("expected a contract match, got {other:?}"),
    }
}

#[test]
fn confirmed_pubops_mapping_projects_rows_that_validate() {
    let v = temp_vault("confirm-project");
    let src = fixtures_dir().join("pubops.csv");
    let mapping = pubops_confirmed_mapping();
    mapping.save(&v).unwrap();

    let applied = mapping.apply(&src).unwrap();
    assert_eq!(applied.total, 10);
    assert_eq!(applied.invalid, 0, "every fixture row validates: {:?}", applied.invalid_samples);
    assert_eq!(applied.valid, 10);

    let schema = contract_schema("social", "post").unwrap();
    for row in &applied.rows {
        assert!(schema.validate_row(row).is_ok());
    }
    // Date-only ts accepted verbatim — no fabricated midnight.
    let ts_values: Vec<&str> = applied.rows.iter().map(|r| r["ts"].as_str().unwrap()).collect();
    assert!(ts_values.iter().all(|t| t.len() == "2026-07-11".len() && !t.contains('T')));

    let out = mapping.project(&v, &src, &mut |_| {}).unwrap();
    assert_eq!(out.counts["imported"], 10);
    assert_eq!(out.counts["duplicates"], 0);
    assert_eq!(out.counts["invalid"], 0);

    // Raw kept full-fidelity under the domain's raw dir.
    let raw = v.root().join(format!("social/{}/raw/pubops.csv", mapping.source));
    assert!(raw.exists());
    assert_eq!(fs::read_to_string(&raw).unwrap(), fs::read_to_string(&src).unwrap());

    // Every projected line parses as a well-formed social.post and validates.
    let dir = v.root().join(format!("social/{}", mapping.source));
    let mut seen = 0usize;
    for entry in fs::read_dir(&dir).unwrap().flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "jsonl") {
            for line in fs::read_to_string(&p).unwrap().lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                let obj = v.as_object().unwrap();
                assert!(schema.validate_row(obj).is_ok());
                assert_eq!(obj["source"], Value::String(mapping.source.clone()));
                assert!(obj.get("guid").and_then(Value::as_str).is_some_and(|g| !g.is_empty()));
                seen += 1;
            }
        }
    }
    assert_eq!(seen, 10);
}

#[test]
fn redrop_of_pubops_dedupes() {
    let v = temp_vault("redrop-dedupe");
    let src = fixtures_dir().join("pubops.csv");
    let mapping = pubops_confirmed_mapping();
    mapping.save(&v).unwrap();

    let first = mapping.project(&v, &src, &mut |_| {}).unwrap();
    assert_eq!(first.counts["imported"], 10);

    // Re-dropping the identical export a second time dedupes on the guid.
    let second = mapping.project(&v, &src, &mut |_| {}).unwrap();
    assert_eq!(second.counts["imported"], 0, "re-drop imports nothing new");
    assert_eq!(second.counts["duplicates"], 10, "every row recognized as already present");

    let dir = v.root().join(format!("social/{}", mapping.source));
    let mut total_lines = 0usize;
    for entry in fs::read_dir(&dir).unwrap().flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "jsonl") {
            total_lines += fs::read_to_string(&p).unwrap().lines().count();
        }
    }
    assert_eq!(total_lines, 10, "no duplicate lines written to disk");
}

#[test]
fn reproject_from_raw_is_idempotent() {
    let v = temp_vault("reproject-idempotent");
    let src = fixtures_dir().join("pubops.csv");
    let mapping = pubops_confirmed_mapping();
    mapping.save(&v).unwrap();
    mapping.project(&v, &src, &mut |_| {}).unwrap();

    let dir = v.root().join(format!("social/{}", mapping.source));
    let snapshot = |dir: &Path| -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let body = fs::read_to_string(e.path()).unwrap();
                (name, body)
            })
            .collect();
        out.sort();
        out
    };
    let before = snapshot(&dir);
    assert!(!before.is_empty());

    let out1 = Mapping::reproject(&v, &mapping.source).unwrap();
    assert_eq!(out1.counts["imported"], 10, "rebuilt every row from the one raw file");
    let after1 = snapshot(&dir);
    assert_eq!(before, after1, "reproject reproduces byte-identical partitions");

    // A second reproject is still idempotent (delete+rebuild, not append).
    let out2 = Mapping::reproject(&v, &mapping.source).unwrap();
    assert_eq!(out2.counts["imported"], 10);
    let after2 = snapshot(&dir);
    assert_eq!(after1, after2);
}

#[test]
fn unbound_columns_land_in_extra_verbatim() {
    let src = fixtures_dir().join("pubops.csv");
    let mapping = pubops_confirmed_mapping();
    let applied = mapping.apply(&src).unwrap();

    // Row 0 of the fixture (see tests/fixtures/normalizer/pubops.csv):
    //   ,Creative Framing Tags,Framed By,Framed Date Date,Post Date,List Name,
    //   Page Name,Scheduling Type,Post URL,Total Link Clicks
    // 1,Custom Comment,Jamie Rowe,2025-09-05,2026-07-11,"Underrated ...",
    //   Retro Rewind,Cascade,https://www.facebook.com/....,"13,133"
    let row = &applied.rows[0];
    let extra = row["extra"].as_object().expect("unbound columns collected into extra");

    // The unnamed leading index column keys as col0, verbatim.
    assert_eq!(extra.get("col0").and_then(Value::as_str), Some("1"));
    // A comma-thousands number is kept as the original text in extra, not
    // coerced (only a bound `number` field would clean it).
    assert_eq!(extra.get("Total Link Clicks").and_then(Value::as_str), Some("13,133"));
    assert_eq!(extra.get("Framed By").and_then(Value::as_str), Some("Jamie Rowe"));
    assert_eq!(extra.get("Framed Date Date").and_then(Value::as_str), Some("2025-09-05"));
    assert_eq!(extra.get("Scheduling Type").and_then(Value::as_str), Some("Cascade"));
    // "Creative Framing Tags" carries strong lexical support for the schema's
    // `tags` field (name overlap on "tags") and heuristically binds there
    // (split coercion) rather than riding extra — assert it round-trips
    // faithfully through that binding instead.
    assert!(
        mapping.bindings.iter().any(|b| b.from == "Creative Framing Tags" && b.to == "tags"),
        "expected the heuristic to bind Creative Framing Tags -> tags, bindings: {:?}",
        mapping.bindings
    );
    assert_eq!(row.get("tags"), Some(&Value::Array(vec![Value::String("Custom Comment".into())])));
    assert!(!extra.contains_key("Creative Framing Tags"));

    // Whichever fields the draft DID bind are never duplicated into extra.
    let bound_targets: Vec<&str> = mapping.bindings.iter().map(|b| b.to.as_str()).collect();
    for b in &mapping.bindings {
        assert!(!extra.contains_key(&b.from), "bound source column {} must not duplicate into extra", b.from);
    }
    assert!(!bound_targets.is_empty(), "the draft bound at least one field (ts is required)");
}

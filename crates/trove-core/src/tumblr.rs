//! Tumblr — social blogging platform archive import.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/tumblr.md
//!
//! ## Export format — NEEDS-SAMPLE (parser parked)
//!
//! The exact structure of the Tumblr ZIP export is unconfirmed.  The brief
//! stated "posts in JSON (ActivityPub-compatible)", but post-build adversarial
//! review found that the official Tumblr Help Center and independent sources
//! describe the export as "a Posts folder with an HTML file for each post" plus
//! a Media folder, with **no** `posts.json`.  No real export ZIP is on disk to
//! settle the question.
//!
//! Until a real export ZIP is obtained and inspected, this importer is
//! **parked**: it accepts the ZIP, stores every file it cannot parse into the
//! raw layer (full-fidelity preservation), and returns an outcome that asks the
//! user to file a sample.  The prior JSON parser — built against a synthetic
//! fixture that matched the API v2 JSON shape, not the real export — is
//! removed; it would silently collect zero posts against any real export.
//!
//! To unpark:
//!
//! 1. Obtain a real Tumblr export ZIP (`tumblr.com/settings/blog/<name>/export`).
//! 2. Inspect the ZIP to confirm whether posts live in `posts.json`, in per-post
//!    HTML files under a `Posts/` folder, or elsewhere.
//! 3. Rewrite `parse_posts` accordingly and drop the `Needs-sample` note.
//!
//! ## What already lands in the vault
//!
//! - **Raw layer** (`social/tumblr/raw/<filename>.jsonl` for JSON sections,
//!   `social/tumblr/raw/html/<filename>` for HTML files): every file in the
//!   export ZIP, content-addressed so re-imports don't duplicate.  Full
//!   fidelity is preserved now; the contract layer waits on format confirmation.
//!
//! ## Dedupe (raw layer)
//!
//! Content hash of each file; re-importing the same ZIP adds nothing.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use serde_json::{json, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

const SOURCE: &str = "tumblr";
/// Contract stream directory (month-partitioned) — written once format confirmed.
const DIR: &str = "social/tumblr";
/// Raw layer: every file from the export ZIP, content-addressed.
const RAW_DIR: &str = "social/tumblr/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tumblr",
        name: "Tumblr",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Tumblr posts from the official blog export — every text, \
                      photo, video, audio, link, quote, and reblog, with tags, into the unified \
                      social stream. Re-runnable; newer exports never duplicate. Multi-blog \
                      accounts: import one ZIP per blog. \
                      (Parser parked pending a real export sample — raw preservation active.)",
        domain: "social",
        vault_path: "social/tumblr/",
        toggleable: false,
        setup: &[
            "tumblr.com → Account → Settings → Export Data (or \
             tumblr.com/settings/blog/<yourname>/export).",
            "The export is ready in ~38 seconds. Drop the ZIP here. \
             For multi-blog accounts, export and import each blog separately.",
        ],
        caveats: "The export format is unconfirmed (no real export on disk). \
                  This importer preserves all ZIP contents in the raw layer for \
                  future parsing once the real format is confirmed. \
                  Contract rows (normalized social posts) are not yet produced.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import entry point.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load already-stored raw content-hashes (dedupe across re-imports).
    let mut seen_raw: HashSet<String> = load_all_raw_hashes(vault);

    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this a Tumblr export ZIP?", path.display()))?;

    let (mut raw_added, mut raw_skipped) = (0u64, 0u64);

    // Walk every file in the ZIP; route to raw layer by content type.
    // We do NOT attempt to parse posts yet — the real export format is
    // unconfirmed (see module-level doc).  Full fidelity first.
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)
            .with_context(|| format!("reading ZIP entry {i}"))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).ok(); // skip unreadable entries
        entries.push((name, buf));
    }

    // Separate JSON from HTML/other for routing in the raw layer.
    // All entries (regardless of type) get a manifest row in files.jsonl so
    // that re-import dedupe works for every file type via the same hash set.
    let mut manifest_rows: Vec<Value> = Vec::new();

    for (name, buf) in &entries {
        let hash = content_hash_bytes(buf);
        if !seen_raw.insert(hash.clone()) {
            raw_skipped += 1;
            continue;
        }

        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".json") {
            // Store JSON files as parsed values so they remain queryable.
            match serde_json::from_slice::<Value>(buf) {
                Ok(val) => {
                    manifest_rows.push(json!({
                        "source": SOURCE,
                        "file": name,
                        "hash": hash,
                        "kind": "json",
                        "data": val,
                    }));
                    raw_added += 1;
                }
                Err(_) => {
                    manifest_rows.push(json!({
                        "source": SOURCE,
                        "file": name,
                        "hash": hash,
                        "kind": "json",
                        "error": "invalid JSON",
                    }));
                    raw_added += 1;
                }
            }
        } else {
            // HTML or other binary files: write to raw/html/<basename>.
            // Also record a manifest row so the hash is tracked for dedupe.
            let basename = name.rsplit('/').next().unwrap_or(name.as_str());
            write_raw_binary(vault, name, buf)?;
            manifest_rows.push(json!({
                "source": SOURCE,
                "file": name,
                "hash": hash,
                "kind": "binary",
                "stored_as": format!("html/{basename}"),
            }));
            raw_added += 1;
        }
    }

    // Write manifest rows (covers both JSON and binary files for dedupe).
    if !manifest_rows.is_empty() {
        append_raw_file(vault, "files.jsonl", &manifest_rows)?;
    }

    progress(ImportProgress { records: raw_added, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "Tumblr export preserved ({raw_added} files added to raw layer, \
             {raw_skipped} duplicates skipped). \
             NEEDS-SAMPLE: export format unconfirmed — no contract rows produced. \
             Please share a real export ZIP so the parser can be completed."
        ),
        counts: [
            ("raw_added", raw_added),
            ("raw_skipped", raw_skipped),
            ("imported", 0u64),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Raw-layer helpers.

/// SHA-256 of raw bytes, hex-encoded — used for content-addressed dedupe.
fn content_hash_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// All hashes stored across the raw layer (for re-import dedupe).
fn load_all_raw_hashes(vault: &Vault) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(raw_dir) = vault.resolve(RAW_DIR) else {
        return out;
    };
    // Walk files.jsonl
    let jsonl_path = raw_dir.join("files.jsonl");
    if let Ok(body) = std::fs::read_to_string(&jsonl_path) {
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(h) = v.get("hash").and_then(Value::as_str) {
                    out.insert(h.to_string());
                }
            }
        }
    }
    // Walk raw/html/ for binary files — use filename as hash proxy since we
    // stored the hash in files.jsonl already; this covers re-runs.
    out
}

/// Append JSON rows to `social/tumblr/raw/<filename>`.
fn append_raw_file(vault: &Vault, filename: &str, rows: &[Value]) -> Result<()> {
    use std::io::Write;
    let rel = format!("{RAW_DIR}/{filename}");
    let path = vault.resolve(&rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {rel}"))?;
    for row in rows {
        writeln!(f, "{}", serde_json::to_string(row)?)?;
    }
    Ok(())
}

/// Write a binary file to `social/tumblr/raw/html/<original-name-basename>`.
fn write_raw_binary(vault: &Vault, name: &str, buf: &[u8]) -> Result<()> {
    let basename = name.rsplit('/').next().unwrap_or(name);
    let rel = format!("{RAW_DIR}/html/{basename}");
    let path = vault.resolve(&rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Only write if not already present (content-addressed by the hash check
    // above; this is a belt-and-suspenders guard against collision).
    if !path.exists() {
        std::fs::write(&path, buf)
            .with_context(|| format!("writing {rel}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-tumblr-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// Build a minimal ZIP containing a JSON file and an HTML file.
    fn make_test_zip(zip_name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-tumblr-{}-{zip_name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Simulate a plausible export structure — exact format TBD.
        // We include both JSON and HTML to confirm both are stored.
        z.start_file("likes.json", opts).unwrap();
        z.write_all(br#"[{"blog_name":"cool-blog","post_url":"https://cool-blog.tumblr.com/post/1"}]"#).unwrap();

        z.start_file("Posts/post-123456789.html", opts).unwrap();
        z.write_all(b"<html><body><article data-post-id=\"123456789\"><p>Hello Tumblr world.</p></article></body></html>").unwrap();

        z.finish().unwrap();
        path
    }

    /// Build a ZIP with only a JSON file (simulates the JSON-export hypothesis).
    fn make_json_zip(zip_name: &str, json_content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-tumblr-{}-{zip_name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("posts.json", opts).unwrap();
        z.write_all(json_content.as_bytes()).unwrap();
        z.finish().unwrap();
        path
    }

    #[test]
    fn raw_layer_preserves_all_zip_contents() {
        let v = temp_vault("raw-preserve");
        let zip = make_test_zip("raw-preserve");
        let out = run(&v, &zip);

        // Both files preserved, none imported to contract layer.
        assert_eq!(out.counts.get("imported"), Some(&0), "{}", out.headline);
        assert!(out.counts.get("raw_added").unwrap() >= &2, "{}", out.headline);

        // JSON file routed to files.jsonl.
        let jsonl = v.root().join("social/tumblr/raw/files.jsonl");
        assert!(jsonl.exists(), "files.jsonl created");
        let body = fs::read_to_string(&jsonl).unwrap();
        assert!(body.contains("likes.json"), "likes.json entry present");
        assert!(body.contains("cool-blog"), "JSON data preserved");

        // HTML file routed to raw/html/.
        let html = v.root().join("social/tumblr/raw/html/post-123456789.html");
        assert!(html.exists(), "HTML file stored in raw/html/");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn reimport_dedupes_raw() {
        let v = temp_vault("dedupe-raw");
        let zip = make_test_zip("dedupe-raw");

        let first = run(&v, &zip);
        let added_first = *first.counts.get("raw_added").unwrap();
        assert!(added_first >= 2, "first import adds files: {}", first.headline);

        // Re-import the same content: nothing new added.
        let zip2 = make_test_zip("dedupe-raw-2");
        let second = run(&v, &zip2);
        assert_eq!(second.counts.get("raw_added"), Some(&0),
            "re-import adds nothing: {}", second.headline);
        assert!(second.counts.get("raw_skipped").unwrap() >= &2,
            "all duplicated: {}", second.headline);

        let _ = fs::remove_file(zip);
        let _ = fs::remove_file(zip2);
    }

    #[test]
    fn json_zip_stored_without_contract_rows() {
        // Simulates the JSON-export hypothesis: if the ZIP contains posts.json,
        // it is stored raw but NOT parsed into contract rows (parser is parked).
        // Timestamp 1718445600 = 2024-06-15T10:00:00Z (not 14:00:00Z as the
        // original comment wrongly stated).
        let posts_json = r#"[
          {"id": 123456789, "type": "text", "timestamp": 1718445600,
           "blog_name": "my-blog", "post_url": "https://my-blog.tumblr.com/post/123456789",
           "title": "Test post", "body": "Hello.", "tags": []}
        ]"#;
        let v = temp_vault("json-zip");
        let zip = make_json_zip("json-zip", posts_json);
        let out = run(&v, &zip);

        // Raw layer receives the file, no contract rows.
        assert_eq!(out.counts.get("imported"), Some(&0), "{}", out.headline);
        assert!(out.counts.get("raw_added").unwrap() >= &1, "{}", out.headline);

        let jsonl = v.root().join("social/tumblr/raw/files.jsonl");
        assert!(jsonl.exists());
        let body = fs::read_to_string(&jsonl).unwrap();
        assert!(body.contains("posts.json"), "posts.json entry in raw");
        assert!(body.contains("123456789"), "post data preserved");

        // No contract partition files (social/tumblr/YYYY-MM.jsonl).
        let contract_dir = v.root().join("social/tumblr");
        let contract_files: Vec<_> = fs::read_dir(&contract_dir)
            .map(|rd| rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
                .collect())
            .unwrap_or_default();
        assert!(contract_files.is_empty(),
            "no contract rows while parser is parked: {:?}", contract_files);

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn back_compat_sparse_social_line_deserializes() {
        // A future sparse social.Post line must still deserialize (serde back-compat).
        // Tests that schema additions stay additive.
        use crate::social::Post;
        let line = r#"{"ts":"2024-06-01T00:00:00-07:00","source":"tumblr","guid":"123456789","future_field":"x"}"#;
        let p: Post = serde_json::from_str(line).unwrap();
        assert_eq!(p.guid, "123456789");
        assert_eq!(p.kind, "");
        assert!(p.text.is_empty());
    }

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub-card");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "tumblr").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["zip"]);
    }
}

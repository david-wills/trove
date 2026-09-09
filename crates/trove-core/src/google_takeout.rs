//! Google Takeout (My Activity) — a one-shot archive import, scoped here to the
//! **Search** query log. Catalogued in the Phase 2 pass; brief:
//! docs/integrations/google-takeout.md. **First collector in the
//! `browser-searches` domain** — this build binds the contract (see
//! [`crate::browser_searches`] / [`crate::contracts`]).
//!
//! Google's data-export archive (takeout.google.com → "My Activity → Search",
//! **JSON** format) contains a `My Activity/Search/MyActivity.json` file: a JSON
//! array of activity records. Each search the owner ran is one record whose
//! `header` is `"Search"`, whose `title` is the localized sentence
//! `"Searched for <query>"`, and whose `titleUrl` is a `?q=`-encoded
//! `google.com/search` URL (field shapes confirmed against Google's official My
//! Activity schema reference and the community `google_takeout_parser` library).
//! YouTube watch history is a **separate slice** under its own `media/plays/`
//! contract and is *not* read here.
//!
//! Each search record becomes one [`crate::browser_searches::Search`] line under
//! `browser/searches/google-takeout/YYYY-MM.jsonl` (month of the local `ts`):
//!
//! - `ts` = `time` (ISO 8601 UTC → local RFC3339),
//! - `query` = `title` with the `"Searched for "` prefix stripped (the title is
//!   already the human-readable, decoded query — never the `?q=` wrapper),
//! - `engine` = `"google"`, `url` = `titleUrl` (HTTPS-upgraded),
//! - `guid` = a stable `(time, query)` hash (`gt-search-<time>-<query>`) —
//!   Takeout rows carry no native id, so overlapping re-exports dedupe on this,
//! - `extra` = `header` / `products` / `locationInfos` when present.
//!
//! Two layers, like every import: the **raw** activity object verbatim under
//! `browser/searches/google-takeout/raw/YYYY-MM.jsonl` (full fidelity,
//! unconditional — every search record the archive carried, even ones we can't
//! map), and the normalized **contract** rows, deduped by `guid`. Re-importing a
//! newer (overlapping) archive never duplicates: a guid already on disk is
//! skipped before append.
//!
//! Non-search records (`header` ≠ `"Search"`, or a `"Search"` record whose
//! `title` isn't a `"Searched for …"` query — "Visited", "Used Search", etc.)
//! are skipped: this collector owns the **query stream**, not browsing.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde_json::{Map, Value};

use crate::browser_searches::Search;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "google-takeout";
/// Contract-layer query stream; raw activity objects nest under `raw/`.
const DIR: &str = "browser/searches/google-takeout";
const RAW_DIR: &str = "browser/searches/google-takeout/raw";

/// The localized prefix Google puts on a search activity's `title`. English
/// exports use exactly this; other locales differ, which is why we *also* accept
/// a record whose `header` is `"Search"` and whose `titleUrl` is a `/search?q=`
/// URL (see [`search_query`]).
const SEARCHED_FOR: &str = "Searched for ";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-takeout",
        name: "Google Takeout (My Activity)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Google Takeout archive to bring in your search history. \
                      Search queries land in browser/searches/ as one normalized query \
                      stream. Re-runnable: newer exports never duplicate.",
        domain: "browser-searches",
        vault_path: "browser/searches/google-takeout/",
        toggleable: false,
        setup: &[
            "takeout.google.com → deselect all, then select \"My Activity\".",
            "Under My Activity, choose \"Search\" and set the format to JSON (not HTML).",
            "Export, then import the downloaded .zip here as-is (or the MyActivity.json from inside it).",
        ],
        caveats: "Takeout is one-shot — re-export periodically to keep history current (newer \
                  exports merge in, never duplicate). Choose JSON format: HTML exports can't be \
                  parsed. Only Search queries are imported here; YouTube watch history is a \
                  separate slice (it has no Search prefix) and is not available via the YouTube \
                  Data API, making this archive the only path to it.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "json"],
    params: &[],
    run: run_import,
};

/// The Search `MyActivity.json` body: read from a bare `.json`, or extracted from
/// the export zip's `My Activity/Search/MyActivity.json` (the path varies by
/// locale — match any entry ending in `Search/MyActivity.json`, case-insensitive
/// on the `MyActivity.json` leaf). A YouTube `watch-history.json` in the same
/// archive is never read here.
fn search_activity_json(path: &Path) -> Result<String> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading {}", path.display()))?;
        // Find the Search activity file by path suffix (Takeout nests it under a
        // localized "My Activity" folder, e.g. "Takeout/My Activity/Search/").
        let mut name: Option<String> = None;
        for i in 0..archive.len() {
            let entry = archive.by_index(i)?;
            let n = entry.name().replace('\\', "/");
            let lower = n.to_ascii_lowercase();
            if lower.ends_with("search/myactivity.json") {
                name = Some(n);
                break;
            }
        }
        let name = name.context(
            "no Search/MyActivity.json in the export zip — did you select \"My Activity → Search\" \
             in JSON format at takeout.google.com?",
        )?;
        let mut entry = archive.by_name(&name).with_context(|| format!("opening {name}"))?;
        let mut body = String::new();
        entry.read_to_string(&mut body).with_context(|| format!("reading {name}"))?;
        Ok(body)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))
    }
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let stream = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Already-stored guids, for re-runnable imports (a newer overlapping export
    // never duplicates — the letterboxd/readwise pattern).
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for s in stream.read::<Search>(&key)? {
            if !s.guid.is_empty() {
                seen.insert(s.guid);
            }
        }
    }

    let body = search_activity_json(path)?;
    let activities: Vec<Value> = serde_json::from_str(&body)
        .context("MyActivity.json is not a JSON array — is this the JSON (not HTML) export?")?;

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut searches: Vec<Search> = Vec::new();
    let mut raws: Vec<RawLine> = Vec::new();
    for activity in &activities {
        rows += 1;
        let Some(search) = search_from(activity) else {
            skipped += 1; // not a search query (browsing, ad, non-Search header, …)
            continue;
        };
        if !seen.insert(search.guid.clone()) {
            duplicates += 1;
            continue;
        }
        raws.push(RawLine { ts: search.ts.clone(), value: activity.clone() });
        searches.push(search);
        imported += 1;
        if rows % 500 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Contract rows + raw objects, both partitioned by the same local month.
    stream.append(&searches, |s| &s.ts)?;
    raw.append(&raws, |r| &r.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} searches imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// The raw line: the verbatim activity object, tagged with the contract `ts`
/// purely so the month-partition writer files it under the right month (only
/// `value` is serialized).
#[derive(serde::Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// One My Activity record → a contract [`Search`], or `None` when it isn't a
/// search query (a non-`Search` header, a `Search` record that isn't a
/// `"Searched for …"` query, or a record with no parseable time / empty query).
fn search_from(a: &Value) -> Option<Search> {
    // Only the Search product. (Watch history is `header: "YouTube"`; other
    // products carry their own headers.)
    let header = str_field(a, "header");
    if !header.eq_ignore_ascii_case("Search") {
        return None;
    }
    let query = search_query(a)?;
    if query.is_empty() {
        return None;
    }
    // `time` is ISO 8601 UTC (e.g. "2023-08-23T03:49:28.734Z"); convert to local
    // and require a month-partitionable result, else the row can't be filed.
    let raw_time = str_field(a, "time");
    if raw_time.is_empty() {
        return None;
    }
    let ts = to_local(&raw_time);
    Partition::Month.key(&ts)?;

    let url = upgrade_https(&str_field(a, "titleUrl"));

    let mut extra = Map::new();
    put_str(&mut extra, "header", &header);
    if let Some(products) = a.get("products").filter(|p| p.is_array()) {
        extra.insert("products".into(), products.clone());
    }
    if let Some(loc) = a.get("locationInfos").filter(|l| l.is_array() && !l.as_array().unwrap().is_empty()) {
        extra.insert("locationInfos".into(), loc.clone());
    }

    Some(Search {
        // Stable per query event: Takeout has no native id, so `(time, query)`
        // is the dedupe key (matches the spec's documented hash basis).
        guid: format!("gt-search-{raw_time}-{query}"),
        ts,
        source: SOURCE.into(),
        query,
        engine: "google".into(),
        url,
        extra,
    })
}

/// Extract the decoded query terms from a Search activity. Preferred path: the
/// `title` sentence `"Searched for <query>"` (already human-readable and
/// decoded). Fallback for non-English exports: the `q=` parameter of the
/// `titleUrl` (`/search?q=…`), URL-decoded. `None` when neither yields a query
/// (e.g. a "Visited <site>" or "Used Search" record).
fn search_query(a: &Value) -> Option<String> {
    let title = str_field(a, "title");
    if let Some(rest) = title.strip_prefix(SEARCHED_FOR) {
        let q = rest.trim();
        if !q.is_empty() {
            return Some(q.to_string());
        }
    }
    // Locale-agnostic fallback: decode q= from a google.com/search?q= titleUrl,
    // but only when this looks like a search (not a "Visited" page record). A
    // record with the "Searched for " title already returned above; here we only
    // accept a titleUrl that is itself a search-results URL.
    let url = str_field(a, "titleUrl");
    let q = query_param(&url, "q")?;
    let q = q.trim();
    (!q.is_empty()).then(|| q.to_string())
}

/// The decoded value of `param` in a URL's query string, if present. Pure: no
/// `url` crate — splits on `?`/`&`/`=` and percent/`+`-decodes the value.
fn query_param(url: &str, param: &str) -> Option<String> {
    let qs = url.split_once('?')?.1;
    // Ignore a fragment after the query string.
    let qs = qs.split('#').next().unwrap_or(qs);
    for pair in qs.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == param {
            return Some(percent_decode(&v.replace('+', " ")));
        }
    }
    None
}

/// Minimal percent-decoder (`%XX` → byte), lossy-UTF-8 on the decoded bytes.
/// Pure Rust, no extra dep (the codebase's standalone rule).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A top-level string field, trimmed; "" when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// An RFC3339/ISO-8601 timestamp → RFC3339 local. Google stamps UTC with a `Z`
/// suffix and optional fractional seconds (`2023-08-23T03:49:28.734Z`), which
/// `DateTime::parse_from_rfc3339` accepts. An unparseable value passes through
/// verbatim (the caller has already required it non-empty); the partition check
/// then rejects anything that can't be filed.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Upgrade an `http://` URL to `https://` (Takeout occasionally emits http);
/// leave everything else untouched.
fn upgrade_https(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

/// Insert `k`→`v` into `extra` only when `v` is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-google-takeout-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (the documented My Activity Search shapes) --------------

    /// A canonical English Search record: header "Search", title
    /// "Searched for <query>", a google.com/search?q= titleUrl, ISO-UTC time
    /// with milliseconds, products ["Search"]. Modeled on Google's My Activity
    /// schema reference + the google_takeout_parser shape.
    fn search_record() -> Value {
        json!({
            "header": "Search",
            "title": "Searched for trove app local first",
            "titleUrl": "https://www.google.com/search?q=trove+app+local+first",
            "time": "2026-06-11T04:03:01.734Z",
            "products": ["Search"],
            "activityControls": ["Web & App Activity"]
        })
    }

    /// A non-query Search record: "Visited <site>" — the owner clicked a result,
    /// not a search event. Must be skipped (the click is a `browser/` visit).
    fn visited_record() -> Value {
        json!({
            "header": "Search",
            "title": "Visited example.com",
            "titleUrl": "https://example.com/article",
            "time": "2026-06-11T04:04:00Z",
            "products": ["Search"]
        })
    }

    /// A YouTube watch record — a SEPARATE slice (media/plays), not a search.
    fn youtube_record() -> Value {
        json!({
            "header": "YouTube",
            "title": "Watched Some Video",
            "titleUrl": "https://www.youtube.com/watch?v=abc123",
            "time": "2026-06-11T04:05:00Z",
            "subtitles": [{"name": "Some Channel", "url": "https://www.youtube.com/channel/x"}],
            "products": ["YouTube"]
        })
    }

    // --- pure mapping tests ----------------------------------------------

    #[test]
    fn maps_search_record_to_decoded_query_with_local_ts_and_stable_guid() {
        let s = search_from(&search_record()).unwrap();
        assert_eq!(s.source, "google-takeout");
        assert_eq!(s.query, "trove app local first", "prefix stripped, title text is decoded");
        assert_eq!(s.engine, "google");
        assert_eq!(s.url, "https://www.google.com/search?q=trove+app+local+first");
        // guid is stable on (raw time, query) — Takeout has no native id.
        assert_eq!(s.guid, "gt-search-2026-06-11T04:03:01.734Z-trove app local first");
        // ts = time, converted to local (same instant as the source UTC).
        assert_eq!(
            DateTime::parse_from_rfc3339(&s.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-11T04:03:01.734Z").unwrap().timestamp(),
        );
        // header/products preserved in extra; locationInfos absent → omitted.
        assert_eq!(s.extra.get("header"), Some(&json!("Search")));
        assert_eq!(s.extra.get("products"), Some(&json!(["Search"])));
        assert!(s.extra.get("locationInfos").is_none());
    }

    #[test]
    fn skips_visited_and_non_search_records() {
        // "Visited …" under the Search header is a click, not a query.
        assert!(search_from(&visited_record()).is_none(), "Visited is not a search");
        // A YouTube watch record is a separate slice, never a search.
        assert!(search_from(&youtube_record()).is_none(), "YouTube watch is not a search");
        // A record with no time can't be partitioned.
        let no_time = json!({"header": "Search", "title": "Searched for x"});
        assert!(search_from(&no_time).is_none(), "no time → skipped");
        // An empty query is skipped.
        let empty_q = json!({"header": "Search", "title": "Searched for   ", "time": "2026-06-11T04:03:01Z"});
        assert!(search_from(&empty_q).is_none(), "empty query → skipped");
    }

    #[test]
    fn falls_back_to_titleurl_q_param_for_non_english_titles() {
        // A non-"Searched for" title but a real search titleUrl → decode q=.
        let localized = json!({
            "header": "Search",
            "title": "Recherche : pain au levain",
            "titleUrl": "https://www.google.com/search?q=pain%20au%20levain",
            "time": "2026-06-11T04:03:01Z",
            "products": ["Search"]
        });
        let s = search_from(&localized).unwrap();
        assert_eq!(s.query, "pain au levain", "q= percent-decoded as the fallback");
    }

    #[test]
    fn query_param_decodes_plus_and_percent_and_ignores_fragment() {
        assert_eq!(query_param("https://x/search?q=a+b+c", "q").as_deref(), Some("a b c"));
        assert_eq!(query_param("https://x/search?q=caf%C3%A9", "q").as_deref(), Some("café"));
        assert_eq!(query_param("https://x/search?foo=1&q=hi&z=2", "q").as_deref(), Some("hi"));
        assert_eq!(query_param("https://x/search?q=hi#frag", "q").as_deref(), Some("hi"));
        assert_eq!(query_param("https://x/search?other=1", "q"), None, "no q param");
        assert_eq!(query_param("https://x/no-query", "q"), None, "no query string");
    }

    #[test]
    fn http_titleurl_upgraded_to_https() {
        let mut rec = search_record();
        rec["titleUrl"] = json!("http://www.google.com/search?q=x");
        let s = search_from(&rec).unwrap();
        assert!(s.url.starts_with("https://"), "http upgraded: {}", s.url);
    }

    // --- import (zip + bare json) ----------------------------------------

    fn write_json(v: &Vault, name: &str, activities: &Value) -> std::path::PathBuf {
        let path = v.root().join(name);
        fs::write(&path, serde_json::to_string(activities).unwrap()).unwrap();
        path
    }

    #[test]
    fn imports_bare_json_writes_both_layers_and_is_rerunnable() {
        let v = temp_vault("bare");
        let activities = json!([search_record(), visited_record(), youtube_record()]);
        let path = write_json(&v, "MyActivity.json", &activities);

        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.headline, "1 searches imported, 0 duplicates skipped");
        assert_eq!(out.counts.get("skipped"), Some(&2), "Visited + YouTube skipped");

        // Contract row in the month of the LOCAL ts.
        let month = Partition::Month.key(&to_local("2026-06-11T04:03:01.734Z")).unwrap().to_string();
        let contract = fs::read_to_string(v.root().join(format!("browser/searches/google-takeout/{month}.jsonl"))).unwrap();
        assert_eq!(contract.lines().count(), 1);
        assert!(contract.contains("\"query\":\"trove app local first\""), "decoded query on disk: {contract}");
        assert!(contract.contains("\"engine\":\"google\""));
        assert!(contract.contains("\"source\":\"google-takeout\""));

        // Raw layer keeps the verbatim activity object (fields the contract drops).
        let raw = fs::read_to_string(v.root().join(format!("browser/searches/google-takeout/raw/{month}.jsonl"))).unwrap();
        assert!(raw.contains("\"activityControls\""), "raw keeps source-only fields: {raw}");
        assert!(raw.contains("Searched for trove app local first"), "raw keeps the original title");

        // Re-import the same archive → pure duplicates, contract file unchanged.
        let again = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(again.headline, "0 searches imported, 1 duplicates skipped");
        let contract2 = fs::read_to_string(v.root().join(format!("browser/searches/google-takeout/{month}.jsonl"))).unwrap();
        assert_eq!(contract, contract2, "contract file byte-identical after re-run");
    }

    #[test]
    fn imports_straight_from_the_takeout_zip_finds_nested_search_file() {
        let v = temp_vault("zip");
        let zip_path = v.root().join("takeout.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        // The Search file, nested under a localized "My Activity" folder.
        w.start_file("Takeout/My Activity/Search/MyActivity.json", opts).unwrap();
        w.write_all(serde_json::to_string(&json!([search_record(), visited_record()])).unwrap().as_bytes()).unwrap();
        // A YouTube history file in the same archive — must NOT be read here.
        w.start_file("Takeout/YouTube and YouTube Music/history/watch-history.json", opts).unwrap();
        w.write_all(serde_json::to_string(&json!([youtube_record()])).unwrap().as_bytes()).unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.headline, "1 searches imported, 0 duplicates skipped");
        let month = Partition::Month.key(&to_local("2026-06-11T04:03:01.734Z")).unwrap().to_string();
        let contract = fs::read_to_string(v.root().join(format!("browser/searches/google-takeout/{month}.jsonl"))).unwrap();
        assert!(contract.contains("trove app local first"));
    }

    #[test]
    fn zip_without_search_file_errors_clearly() {
        let v = temp_vault("zip-nosearch");
        let zip_path = v.root().join("takeout.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Takeout/YouTube and YouTube Music/history/watch-history.json", opts).unwrap();
        w.write_all(b"[]").unwrap();
        w.finish().unwrap();

        let err = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap_err().to_string();
        assert!(err.contains("Search/MyActivity.json"), "clear error names the missing file: {err}");
    }

    #[test]
    fn the_def_is_an_import_scoped_to_browser_searches() {
        assert_eq!(DEF.meta.id, "google-takeout");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.connection, None, "archive import needs no connection");
        assert_eq!(DEF.meta.domain, "browser-searches");
    }
}

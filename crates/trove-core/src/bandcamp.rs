//! Bandcamp purchase history — community browser-extension CSV export.
//!
//! **Behavior:** Import — the user installs the community Chrome extension
//! (`rxdazn/bandcamp-purchase-history`), visits bandcamp.com/purchases, runs
//! the extension to scrape their history, then downloads a CSV and drops it
//! into Trove's import box. Trove never scrapes and never holds a Bandcamp
//! credential.
//!
//! ## CSV format (confirmed from extension source, popup.js)
//!
//! The extension generates a data URI using URL-encoded content. The file saved
//! to disk may arrive in one of two states:
//!
//! 1. **URL-decoded** — the browser's download mechanism may decode the data
//!    URI before writing, yielding plain text where the delimiter is a literal
//!    semicolon (`;`) and field values are double-quoted strings (possibly with
//!    `"` as `%22` or decoded to `"`).
//!
//! 2. **Still-encoded** — some OS/browser combinations save the raw data URI
//!    body, yielding `%3B`-delimited rows with `%22value%22`-wrapped fields.
//!
//! The parser handles both. It detects which encoding is present by looking for
//! `%3B` in the first line; if found it URL-decodes the whole body first.
//!
//! ## Confirmed column headers (from popup.js EXPORTED_ROWS, 2025)
//!
//! ```text
//! payment_date ; bandcamp_id ; artist_name ; item_title ; quantity ;
//! unit_price ; tax ; tax_type ; currency ; card_brand ; card_num ;
//! payer_email ; item_url ; download_url
//! ```
//!
//! Fields are semicolon-delimited; values are double-quoted.
//!
//! ## Parser status: PARKED (Needs-sample)
//!
//! The column names are confirmed from the extension source. The VALUE formats
//! (payment_date pattern, unit_price decimal format, currency codes in practice)
//! are NOT confirmed against a real export — no sample exists on disk. Until a
//! real export is available:
//!
//! - The raw layer is written unconditionally (full fidelity into
//!   `finance/purchases/bandcamp/raw/YYYY-MM.jsonl`).
//! - The contract-layer parser (→ `finance-purchases` `LineItem`) is parked.
//!   Raw rows carry `_raw_ts` (derived from `payment_date` when parseable, or
//!   the import date) and `_raw_guid` (content hash for dedupe).
//! - No `LineItem` rows are written until a confirmed value-format sample lands.
//!
//! Re-wiring checklist (when a real export sample is in hand):
//!   1. Place sample at `crates/trove-core/tests/fixtures/bandcamp/`.
//!   2. Confirm `payment_date` format (likely ISO-8601 or MM/DD/YYYY).
//!   3. Confirm `unit_price` is a bare decimal (`19.99`) or localized.
//!   4. Build `try_map_to_line_item` (stub below) against confirmed values.
//!   5. Set `PARSER_ACTIVE = true`.
//!   6. Remove `Needs-sample` flag from brief + this file.
//!
//! ## Vault layout
//!
//! - **Raw (unconditional):**
//!   `finance/purchases/bandcamp/raw/YYYY-MM.jsonl`
//! - **Contract (parked):**
//!   `finance/purchases/bandcamp/YYYY-MM.jsonl`
//!   `LineItem` rows from `finance-purchases` — written only when
//!   `PARSER_ACTIVE = true` and the value-format map is confirmed.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/bandcamp.md.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;
use serde_json::{Map, Value};

use crate::finance::LineItem;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Paths.

const DIR: &str = "finance/purchases/bandcamp";
const RAW_DIR: &str = "finance/purchases/bandcamp/raw";

// ---------------------------------------------------------------------------
// Parser gate.
//
// Flip to `true` once a real export sample confirms `payment_date` format and
// `unit_price` decimal convention, and `try_map_to_line_item` is implemented.
const PARSER_ACTIVE: bool = false;

// ---------------------------------------------------------------------------
// CSV decoding.
//
// The extension's CSV is built with `window.encodeURIComponent(";")` as the
// row delimiter. Depending on the browser and OS, the saved file may still be
// URL-encoded (delimiter = `%3B`) or may have been decoded by the download
// mechanism (delimiter = `;`). We detect and handle both.

/// Decode the raw file body into plain text with literal `;` delimiters.
///
/// Detects URL-encoding by the presence of `%3B` in the first 512 bytes.
/// When encoded, applies a full `percent_decode` over the entire body so that
/// field values (e.g. `%22artist+name%22` → `"artist name"`) are also clean.
fn decode_body(raw: &str) -> String {
    let probe = raw.get(..raw.len().min(512)).unwrap_or(raw);
    if probe.contains("%3B") || probe.contains("%3b") {
        // URL-encoded — decode the whole body first.
        percent_decode(raw)
    } else {
        raw.to_string()
    }
}

/// Minimal percent-decode: replaces `%XX` escape sequences with the
/// corresponding byte. Non-UTF-8 sequences are replaced with `?`.
///
/// NOTE: We do NOT treat `+` as a space here. The extension builds its CSV
/// with `window.encodeURIComponent`, which is NOT `application/x-www-form-urlencoded`:
/// it encodes spaces as `%20` (never `+`) and encodes literal `+` as `%2B`.
/// A blanket `+`→space replacement would therefore only ever corrupt genuine `+`
/// characters (e.g. Gmail-alias payer emails like `user+bandcamp@gmail.com`,
/// or band names like `+/-`) after they were correctly decoded from `%2B`.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_nibble(bytes[i + 1]);
            let lo = hex_nibble(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Raw row.

/// A raw row written unconditionally to `raw/YYYY-MM.jsonl`.
#[derive(serde::Serialize)]
struct RawRow {
    /// RFC3339 local timestamp used for month-partitioning.
    _raw_ts: String,
    /// Content hash (djb2 over sorted field key=value pairs) — the dedupe key.
    _raw_guid: String,
    /// Verbatim CSV field values (field name → string value).
    #[serde(flatten)]
    fields: Map<String, Value>,
}

/// Build one raw row from a header record + a data record.
fn csv_row_to_raw(
    headers: &csv::StringRecord,
    record: &csv::StringRecord,
) -> Map<String, Value> {
    let mut obj = Map::new();
    for (h, v) in headers.iter().zip(record.iter()) {
        // The csv reader is already configured with Trim::All and quoting(true),
        // so values arrive with surrounding whitespace and quote chars stripped.
        // We keep trim_matches('"') only as a defensive guard for any residual
        // literal quotes that slipped through (e.g. unquoted %22-encoded headers
        // in the still-encoded path that were decoded but not re-parsed by csv).
        // We intentionally do NOT call .trim() here — Trim::All already handles
        // whitespace and a bare .trim() would silently strip meaningful interior
        // leading/trailing whitespace, violating raw-layer fidelity.
        let key = h.trim_matches('"').to_string();
        let val = v.trim_matches('"').to_string();
        if !key.is_empty() {
            obj.insert(key, Value::String(val));
        }
    }
    // Derive a partition timestamp from `payment_date` if parseable.
    let ts = guess_payment_ts(&obj).unwrap_or_else(|| Local::now().to_rfc3339());
    obj.insert("_raw_ts".to_string(), Value::String(ts));

    // Lightweight content hash over original fields (deterministic, sorted).
    let mut parts: Vec<(&str, &str)> = obj
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| (k.as_str(), v.as_str().unwrap_or("")))
        .collect();
    parts.sort_by_key(|(k, _)| *k);
    let hash_input = parts
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("|");
    let guid = format!("{:x}", djb2(&hash_input));
    obj.insert("_raw_guid".to_string(), Value::String(guid));

    obj
}

/// Attempt to derive a local RFC3339 timestamp from the `payment_date` field.
///
/// The actual format is not confirmed (Needs-sample). We try the most likely
/// patterns used by Bandcamp's API. Falls back to `None` on any parse failure.
fn guess_payment_ts(obj: &Map<String, Value>) -> Option<String> {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone};

    let raw = obj.get("payment_date")?.as_str()?.trim().to_string();
    if raw.is_empty() || raw == "-" {
        return None;
    }

    // Try ISO 8601 datetime first (most likely from the API).
    let patterns_dt: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%SZ",
        "%Y-%m-%d %H:%M:%SZ",
    ];
    for pat in patterns_dt {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&raw, pat) {
            return Local
                .from_local_datetime(&dt)
                .earliest()
                .map(|d| d.to_rfc3339());
        }
    }

    // Try date-only patterns.
    let patterns_d: &[&str] = &["%Y-%m-%d", "%m/%d/%Y", "%d-%b-%Y", "%d/%m/%Y"];
    for pat in patterns_d {
        if let Ok(d) = NaiveDate::parse_from_str(&raw, pat) {
            return Local
                .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
                .earliest()
                .map(|dt| dt.to_rfc3339());
        }
    }

    None
}

/// djb2-inspired hash: a lightweight non-cryptographic content key.
fn djb2(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_shl(5).wrapping_add(h).wrapping_add(b as u64);
    }
    h
}

// ---------------------------------------------------------------------------
// Contract layer (parked — PARSER_ACTIVE = false until a sample lands).
//
// When PARSER_ACTIVE flips to true:
//   - Confirm the `payment_date` value format against a real export.
//   - Confirm `unit_price` is a bare decimal (e.g. `19.99`) or locale-formatted.
//   - Map `bandcamp_id` → guid (stable per-purchase id; prefer it over the
//     content hash once confirmed to be truly stable).
//   - Map `unit_price` → `amount` (f64), `currency` → `currency`.
//   - Map `artist_name` → `merchant`, `item_title` → `item`.
//   - `item_url` → `url`; `download_url` → extra.
//   - `card_brand`, `card_num`, `payer_email`, `tax`, `tax_type` → `extra`.

#[allow(dead_code)]
fn try_map_to_line_item(_raw: &Map<String, Value>) -> Option<LineItem> {
    if !PARSER_ACTIVE {
        return None;
    }
    // TODO (Needs-sample): implement when value-format is confirmed.
    // Mapping sketch (unverified against a real export):
    //   guid     = _raw["bandcamp_id"] (confirm it is stable across exports)
    //   ts       = parse payment_date (format TBD)
    //   merchant = _raw["artist_name"]
    //   item     = _raw["item_title"]
    //   amount   = parse _raw["unit_price"] as f64
    //   currency = _raw["currency"]
    //   url      = _raw["item_url"]
    //   extra    = {bandcamp_id, card_brand, card_num, payer_email, tax, tax_type, download_url}
    None
}

// ---------------------------------------------------------------------------
// Core import engine.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "csv" {
        anyhow::bail!(
            "Unsupported file type: .{ext}. \
             Please import the CSV downloaded from the Bandcamp purchase-history extension."
        );
    }

    let raw_body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    // Strip the `data:text/csv;charset=utf-8,` data-URI prefix if present
    // (some browser/OS combinations include it in the saved file).
    let body_str = if let Some(rest) = raw_body.trim_start().strip_prefix("data:") {
        // Find the comma that separates the header from the data.
        rest.find(',').map(|i| &rest[i + 1..]).unwrap_or(raw_body.as_str())
    } else {
        raw_body.as_str()
    };

    let body = decode_body(body_str);

    // Build seen-guid set from existing raw rows (idempotent re-import).
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let contract_stream = vault.stream(DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in raw_stream.partitions()? {
        for row in raw_stream.read::<Value>(&key)? {
            if let Some(g) = row.get("_raw_guid").and_then(Value::as_str) {
                seen.insert(g.to_string());
            }
        }
    }

    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(b';')
        .quoting(true)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(body.as_bytes());

    let headers = rdr
        .headers()
        .context("bandcamp CSV: failed to read header row")?
        .clone();

    let (mut rows, mut imported_raw, mut duplicates, mut skipped) = (0u64, 0u64, 0u64, 0u64);
    let mut raw_buf: Vec<RawRow> = Vec::new();
    let mut contract_buf: Vec<LineItem> = Vec::new();

    for result in rdr.records() {
        rows += 1;
        let record = match result {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let raw_map = csv_row_to_raw(&headers, &record);
        let guid = raw_map
            .get("_raw_guid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let ts = raw_map
            .get("_raw_ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if guid.is_empty() {
            skipped += 1;
            continue;
        }
        if !seen.insert(guid.clone()) {
            duplicates += 1;
            continue;
        }

        let fields: Map<String, Value> = raw_map
            .into_iter()
            .filter(|(k, _)| k != "_raw_guid" && k != "_raw_ts")
            .collect();

        if PARSER_ACTIVE {
            if let Some(li) = try_map_to_line_item(&fields) {
                contract_buf.push(li);
            }
        }

        raw_buf.push(RawRow { _raw_ts: ts, _raw_guid: guid, fields });
        imported_raw += 1;

        if rows % 100 == 0 {
            progress(ImportProgress { records: imported_raw, percent: 0.0 });
        }
    }

    raw_stream.append(&raw_buf, |r| &r._raw_ts)?;
    if PARSER_ACTIVE && !contract_buf.is_empty() {
        contract_stream.append(&contract_buf, |li| &li.ts)?;
    }

    progress(ImportProgress { records: imported_raw, percent: 100.0 });

    let headline = if PARSER_ACTIVE {
        format!(
            "{imported_raw} purchases imported, {duplicates} duplicates skipped \
             — contract rows written to finance/purchases/bandcamp/"
        )
    } else {
        format!(
            "{imported_raw} purchases imported to raw layer, {duplicates} duplicates skipped \
             (contract parser parked — Needs-sample: see docs/integrations/bandcamp.md)"
        )
    };

    Ok(ImportOutcome {
        headline,
        counts: [
            ("rows", rows),
            ("imported_raw", imported_raw),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(RAW_DIR))
}

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bandcamp",
        name: "Bandcamp",
        kind: IntegrationKind::Import,
        // Financial detail — opt-in with explicit acknowledgement.
        default_on: false,
        description: "Import your Bandcamp purchase history — every album and track you own, \
                      with artist, price, and date paid. Because Bandcamp has no official buyer \
                      export, this uses a community Chrome extension to scrape your \
                      bandcamp.com/purchases page; Trove never touches your Bandcamp login. \
                      Re-runnable: re-importing a fresh export is a safe no-op.",
        domain: "finance",
        vault_path: "finance/purchases/bandcamp/",
        toggleable: false,
        setup: &[
            "Install the \"Bandcamp Purchase History\" Chrome extension from the Chrome Web Store \
             (search for it, or find it at github.com/rxdazn/bandcamp-purchase-history).",
            "Log in to bandcamp.com in Chrome, then go to bandcamp.com/purchases.",
            "Click the extension icon → Load purchases history → Download CSV.",
            "Drop the downloaded CSV file here.",
        ],
        caveats: "The CSV is produced by a community extension that scrapes live DOM — \
                  it may break if Bandcamp changes its page structure. The raw CSV is stored \
                  in the vault at full fidelity so past imports survive any future format change. \
                  The contract-layer parser is parked pending a real sample to confirm exact \
                  value formats (payment_date pattern, price format); raw rows are stored immediately.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-bandcamp-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // SCAFFOLD FIXTURES — column names confirmed from extension source.
    // VALUE FORMATS are NOT confirmed against a real export (Needs-sample).
    // These fixtures use plausible values for structural testing only.

    /// Semicolon-delimited CSV matching the confirmed column headers.
    /// Values are unquoted plain text (browser-decoded variant).
    const PLAIN_CSV: &str = "\
payment_date;bandcamp_id;artist_name;item_title;quantity;unit_price;tax;tax_type;currency;card_brand;card_num;payer_email;item_url;download_url
2024-03-15;bc-111111;Floating Points;Elaenia;1;15.00;0.00;none;USD;Visa;1234;user@example.com;https://floatingpoints.bandcamp.com/album/elaenia;https://bandcamp.com/download/album/bc-111111
2023-11-20;bc-222222;Four Tet;There Is Love In You;1;12.00;0.00;none;USD;Visa;1234;user@example.com;https://fourtet.bandcamp.com/album/there-is-love-in-you;https://bandcamp.com/download/album/bc-222222
";

    /// URL-encoded variant (what the extension actually generates via data URI).
    ///
    /// Matches what popup.js produces:
    ///   - Headers are BARE (no %22 wrapping) joined by `%3B`.
    ///   - Data-cell values are `%22value%22`-wrapped, joined by `%3B`.
    ///   - Spaces encode as `%20`; literal `+` encodes as `%2B`.
    ///
    /// The row with `user%2Bbandcamp%40gmail.com` and `%2B%2F-` exercises the
    /// regression: a `+` that was correctly decoded from `%2B` must survive
    /// into the raw row unchanged (must not become a space).
    fn url_encoded_csv() -> String {
        // Header: bare names joined by %3B (no %22 wrapping).
        let header = [
            "payment_date", "bandcamp_id", "artist_name", "item_title",
            "quantity", "unit_price", "tax", "tax_type", "currency",
            "card_brand", "card_num", "payer_email", "item_url", "download_url",
        ]
        .join("%3B");

        // Row 1: normal purchase (spaces as %20).
        let row1_vals = [
            "2024-03-15", "bc-111111", "Floating%20Points", "Elaenia",
            "1", "15.00", "0.00", "none", "USD", "Visa", "1234",
            "user%40example.com",
            "https%3A%2F%2Ffloatingpoints.bandcamp.com%2Falbum%2Felaenia",
            "https%3A%2F%2Fbandcamp.com%2Fdownload%2Falbum%2Fbc-111111",
        ]
        .iter()
        .map(|v| format!("%22{v}%22"))
        .collect::<Vec<_>>()
        .join("%3B");

        // Row 2: regression fixture — payer_email is a Gmail alias with literal '+',
        // and artist_name is '+/-'. Both '+' chars encode as %2B (not space).
        // After decoding, the raw layer must contain '+' not ' '.
        let row2_vals = [
            "2023-11-20", "bc-222222",
            "%2B%2F-",      // artist_name = "+/-"
            "Hyperdub%2010th%20Anniversary",
            "1", "12.00", "0.00", "none", "GBP", "Visa", "5678",
            "user%2Bbandcamp%40gmail.com",  // payer_email = "user+bandcamp@gmail.com"
            "https%3A%2F%2Fplusminus.bandcamp.com%2Falbum%2Fhyperdub",
            "https%3A%2F%2Fbandcamp.com%2Fdownload%2Falbum%2Fbc-222222",
        ]
        .iter()
        .map(|v| format!("%22{v}%22"))
        .collect::<Vec<_>>()
        .join("%3B");

        format!("{header}\n{row1_vals}\n{row2_vals}\n")
    }

    fn import_csv(v: &Vault, body: &str) -> ImportOutcome {
        let path = v.root().join("purchases.csv");
        fs::write(&path, body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn plain_semicolon_csv_imports_to_raw_layer() {
        let v = temp_vault("plain");
        let out = import_csv(&v, PLAIN_CSV);

        assert!(
            out.headline.contains("2 purchases imported"),
            "unexpected headline: {}",
            out.headline
        );
        assert_eq!(*out.counts.get("imported_raw").unwrap(), 2);
        assert_eq!(*out.counts.get("duplicates").unwrap(), 0);

        // Raw rows land in finance/purchases/bandcamp/raw/.
        let raw_dir = v.root().join("finance/purchases/bandcamp/raw");
        assert!(raw_dir.exists(), "raw dir created");
        let files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .collect();
        assert!(!files.is_empty(), "at least one raw JSONL partition written");

        // Verify raw row structure.
        let raw_body = fs::read_to_string(files[0].path()).unwrap();
        let first_line: Value = serde_json::from_str(raw_body.lines().next().unwrap()).unwrap();
        assert!(first_line.get("_raw_guid").is_some(), "_raw_guid present");
        assert!(first_line.get("_raw_ts").is_some(), "_raw_ts present");
        // Known field names must be present.
        assert!(first_line.get("bandcamp_id").is_some(), "bandcamp_id present");
        assert!(first_line.get("artist_name").is_some(), "artist_name present");
        assert!(first_line.get("item_title").is_some(), "item_title present");
    }

    #[test]
    fn url_encoded_csv_decodes_and_imports() {
        let v = temp_vault("urlenc");
        let encoded = url_encoded_csv();
        let out = import_csv(&v, &encoded);

        // Should detect encoding, decode both rows.
        assert_eq!(
            *out.counts.get("imported_raw").unwrap(), 2,
            "url-encoded variant parsed 2 rows: {}",
            out.headline
        );

        // Collect all raw JSONL lines across all partition files.
        let raw_dir = v.root().join("finance/purchases/bandcamp/raw");
        let files: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        let raw_body: String = files
            .iter()
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();

        // Row 1: artist name with space decoded from %20 must appear.
        assert!(
            raw_body.contains("Floating Points"),
            "artist name (space from %20) decoded in raw row: {raw_body}"
        );

        // No leftover URL-encoding.
        assert!(
            !raw_body.contains("%22"),
            "no leftover %22 URL-encoding in raw row: {raw_body}"
        );

        // Regression: '+' that came from %2B must survive as '+', not become a space.
        // Row 2 has artist_name "+/-" and payer_email "user+bandcamp@gmail.com".
        assert!(
            raw_body.contains("+/-"),
            "artist name '+/-' (from %2B%2F-) must survive as '+/-', not ' /-': {raw_body}"
        );
        assert!(
            raw_body.contains("user+bandcamp@gmail.com"),
            "payer_email 'user+bandcamp@gmail.com' (from %2B) must survive with '+': {raw_body}"
        );
        // Also confirm a space-in-name was NOT turned into '+' -> space confusion.
        assert!(
            !raw_body.contains("user bandcamp"),
            "'+' must NOT have been decoded as space: {raw_body}"
        );
    }

    #[test]
    fn rejects_non_csv() {
        let v = temp_vault("reject");
        let path = v.root().join("export.zip");
        fs::write(&path, b"PK\x03\x04").unwrap();
        let result = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {});
        assert!(result.is_err(), "non-CSV rejected");
    }

    #[test]
    fn re_import_is_idempotent() {
        let v = temp_vault("rerun");
        let out1 = import_csv(&v, PLAIN_CSV);
        assert_eq!(*out1.counts.get("imported_raw").unwrap(), 2);

        let out2 = import_csv(&v, PLAIN_CSV);
        assert_eq!(*out2.counts.get("imported_raw").unwrap(), 0, "re-import: no new rows");
        assert_eq!(*out2.counts.get("duplicates").unwrap(), 2, "re-import: all rows are duplicates");

        // File content unchanged.
        let raw_dir = v.root().join("finance/purchases/bandcamp/raw");
        let count_before: usize = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .map(|e| fs::read_to_string(e.path()).unwrap().lines().count())
            .sum();
        let out3 = import_csv(&v, PLAIN_CSV);
        let count_after: usize = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .map(|e| fs::read_to_string(e.path()).unwrap().lines().count())
            .sum();
        assert_eq!(count_before, count_after, "no rows appended on third import");
        assert_eq!(*out3.counts.get("duplicates").unwrap(), 2);
    }

    #[test]
    fn data_uri_prefix_stripped() {
        let v = temp_vault("datauri");
        let with_prefix = format!(
            "data:text/csv;charset=utf-8,{}",
            url_encoded_csv()
        );
        let path = v.root().join("purchases.csv");
        fs::write(&path, &with_prefix).unwrap();
        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert!(
            *out.counts.get("imported_raw").unwrap() >= 1,
            "data URI prefix stripped: {}",
            out.headline
        );
    }

    #[test]
    fn empty_csv_is_a_no_op() {
        let v = temp_vault("empty");
        let out = import_csv(&v, "payment_date;bandcamp_id;artist_name;item_title;quantity;unit_price;tax;tax_type;currency;card_brand;card_num;payer_email;item_url;download_url\n");
        assert_eq!(*out.counts.get("imported_raw").unwrap(), 0);
        assert_eq!(*out.counts.get("rows").unwrap(), 0);
    }

    #[test]
    fn percent_decode_handles_plus_and_encoded_chars() {
        // URL-encoded body (has %3B) → %3B decoded to `;`.
        assert_eq!(decode_body("a%3Bb"), "a;b");
        // %22 in a URL-encoded body → decoded to double-quote.
        assert_eq!(decode_body("%22quoted%22%3Bend"), "\"quoted\";end");
        // %20 decodes to space (encodeURIComponent space encoding).
        assert_eq!(decode_body("hello%20world%3Bend"), "hello world;end");
        // '+' in a URL-encoded body (came from %2B) MUST stay as '+', NOT become space.
        // The extension uses encodeURIComponent (not form-encoding), so '+' is never
        // a space placeholder — it always means a literal '+' character.
        assert_eq!(decode_body("user%2Bbandcamp%40gmail.com%3Bend"),
                   "user+bandcamp@gmail.com;end",
                   "'+' from %2B must survive as '+', not become a space");
        // A literal '+' in a URL-encoded body (no %3B means plain path — pass through).
        // Even in the decoded path, a bare '+' that wasn't encoded as %2B must survive.
        assert_eq!(decode_body("%2B%2F-%3Bend"), "+/-;end",
                   "'+/-' band name (from %2B%2F-) must decode to '+/-'");
        // Plain text (no %3B) → passed through unchanged.
        assert_eq!(decode_body("hello;world"), "hello;world");
        assert_eq!(decode_body("plain+text"), "plain+text");
    }

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "bandcamp").expect("bandcamp card");
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["csv"]);
        assert!(!card.enabled, "opt-in: default_on=false means not enabled by default");
    }
}

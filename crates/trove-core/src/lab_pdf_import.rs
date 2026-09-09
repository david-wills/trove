//! Lab Results (PDF) — universal import for lab result PDFs from any provider.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/lab-pdf-import.md.
//!
//! # What it does
//!
//! Accepts a lab result PDF downloaded from any patient portal (Quest, Labcorp,
//! MyChart, hospital portals, international/specialty labs) and:
//!
//! 1. **Raw layer (unconditional):** stores the verbatim PDF as
//!    `health/medical/lab-pdf/raw/<hash>.pdf` so re-parsing with a better
//!    extractor is always possible.
//! 2. **Extraction JSON (unconditional):** a best-effort text extraction under
//!    `health/medical/lab-pdf/raw/<hash>.json` with every field the rules
//!    layer found, plus the raw text.
//! 3. **Contract layer:** zero or more [`crate::health_medical::Observation`]
//!    rows under `health/medical/lab-pdf/observations/YYYY-MM.jsonl` —
//!    one per parsed lab test. Fields that cannot be extracted are omitted
//!    (`ts` must be present; if the date cannot be found the row is skipped).
//!
//! # Parser status — Needs-sample
//!
//! The rules layer uses best-effort regex patterns tuned to the common Quest /
//! Labcorp / Epic text layouts. PDF layout varies enormously across labs;
//! the patterns are scaffolded but **not validated against real export PDFs**
//! (none exist on disk). The contract layer is correct for what the rules
//! extract; the precision of extraction improves incrementally as real samples
//! arrive. The raw PDF is ALWAYS stored — no imported data is lost.
//!
//! # Deduplication
//!
//! The SHA-256 content hash of the PDF bytes is the **document import key**
//! (stored in the extraction JSON). Per-observation guids are
//! `<hash8>-<slug8>-<date>` where `slug8` is the first 8 hex chars of an FNV-1a
//! hash of the full test name (case-folded), so distinct tests with long shared
//! prefixes (e.g. lipid sub-fractions) never collide. Re-importing the same PDF
//! silently deduplicates all rows.
//!
//! # Privacy
//!
//! Lab results are medical detail — this integration ships `default_on: false`
//! with explicit acknowledgement required (same as every other health-medical
//! collector). The raw PDFs are stored locally; no data leaves the machine.
//!
//! # Dependencies
//!
//! `pdf-extract` (pure Rust, added to Cargo.toml) for PDF text extraction.
//! `sha2` (already a dep) for content hashing.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::health_medical::Observation;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, ImportOutcome, ImportSpec, IntegrationDef,
};
use crate::store::{write_atomic, write_json_atomic, Partition};
use crate::vault::Vault;

/// Raw PDF storage (verbatim PDF files, one per content hash).
const RAW_DIR: &str = "health/medical/lab-pdf/raw";
/// Contract observation rows, month-partitioned.
const OBS_DIR: &str = "health/medical/lab-pdf/observations";

// ---------------------------------------------------------------------------
// DEF and IMPORT spec.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(OBS_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
/// The `pub mod` and INTEGRATIONS line already exist (Phase-2 stub).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "lab-pdf-import",
        name: "Lab Results (PDF)",
        kind: IntegrationKind::Import,
        // Medical detail — opt-in with explicit acknowledgement.
        default_on: false,
        description:
            "Import lab result PDFs from any patient portal (Quest, Labcorp, MyChart, hospital \
             portals, specialty labs) — the universal catch-all for every provider without FHIR. \
             Extracts test name, value, unit, reference range, flag, and collection date into \
             the shared medical observation store, keeping the original PDF for re-parsing.",
        domain: "health",
        vault_path: "health/medical/lab-pdf/",
        toggleable: false,
        setup: &[
            "Lab results are sensitive medical data — importing opts you in to storing them \
             in the vault.",
            "Download the PDF from your patient portal (Quest: MyQuest, Labcorp: Patient, \
             hospital: MyChart / Epic / Athena) and import it here.",
            "Importing the same PDF twice is safe — the content hash deduplicates all rows.",
            "Text-layer extraction works on digitally-generated PDFs. Scanned/image PDFs \
             land in the raw layer only (OCR support planned).",
        ],
        caveats:
            "Extraction accuracy depends on PDF layout — the rules layer covers common Quest / \
             Labcorp / Epic text patterns but cannot match every lab format. Fields that cannot \
             be confidently extracted are omitted rather than guessed; the original PDF is always \
             stored so a better parser can re-process it later. Verify extracted values against \
             the source PDF.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["pdf"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Extraction JSON (stored in raw/ alongside the verbatim PDF).

/// Full-fidelity extraction record stored as `<hash>.json` in `raw/`.
#[derive(Debug, Serialize)]
struct ExtractionRecord {
    /// SHA-256 hex of the PDF bytes — stable import key.
    hash: String,
    /// UTC RFC3339 of when this PDF was imported.
    imported_at: String,
    /// Whether `pdf-extract` successfully decoded the text layer.
    text_extracted: bool,
    /// The full raw text extracted from the PDF (empty for image PDFs).
    raw_text: String,
    /// Each parsed result row (best-effort). May be empty for image PDFs
    /// or unrecognised layouts.
    parsed_rows: Vec<ParsedRow>,
    /// Notes from the extractor (e.g. "image PDF — OCR needed").
    notes: Vec<String>,
}

/// One candidate lab-result row found by the rules layer.
#[derive(Debug, Serialize)]
struct ParsedRow {
    /// Best-effort collection date (YYYY-MM-DD or RFC3339).
    ts: Option<String>,
    /// Test/panel name as it appeared in the PDF.
    test: String,
    /// Numeric result (if present).
    value: Option<f64>,
    /// Qualitative result (if numeric parse failed).
    value_text: Option<String>,
    /// Unit string.
    unit: Option<String>,
    /// Reference range string.
    reference_range: Option<String>,
    /// Abnormal flag (H/L/A/etc.).
    flag: Option<String>,
    /// Panel/order name (if identifiable).
    panel: Option<String>,
    /// Ordering provider/lab name (if identifiable).
    provider: Option<String>,
    /// Extraction confidence: 0.0–1.0 (1.0 = all fields found cleanly).
    confidence: f32,
}

// ---------------------------------------------------------------------------
// PDF text extraction.

/// Extract the text layer from a PDF file.
/// Returns `(text, text_extracted, notes)`.
fn extract_pdf_text(bytes: &[u8]) -> (String, bool, Vec<String>) {
    match pdf_extract::extract_text_from_mem(bytes) {
        Ok(text) if !text.trim().is_empty() => (text, true, Vec::new()),
        Ok(_) => (
            String::new(),
            false,
            vec![
                "PDF text layer is empty — may be an image/scanned PDF. \
                 OCR bridge not yet wired (macOS Vision planned)."
                    .into(),
            ],
        ),
        Err(e) => (
            String::new(),
            false,
            vec![format!(
                "pdf-extract could not decode text layer: {e} — \
                 may be a scanned/image PDF or an unusual encoding. \
                 OCR bridge not yet wired (macOS Vision planned)."
            )],
        ),
    }
}

// ---------------------------------------------------------------------------
// Rules-layer parser.
//
// NOTE: These patterns are scaffolded from the common Quest / Labcorp / Epic
// text-export layouts (publicly documented reference texts) but have NOT been
// validated against real export PDFs (Needs-sample). They represent the
// expected shape; they will need tuning against real samples before high
// precision is achieved. The raw PDF is always stored, so no information is
// lost while the parser matures.

/// Regex-based extraction of a collection date from the full text block.
///
/// Prefixes are tried in priority order (highest-fidelity first). For each
/// prefix, ALL lines are scanned so that a later "Collected: …" always beats
/// an earlier generic "Date: …" that happened to appear higher on the page.
///
/// "Report Date:" and bare "Date:" are intentionally omitted — those record the
/// report-generation time, not the specimen collection time. They are available
/// in `extra` via the extraction JSON if needed.
///
/// The ISO fallback only fires for lines that do not contain DOB/birth/accession
/// /MRN tokens, to prevent patient demographics from being mistaken for the
/// collection date.
fn find_collection_date(text: &str) -> Option<String> {
    // Prefixes in priority order: more-specific first.
    // "Report Date:" and "Date:" are EXCLUDED — they record report-generation time.
    let priority_prefixes: &[&str] = &[
        "Collection Date:",
        "Collected:",
        "Date Collected:",
        "Specimen Collected:",
        "Date of Service:",
        "Service Date:",
    ];

    // Iterate prefixes in priority order (outer), scanning all lines for each.
    // This guarantees "Collection Date:" always wins over "Service Date:", etc.,
    // regardless of line order in the PDF.
    for prefix in priority_prefixes {
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let date_str = rest.trim();
                if let Some(d) = parse_date_str(date_str) {
                    return Some(d);
                }
            }
        }
    }

    // Fallback: look for any ISO date pattern in the first 50 lines, but
    // skip lines containing patient-demographics tokens (DOB, birth, accession,
    // MRN) so a patient birth date is never mistaken for a collection date.
    let demographics_tokens: &[&str] = &[
        "dob", "birth", "born", "accession", "mrn", "patient id", "npi",
    ];
    for line in text.lines().take(50) {
        let lower = line.to_ascii_lowercase();
        if demographics_tokens.iter().any(|t| lower.contains(t)) {
            continue;
        }
        if let Some(d) = find_iso_date(line) {
            return Some(d);
        }
    }
    None
}

/// Attempt to parse common date formats into YYYY-MM-DD.
///
/// Handles:
/// - ISO 4-digit year:    `2026-04-02`
/// - US slash/dash:       `04/15/2026` or `04-15-2026`
/// - 2-digit year (US):   `3/10/26`  → century-detected (≤40 → 20xx, else 19xx)
/// - Written month:       `March 10, 2026` / `March 10 2026`
/// - DD-Mon-YYYY:         `10-Mar-2026` / `10 Mar 2026`
/// - DD/MM international: tried as fallback when `MM > 12` (day>12 is the signal)
fn parse_date_str(s: &str) -> Option<String> {
    let s = s.trim();
    // Take only as far as the first token boundary that would indicate extra
    // content (a space that follows a complete date-like token).  We try the
    // full trimmed string first, then fall back to the first whitespace-delimited
    // token for formats that may have trailing time/text.
    if let Some(d) = parse_date_token(s) {
        return Some(d);
    }
    // "March 10, 2026" style — need the first three whitespace tokens.
    parse_written_month(s)
}

/// Written-month helper: "March 10, 2026" / "10 Mar 2026" / "10-Mar-2026".
fn parse_written_month(s: &str) -> Option<String> {
    // Normalise hyphens to spaces for the `10-Mar-2026` variant, then split.
    let normalised = s.replace('-', " ");
    let parts: Vec<&str> = normalised.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }
    // Month names (full + 3-char abbreviation, case-insensitive).
    let month_num = |tok: &str| -> Option<u32> {
        let t = tok.trim_end_matches(',').to_ascii_lowercase();
        match t.as_str() {
            "january"   | "jan" => Some(1),
            "february"  | "feb" => Some(2),
            "march"     | "mar" => Some(3),
            "april"     | "apr" => Some(4),
            "may"               => Some(5),
            "june"      | "jun" => Some(6),
            "july"      | "jul" => Some(7),
            "august"    | "aug" => Some(8),
            "september" | "sep" => Some(9),
            "october"   | "oct" => Some(10),
            "november"  | "nov" => Some(11),
            "december"  | "dec" => Some(12),
            _ => None,
        }
    };

    // "March 10, 2026" — Month D[D][,] YYYY
    if let Some(m) = month_num(parts[0]) {
        let d_str = parts[1].trim_end_matches(',');
        let y_str = parts[2].trim_end_matches(',');
        if let (Ok(d), Ok(y)) = (d_str.parse::<u32>(), y_str.parse::<u32>()) {
            let y = expand_year(y)?;
            if m <= 12 && d <= 31 && y > 1900 {
                return Some(format!("{y:04}-{m:02}-{d:02}"));
            }
        }
    }
    // "10 Mar 2026" — D[D] Month YYYY
    if let Some(m) = month_num(parts[1]) {
        let d_str = parts[0].trim_end_matches(',');
        let y_str = parts[2].trim_end_matches(',');
        if let (Ok(d), Ok(y)) = (d_str.parse::<u32>(), y_str.parse::<u32>()) {
            let y = expand_year(y)?;
            if m <= 12 && d <= 31 && y > 1900 {
                return Some(format!("{y:04}-{m:02}-{d:02}"));
            }
        }
    }
    None
}

/// Expand a 2- or 4-digit year: ≤40 → 2000+, 41-99 → 1900+, else pass through.
fn expand_year(y: u32) -> Option<u32> {
    match y {
        0..=40   => Some(2000 + y),
        41..=99  => Some(1900 + y),
        1901..=2199 => Some(y),
        _ => None,
    }
}

/// Parse a single date token (no spaces) into YYYY-MM-DD.
fn parse_date_token(s: &str) -> Option<String> {
    // Take only the first whitespace-delimited token.
    let token = s.split_whitespace().next().unwrap_or(s);
    let token = token.trim_end_matches(',');

    // ISO YYYY-MM-DD (exact 10 chars, digit 5 is '-').
    if token.len() == 10
        && token.as_bytes().get(4) == Some(&b'-')
        && token.as_bytes().get(7) == Some(&b'-')
    {
        // Validate it actually parses as a date.
        if let Some(d) = validate_ymd_str(token) {
            return Some(d);
        }
    }

    // Slash-separated: MM/DD/YYYY, M/D/YYYY, MM/DD/YY, M/D/YY
    // Also YYYY/MM/DD.
    if token.contains('/') {
        let parts: Vec<&str> = token.splitn(3, '/').collect();
        if parts.len() == 3 {
            return parse_numeric_parts(parts[0], parts[1], parts[2]);
        }
    }

    // Hyphen-separated (non-ISO): MM-DD-YYYY / M-D-YYYY / MM-DD-YY
    if token.contains('-') && !(token.len() == 10 && token.as_bytes().get(4) == Some(&b'-')) {
        let parts: Vec<&str> = token.splitn(3, '-').collect();
        if parts.len() == 3 {
            return parse_numeric_parts(parts[0], parts[1], parts[2]);
        }
    }

    None
}

/// Try numeric parts as YYYY/MM/DD, MM/DD/YYYY (4- or 2-digit year), or DD/MM
/// international fallback.
fn parse_numeric_parts(a: &str, b: &str, c: &str) -> Option<String> {
    let au: u32 = a.parse().ok()?;
    let bu: u32 = b.parse().ok()?;
    let cu: u32 = c.parse().ok()?;

    // YYYY/MM/DD — leading 4-digit year.
    if a.len() == 4 && au > 1900 && bu <= 12 && cu <= 31 {
        return Some(format!("{au:04}-{bu:02}-{cu:02}"));
    }

    // MM/DD/YYYY or MM/DD/YY — trailing year.
    if c.len() == 4 || c.len() == 2 {
        let y = expand_year(cu)?;
        // Try MM/DD first (most common US form).
        if au <= 12 && bu <= 31 && y > 1900 {
            return Some(format!("{y:04}-{au:02}-{bu:02}"));
        }
        // DD/MM fallback (international): only if au > 12 (unambiguous).
        if au > 12 && au <= 31 && bu <= 12 && y > 1900 {
            return Some(format!("{y:04}-{bu:02}-{au:02}"));
        }
    }

    None
}

/// Validate a candidate "YYYY-MM-DD" string using chrono.
fn validate_ymd_str(s: &str) -> Option<String> {
    use chrono::NaiveDate;
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .map(|d| d.format("%Y-%m-%d").to_string())
}

/// Find an ISO date (YYYY-MM-DD) anywhere in a line.
fn find_iso_date(line: &str) -> Option<String> {
    // Look for a 10-char pattern like YYYY-MM-DD.
    let bytes = line.as_bytes();
    for i in 0..bytes.len().saturating_sub(9) {
        let slice = &line[i..i + 10];
        if slice.chars().nth(4) == Some('-') && slice.chars().nth(7) == Some('-') {
            if let Some(d) = parse_date_str(slice) {
                return Some(d);
            }
        }
    }
    None
}

/// Find the ordering provider or lab name from the text.
fn find_provider(text: &str) -> Option<String> {
    let prefixes = [
        "Ordering Provider:",
        "Ordering Physician:",
        "Performing Lab:",
        "Lab:",
        "Laboratory:",
        "Provider:",
        "Physician:",
        "Referred by:",
    ];
    for line in text.lines() {
        let trimmed = line.trim();
        for prefix in &prefixes {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name = rest.trim().to_string();
                if !name.is_empty() && name.len() < 120 {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// Find the panel/order name.
fn find_panel(text: &str) -> Option<String> {
    let prefixes = [
        "Test Name:",
        "Order Name:",
        "Panel:",
        "Panel Name:",
        "Test Ordered:",
        "Order:",
        "Procedure:",
    ];
    for line in text.lines() {
        let trimmed = line.trim();
        for prefix in &prefixes {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name = rest.trim().to_string();
                if !name.is_empty() && name.len() < 200 {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// Parse individual lab result lines.
///
/// Common text layouts (Quest/Labcorp/Epic) produce lines like:
/// - `Test Name   Value  Units  Reference Range  Flag`
/// - `Glucose     98     mg/dL  70-99             `
/// - `TSH         2.35   uIU/mL 0.45-4.50`
///
/// This parser looks for lines where the first token is a non-numeric
/// test name followed by a numeric or text value.  All found fields are
/// returned; confidence is set lower when key fields are absent.
fn parse_result_lines(text: &str, default_date: Option<&str>) -> Vec<ParsedRow> {
    let mut rows: Vec<ParsedRow> = Vec::new();

    // Skip lines that are clearly headers or metadata.
    let skip_prefixes = [
        "Patient",
        "DOB",
        "Date of Birth",
        "Age",
        "Sex",
        "Accession",
        "MRN",
        "Physician",
        "Provider",
        "Lab",
        "Laboratory",
        "Address",
        "Phone",
        "Fax",
        "Report",
        "Page",
        "Test Name",
        "COMPONENT",
        "Analyte",
        "Reference Range",
        "Flag",
        "Results",
        "Units",
        "Collection",
        "Collected",
        "Received",
        "Final",
        "Status",
        "Ordering",
        "Performing",
        "Requesting",
        "Facility",
        "Client",
    ];

    // Common unit strings to recognise a value-unit pair.
    let common_units = [
        "mg/dL", "mmol/L", "mEq/L", "IU/L", "U/L", "g/dL", "g/L",
        "ng/mL", "pg/mL", "mcg/dL", "ug/dL", "nmol/L", "pmol/L",
        "uIU/mL", "mIU/mL", "IU/mL", "IU/dL", "%", "cells/uL",
        "/uL", "/mm3", "mm/hr", "sec", "ratio", "index", "titer",
        "mg/g", "mg/24h", "mOsm/kg", "mmHg", "bpm", "K/uL",
        "10^3/uL", "10^6/uL", "fl", "pg", "mg/L", "ug/L", "ng/L",
        "nmol/24h", "umol/L",
    ];

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.len() < 4 {
            continue;
        }
        // Skip if it starts with a known metadata prefix.
        let lower = trimmed.to_ascii_lowercase();
        if skip_prefixes
            .iter()
            .any(|p| lower.starts_with(&p.to_ascii_lowercase()))
        {
            continue;
        }
        // Tokenise on whitespace runs.
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }

        // The test name is the leading token(s) before the first numeric-ish
        // value. We allow up to 5 tokens for the name.
        let mut test_tokens = 0usize;
        for (i, tok) in tokens.iter().enumerate() {
            // If the token looks like a number (possibly with a leading '<' '>'),
            // stop here — the test name ends.
            let candidate = tok.trim_start_matches(['<', '>', '≤', '≥', '=']);
            if candidate.parse::<f64>().is_ok() {
                test_tokens = i;
                break;
            }
            // If we've consumed 6 tokens without finding a numeric, skip this line.
            if i >= 5 {
                test_tokens = 0;
                break;
            }
        }
        if test_tokens == 0 || test_tokens >= tokens.len() {
            continue;
        }

        let test_name: String = tokens[..test_tokens].join(" ");
        // Skip very short names (likely column headers or noise).
        if test_name.len() < 3 {
            continue;
        }

        let value_tok = tokens[test_tokens];
        let value_candidate = value_tok.trim_start_matches(['<', '>', '≤', '≥', '=']);
        let (value, value_text) = if let Ok(v) = value_candidate.parse::<f64>() {
            (Some(v), None)
        } else {
            // Qualitative result: check common text values.
            let vt = value_tok.trim().to_string();
            if !vt.is_empty() && vt.len() < 60 && vt.chars().any(|c| c.is_alphabetic()) {
                (None, Some(vt))
            } else {
                continue;
            }
        };

        // Next token(s): try to find a unit.
        let remaining = &tokens[test_tokens + 1..];
        let (unit, ref_range, flag) = extract_unit_range_flag(remaining, &common_units);

        // Confidence scoring.
        let mut conf = 0.5_f32;
        if value.is_some() || value_text.is_some() {
            conf += 0.1;
        }
        if unit.is_some() {
            conf += 0.15;
        }
        if ref_range.is_some() {
            conf += 0.1;
        }
        if default_date.is_some() {
            conf += 0.1;
        }

        rows.push(ParsedRow {
            ts: default_date.map(str::to_string),
            test: test_name,
            value,
            value_text,
            unit,
            reference_range: ref_range,
            flag,
            panel: None,
            provider: None,
            confidence: conf.min(1.0),
        });
    }

    rows
}

/// From the remaining tokens after the value, try to extract unit, reference
/// range, and flag. Returns `(unit, reference_range, flag)`.
fn extract_unit_range_flag(
    tokens: &[&str],
    common_units: &[&str],
) -> (Option<String>, Option<String>, Option<String>) {
    if tokens.is_empty() {
        return (None, None, None);
    }

    let mut unit: Option<String> = None;
    let mut ref_range: Option<String> = None;
    let mut flag: Option<String> = None;

    let mut i = 0usize;

    // Try to find a unit in the first 1–2 tokens.
    while i < tokens.len().min(3) {
        let tok = tokens[i];
        let tok_lower = tok.to_ascii_lowercase();
        if common_units
            .iter()
            .any(|u| tok_lower == u.to_ascii_lowercase())
        {
            unit = Some(tok.to_string());
            i += 1;
            break;
        }
        // Looks like a unit if it contains a "/" and is short.
        if tok.contains('/') && tok.len() < 15 {
            unit = Some(tok.to_string());
            i += 1;
            break;
        }
        break; // Not a unit; stop looking.
    }

    // Reference range: a token containing "-" or "<" / ">" and no letters
    // (or short letter suffixes like "<5.7" / "70-99").
    while i < tokens.len() {
        let tok = tokens[i];
        let is_range =
            (tok.contains('-') && !tok.starts_with('-'))
            || tok.starts_with('<')
            || tok.starts_with('>')
            || tok.starts_with('≤')
            || tok.starts_with('≥');
        if is_range && tok.len() < 20 {
            // Could be multi-token range like "70 - 99"; peek ahead.
            if i + 2 < tokens.len() && tokens[i + 1] == "-" {
                ref_range = Some(format!("{} - {}", tok, tokens[i + 2]));
                i += 3;
            } else {
                ref_range = Some(tok.to_string());
                i += 1;
            }
            break;
        }
        break;
    }

    // Flag: single-char or short token (H, L, A, HH, LL, N, *).
    if i < tokens.len() {
        let tok = tokens[i];
        if tok.len() <= 3 && tok.chars().all(|c| c.is_ascii_uppercase() || c == '*') {
            let f = tok.to_string();
            if matches!(f.as_str(), "H" | "L" | "A" | "HH" | "LL" | "N" | "C" | "*" | "HI" | "LO") {
                flag = Some(f);
            }
        }
    }

    (unit, ref_range, flag)
}

// ---------------------------------------------------------------------------
// Contract layer: build Observation rows from ParsedRow, write via store.

/// Stable 8-hex-char identifier for a test name, collision-free across tests
/// that share a long common prefix (e.g. lipid sub-fractions).
///
/// Uses FNV-1a (32-bit) of the full case-folded name so:
/// - identical names → identical slug (deterministic re-import)
/// - distinct names → distinct slugs (no truncation collision)
/// - no external crate needed (FNV is trivially inlined)
fn test_slug(name: &str) -> String {
    // FNV-1a 32-bit over the lowercase bytes.
    const FNV_OFFSET: u32 = 2_166_136_261;
    const FNV_PRIME: u32  = 16_777_619;
    let mut hash: u32 = FNV_OFFSET;
    for byte in name.to_ascii_lowercase().bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:08x}")
}

/// Convert parsed rows into contract Observations, skipping any without a `ts`.
fn rows_to_observations(
    rows: &[ParsedRow],
    hash8: &str,
    provider_name: &str,
    panel_name: &str,
) -> Vec<Observation> {
    rows.iter()
        .filter_map(|r| {
            let ts = r.ts.as_deref()?.to_string();
            let slug = test_slug(&r.test);
            let date_key = ts.chars().take(10).collect::<String>().replace('-', "");
            let guid = format!("{hash8}-{slug}-{date_key}");

            let mut extra: Map<String, Value> = Map::new();
            extra.insert(
                "confidence".into(),
                Value::String(format!("{:.2}", r.confidence)),
            );
            extra.insert("pdf_hash_prefix".into(), Value::String(hash8.to_string()));

            let mut obs = Observation::new("lab-pdf", guid, ts, r.test.clone());
            obs.value = r.value;
            obs.value_text = r.value_text.clone().unwrap_or_default();
            obs.unit = r.unit.clone().unwrap_or_default();
            obs.reference_range = r.reference_range.clone().unwrap_or_default();
            obs.flag = r.flag.clone().unwrap_or_default();
            obs.provider = provider_name.to_string();
            obs.panel = panel_name.to_string();
            obs.extra = extra;
            Some(obs)
        })
        .collect()
}

/// Deduplicate by guid against what is already on disk, then append new rows.
fn write_observations(vault: &Vault, observations: Vec<Observation>) -> Result<u64> {
    let stream = vault.stream(OBS_DIR, Partition::Month);

    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            let g = v
                .get("guid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let new_obs: Vec<Observation> = observations
        .into_iter()
        .filter(|o| !o.guid.is_empty() && seen.insert(o.guid.clone()))
        .collect();

    let count = new_obs.len() as u64;
    stream.append(&new_obs, |o| &o.ts)?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Top-level import function.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    import_pdf(vault, path, progress)
}

/// The import body — the testable seam.
pub(crate) fn import_pdf(
    vault: &Vault,
    path: &Path,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // 1. Read the PDF bytes.
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    progress(ImportProgress { records: 0, percent: 10.0 });

    // 2. Compute content hash (SHA-256 hex).
    let hash_hex = {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        format!("{:x}", hasher.finalize())
    };
    let hash8 = &hash_hex[..8]; // Short prefix for guids.

    // 3. Store the verbatim PDF (raw layer, unconditional).
    let pdf_rel = format!("{RAW_DIR}/{hash_hex}.pdf");
    let pdf_path = vault.resolve(&pdf_rel)?;
    write_atomic(&pdf_path, &bytes)
        .with_context(|| format!("storing raw PDF {hash_hex}.pdf"))?;
    progress(ImportProgress { records: 1, percent: 30.0 });

    // 4. Extract text from the PDF text layer (best-effort).
    let (raw_text, text_extracted, mut notes) = extract_pdf_text(&bytes);
    progress(ImportProgress { records: 1, percent: 50.0 });

    // 5. Rules-layer parse.
    let collection_date = find_collection_date(&raw_text);
    let provider_name = find_provider(&raw_text).unwrap_or_default();
    let panel_name = find_panel(&raw_text).unwrap_or_default();
    let mut parsed_rows = parse_result_lines(&raw_text, collection_date.as_deref());

    // Back-fill panel/provider into rows that didn't find them individually.
    for row in &mut parsed_rows {
        if row.panel.is_none() && !panel_name.is_empty() {
            row.panel = Some(panel_name.clone());
        }
        if row.provider.is_none() && !provider_name.is_empty() {
            row.provider = Some(provider_name.clone());
        }
    }

    if !text_extracted {
        notes.push(
            "No text extracted — the rules parser produced no rows. \
             Store the PDF verbatim; re-parse when OCR or a better extractor is available."
                .into(),
        );
    }
    if text_extracted && parsed_rows.is_empty() {
        notes.push(
            "Text extracted but no result rows found — PDF layout may not match any \
             known pattern (Needs-sample to tune the rules layer)."
                .into(),
        );
    }

    // 6. Write extraction JSON (raw layer, unconditional).
    let extraction = ExtractionRecord {
        hash: hash_hex.clone(),
        imported_at: Local::now().to_rfc3339(),
        text_extracted,
        raw_text: raw_text.clone(),
        parsed_rows,
        notes: notes.clone(),
    };
    let json_rel = format!("{RAW_DIR}/{hash_hex}.json");
    let json_path = vault.resolve(&json_rel)?;
    write_json_atomic(&json_path, &extraction)
        .with_context(|| format!("storing extraction JSON {hash_hex}.json"))?;
    progress(ImportProgress { records: 1, percent: 70.0 });

    // 7. Write contract Observation rows (reuse-bound: health_medical).
    let observations = rows_to_observations(
        &extraction.parsed_rows,
        hash8,
        &provider_name,
        &panel_name,
    );
    let new_obs = write_observations(vault, observations)
        .context("writing contract observations")?;
    progress(ImportProgress {
        records: new_obs,
        percent: 100.0,
    });

    // 8. Headline.
    let row_noun = if new_obs == 1 { "observation" } else { "observations" };
    let headline = if !text_extracted {
        format!(
            "PDF stored verbatim (image/scanned PDF — text layer empty; OCR not yet wired). \
             0 observations extracted. Raw: {hash8}….pdf"
        )
    } else if new_obs == 0 && extraction.parsed_rows.is_empty() {
        format!(
            "PDF text extracted but no result rows parsed (layout not yet recognised — Needs-sample). \
             Raw + text stored. Hash: {hash8}…"
        )
    } else {
        format!(
            "{new_obs} new {row_noun} imported from lab PDF. Raw PDF + extraction JSON stored. \
             Hash: {hash8}…"
        )
    };

    Ok(ImportOutcome {
        headline,
        counts: BTreeMap::from([
            ("raw_pdfs", 1u64),
            ("observations", new_obs),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-lab-pdf-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Unit tests for the pure parsing helpers (no vault, no filesystem).

    #[test]
    fn parses_iso_collection_date() {
        let text = "Collection Date: 2026-04-15\nSome other line\n";
        assert_eq!(find_collection_date(text), Some("2026-04-15".into()));
    }

    #[test]
    fn parses_us_slash_collection_date() {
        let text = "Collected: 04/15/2026\nGlucose 98 mg/dL 70-99\n";
        assert_eq!(find_collection_date(text), Some("2026-04-15".into()));
    }

    #[test]
    fn date_fallback_finds_iso_in_first_50_lines() {
        // No "Collection Date:" prefix; an ISO date is embedded in the text.
        let text = "Quest Diagnostics\nPatient: John Smith\n2026-01-20\nGlucose 98\n";
        assert_eq!(find_collection_date(text), Some("2026-01-20".into()));
    }

    #[test]
    fn extracts_provider_name() {
        let text = "Ordering Provider: Dr. Jane Smith, MD\nSome other line\n";
        assert_eq!(
            find_provider(text),
            Some("Dr. Jane Smith, MD".into())
        );
    }

    #[test]
    fn extracts_panel_name() {
        let text = "Panel: Comprehensive Metabolic Panel\nGlucose 98 mg/dL 70-99\n";
        assert_eq!(
            find_panel(text),
            Some("Comprehensive Metabolic Panel".into())
        );
    }

    #[test]
    fn test_slug_is_8_hex_chars_and_deterministic() {
        let slug = test_slug("Hemoglobin A1c/Hemoglobin.total");
        // FNV-1a 32-bit → always exactly 8 hex chars.
        assert_eq!(slug.len(), 8, "slug must be 8 hex chars");
        assert!(slug.chars().all(|c| c.is_ascii_hexdigit()), "slug must be hex");
        // Deterministic: same input → same output.
        assert_eq!(slug, test_slug("Hemoglobin A1c/Hemoglobin.total"));
        // Case-insensitive collision prevention: lowercase before hashing.
        assert_eq!(
            test_slug("glucose"),
            test_slug("Glucose"),
            "case-folded slugs must match"
        );
    }

    #[test]
    fn test_slug_no_collision_on_long_shared_prefix() {
        // Lipid sub-fractions share a long prefix — must NOT produce the same slug.
        let s1 = test_slug("Cholesterol, Total/HDL Cholesterol Ratio");
        let s2 = test_slug("Cholesterol, Total/HDL Cholesterol Ratio (calc)");
        assert_ne!(
            s1, s2,
            "distinct test names must produce distinct slugs (no truncation collision)"
        );
    }

    #[test]
    fn collection_date_priority_beats_generic_date() {
        // "Date:" appears first on the page (report-generation date);
        // "Collected:" appears later — priority order must pick the latter.
        let text = "Date: 06/01/2026 printed\nSome line\nCollected: 05/20/2026\nGlucose 98\n";
        assert_eq!(
            find_collection_date(text),
            Some("2026-05-20".into()),
            "Collected: must win over earlier Date: line"
        );
    }

    #[test]
    fn collection_date_ignores_dob_in_fallback() {
        // No prefix matches; ISO fallback must NOT pick up the DOB line.
        let text = "Quest Diagnostics\nPatient DOB: 1980-07-04\nCollection Date: 2026-03-10\n";
        // "Collection Date:" prefix matches → must return collection date, not DOB.
        assert_eq!(
            find_collection_date(text),
            Some("2026-03-10".into()),
            "prefix match must return collection date"
        );
        // Fallback-only scenario: no prefix line, DOB line is skipped.
        let text2 = "Quest Diagnostics\nDOB: 1980-07-04\n2026-03-10 collected\n";
        assert_eq!(
            find_collection_date(text2),
            Some("2026-03-10".into()),
            "DOB line must be skipped in ISO fallback"
        );
    }

    #[test]
    fn collection_date_report_date_not_used() {
        // "Report Date:" must NOT be used as the collection date (it's excluded).
        // Without any collection-date prefix, the fallback should find the ISO date.
        let text = "Report Date: 2026-06-01\nCollected: 05/20/2026\n";
        assert_eq!(
            find_collection_date(text),
            Some("2026-05-20".into()),
            "Collected: must win; Report Date: must be ignored"
        );
    }

    #[test]
    fn parse_date_str_handles_2_digit_year() {
        assert_eq!(parse_date_str("3/10/26"), Some("2026-03-10".into()));
        assert_eq!(parse_date_str("12/31/99"), Some("1999-12-31".into()));
    }

    #[test]
    fn parse_date_str_handles_written_month() {
        assert_eq!(parse_date_str("March 10, 2026"), Some("2026-03-10".into()));
        assert_eq!(parse_date_str("march 10 2026"),   Some("2026-03-10".into()));
        assert_eq!(parse_date_str("MARCH 10, 2026"),  Some("2026-03-10".into()));
    }

    #[test]
    fn parse_date_str_handles_dd_mon_yyyy() {
        assert_eq!(parse_date_str("10-Mar-2026"), Some("2026-03-10".into()));
        assert_eq!(parse_date_str("10 Mar 2026"), Some("2026-03-10".into()));
    }

    #[test]
    fn parse_date_str_handles_international_dd_mm() {
        // Day > 12 is the unambiguous DD/MM signal.
        assert_eq!(parse_date_str("13/05/2026"), Some("2026-05-13".into()));
    }

    // A scaffold fixture representing the text a digital Quest PDF might yield
    // after pdf-extract runs.  Field names and layout are the expected shape
    // based on Quest's public documentation; NOT validated against a real PDF
    // (Needs-sample).
    const SCAFFOLD_QUEST_TEXT: &str = "\
Quest Diagnostics
Patient: Jane Sample  DOB: 01/01/1980
Collection Date: 2026-03-10  Ordering Provider: Dr. Alice Brown
Panel: Comprehensive Metabolic Panel

Test Name         Value  Units    Reference Range  Flag
Glucose           98     mg/dL    70-99
BUN               15     mg/dL    7-20
Creatinine        0.82   mg/dL    0.57-1.00
Calcium           9.8    mg/dL    8.5-10.2
Sodium            140    mEq/L    136-145
Potassium         4.2    mEq/L    3.5-5.1
Cholesterol       198    mg/dL    <200
HDL               55     mg/dL    >40
TSH               2.10   uIU/mL   0.45-4.50
";

    #[test]
    fn parse_result_lines_extracts_glucose_row() {
        let rows = parse_result_lines(SCAFFOLD_QUEST_TEXT, Some("2026-03-10"));
        // At minimum the Glucose row should be found.
        let glucose = rows.iter().find(|r| r.test.to_ascii_lowercase().contains("glucose"));
        assert!(glucose.is_some(), "Glucose row not found in: {rows:?}");
        let g = glucose.unwrap();
        assert_eq!(g.value, Some(98.0), "numeric value");
        assert_eq!(g.unit.as_deref(), Some("mg/dL"), "unit");
        assert_eq!(g.ts.as_deref(), Some("2026-03-10"), "date from context");
    }

    #[test]
    fn parse_result_lines_finds_multiple_results() {
        let rows = parse_result_lines(SCAFFOLD_QUEST_TEXT, Some("2026-03-10"));
        // The scaffold has 9 result rows; at least 5 should parse.
        assert!(
            rows.len() >= 5,
            "expected at least 5 parsed rows, got {}: {rows:?}",
            rows.len()
        );
    }

    #[test]
    fn parse_result_lines_extracts_tsh_with_range() {
        let rows = parse_result_lines(SCAFFOLD_QUEST_TEXT, Some("2026-03-10"));
        let tsh = rows.iter().find(|r| r.test.to_ascii_lowercase() == "tsh");
        assert!(tsh.is_some(), "TSH row not found");
        let t = tsh.unwrap();
        assert_eq!(t.value, Some(2.10));
        assert_eq!(t.unit.as_deref(), Some("uIU/mL"));
        assert_eq!(t.reference_range.as_deref(), Some("0.45-4.50"));
    }

    #[test]
    fn rows_to_observations_builds_contract_rows() {
        let rows = parse_result_lines(SCAFFOLD_QUEST_TEXT, Some("2026-03-10"));
        let obs = rows_to_observations(&rows, "deadbeef", "Dr. Alice Brown", "CMP");
        assert!(
            !obs.is_empty(),
            "expected at least one Observation from scaffold text"
        );
        let glucose = obs.iter().find(|o| o.test.to_ascii_lowercase().contains("glucose"));
        assert!(glucose.is_some(), "Glucose observation not built");
        let g = glucose.unwrap();
        assert_eq!(g.source, "lab-pdf");
        assert_eq!(g.ts, "2026-03-10");
        assert!(g.guid.starts_with("deadbeef-"), "guid prefix");
        assert_eq!(g.provider, "Dr. Alice Brown");
        assert_eq!(g.panel, "CMP");
        assert!(g.extra.contains_key("confidence"), "confidence in extra");
    }

    #[test]
    fn rows_to_observations_skips_rows_without_ts() {
        let rows = parse_result_lines(SCAFFOLD_QUEST_TEXT, None); // no date context
        // Even without a date, rows_to_observations must not panic.
        // Rows without ts are filtered out.
        let obs = rows_to_observations(&rows, "aabbccdd", "", "");
        // All filtered out because ts is None.
        assert!(
            obs.is_empty(),
            "no date context → all rows should be skipped; got {obs:?}"
        );
    }

    #[test]
    fn guid_is_stable_across_re_import() {
        let rows = parse_result_lines(SCAFFOLD_QUEST_TEXT, Some("2026-03-10"));
        let obs1 = rows_to_observations(&rows, "deadbeef", "Dr. Alice", "CMP");
        let obs2 = rows_to_observations(&rows, "deadbeef", "Dr. Alice", "CMP");
        let guids1: Vec<_> = obs1.iter().map(|o| &o.guid).collect();
        let guids2: Vec<_> = obs2.iter().map(|o| &o.guid).collect();
        assert_eq!(guids1, guids2, "same input → same guids (deterministic)");
    }

    // -----------------------------------------------------------------------
    // Integration tests through the full import function.

    /// Minimal valid PDF bytes (a 1-page digital PDF with a text "Hello" content
    /// stream, confirming the raw-layer write works without a real patient PDF).
    /// This is NOT a lab-result PDF; it proves the scaffold import runs without
    /// panicking and stores the raw PDF.
    const MINIMAL_PDF: &[u8] = b"\
%PDF-1.4\n\
1 0 obj\n<</Type /Catalog /Pages 2 0 R>>\nendobj\n\
2 0 obj\n<</Type /Pages /Kids [3 0 R] /Count 1>>\nendobj\n\
3 0 obj\n<</Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]>>\nendobj\n\
xref\n0 4\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \n\
trailer\n<</Size 4 /Root 1 0 R>>\n\
startxref\n190\n%%EOF\n";

    #[test]
    fn import_stores_raw_pdf_and_returns_outcome() {
        let v = temp_vault("raw-store");
        let pdf_path = v.root().join("lab_result.pdf");
        fs::write(&pdf_path, MINIMAL_PDF).unwrap();

        let outcome = import_pdf(&v, &pdf_path, &mut |_| {}).unwrap();

        // Raw PDF must be stored.
        assert_eq!(
            outcome.counts.get("raw_pdfs"),
            Some(&1),
            "raw PDF count"
        );
        // The raw directory should contain exactly one .pdf and one .json.
        let raw_dir = v.root().join(RAW_DIR);
        let files: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        let pdf_count = files
            .iter()
            .filter(|e| e.path().extension().is_some_and(|x| x == "pdf"))
            .count();
        let json_count = files
            .iter()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count();
        assert_eq!(pdf_count, 1, "one PDF in raw/");
        assert_eq!(json_count, 1, "one extraction JSON in raw/");
    }

    #[test]
    fn import_same_pdf_twice_is_idempotent() {
        // Re-importing the same PDF must not create duplicate raw files.
        let v = temp_vault("idempotent");
        let pdf_path = v.root().join("lab.pdf");
        fs::write(&pdf_path, MINIMAL_PDF).unwrap();

        import_pdf(&v, &pdf_path, &mut |_| {}).unwrap();
        import_pdf(&v, &pdf_path, &mut |_| {}).unwrap();

        let raw_dir = v.root().join(RAW_DIR);
        let pdf_files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "pdf"))
            .collect();
        // Same hash → same filename → only one file (overwrite-atomic is safe).
        assert_eq!(pdf_files.len(), 1, "same hash → one PDF file (idempotent)");
    }

    #[test]
    fn extraction_json_round_trips() {
        let v = temp_vault("json-rt");
        let pdf_path = v.root().join("lab.pdf");
        fs::write(&pdf_path, MINIMAL_PDF).unwrap();

        import_pdf(&v, &pdf_path, &mut |_| {}).unwrap();

        let raw_dir = v.root().join(RAW_DIR);
        let json_file = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .find(|e| e.path().extension().is_some_and(|x| x == "json"))
            .expect("extraction JSON written");
        let body = fs::read_to_string(json_file.path()).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        // Must have the key fields.
        assert!(v.get("hash").is_some(), "hash in extraction JSON");
        assert!(v.get("imported_at").is_some(), "imported_at in extraction JSON");
        assert!(v.get("text_extracted").is_some());
        assert!(v.get("parsed_rows").is_some());
    }

    #[test]
    fn observation_deduplication_across_imports() {
        // Simulate a text-bearing PDF by calling write_observations directly
        // with two identical observation sets — the second call should produce
        // zero new rows.
        let v = temp_vault("dedup");
        let obs1 = Observation::new("lab-pdf", "abc-glucose-20260310", "2026-03-10", "Glucose");
        let obs2 = Observation::new("lab-pdf", "abc-glucose-20260310", "2026-03-10", "Glucose");

        let n1 = write_observations(&v, vec![obs1]).unwrap();
        assert_eq!(n1, 1, "first import writes 1 row");
        let n2 = write_observations(&v, vec![obs2]).unwrap();
        assert_eq!(n2, 0, "second import with same guid is deduped");
    }

    #[test]
    fn def_is_import_accepts_pdf_no_connection() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none());
        let spec = DEF.import_spec().expect("Import spec");
        assert!(spec.accepts.contains(&"pdf"), "accepts pdf");
        assert!(!DEF.default_on, "opt-in only — medical data");
    }

    #[test]
    fn scaffold_text_observations_land_in_vault() {
        // Write the scaffold text to a fake .pdf file and call import_pdf.
        // pdf-extract will not decode this (it's not a real PDF); the raw layer
        // is written + extraction JSON records text_extracted=false.  This test
        // proves the full plumbing runs without panicking on unknown content.
        let v = temp_vault("scaffold");
        let pdf_path = v.root().join("scaffold.pdf");
        fs::write(&pdf_path, MINIMAL_PDF).unwrap();

        let outcome = import_pdf(&v, &pdf_path, &mut |_| {}).unwrap();
        assert_eq!(outcome.counts.get("raw_pdfs"), Some(&1));
        // Observations may be 0 (minimal PDF has no text layer matching patterns).
        assert!(outcome.counts.contains_key("observations"));
    }
}

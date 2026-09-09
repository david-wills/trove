//! vCard Import (.vcf) — generic RFC 6350 (4.0) and vCard 3.0 contact file
//! importer. Covers every major export path: iCloud.com, Google Contacts /
//! Takeout, Outlook, and any CardDAV server. One drop covers virtually every
//! contacts source that lacks a direct connector.
//!
//! Brief: docs/integrations/vcard.md
//!
//! **Parser design.** vCard format is line-based and well-specified enough for
//! a hand-rolled parser that handles both 3.0 and 4.0 without pulling in an
//! extra crate. The rules: unfold RFC-style line continuations (CRLF+SP or
//! LF+SP), split each logical line on the first `:`, left side is
//! `PROPERTY[;PARAMS...]`, right side is the value. Property names and param
//! keys are case-insensitive. Inline base64 PHOTO blobs are stripped at parse
//! time — the contract stores real URLs only, and a photo URL from a 4.0 URI
//! reference would be a genuine URL, which we do keep.
//!
//! **Vault target.** `contacts/vcard/contacts.jsonl` — a snapshot file
//! ([`Vault::write_snapshot`]) rewritten atomically on each import, one
//! [`crate::contacts::Contact`] per line, sorted by `id`. Re-imports are
//! idempotent: existing contacts are merged by id, incoming rows replace
//! previous ones.
//!
//! **Dedupe key (id).** vCard UID when present (a UUID in 4.0 exports, and
//! many Google/iCloud 3.0 exports carry it); otherwise a composite
//! `name\x1eemail` key from the first email or just the FN.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::contacts::{normalize_email, normalize_phone, Contact, ContactOrg};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

const SNAPSHOT_REL: &str = "contacts/vcard/contacts.jsonl";
const SOURCE: &str = "vcard";

// ---------------------------------------------------------------------------
// DEF

fn def_last_data(vault: &Vault) -> Option<String> {
    let path = vault.resolve(SNAPSHOT_REL).ok()?;
    crate::registry::file_mtime(&path)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "vcard",
        name: "vCard Import (.vcf)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import a .vcf file exported from iCloud, Google Contacts, \
                      Outlook, or any CardDAV service. A universal fallback that \
                      covers virtually every contact source in one importer.",
        domain: "contacts",
        vault_path: "contacts/vcard/",
        toggleable: false,
        setup: &[
            "iCloud.com → Contacts → select all → Export vCard.",
            "Google Takeout → Contacts → .vcf (preferred over .csv).",
            "Outlook → File → Open & Export → Import/Export → Export to vCard.",
            "Any CardDAV client: export the address book as a .vcf file.",
        ],
        caveats: "Base64-encoded PHOTO blobs are stripped at import time to keep \
                  vault files lean. Re-importing a newer export is safe: \
                  existing contacts are updated in place (dedupe by UID or name+email).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["vcf", "vcard"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import runner

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load existing snapshot for merge.
    let mut existing: HashMap<String, Contact> = vault
        .read_snapshot::<Contact>(SNAPSHOT_REL)?
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();

    let (mut imported, mut skipped) = (0u64, 0u64);
    let mut card_count = 0u64;

    // Stream cards one at a time — never load the full file into memory.
    iter_vcards(path, |block| {
        if card_count % 100 == 0 {
            progress(ImportProgress {
                records: imported,
                percent: (card_count as f32 / (card_count + 1).max(1) as f32) * 90.0,
            });
        }
        card_count += 1;
        match parse_vcard(&block) {
            Some(contact) => {
                existing.insert(contact.id.clone(), contact);
                imported += 1;
            }
            None => {
                skipped += 1;
            }
        }
    })?;

    // Snapshot: sorted by id for clean diffs.
    let mut out: Vec<Contact> = existing.into_values().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));

    vault.write_snapshot(SNAPSHOT_REL, &out)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!("{imported} contacts imported, {skipped} skipped"),
        counts: [("imported", imported), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// vCard parser (hand-rolled; handles both RFC 2426 / 3.0 and RFC 6350 / 4.0)

/// Iterate over vCard blocks in a file one at a time, calling `cb` with each
/// unfolded BEGIN…END block. Large exports (10k+ contacts) are streamed:
/// only one logical card is buffered at a time; the input file is never
/// fully loaded into memory.
fn iter_vcards(path: &Path, mut cb: impl FnMut(String)) -> Result<()> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut in_card = false;
    let mut current: Vec<String> = Vec::new();
    // Unfold state: carry the previous logical line until we know whether the
    // next physical line is a continuation.
    let mut pending: Option<String> = None;

    let process_logical = |logical: String, in_card: &mut bool, current: &mut Vec<String>, cb: &mut dyn FnMut(String)| {
        let upper = logical.trim().to_uppercase();
        if upper == "BEGIN:VCARD" {
            *in_card = true;
            current.clear();
        } else if upper == "END:VCARD" {
            if *in_card {
                cb(current.join("\n"));
                current.clear();
            }
            *in_card = false;
        } else if *in_card {
            current.push(logical);
        }
    };

    for raw_line in reader.lines() {
        let raw = raw_line.with_context(|| format!("reading {}", path.display()))?;
        // RFC unfolding: if this line starts with SP or HT it is a continuation.
        if raw.starts_with(' ') || raw.starts_with('\t') {
            if let Some(ref mut prev) = pending {
                prev.push_str(raw.trim_start());
            }
            // (If no pending line, malformed — just skip the whitespace prefix.)
            continue;
        }
        // Flush the previously accumulated logical line.
        if let Some(logical) = pending.take() {
            process_logical(logical, &mut in_card, &mut current, &mut cb);
        }
        // CRLF normalisation: raw_line strips the trailing '\n' but may keep '\r'.
        pending = Some(raw.trim_end_matches('\r').to_string());
    }
    // Flush last logical line.
    if let Some(logical) = pending.take() {
        process_logical(logical, &mut in_card, &mut current, &mut cb);
    }
    Ok(())
}


/// One parsed property line: name, params (lowercased key → value), value.
struct Prop<'a> {
    name: &'a str,
    /// Parameters keyed by lowercased param name.
    /// For `type`, all tokens are accumulated and joined with `,` so that
    /// `EMAIL;type=INTERNET;type=pref` → `"INTERNET,pref"` and
    /// `TEL;TYPE=WORK,VOICE` stores `"WORK,VOICE"` as-is (already comma-joined).
    params: HashMap<String, String>,
    value: &'a str,
}

/// Parse a single logical vCard line into a [`Prop`]. Returns `None` for
/// blank lines or lines that don't contain `:`.
fn parse_prop(line: &str) -> Option<Prop<'_>> {
    let colon = line.find(':')?;
    let lhs = &line[..colon];
    let value = &line[colon + 1..];

    // Split lhs on `;` → [name, param1=val1, param2=val2, ...]
    let mut parts = lhs.splitn(100, ';');
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }

    let mut params: HashMap<String, String> = HashMap::new();
    // Accumulate all type= tokens so multi-TYPE labels survive.
    let mut type_tokens: Vec<String> = Vec::new();

    for param in parts {
        let param = param.trim();
        if let Some(eq) = param.find('=') {
            let k = param[..eq].trim().to_lowercase();
            let v = param[eq + 1..].trim().to_string();
            // Strip surrounding quotes if present.
            let v = v.trim_matches('"').to_string();
            if k == "type" {
                // Collect each type token; comma-joined values are kept as-is.
                type_tokens.push(v);
            } else {
                params.insert(k, v);
            }
        } else {
            // Bare param like TYPE=WORK written without `=`: treat whole token
            // as a `type` value (vCard 3.0 style: `TEL;type=CELL` or `TEL;CELL`).
            if !param.is_empty() {
                type_tokens.push(param.to_string());
            }
        }
    }
    if !type_tokens.is_empty() {
        params.insert("type".to_string(), type_tokens.join(","));
    }
    Some(Prop { name, params, value })
}

/// Determine if a property carries an inline base64 PHOTO blob that should be
/// stripped. Criteria:
/// - vCard 3.0: PHOTO;ENCODING=b or PHOTO;ENCODING=BASE64 (case-insensitive)
/// - vCard 4.0: PHOTO:data:image/...;base64,...
fn is_photo_blob(prop: &Prop<'_>) -> bool {
    let name_upper = prop.name.to_uppercase();
    if name_upper != "PHOTO" {
        return false;
    }
    // 4.0 style: value starts with data: URI
    if prop.value.trim_start().starts_with("data:") {
        return true;
    }
    // 3.0 style: ENCODING=b or ENCODING=BASE64 param
    if let Some(enc) = prop.params.get("encoding") {
        let enc = enc.to_uppercase();
        if enc == "B" || enc == "BASE64" {
            return true;
        }
    }
    // Multi-line base64 blob heuristic: very long value of base64 characters
    // (no space, no obvious URL pattern)
    if prop.value.len() > 200 && !prop.value.contains("://") && prop.value.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=') {
        return true;
    }
    false
}

/// Parse a vcard block (lines between BEGIN:VCARD and END:VCARD) into a
/// [`Contact`]. Returns `None` for a block with no usable identity.
fn parse_vcard(block: &str) -> Option<Contact> {
    let mut uid = String::new();
    let mut fn_ = String::new();
    let mut given = String::new();
    let mut family = String::new();
    let mut emails: Vec<String> = Vec::new();
    let mut phones: Vec<String> = Vec::new();
    let mut orgs: Vec<ContactOrg> = Vec::new();
    let mut photo_url = String::new();
    let mut updated: Option<String> = None;
    let mut extra: Map<String, Value> = Map::new();

    // Accumulate extra arrays.
    let mut addresses: Vec<Value> = Vec::new();
    let mut urls: Vec<Value> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut birthdays: Vec<Value> = Vec::new();
    let mut impp_vals: Vec<String> = Vec::new();

    for line in block.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(prop) = parse_prop(line) else {
            continue;
        };
        let name_upper = prop.name.to_uppercase();

        // Handle group-prefixed properties (e.g. `item1.EMAIL`)
        let name_upper = if let Some(dot) = name_upper.rfind('.') {
            &name_upper[dot + 1..]
        } else {
            name_upper.as_str()
        };

        match name_upper {
            "VERSION" | "PRODID" | "BEGIN" | "END" => {}

            "UID" => {
                uid = prop.value.trim().to_string();
            }

            "FN" => {
                if fn_.is_empty() {
                    fn_ = decode_value(prop.value);
                }
            }

            "N" => {
                // N:family;given;additional;prefix;suffix
                let parts: Vec<&str> = prop.value.splitn(6, ';').collect();
                if family.is_empty() {
                    family = parts.first().map(|s| decode_value(s.trim())).unwrap_or_default();
                }
                if given.is_empty() {
                    given = parts.get(1).map(|s| decode_value(s.trim())).unwrap_or_default();
                }
            }

            "EMAIL" => {
                let e = normalize_email(&decode_value(prop.value));
                if !e.is_empty() {
                    emails.push(e);
                }
            }

            "TEL" => {
                let raw = decode_value(prop.value);
                let p = normalize_phone(raw.trim(), None);
                if !p.is_empty() {
                    phones.push(p);
                }
            }

            "ORG" => {
                // ORG:Company;Department (semicolon-separated)
                let parts: Vec<&str> = prop.value.splitn(3, ';').collect();
                let org_name = parts
                    .first()
                    .map(|s| decode_value(s.trim()))
                    .filter(|s| !s.is_empty());
                let title_from_org: Option<String> = None; // TITLE comes separately
                if org_name.is_some() {
                    orgs.push(ContactOrg { name: org_name, title: title_from_org });
                }
            }

            "TITLE" => {
                let t = decode_value(prop.value);
                if !t.is_empty() {
                    if let Some(last) = orgs.last_mut() {
                        if last.title.is_none() {
                            last.title = Some(t.clone());
                        }
                    } else {
                        orgs.push(ContactOrg { name: None, title: Some(t) });
                    }
                }
            }

            "PHOTO" => {
                if !is_photo_blob(&prop) {
                    let url = prop.value.trim().to_string();
                    if !url.is_empty() && url.contains("://") {
                        photo_url = url;
                    }
                }
                // Blobs are silently dropped (stripped at parse time).
            }

            "ADR" => {
                // ADR:PO Box;ext;street;city;state;zip;country
                let parts: Vec<&str> = prop.value.splitn(8, ';').collect();
                let formatted = parts
                    .iter()
                    .map(|s| decode_value(s.trim()))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                if !formatted.is_empty() {
                    let type_label = prop.params.get("type").cloned().unwrap_or_default();
                    let mut obj = Map::new();
                    if !type_label.is_empty() {
                        obj.insert("label".into(), Value::String(type_label));
                    }
                    obj.insert("value".into(), Value::String(formatted));
                    addresses.push(Value::Object(obj));
                }
            }

            "URL" => {
                let url_val = decode_value(prop.value.trim());
                if !url_val.is_empty() {
                    let type_label = prop.params.get("type").cloned().unwrap_or_default();
                    let mut obj = Map::new();
                    if !type_label.is_empty() {
                        obj.insert("label".into(), Value::String(type_label));
                    }
                    obj.insert("value".into(), Value::String(url_val));
                    urls.push(Value::Object(obj));
                }
            }

            "NOTE" => {
                let n = decode_value(prop.value);
                if !n.is_empty() {
                    notes.push(n);
                }
            }

            "BDAY" => {
                // vCard 3.0: BDAY:19901231 or BDAY:--1231 (no year)
                // vCard 4.0: BDAY:19901231 / BDAY;VALUE=text:circa 1990
                // iCloud year-less: BDAY;X-APPLE-OMIT-YEAR=1604:1604-MM-DD
                //   Apple uses 1604 as a sentinel year; we must drop it.
                let raw = prop.value.trim();
                let apple_omit_year = prop.params.contains_key("x-apple-omit-year");
                if let Some(bd) = parse_date_partial(raw) {
                    // If Apple's omit-year param is set, or the parsed year is
                    // 1604 (Apple's sentinel), strip the year key so downstream
                    // readers see only {month, day}, matching apple_contacts.rs.
                    let bd = if apple_omit_year {
                        strip_year(bd)
                    } else if let Some(y) = bd.as_object().and_then(|o| o.get("year")).and_then(|v| v.as_i64()) {
                        if y == 1604 { strip_year(bd) } else { bd }
                    } else {
                        bd
                    };
                    birthdays.push(bd);
                }
            }

            "IMPP" => {
                let v = decode_value(prop.value.trim());
                if !v.is_empty() {
                    impp_vals.push(v);
                }
            }

            "REV" => {
                // REV is a last-modified timestamp (ISO 8601 / RFC 3339).
                let rev = prop.value.trim().to_string();
                if !rev.is_empty() && updated.is_none() {
                    updated = Some(normalize_rev(&rev));
                }
            }

            _ => {
                // Unknown properties stored verbatim in extra under their
                // group-stripped, lowercased name so that iCloud `item1.X-FOO`
                // and `item3.X-FOO` both land under `x-foo` and accumulate.
                let key = name_upper.to_lowercase();
                let val = Value::String(decode_value(prop.value));
                match extra.get_mut(&key) {
                    Some(Value::Array(arr)) => arr.push(val),
                    Some(existing_val) => {
                        let prev = existing_val.clone();
                        *existing_val = Value::Array(vec![prev, val]);
                    }
                    None => {
                        extra.insert(key, val);
                    }
                }
            }
        }
    }

    // Build extras from structured arrays.
    if !addresses.is_empty() {
        extra.insert("addresses".into(), Value::Array(addresses));
    }
    if !urls.is_empty() {
        extra.insert("urls".into(), Value::Array(urls));
    }
    if !notes.is_empty() {
        // Always a single String (join multiple NOTEs with a newline separator),
        // matching apple_contacts.rs so cross-source readers see a stable type.
        extra.insert("note".into(), Value::String(notes.join("\n")));
    }
    if !birthdays.is_empty() {
        extra.insert(
            "birthdays".into(),
            Value::Array(
                birthdays
                    .into_iter()
                    .map(|d| {
                        let mut m = Map::new();
                        m.insert("date".into(), d);
                        Value::Object(m)
                    })
                    .collect(),
            ),
        );
    }
    if !impp_vals.is_empty() {
        extra.insert(
            "impp".into(),
            Value::Array(impp_vals.into_iter().map(Value::String).collect()),
        );
    }

    // Dedupe emails (preserve first-seen order after normalization).
    let emails = {
        let mut seen = std::collections::HashSet::new();
        emails.into_iter().filter(|e| seen.insert(e.clone())).collect::<Vec<_>>()
    };
    // Dedupe phones.
    let phones = {
        let mut seen = std::collections::HashSet::new();
        phones.into_iter().filter(|p| seen.insert(p.clone())).collect::<Vec<_>>()
    };

    // Display name: prefer FN; fall back to given+family assembly.
    let name = if !fn_.trim().is_empty() {
        fn_.trim().to_string()
    } else {
        format!("{} {}", given.trim(), family.trim()).trim().to_string()
    };

    // Stable dedupe key: UID wins; else composite.
    let id = if !uid.trim().is_empty() {
        uid.trim().to_string()
    } else if !name.is_empty() || !emails.is_empty() {
        let email_part = emails.first().map(|s| s.as_str()).unwrap_or("");
        format!("{}\x1e{}", name, email_part)
    } else {
        // No usable identity → skip.
        return None;
    };

    Some(Contact {
        source: SOURCE.to_string(),
        id,
        account: String::new(),
        name,
        given,
        family,
        emails,
        phones,
        orgs,
        photo: photo_url,
        other: false,
        updated,
        extra,
    })
}

/// Decode a vCard property value: handle `\n` escape sequences (literal
/// backslash-n becomes newline), `\,` → `,`, `\\` → `\`. Charset params
/// (vCard 3.0 CHARSET=UTF-8) are ignored since Rust strings are always UTF-8.
fn decode_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') | Some('N') => {
                    chars.next();
                    out.push('\n');
                }
                Some(',') => {
                    chars.next();
                    out.push(',');
                }
                Some(';') => {
                    chars.next();
                    out.push(';');
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Remove the `year` key from a `{year, month, day}` Value::Object, returning
/// the remaining `{month, day}` object. Used to normalise Apple's 1604-sentinel
/// year-less birthdays and explicit X-APPLE-OMIT-YEAR exports.
fn strip_year(mut v: Value) -> Value {
    if let Value::Object(ref mut m) = v {
        m.remove("year");
    }
    v
}

/// Parse a vCard date string into a `{month, day[, year]}` object.
///
/// Handled formats (vCard 3.0 + 4.0):
/// - `YYYYMMDD` → year+month+day
/// - `--MMDD` → month+day (no year), vCard 4.0 compact year-less form
/// - `--MM-DD` → month+day (no year), ISO 8601:2000 truncated dashed form
/// - `YYYY-MM-DD` → year+month+day (common in 3.0 exports and iCloud)
///
/// Returns `None` for anything that can't be parsed.
fn parse_date_partial(raw: &str) -> Option<Value> {
    let s = raw.trim();
    // Year-less forms begin with `--`.
    if let Some(rest) = s.strip_prefix("--") {
        // `--MMDD` (4 chars, no dash) — vCard 4.0 compact form.
        if rest.len() == 4 && rest.chars().all(|c| c.is_ascii_digit()) {
            let month: i32 = rest[..2].parse().ok()?;
            let day: i32 = rest[2..].parse().ok()?;
            let mut m = Map::new();
            m.insert("month".into(), Value::from(month));
            m.insert("day".into(), Value::from(day));
            return Some(Value::Object(m));
        }
        // `--MM-DD` (5 chars with dash) — ISO 8601:2000 truncated dashed form.
        if rest.len() == 5 {
            let parts: Vec<&str> = rest.splitn(2, '-').collect();
            if parts.len() == 2 {
                let month: i32 = parts[0].parse().ok()?;
                let day: i32 = parts[1].parse().ok()?;
                let mut m = Map::new();
                m.insert("month".into(), Value::from(month));
                m.insert("day".into(), Value::from(day));
                return Some(Value::Object(m));
            }
        }
        return None;
    }
    // `YYYYMMDD` — compact form.
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        let year: i32 = s[..4].parse().ok()?;
        let month: i32 = s[4..6].parse().ok()?;
        let day: i32 = s[6..].parse().ok()?;
        let mut m = Map::new();
        m.insert("year".into(), Value::from(year));
        m.insert("month".into(), Value::from(month));
        m.insert("day".into(), Value::from(day));
        return Some(Value::Object(m));
    }
    // `YYYY-MM-DD` — dashed form (includes iCloud's `1604-MM-DD` sentinel).
    if s.contains('-') {
        if let Some(v) = try_parse_ymd_dashes(s) {
            return Some(v);
        }
    }
    None
}

/// Try to parse `YYYY-MM-DD` form.
fn try_parse_ymd_dashes(s: &str) -> Option<Value> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: i32 = parts[0].parse().ok()?;
    let month: i32 = parts[1].parse().ok()?;
    let day: i32 = parts[2].trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok()?;
    if month < 1 || month > 12 || day < 1 || day > 31 {
        return None;
    }
    let mut m = Map::new();
    m.insert("year".into(), Value::from(year));
    m.insert("month".into(), Value::from(month));
    m.insert("day".into(), Value::from(day));
    Some(Value::Object(m))
}

/// Normalize a REV timestamp to RFC 3339. vCard 3.0 uses `YYYYMMDDTHHMMSSZ`
/// (basic ISO); vCard 4.0 uses full ISO 8601. Pass through strings that
/// already look like RFC 3339 (contain `-`). Returns the input trimmed when
/// normalization fails.
fn normalize_rev(raw: &str) -> String {
    let s = raw.trim();
    // Already contains dashes → probably already ISO 8601 / RFC 3339.
    if s.contains('-') {
        return s.to_string();
    }
    // Basic form: 20240225T020408Z → 2024-02-25T02:04:08Z
    if s.len() >= 15 && s.chars().nth(8) == Some('T') {
        let date = &s[..8];
        let time = &s[9..];
        if date.chars().all(|c| c.is_ascii_digit()) && time.len() >= 6 {
            let suffix = if time.ends_with('Z') { "Z" } else { "" };
            let td = &time[..6.min(time.len())];
            if td.chars().all(|c| c.is_ascii_digit()) {
                return format!(
                    "{}-{}-{}T{}:{}:{}{}",
                    &date[..4],
                    &date[4..6],
                    &date[6..8],
                    &td[..2],
                    &td[2..4],
                    &td[4..6],
                    suffix
                );
            }
        }
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-vcard-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn import_str(v: &Vault, vcf: &str) -> ImportOutcome {
        let path = v.root().join("contacts.vcf");
        fs::write(&path, vcf).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // vCard 3.0 — Google Takeout / iCloud style: no UID, bare ENCODING=b PHOTO,
    // type params as TYPE=WORK, semicolons in N field.
    const VCARD3_ALICE: &str = "\
BEGIN:VCARD
VERSION:3.0
PRODID:-//Apple Inc.//macOS 12.5//EN
N:Example;Alice;;;
FN:Alice Example
EMAIL;type=INTERNET;type=pref:Alice@Example.COM
EMAIL;type=INTERNET:alice@work.com
TEL;type=WORK:+1 (415) 555-0142
ORG:Example Corp;Engineering
TITLE:CTO
ADR;type=HOME:;;1 Main St;Springfield;IL;62704;USA
URL:https://alice.example
NOTE:A note with \\nline break.
BDAY:19901231
REV:2024-02-25T02:04:08.382Z
END:VCARD
";

    // vCard 3.0 with base64 PHOTO blob — blob must be stripped, URL kept when present.
    const VCARD3_PHOTO_BLOB: &str = "\
BEGIN:VCARD
VERSION:3.0
N:;inKind;;;
FN:inKind
PHOTO;ENCODING=b;TYPE=image/png:iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==
TEL;TYPE=WORK,VOICE:65028
URL:https://inkind.com/
REV:2024-02-25T02:04:08.382Z
END:VCARD
";

    // vCard 4.0 — with UID, year-less birthday, IMPP.
    const VCARD4_BOB: &str = "\
BEGIN:VCARD
VERSION:4.0
UID:urn:uuid:4fbe8971-0bc3-424c-9c26-36c3e1eff6b1
FN:Bob Builder
N:Builder;Bob;;;
EMAIL;TYPE=work:bob@example.com
TEL;TYPE=cell:+14155550001
BDAY:--0314
IMPP:xmpp:bob@jabber.example
END:VCARD
";

    // vCard with just a phone, no email (minimal — id = name+empty-email).
    const VCARD3_PHONE_ONLY: &str = "\
BEGIN:VCARD
VERSION:3.0
N:;Joshua;;;
FN:Joshua
TEL;type=pref:+1 (423) 407-6103
END:VCARD
";

    // Multi-card file — two contacts in one .vcf.
    const TWO_CARDS: &str = "\
BEGIN:VCARD
VERSION:3.0
FN:Carol
EMAIL:carol@example.com
END:VCARD
BEGIN:VCARD
VERSION:3.0
FN:Dave
EMAIL:dave@example.com
END:VCARD
";

    // Folded long line (RFC line continuation with CRLF+SP).
    // The NOTE value itself is folded: `NOTE:This note is\r\n split across\r\n  three lines.`
    // The first continuation (SP after \r\n) merges with NOTE; second (double-SP) merges again.
    const FOLDED: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Long\r\nNOTE:This note is\r\n split across\r\n three lines.\r\nEMAIL:long@example.com\r\nEND:VCARD\r\n";

    #[test]
    fn vcard3_full_fields_map_correctly() {
        let v = temp_vault("vcard3");
        let out = import_str(&v, VCARD3_ALICE);
        assert_eq!(out.counts["imported"], 1);

        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(loaded.len(), 1);
        let c = &loaded[0];
        assert_eq!(c.source, "vcard");
        assert_eq!(c.name, "Alice Example");
        assert_eq!(c.given, "Alice");
        assert_eq!(c.family, "Example");
        assert_eq!(c.emails, vec!["alice@example.com", "alice@work.com"], "lowercased + deduped");
        assert_eq!(c.phones, vec!["+14155550142"], "normalized to E.164");
        assert_eq!(c.orgs.len(), 1);
        assert_eq!(c.orgs[0].name.as_deref(), Some("Example Corp"));
        assert_eq!(c.orgs[0].title.as_deref(), Some("CTO"));
        // ADR → extra.addresses
        assert!(c.extra.contains_key("addresses"), "address parked in extra");
        let addrs = c.extra["addresses"].as_array().unwrap();
        assert!(!addrs.is_empty());
        let addr_val = addrs[0]["value"].as_str().unwrap();
        assert!(addr_val.contains("Springfield"), "address text preserved: {addr_val}");
        // URL → extra.urls
        assert!(c.extra.contains_key("urls"), "url parked in extra");
        // NOTE with escaped \n
        let note = c.extra["note"].as_str().unwrap();
        assert!(note.contains('\n'), "backslash-n decoded to newline: {:?}", note);
        // BDAY → extra.birthdays
        let bdays = c.extra["birthdays"].as_array().unwrap();
        assert_eq!(bdays[0]["date"]["year"], 1990);
        assert_eq!(bdays[0]["date"]["month"], 12);
        assert_eq!(bdays[0]["date"]["day"], 31);
        // REV → updated
        assert_eq!(c.updated.as_deref(), Some("2024-02-25T02:04:08.382Z"));
        // No UID in 3.0 → composite id
        assert!(c.id.contains("Alice Example"), "id contains name: {}", c.id);
    }

    #[test]
    fn photo_blob_is_stripped_photo_url_is_kept() {
        let v = temp_vault("photo");
        import_str(&v, VCARD3_PHOTO_BLOB);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let c = &loaded[0];
        assert!(c.photo.is_empty(), "base64 PHOTO blob must be stripped, got: {:?}", c.photo);

        // Photo URL case: 4.0-style PHOTO with a real URL should be kept.
        let with_url = "\
BEGIN:VCARD
VERSION:4.0
FN:WithPhoto
EMAIL:wp@example.com
PHOTO:https://example.com/photo.jpg
END:VCARD
";
        let v2 = temp_vault("photo_url");
        import_str(&v2, with_url);
        let loaded2: Vec<Contact> = v2.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(loaded2[0].photo, "https://example.com/photo.jpg");
    }

    #[test]
    fn vcard4_uid_and_yearless_bday() {
        let v = temp_vault("vcard4");
        import_str(&v, VCARD4_BOB);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let c = &loaded[0];
        assert_eq!(c.id, "urn:uuid:4fbe8971-0bc3-424c-9c26-36c3e1eff6b1", "UID is the dedupe key");
        assert_eq!(c.name, "Bob Builder");
        // Year-less birthday: month=3, day=14, no year key.
        let bdays = c.extra["birthdays"].as_array().unwrap();
        assert_eq!(bdays[0]["date"]["month"], 3);
        assert_eq!(bdays[0]["date"]["day"], 14);
        assert!(bdays[0]["date"].get("year").is_none(), "no year for --MMDD form");
        // IMPP → extra.impp
        let impp = c.extra["impp"].as_array().unwrap();
        assert_eq!(impp[0].as_str(), Some("xmpp:bob@jabber.example"));
    }

    #[test]
    fn multi_card_file_imports_both() {
        let v = temp_vault("multi");
        let out = import_str(&v, TWO_CARDS);
        assert_eq!(out.counts["imported"], 2);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn folded_lines_are_unfolded_correctly() {
        let v = temp_vault("fold");
        import_str(&v, FOLDED);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(loaded.len(), 1);
        let c = &loaded[0];
        assert_eq!(c.name, "Long");
        // The NOTE was folded across 3 physical lines → reunited.
        let note = c.extra.get("note").and_then(|n| n.as_str()).unwrap_or("");
        assert!(note.contains("split across"), "folded note reunited: {note:?}");
    }

    #[test]
    fn phone_only_contact_gets_composite_id() {
        let v = temp_vault("phone_only");
        import_str(&v, VCARD3_PHONE_ONLY);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(loaded.len(), 1);
        let c = &loaded[0];
        assert_eq!(c.name, "Joshua");
        assert_eq!(c.phones, vec!["+14234076103"]);
        // Composite id: name + empty email (still unique per person).
        assert!(c.id.starts_with("Joshua"), "id starts with name: {}", c.id);
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("idem");
        import_str(&v, TWO_CARDS);
        let first: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        import_str(&v, TWO_CARDS);
        let second: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(first.len(), second.len());
        let first_ids: Vec<&str> = first.iter().map(|c| c.id.as_str()).collect();
        let second_ids: Vec<&str> = second.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(first_ids, second_ids, "same contacts, same order");
    }

    #[test]
    fn snapshot_round_trips_contact() {
        let v = temp_vault("roundtrip");
        import_str(&v, VCARD3_ALICE);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(loaded.len(), 1);
        // Re-serialise + deserialise → same fields.
        let line = serde_json::to_string(&loaded[0]).unwrap();
        let back: Contact = serde_json::from_str(&line).unwrap();
        assert_eq!(back.emails, loaded[0].emails);
        assert_eq!(back.extra, loaded[0].extra);
    }

    #[test]
    fn vcard_with_no_identity_is_skipped() {
        let no_id = "BEGIN:VCARD\nVERSION:3.0\nEND:VCARD\n";
        let v = temp_vault("noid");
        let out = import_str(&v, no_id);
        assert_eq!(out.counts["skipped"], 1);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(loaded.len(), 0);
    }

    #[test]
    fn rev_basic_iso_is_normalized() {
        assert_eq!(normalize_rev("20240225T020408Z"), "2024-02-25T02:04:08Z");
        assert_eq!(normalize_rev("2024-02-25T02:04:08.382Z"), "2024-02-25T02:04:08.382Z");
    }

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "vcard").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["vcf", "vcard"]);
    }

    // -----------------------------------------------------------------------
    // Defect fixes

    // Apple/iCloud year-less birthday: BDAY;X-APPLE-OMIT-YEAR=1604:1604-MM-DD
    // must yield {month, day} with NO year key — not {year:1604, month, day}.
    #[test]
    fn apple_omit_year_bday_drops_year() {
        let vcf = "\
BEGIN:VCARD
VERSION:3.0
UID:apple-omit-year-test
FN:Eve Apple
BDAY;X-APPLE-OMIT-YEAR=1604:1604-07-04
END:VCARD
";
        let v = temp_vault("apple_bday");
        import_str(&v, vcf);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let c = &loaded[0];
        let bdays = c.extra["birthdays"].as_array().unwrap();
        let date = &bdays[0]["date"];
        assert_eq!(date["month"], 7, "month preserved");
        assert_eq!(date["day"], 4, "day preserved");
        assert!(date.get("year").is_none(), "year must be absent (omit-year), got: {date:?}");
    }

    // Apple sentinel year 1604 without the param (defensive fallback):
    // BDAY:1604-03-15 → {month:3, day:15, no year}.
    #[test]
    fn apple_sentinel_year_1604_dropped_even_without_param() {
        let vcf = "\
BEGIN:VCARD
VERSION:3.0
UID:apple-sentinel-test
FN:Sentinel Sam
BDAY:1604-03-15
END:VCARD
";
        let v = temp_vault("sentinel");
        import_str(&v, vcf);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let c = &loaded[0];
        let bdays = c.extra["birthdays"].as_array().unwrap();
        let date = &bdays[0]["date"];
        assert!(date.get("year").is_none(), "sentinel year 1604 must be stripped: {date:?}");
        assert_eq!(date["month"], 3);
        assert_eq!(date["day"], 15);
    }

    // Group-prefixed unknown properties (item1.X-ABDATE, item3.X-ABDATE) must
    // accumulate under one key `x-abdate`, not two separate `item1.x-abdate`.
    #[test]
    fn group_prefixed_props_accumulate_under_stripped_key() {
        let vcf = "\
BEGIN:VCARD
VERSION:3.0
UID:group-prefix-test
FN:Frank Grouped
item1.X-ABDATE:2000-01-01
item3.X-ABDATE:2005-06-15
item1.X-ABLabel:Anniversary
END:VCARD
";
        let v = temp_vault("grouped");
        import_str(&v, vcf);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let c = &loaded[0];
        // Both X-ABDATE entries must land under the same key.
        assert!(!c.extra.contains_key("item1.x-abdate"), "group-prefixed key must not appear in extra");
        assert!(!c.extra.contains_key("item3.x-abdate"), "group-prefixed key must not appear in extra");
        let abdate = c.extra.get("x-abdate").expect("x-abdate key must exist");
        let arr = abdate.as_array().expect("x-abdate must be an array when there are two entries");
        assert_eq!(arr.len(), 2, "two X-ABDATE entries must accumulate into one array");
        // X-ABLabel also gets stripped.
        assert!(c.extra.contains_key("x-ablabel"), "x-ablabel should be stored under stripped key");
    }

    // `--MM-DD` (ISO 8601:2000 dashed truncated form) must parse month+day.
    #[test]
    fn dashed_yearless_bday_parses_correctly() {
        let vcf = "\
BEGIN:VCARD
VERSION:3.0
UID:dashed-yearless-test
FN:Dasha Dash
BDAY:--12-31
END:VCARD
";
        let v = temp_vault("dashed_bday");
        import_str(&v, vcf);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let c = &loaded[0];
        let bdays = c.extra["birthdays"].as_array().unwrap();
        let date = &bdays[0]["date"];
        assert_eq!(date["month"], 12);
        assert_eq!(date["day"], 31);
        assert!(date.get("year").is_none(), "--MM-DD must have no year: {date:?}");
    }

    // Multiple NOTE fields must be joined into a single String, not an array.
    #[test]
    fn multiple_notes_joined_to_single_string() {
        let vcf = "\
BEGIN:VCARD
VERSION:3.0
UID:multi-note-test
FN:Nina Notes
NOTE:First note.
NOTE:Second note.
END:VCARD
";
        let v = temp_vault("multi_note");
        import_str(&v, vcf);
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let c = &loaded[0];
        let note = c.extra.get("note").expect("note must exist");
        assert!(note.is_string(), "note must always be a String, got: {note:?}");
        let s = note.as_str().unwrap();
        assert!(s.contains("First note"), "first note present: {s:?}");
        assert!(s.contains("Second note"), "second note present: {s:?}");
    }

    // Streaming: 10k contacts must import correctly without loading the whole
    // file into memory at once (exercised via a generated large fixture).
    #[test]
    fn large_export_10k_contacts_streams_correctly() {
        let mut vcf = String::with_capacity(512 * 10_000);
        for i in 0..10_000u32 {
            vcf.push_str(&format!(
                "BEGIN:VCARD\nVERSION:3.0\nUID:uid-{i}\nFN:Contact {i}\nEMAIL:contact{i}@example.com\nEND:VCARD\n"
            ));
        }
        let v = temp_vault("large10k");
        let out = import_str(&v, &vcf);
        assert_eq!(out.counts["imported"], 10_000, "all 10k contacts imported");
        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(loaded.len(), 10_000, "all 10k contacts in snapshot");
        // Spot-check first and last.
        assert!(loaded.iter().any(|c| c.id == "uid-0"), "uid-0 present");
        assert!(loaded.iter().any(|c| c.id == "uid-9999"), "uid-9999 present");
    }
}

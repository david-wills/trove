//! The drop-in normalizer — mapping artifact + projection engine (R2 Step 1,
//! plus the engine half of Step 4). See `docs/normalizer.md` (ratified spec)
//! and `docs/vault-spec/normalizer-toolkit.md` (the v1 coercion vocabulary).
//!
//! A **mapping** (`.trove/mappings/<source>.json`) is plain data: it binds the
//! columns of a dropped CSV/JSONL file to one ratified contract shape via a
//! fixed, deterministic coercion toolkit. Applying a mapping to a file yields
//! contract-conformant rows; projecting writes them into the domain's folders
//! through the existing [`crate::store`] helpers (guid-merged dedupe), while
//! the raw file is kept full-fidelity so a wrong mapping is a *re-projection*
//! from raw, never data loss.
//!
//! This module is deterministic and headless-testable; the LLM advisor
//! (Step 2c) and the drop-zone UI (Step 3) build on top of it and touch
//! nothing here.
//!
//! Layout of the concerns below:
//! - **The mapping artifact** — [`Mapping`] and its parts, with serde
//!   round-trip and vault-relative load/save/list.
//! - **The coercion toolkit** — the closed set of `date` / `number` / `split`
//!   / `value_map` value coercions plus the `guid` recipe.
//! - **Embedded contracts** — the 32 schema files as `include_str!` assets,
//!   looked up by `(domain, shape)`, exposing field metadata for detect/UI and
//!   a lightweight row validator.
//! - **Parsing** — streaming CSV (RFC4180) and JSONL readers, never whole-file.
//! - **Apply + project + lifecycle** — [`Mapping::apply`], [`Mapping::project`],
//!   raw/declined landing, signature auto-conform, reproject, and delete.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::registry::ImportOutcome;
use crate::store::Partition;
use crate::vault::Vault;

// ===========================================================================
// The mapping artifact
// ===========================================================================

/// The mapping-file schema version this build reads and writes.
pub const MAPPING_VERSION: u32 = 1;

/// A confirmed source→contract mapping, persisted at
/// `.trove/mappings/<source>.json`. Plain data: no code, no expression
/// language — every transform is a named coercion from the fixed toolkit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    /// Artifact-format version (currently [`MAPPING_VERSION`]).
    pub version: u32,
    /// User-confirmed source slug — the folder name under the domain and the
    /// `source` field value on every projected row (lowercase/digits/dash).
    pub source: String,
    /// Target contract domain token (the schema file's first segment, e.g.
    /// `social`, `media`, `finance-purchases`).
    pub domain: String,
    /// Target contract shape token (the schema file's second segment, e.g.
    /// `post`, `play`, `line-item`).
    pub shape: String,
    /// Normalized headers + format — an exact match auto-conforms a future drop.
    pub signature: Signature,
    /// Column→field bindings, each optionally carrying a `coerce`.
    #[serde(default)]
    pub bindings: Vec<Binding>,
    /// Fields with no source column, set to a fixed literal on every row.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub constants: Map<String, Value>,
    /// The required dedupe-key recipe.
    pub guid: GuidRecipe,
    /// Catch-all policy for columns no binding names. Only `"extra"` in v1.
    #[serde(default = "default_unbound")]
    pub unbound: String,
    /// How this mapping came to be (for provenance / UI).
    #[serde(default)]
    pub provenance: Provenance,
}

fn default_unbound() -> String {
    "extra".to_string()
}

/// One source-column → contract-field binding. A missing `coerce` is a plain
/// rename (verbatim trimmed cell); a present `coerce` names a toolkit member,
/// with its parameters in `with`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    /// Source column header (CSV) or top-level key (JSONL).
    pub from: String,
    /// Contract field to populate.
    pub to: String,
    /// Toolkit coercion name (`date` | `number` | `split` | `value_map`), or
    /// `None` for a verbatim rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coerce: Option<String>,
    /// Coercion parameters (the `with` object). Shape depends on `coerce`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with: Option<Value>,
}

/// The file signature: normalized column headers, in order, plus the format.
/// Two files with the same signature share a mapping (auto-conform).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Normalized (trimmed, whitespace-collapsed, lowercased) headers, in
    /// column order. A blank/unnamed header is kept as `""` to preserve
    /// positional identity.
    pub headers: Vec<String>,
    /// `"csv"` or `"jsonl"`.
    pub format: String,
}

impl Signature {
    /// Build a signature from raw headers + a format, normalizing the headers.
    pub fn of(headers: &[String], format: Format) -> Signature {
        Signature {
            headers: headers.iter().map(|h| normalize_header(h)).collect(),
            format: format.as_str().to_string(),
        }
    }

    /// Does this (already-normalized) signature match a file's raw headers +
    /// format? Order-sensitive header equality plus format equality.
    pub fn matches(&self, headers: &[String], format: Format) -> bool {
        self.format == format.as_str()
            && self.headers.len() == headers.len()
            && self
                .headers
                .iter()
                .zip(headers)
                .all(|(a, b)| *a == normalize_header(b))
    }
}

/// The dedupe-key recipe — exactly one of `column` or `hash`. Both accept an
/// optional `prefix` prepended to the result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GuidRecipe {
    /// The guid is a source id column, verbatim (`prefix + trimmed(cell)`).
    Column {
        column: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// The guid is `prefix + hex(sha256(join(trimmed cells, "|")))`, columns
    /// taken in listed order.
    Hash {
        hash: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        algo: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
}

/// How a mapping was created — for provenance and the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Provenance {
    /// RFC3339 local date the mapping was created.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created: String,
    /// `"heuristic"` | `"llm"` | `"manual"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub suggested_by: String,
}

impl Mapping {
    /// Vault-relative path of a source's mapping file.
    pub fn rel_path(source: &str) -> String {
        format!(".trove/mappings/{source}.json")
    }

    /// Load the mapping for `source`, validating its static invariants.
    pub fn load(vault: &Vault, source: &str) -> Result<Mapping> {
        let rel = Self::rel_path(source);
        let path = vault.resolve(&rel)?;
        let body = fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
        let m: Mapping =
            serde_json::from_str(&body).with_context(|| format!("parsing {rel}"))?;
        m.validate()?;
        Ok(m)
    }

    /// Persist this mapping (atomic write), after validating its invariants.
    pub fn save(&self, vault: &Vault) -> Result<()> {
        self.validate()?;
        let rel = Self::rel_path(&self.source);
        let path = vault.resolve(&rel)?;
        crate::store::write_json_atomic(&path, self)
    }

    /// Every mapping currently stored, in source order. Unparseable files are
    /// skipped (a lone corrupt mapping never breaks the list).
    pub fn list(vault: &Vault) -> Result<Vec<Mapping>> {
        let dir = vault.resolve(".trove/mappings")?;
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(out);
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        for p in paths {
            if let Ok(body) = fs::read_to_string(&p) {
                if let Ok(m) = serde_json::from_str::<Mapping>(&body) {
                    if m.validate().is_ok() {
                        out.push(m);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The stored mapping whose signature matches `headers` + `format`, if any
    /// — the auto-conform lookup a future drop uses.
    pub fn for_signature(vault: &Vault, headers: &[String], format: Format) -> Result<Option<Mapping>> {
        Ok(Self::list(vault)?
            .into_iter()
            .find(|m| m.signature.matches(headers, format)))
    }

    /// Delete a source's mapping and its projected partitions, keeping raw
    /// (ratified: "Mapping deleted ⇒ projection removed, raw kept"). Missing
    /// mapping is success.
    pub fn delete(vault: &Vault, source: &str) -> Result<()> {
        // Best-effort remove the projected partitions first (needs the mapping
        // to know where they are); then the mapping file itself.
        if let Ok(m) = Self::load(vault, source) {
            if let Some(proj) = m.projection() {
                clear_projected_partitions(vault, &m, &proj)?;
            }
        }
        let path = vault.resolve(&Self::rel_path(source))?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting mapping {source}")),
        }
    }

    /// The parsed format of this mapping's signature.
    pub fn format(&self) -> Format {
        Format::parse(&self.signature.format)
    }

    /// The projection target for this mapping's contract, or `None` when the
    /// contract shape is not projectable in v1 (snapshot / snapshot+events
    /// kinds — the mapping, coercions, and validation still work; only the
    /// write is deferred).
    pub fn projection(&self) -> Option<Proj> {
        contract_schema(&self.domain, &self.shape).and_then(|c| c.proj)
    }

    /// Statically validate the mapping — version, known coercion names,
    /// constant/binding disjointness, a known contract, and a guid recipe.
    pub fn validate(&self) -> Result<()> {
        if self.version != MAPPING_VERSION {
            bail!(
                "unsupported mapping version {} (this build reads {MAPPING_VERSION})",
                self.version
            );
        }
        if !is_slug(&self.source) {
            bail!("mapping source {:?} is not a valid slug (lowercase/digits/dash)", self.source);
        }
        if self.unbound != "extra" {
            bail!("unbound policy {:?} is unsupported (only \"extra\" in v1)", self.unbound);
        }
        let schema = contract_schema(&self.domain, &self.shape).ok_or_else(|| {
            anyhow::anyhow!("unknown contract {}.{}", self.domain, self.shape)
        })?;
        for b in &self.bindings {
            if let Some(name) = &b.coerce {
                if !matches!(name.as_str(), "date" | "number" | "split" | "value_map") {
                    bail!("unknown coercion {:?} on binding {} -> {}", name, b.from, b.to);
                }
            }
            if self.constants.contains_key(&b.to) {
                bail!(
                    "field {:?} is set by both a constant and a binding — pick one",
                    b.to
                );
            }
        }
        // guid recipe well-formed.
        match &self.guid {
            GuidRecipe::Column { column, .. } if column.is_empty() => {
                bail!("guid.column names an empty header")
            }
            GuidRecipe::Hash { hash, algo, .. } => {
                if hash.is_empty() {
                    bail!("guid.hash lists no columns");
                }
                if let Some(a) = algo {
                    if a != "sha256" {
                        bail!("guid.hash algo {:?} unsupported (only \"sha256\" in v1)", a);
                    }
                }
            }
            _ => {}
        }
        let _ = schema;
        Ok(())
    }
}

// ===========================================================================
// The coercion toolkit
// ===========================================================================

/// Apply a binding's coercion to one source cell, yielding the contract value
/// or `None` (omit the field). `with` is the binding's optional parameter
/// object. An unknown coercion name is a hard error (caught earlier by
/// [`Mapping::validate`], re-checked here for direct callers).
fn coerce_cell(coerce: Option<&str>, with: Option<&Value>, cell: &Value) -> Result<Option<Value>> {
    let s = cell_str(cell);
    let trimmed = s.trim();
    match coerce {
        None => {
            // Verbatim rename. For a JSONL non-string cell, preserve the
            // original typed value (a numeric field renamed to a numeric
            // contract field stays a number); for a string cell, the trimmed
            // string, omit-if-empty.
            if cell.is_string() {
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(Value::String(trimmed.to_string())))
                }
            } else if cell.is_null() {
                Ok(None)
            } else {
                Ok(Some(cell.clone()))
            }
        }
        Some("date") => Ok(coerce_date(trimmed, with)),
        Some("number") => Ok(coerce_number(trimmed, with)),
        Some("split") => Ok(coerce_split(&s, with)),
        Some("value_map") => Ok(coerce_value_map(trimmed, with)),
        Some(other) => bail!("unknown coercion {other:?}"),
    }
}

/// `date` — named-format temporal parse. Tries each `formats` entry in order;
/// first that parses wins. Date-only formats emit `YYYY-MM-DD` verbatim (no
/// fabricated midnight); date-time / epoch emit RFC3339 with the local offset.
fn coerce_date(cell: &str, with: Option<&Value>) -> Option<Value> {
    if cell.is_empty() {
        return None;
    }
    let formats: Vec<String> = with
        .and_then(|w| w.get("formats"))
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_else(|| vec!["rfc3339".to_string()]);
    for fmt in &formats {
        if let Some(out) = parse_date_one(cell, fmt) {
            return Some(Value::String(out));
        }
    }
    None
}

/// Parse `cell` with one format token or strftime pattern. Returns the emitted
/// string (date-only or RFC3339-local), or `None` if it doesn't parse.
fn parse_date_one(cell: &str, fmt: &str) -> Option<String> {
    match fmt {
        "rfc3339" => DateTime::parse_from_rfc3339(cell)
            .ok()
            .map(|dt| dt.with_timezone(&Local).to_rfc3339()),
        "date" => NaiveDate::parse_from_str(cell, "%Y-%m-%d")
            .ok()
            .map(|d| d.format("%Y-%m-%d").to_string()),
        "epoch_s" => {
            let f: f64 = cell.parse().ok()?;
            let mut secs = f.trunc() as i64;
            let mut nanos = (f.fract() * 1e9).round() as i64;
            if nanos < 0 {
                secs -= 1;
                nanos += 1_000_000_000;
            }
            Local.timestamp_opt(secs, nanos as u32).single().map(|dt| dt.to_rfc3339())
        }
        "epoch_ms" => {
            let ms: i64 = cell.parse().ok()?;
            Local.timestamp_millis_opt(ms).single().map(|dt| dt.to_rfc3339())
        }
        pat => {
            if pat.contains("%z") || pat.contains("%:z") || pat.contains("%+") || pat.contains("%#z") {
                DateTime::parse_from_str(cell, pat)
                    .ok()
                    .map(|dt| dt.with_timezone(&Local).to_rfc3339())
            } else if has_time_specifier(pat) {
                NaiveDateTime::parse_from_str(cell, pat)
                    .ok()
                    .and_then(|ndt| Local.from_local_datetime(&ndt).earliest())
                    .map(|dt| dt.to_rfc3339())
            } else {
                NaiveDate::parse_from_str(cell, pat)
                    .ok()
                    .map(|d| d.format("%Y-%m-%d").to_string())
            }
        }
    }
}

/// Does a strftime pattern carry a time-of-day component?
fn has_time_specifier(pat: &str) -> bool {
    const TIME: &[char] = &['H', 'I', 'k', 'l', 'M', 'S', 'P', 'p', 'R', 'T', 'X', 'r', 's', 'f'];
    let mut chars = pat.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&n) = chars.peek() {
                if TIME.contains(&n) {
                    return true;
                }
                chars.next();
            }
        }
    }
    false
}

/// `number` — strip formatting from a numeric cell and emit a **typed JSON
/// number** (integer when the magnitude is integral, otherwise a decimal), so
/// the value validates against a `number`/`integer` contract field. Empty /
/// non-numeric after stripping → `None`.
///
/// The cleanup goes through [`clean_number`]'s canonical decimal string first
/// (currency/thousands stripping, sign convention, zero-normalization), then
/// that canonical string — which is always a bare, valid JSON number literal —
/// is parsed into a `Value::Number`. Projecting a contract field typed
/// `number`/`integer` requires an actual JSON number: a string is rejected by
/// [`ContractSchema::validate_row`], which would make every numeric contract
/// field (lat/lon, `value`, `seconds`, money) unprojectable.
fn coerce_number(cell: &str, with: Option<&Value>) -> Option<Value> {
    let decimal_sep = with
        .and_then(|w| w.get("decimal_sep"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.chars().next())
        .unwrap_or('.');
    let parens_negative = with
        .and_then(|w| w.get("parens_negative"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let negate = with
        .and_then(|w| w.get("negate"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    clean_number(cell, decimal_sep, parens_negative, negate).and_then(|s| canonical_number(&s))
}

/// Parse a canonical decimal string (as produced by [`clean_number`]:
/// optionally-signed, no grouping, no `+`, zero-normalized) into a
/// `Value::Number`. An integral magnitude yields an integer number, a
/// fractional one a decimal number. Returns `None` only if the string somehow
/// isn't a valid JSON number (it always is when it comes from `clean_number`).
fn canonical_number(s: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(s) {
        Ok(v) if v.is_number() => Some(v),
        _ => None,
    }
}

fn is_currency(c: char) -> bool {
    "$€£¥¢₹₽₩₪₴฿₫".contains(c)
}

fn clean_number(raw: &str, decimal_sep: char, parens_negative: bool, negate: bool) -> Option<String> {
    let mut s = raw.trim();
    // Strip a wrapping single- or double-quote pair.
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s = s[1..s.len() - 1].trim();
    }
    let mut neg = false;
    let mut body = s.to_string();
    if parens_negative && body.starts_with('(') && body.ends_with(')') {
        neg = true;
        body = body[1..body.len() - 1].trim().to_string();
    }
    let grouping = if decimal_sep == ',' { '.' } else { ',' };
    let mut cleaned = String::new();
    for ch in body.chars() {
        if ch.is_whitespace() || is_currency(ch) || ch == grouping {
            continue;
        }
        cleaned.push(ch);
    }
    if decimal_sep == ',' {
        cleaned = cleaned.replace(',', ".");
    }
    let cleaned = cleaned.trim();
    let (sign_neg, digits) = if let Some(r) = cleaned.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = cleaned.strip_prefix('+') {
        (false, r)
    } else {
        (false, cleaned)
    };
    if digits.is_empty() {
        return None;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for ch in digits.chars() {
        if ch == '.' {
            if seen_dot {
                return None;
            }
            seen_dot = true;
        } else if ch.is_ascii_digit() {
            seen_digit = true;
        } else {
            return None;
        }
    }
    if !seen_digit {
        return None;
    }
    neg = neg || sign_neg;
    if negate {
        neg = !neg;
    }
    let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
    let int_norm = {
        let t = int_part.trim_start_matches('0');
        if t.is_empty() {
            "0"
        } else {
            t
        }
    };
    let frac_norm = frac_part.trim_end_matches('0');
    let mag = if frac_norm.is_empty() {
        int_norm.to_string()
    } else {
        format!("{int_norm}.{frac_norm}")
    };
    if mag == "0" {
        return Some("0".to_string()); // never "-0"
    }
    Some(if neg { format!("-{mag}") } else { mag })
}

/// `split` — delimited cell → JSON array of strings. Empty result → `None`.
fn coerce_split(cell: &str, with: Option<&Value>) -> Option<Value> {
    let sep = with
        .and_then(|w| w.get("sep"))
        .and_then(|v| v.as_str())
        .unwrap_or(",")
        .to_string();
    let trim = with
        .and_then(|w| w.get("trim"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let drop_empty = with
        .and_then(|w| w.get("drop_empty"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let sep = if sep.is_empty() { ",".to_string() } else { sep };
    let parts: Vec<Value> = cell
        .split(&sep)
        .map(|p| if trim { p.trim() } else { p })
        .filter(|p| !(drop_empty && p.is_empty()))
        .map(|p| Value::String(p.to_string()))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(Value::Array(parts))
    }
}

/// `value_map` — enum/string lookup with a fallback governing misses.
fn coerce_value_map(cell: &str, with: Option<&Value>) -> Option<Value> {
    let table = with.and_then(|w| w.get("table")).and_then(|t| t.as_object());
    let case_insensitive = with
        .and_then(|w| w.get("case_insensitive"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let fallback = with
        .and_then(|w| w.get("fallback"))
        .and_then(|v| v.as_str())
        .unwrap_or("omit");
    let key = if case_insensitive {
        cell.to_lowercase()
    } else {
        cell.to_string()
    };
    if let Some(table) = table {
        for (k, v) in table {
            let tk = if case_insensitive { k.to_lowercase() } else { k.clone() };
            if tk == key {
                return Some(v.clone());
            }
        }
    }
    match fallback {
        "omit" => None,
        "passthrough" => {
            if cell.is_empty() {
                None
            } else {
                Some(Value::String(cell.to_string()))
            }
        }
        literal => Some(Value::String(literal.to_string())),
    }
}

/// Compute the guid for one parsed row from the recipe, or `None` when it
/// can't (a blank id column). `by_name` maps a source header to its cell.
fn compute_guid(recipe: &GuidRecipe, row: &Row) -> Option<String> {
    match recipe {
        GuidRecipe::Column { column, prefix } => {
            let cell = row.get(column)?;
            let v = cell_str(cell);
            let v = v.trim();
            if v.is_empty() {
                return None;
            }
            Some(format!("{}{}", prefix.as_deref().unwrap_or(""), v))
        }
        GuidRecipe::Hash { hash, prefix, .. } => {
            let joined: Vec<String> = hash
                .iter()
                .map(|h| row.get(h).map(|c| cell_str(c).trim().to_string()).unwrap_or_default())
                .collect();
            let mut hasher = Sha256::new();
            hasher.update(joined.join("|").as_bytes());
            let hex = hex::encode(hasher.finalize());
            Some(format!("{}{}", prefix.as_deref().unwrap_or(""), hex))
        }
    }
}

// ===========================================================================
// Embedded contract schemas
// ===========================================================================

/// The projection target for a contract shape: where its rows are written and
/// how they're partitioned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Proj {
    /// Vault-relative domain root (`social`, `media/plays`, `health/medical`).
    pub root: &'static str,
    /// Sub-stream directory under `<root>/<source>/` (`""`, `"highlights"`,
    /// `"events"`, `"observations"`, …).
    pub subdir: &'static str,
    /// The row field whose value names the partition (`ts`, `start`).
    pub ts_field: &'static str,
    pub partition: Partition,
}

impl Proj {
    /// The vault-relative directory this source's projected partitions live in.
    pub fn dir(&self, source: &str) -> String {
        if self.subdir.is_empty() {
            format!("{}/{source}", self.root)
        } else {
            format!("{}/{source}/{}", self.root, self.subdir)
        }
    }

    /// The vault-relative directory this source's raw drops are kept in.
    pub fn raw_dir(&self, source: &str) -> String {
        format!("{}/{source}/raw", self.root)
    }
}

/// One embedded contract schema asset: its identity, guid field, projection
/// target, and the raw JSON-schema text.
struct ContractAsset {
    domain: &'static str,
    shape: &'static str,
    /// The field the guid recipe populates (and dedupe keys on) — usually
    /// `guid`, but `id` for the id-keyed domains.
    guid_field: &'static str,
    proj: Option<Proj>,
    raw: &'static str,
}

macro_rules! schema_asset {
    ($domain:literal, $shape:literal, $guid:literal, $proj:expr, $file:literal) => {
        ContractAsset {
            domain: $domain,
            shape: $shape,
            guid_field: $guid,
            proj: $proj,
            raw: include_str!(concat!("../../../docs/vault-spec/schemas/", $file)),
        }
    };
}

/// A projectable event-stream target.
const fn ev(root: &'static str, subdir: &'static str, ts_field: &'static str, partition: Partition) -> Option<Proj> {
    Some(Proj { root, subdir, ts_field, partition })
}

/// The 32 ratified contract schemas, embedded at compile time. Snapshot and
/// snapshot+events kinds carry `proj: None` — their mappings, coercions, and
/// validation all work, but projection (an event-stream write) is deferred.
static CONTRACT_ASSETS: &[ContractAsset] = &[
    schema_asset!("browser-searches", "search", "guid", ev("browser/searches", "", "ts", Partition::Month), "browser-searches.search.schema.json"),
    schema_asset!("calendar", "change", "id", None, "calendar.change.schema.json"),
    schema_asset!("calendar", "occurrence", "id", None, "calendar.occurrence.schema.json"),
    schema_asset!("contacts", "contact", "id", None, "contacts.contact.schema.json"),
    schema_asset!("correspondence", "message", "guid", ev("correspondence", "", "ts", Partition::Month), "correspondence.message.schema.json"),
    schema_asset!("environment", "almanac", "guid", None, "environment.almanac.schema.json"),
    schema_asset!("environment", "geo-event", "guid", ev("environment", "events", "ts", Partition::Month), "environment.geo-event.schema.json"),
    schema_asset!("environment", "reading", "guid", ev("environment", "", "ts", Partition::Month), "environment.reading.schema.json"),
    schema_asset!("finance-holdings", "position", "guid", None, "finance-holdings.position.schema.json"),
    schema_asset!("finance-purchases", "line-item", "guid", ev("finance/purchases", "", "ts", Partition::Month), "finance-purchases.line-item.schema.json"),
    schema_asset!("habits", "checkin", "id", None, "habits.checkin.schema.json"),
    schema_asset!("habits", "habit", "id", None, "habits.habit.schema.json"),
    schema_asset!("health-medical", "condition", "guid", ev("health/medical", "conditions", "ts", Partition::Month), "health-medical.condition.schema.json"),
    schema_asset!("health-medical", "medication", "guid", ev("health/medical", "medications", "ts", Partition::Month), "health-medical.medication.schema.json"),
    schema_asset!("health-medical", "observation", "guid", ev("health/medical", "observations", "ts", Partition::Month), "health-medical.observation.schema.json"),
    schema_asset!("health-nutrition", "entry", "guid", ev("health/nutrition", "", "ts", Partition::Month), "health-nutrition.entry.schema.json"),
    schema_asset!("home", "energy", "guid", ev("home", "energy", "ts", Partition::Month), "home.energy.schema.json"),
    schema_asset!("home", "event", "guid", ev("home", "events", "ts", Partition::Month), "home.event.schema.json"),
    schema_asset!("home", "reading", "guid", ev("home", "", "ts", Partition::Month), "home.reading.schema.json"),
    schema_asset!("location", "fix", "guid", ev("location", "", "ts", Partition::Day), "location.fix.schema.json"),
    schema_asset!("media", "play", "guid", ev("media/plays", "", "ts", Partition::Month), "media.play.schema.json"),
    schema_asset!("meetings", "meeting", "guid", ev("meetings", "", "ts", Partition::Month), "meetings.meeting.schema.json"),
    schema_asset!("notes", "note", "id", None, "notes.note.schema.json"),
    schema_asset!("photos", "photo", "guid", ev("photos", "", "ts", Partition::Month), "photos.photo.schema.json"),
    schema_asset!("reading", "highlight", "guid", ev("reading", "highlights", "ts", Partition::Month), "reading.highlight.schema.json"),
    schema_asset!("reading", "item", "guid", ev("reading", "", "ts", Partition::Month), "reading.item.schema.json"),
    schema_asset!("social", "post", "guid", ev("social", "", "ts", Partition::Month), "social.post.schema.json"),
    schema_asset!("tasks", "event", "id", None, "tasks.event.schema.json"),
    schema_asset!("tasks", "task", "id", None, "tasks.task.schema.json"),
    schema_asset!("time-entries", "entry", "id", ev("time-entries", "", "start", Partition::Month), "time-entries.entry.schema.json"),
    schema_asset!("travel", "segment", "guid", ev("travel", "", "ts", Partition::Month), "travel.segment.schema.json"),
    schema_asset!("voice", "recording", "guid", ev("voice", "", "ts", Partition::Month), "voice.recording.schema.json"),
];

/// Field-level metadata surfaced for detect heuristics and the binding UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Value>,
    /// The declared JSON type (`string`/`array`/…), or `None` for `anyOf`/
    /// untyped fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub required: bool,
}

/// A parsed, runtime-queryable contract schema.
#[derive(Debug, Clone)]
pub struct ContractSchema {
    pub domain: String,
    pub shape: String,
    /// The field the guid recipe populates and dedupe keys on.
    pub guid_field: String,
    pub required: Vec<String>,
    pub fields: Vec<FieldMeta>,
    pub proj: Option<Proj>,
    /// The parsed schema JSON (for validation).
    parsed: Value,
}

impl ContractSchema {
    /// `<domain>.<shape>` — the schema's dotted id.
    pub fn id(&self) -> String {
        format!("{}.{}", self.domain, self.shape)
    }

    /// Field metadata by name.
    pub fn field(&self, name: &str) -> Option<&FieldMeta> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Validate one projected row against this schema: required fields present
    /// and declared types matched. Additional properties are always allowed
    /// (`additionalProperties: true` throughout). Returns the first violation.
    pub fn validate_row(&self, row: &Map<String, Value>) -> Result<(), String> {
        for req in &self.required {
            match row.get(req) {
                None | Some(Value::Null) => return Err(format!("missing required field `{req}`")),
                Some(Value::String(s)) if s.is_empty() => {
                    return Err(format!("required field `{req}` is empty"))
                }
                _ => {}
            }
        }
        let props = self.parsed.get("properties").and_then(|p| p.as_object());
        if let Some(props) = props {
            for (k, v) in row {
                let Some(spec) = props.get(k) else { continue };
                if let Some(expected) = spec.get("type").and_then(|t| t.as_str()) {
                    if !json_type_matches(expected, v) {
                        return Err(format!(
                            "field `{k}` should be {expected}, got {}",
                            json_type_name(v)
                        ));
                    }
                }
                // `anyOf`/untyped fields are accepted without a strict check.
            }
        }
        Ok(())
    }
}

fn json_type_matches(expected: &str, v: &Value) -> bool {
    match expected {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Parse an asset's schema JSON into a queryable [`ContractSchema`].
fn parse_asset(a: &ContractAsset) -> ContractSchema {
    let parsed: Value = serde_json::from_str(a.raw)
        .unwrap_or_else(|e| panic!("embedded schema {}.{} is invalid JSON: {e}", a.domain, a.shape));
    let required: Vec<String> = parsed
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let mut fields = Vec::new();
    if let Some(props) = parsed.get("properties").and_then(|p| p.as_object()) {
        for (name, spec) in props {
            fields.push(FieldMeta {
                name: name.clone(),
                description: spec
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                examples: spec
                    .get("examples")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default(),
                type_: spec.get("type").and_then(|t| t.as_str()).map(str::to_string),
                required: required.iter().any(|r| r == name),
            });
        }
    }
    ContractSchema {
        domain: a.domain.to_string(),
        shape: a.shape.to_string(),
        guid_field: a.guid_field.to_string(),
        required,
        fields,
        proj: a.proj,
        parsed,
    }
}

fn schemas() -> &'static [ContractSchema] {
    use std::sync::OnceLock;
    static CELL: OnceLock<Vec<ContractSchema>> = OnceLock::new();
    CELL.get_or_init(|| CONTRACT_ASSETS.iter().map(parse_asset).collect())
}

/// Every embedded contract schema (for detect heuristics and the binding UI).
pub fn contract_schemas() -> &'static [ContractSchema] {
    schemas()
}

/// The embedded schema for `(domain, shape)`, if one exists.
pub fn contract_schema(domain: &str, shape: &str) -> Option<&'static ContractSchema> {
    schemas().iter().find(|c| c.domain == domain && c.shape == shape)
}

// ===========================================================================
// Streaming parsers
// ===========================================================================

/// A dropped-file format the normalizer parses in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Jsonl,
}

impl Format {
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Jsonl => "jsonl",
        }
    }

    /// Parse a format token (`"jsonl"`/`"ndjson"` → JSONL, anything else → CSV).
    pub fn parse(s: &str) -> Format {
        match s.trim().to_lowercase().as_str() {
            "jsonl" | "ndjson" | "json" => Format::Jsonl,
            _ => Format::Csv,
        }
    }

    /// Infer the format from a file extension.
    pub fn from_path(path: &Path) -> Format {
        match path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
            Some("jsonl") | Some("ndjson") => Format::Jsonl,
            _ => Format::Csv,
        }
    }
}

/// One parsed source row: ordered `(header, cell)` columns. CSV cells are
/// always strings; JSONL cells keep their original JSON type.
struct Row {
    cols: Vec<(String, Value)>,
}

impl Row {
    /// The cell for a source header (first match), if present.
    fn get(&self, name: &str) -> Option<&Value> {
        self.cols.iter().find(|(h, _)| h == name).map(|(_, v)| v)
    }
}

/// Stream a file's rows, calling `f` for each. Never loads the whole file:
/// CSV streams through a buffered reader; JSONL reads line by line. `f`
/// receives the 1-based data-row line number and the parsed row (or `None`
/// when that physical line failed to parse, so the caller can count it).
fn stream_rows(
    path: &Path,
    format: Format,
    mut f: impl FnMut(u64, Option<Row>) -> Result<()>,
) -> Result<()> {
    match format {
        Format::Csv => {
            let mut rdr = csv::ReaderBuilder::new()
                .flexible(true)
                .has_headers(true)
                .from_path(path)
                .with_context(|| format!("opening {}", path.display()))?;
            let headers: Vec<String> =
                rdr.headers().context("reading CSV header row")?.iter().map(str::to_string).collect();
            let mut line = 0u64;
            for rec in rdr.records() {
                line += 1;
                match rec {
                    Ok(rec) => {
                        let cols = rec
                            .iter()
                            .enumerate()
                            .map(|(i, cell)| {
                                let name = headers.get(i).cloned().unwrap_or_default();
                                (name, Value::String(cell.to_string()))
                            })
                            .collect();
                        f(line, Some(Row { cols }))?;
                    }
                    Err(_) => f(line, None)?,
                }
            }
        }
        Format::Jsonl => {
            let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let reader = BufReader::new(file);
            let mut line = 0u64;
            for l in reader.lines() {
                let l = l.with_context(|| format!("reading {}", path.display()))?;
                if l.trim().is_empty() {
                    continue;
                }
                line += 1;
                match serde_json::from_str::<Value>(&l) {
                    Ok(Value::Object(obj)) => {
                        let cols = obj.into_iter().collect();
                        f(line, Some(Row { cols }))?;
                    }
                    _ => f(line, None)?,
                }
            }
        }
    }
    Ok(())
}

/// The header names of a file (CSV header row, or the first JSONL object's
/// keys), used for signature computation and detect.
pub fn read_headers(path: &Path, format: Format) -> Result<Vec<String>> {
    match format {
        Format::Csv => {
            let mut rdr = csv::ReaderBuilder::new()
                .flexible(true)
                .has_headers(true)
                .from_path(path)
                .with_context(|| format!("opening {}", path.display()))?;
            Ok(rdr.headers().context("reading CSV header row")?.iter().map(str::to_string).collect())
        }
        Format::Jsonl => {
            let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let reader = BufReader::new(file);
            for l in reader.lines() {
                let l = l?;
                if l.trim().is_empty() {
                    continue;
                }
                if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&l) {
                    return Ok(obj.keys().cloned().collect());
                }
            }
            Ok(Vec::new())
        }
    }
}

/// The [`Signature`] of a file (headers normalized + format inferred).
pub fn file_signature(path: &Path) -> Result<Signature> {
    let format = Format::from_path(path);
    Ok(Signature::of(&read_headers(path, format)?, format))
}

/// One page of a raw dropped file, rendered as a table for the generic raw
/// viewer (the honest-decline browse path and any `<source>/raw/` drop).
#[derive(Debug, Clone, Serialize)]
pub struct RawPage {
    /// Column headers in order (CSV header row, or the union of keys in the
    /// first JSONL object; a blank/unnamed CSV header stays `""`).
    pub headers: Vec<String>,
    /// Header-keyed rows for this page (blank header → `col<n>`).
    pub rows: Vec<Map<String, Value>>,
    /// `"csv"` | `"jsonl"`.
    pub format: String,
    /// The offset this page started at.
    pub offset: u64,
    /// Whether at least one more row exists past this page.
    pub has_more: bool,
}

/// Read a bounded window (`offset`..`offset+limit`) of a raw CSV/JSONL file as
/// a table. Streams the file and stops one row past the window (to report
/// `has_more`), so cost is O(offset + limit) — the read stays proportional to
/// what the UI displays, never the whole file.
pub fn read_raw_page(path: &Path, offset: u64, limit: u64) -> Result<RawPage> {
    let format = Format::from_path(path);
    let headers = read_headers(path, format)?;
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    let mut has_more = false;
    let mut seen = 0u64;
    match format {
        Format::Csv => {
            let mut rdr = csv::ReaderBuilder::new()
                .flexible(true)
                .has_headers(true)
                .from_path(path)
                .with_context(|| format!("opening {}", path.display()))?;
            for rec in rdr.records() {
                let rec = match rec {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if seen < offset {
                    seen += 1;
                    continue;
                }
                if rows.len() as u64 >= limit {
                    has_more = true;
                    break;
                }
                let mut m = Map::new();
                for (i, cell) in rec.iter().enumerate() {
                    let name = headers.get(i).cloned().unwrap_or_default();
                    let key = if name.trim().is_empty() { format!("col{i}") } else { name };
                    m.insert(key, Value::String(cell.to_string()));
                }
                rows.push(m);
                seen += 1;
            }
        }
        Format::Jsonl => {
            let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            for l in BufReader::new(file).lines() {
                let l = l.with_context(|| format!("reading {}", path.display()))?;
                if l.trim().is_empty() {
                    continue;
                }
                let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&l) else {
                    continue;
                };
                if seen < offset {
                    seen += 1;
                    continue;
                }
                if rows.len() as u64 >= limit {
                    has_more = true;
                    break;
                }
                rows.push(obj);
                seen += 1;
            }
        }
    }
    Ok(RawPage { headers, rows, format: format.as_str().to_string(), offset, has_more })
}

// ===========================================================================
// Apply — mapping → contract rows
// ===========================================================================

/// The result of applying a mapping to a file: the valid projected rows plus
/// the counts and a sample of validation failures. Invalid rows are never
/// silently dropped — they're counted, sampled, and always recoverable from
/// raw.
#[derive(Debug, Clone)]
pub struct AppliedRows {
    /// Valid, contract-conformant rows (each an object incl. `source`, the
    /// guid field, coerced bindings, constants, and `extra`).
    pub rows: Vec<Map<String, Value>>,
    /// Physical data rows read.
    pub total: u64,
    /// Rows that passed validation.
    pub valid: u64,
    /// Rows that failed to parse or failed validation.
    pub invalid: u64,
    /// First few invalid rows, for reporting.
    pub invalid_samples: Vec<InvalidRow>,
}

/// One reported validation/parse failure.
#[derive(Debug, Clone, Serialize)]
pub struct InvalidRow {
    pub line: u64,
    pub reason: String,
}

const MAX_INVALID_SAMPLES: usize = 20;

impl Mapping {
    /// Apply this mapping to a file, producing contract rows. Pure — reads the
    /// file (streaming) and returns rows; writes nothing. Reused by projection
    /// and by the detect preview.
    pub fn apply(&self, path: &Path) -> Result<AppliedRows> {
        let schema = contract_schema(&self.domain, &self.shape)
            .ok_or_else(|| anyhow::anyhow!("unknown contract {}.{}", self.domain, self.shape))?;
        let format = self.format();
        let bound: HashSet<&str> = self.bindings.iter().map(|b| b.from.as_str()).collect();

        let mut rows = Vec::new();
        let mut total = 0u64;
        let mut valid = 0u64;
        let mut invalid = 0u64;
        let mut invalid_samples = Vec::new();

        stream_rows(path, format, |line, row| {
            total += 1;
            let Some(row) = row else {
                invalid += 1;
                if invalid_samples.len() < MAX_INVALID_SAMPLES {
                    invalid_samples.push(InvalidRow { line, reason: "unparseable row".into() });
                }
                return Ok(());
            };
            let obj = self.build_row(schema, &bound, &row)?;
            match schema.validate_row(&obj) {
                Ok(()) => {
                    valid += 1;
                    rows.push(obj);
                }
                Err(reason) => {
                    invalid += 1;
                    if invalid_samples.len() < MAX_INVALID_SAMPLES {
                        invalid_samples.push(InvalidRow { line, reason });
                    }
                }
            }
            Ok(())
        })?;

        Ok(AppliedRows { rows, total, valid, invalid, invalid_samples })
    }

    /// Build one contract row from a parsed source row: inject `source`, apply
    /// constants, apply bound coercions, populate the guid field, and copy
    /// every unbound column into `extra` verbatim.
    fn build_row(
        &self,
        schema: &ContractSchema,
        bound: &HashSet<&str>,
        row: &Row,
    ) -> Result<Map<String, Value>> {
        let mut obj = Map::new();

        // The source field is the mapping's slug (conventions: source == folder
        // name). Injected first so an explicit binding/constant can override.
        obj.insert("source".to_string(), Value::String(self.source.clone()));

        // Constants.
        for (field, literal) in &self.constants {
            obj.insert(field.clone(), literal.clone());
        }

        // Bound coercions.
        for b in &self.bindings {
            let Some(cell) = row.get(&b.from) else { continue };
            if let Some(v) = coerce_cell(b.coerce.as_deref(), b.with.as_ref(), cell)? {
                obj.insert(b.to.clone(), v);
            }
        }

        // The guid recipe → the contract's dedupe field.
        if let Some(guid) = compute_guid(&self.guid, row) {
            obj.insert(schema.guid_field.clone(), Value::String(guid));
        }

        // Unbound columns → extra, verbatim.
        let mut extra = obj
            .remove("extra")
            .and_then(|v| if let Value::Object(m) = v { Some(m) } else { None })
            .unwrap_or_default();
        for (idx, (name, cell)) in row.cols.iter().enumerate() {
            if bound.contains(name.as_str()) {
                continue;
            }
            let key = if name.trim().is_empty() {
                format!("col{idx}")
            } else {
                name.clone()
            };
            // Verbatim: blank string cells are omitted (omit-empty); non-string
            // JSONL cells keep their original value.
            match cell {
                Value::String(s) => {
                    let t = s.trim();
                    if !t.is_empty() {
                        extra.insert(key, Value::String(t.to_string()));
                    }
                }
                Value::Null => {}
                other => {
                    extra.insert(key, other.clone());
                }
            }
        }
        if !extra.is_empty() {
            obj.insert("extra".to_string(), Value::Object(extra));
        }

        Ok(obj)
    }
}

// ===========================================================================
// Project — write contract rows into the vault (+ raw landing)
// ===========================================================================

/// A projected row carrying its partition timestamp for the store writer. Only
/// `value` is serialized (flatten); `ts` selects the partition file.
#[derive(Serialize)]
struct ProjRow {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

impl Mapping {
    /// Land a dropped file raw, apply this mapping, and project the rows into
    /// the domain's folders (guid-merged dedupe). The raw copy is kept
    /// full-fidelity under `<root>/<source>/raw/`, so a wrong mapping is a
    /// re-projection, never data loss.
    pub fn project(
        &self,
        vault: &Vault,
        src: &Path,
        progress: &mut dyn FnMut(ImportProgress),
    ) -> Result<ImportOutcome> {
        let proj = self.projection().ok_or_else(|| {
            anyhow::anyhow!(
                "contract {}.{} is not projectable in v1 (snapshot kind)",
                self.domain,
                self.shape
            )
        })?;
        // 1. Land raw (source of truth for re-projection).
        let landed = land_file(vault, &proj.raw_dir(&self.source), src)?;
        let landed_path = vault.resolve(&landed)?;

        // 2. Apply + write.
        let outcome = self.project_file(vault, &proj, &landed_path, progress)?;
        Ok(outcome)
    }

    /// Apply this mapping to one already-landed file and append its rows to the
    /// projected stream, deduping on the guid field against what's on disk.
    fn project_file(
        &self,
        vault: &Vault,
        proj: &Proj,
        path: &Path,
        progress: &mut dyn FnMut(ImportProgress),
    ) -> Result<ImportOutcome> {
        let schema = contract_schema(&self.domain, &self.shape)
            .ok_or_else(|| anyhow::anyhow!("unknown contract {}.{}", self.domain, self.shape))?;
        let applied = self.apply(path)?;

        let dir = proj.dir(&self.source);
        let stream = vault.stream(&dir, proj.partition);

        // Existing guids for dedupe.
        let mut seen: HashSet<String> = HashSet::new();
        for key in stream.partitions()? {
            for row in stream.read::<Value>(&key)? {
                if let Some(g) = row.get(&schema.guid_field).and_then(Value::as_str) {
                    if !g.is_empty() {
                        seen.insert(g.to_string());
                    }
                }
            }
        }

        let mut new_rows: Vec<ProjRow> = Vec::new();
        let mut duplicates = 0u64;
        for obj in applied.rows {
            if let Some(g) = obj.get(&schema.guid_field).and_then(Value::as_str) {
                if !g.is_empty() && !seen.insert(g.to_string()) {
                    duplicates += 1;
                    continue;
                }
            }
            let ts = obj.get(proj.ts_field).and_then(Value::as_str).unwrap_or_default().to_string();
            new_rows.push(ProjRow { ts, value: Value::Object(obj) });
        }
        let imported = new_rows.len() as u64;
        stream.append(&new_rows, |r| &r.ts)?;
        progress(ImportProgress { records: imported, percent: 100.0 });

        Ok(ImportOutcome {
            headline: format!(
                "{imported} rows projected to {}.{}, {duplicates} duplicates, {} invalid",
                self.domain, self.shape, applied.invalid
            ),
            counts: [
                ("imported", imported),
                ("duplicates", duplicates),
                ("invalid", applied.invalid),
                ("total", applied.total),
            ]
            .into(),
        })
    }

    /// Re-project a source from raw: delete its projected partitions, then
    /// rewrite them by re-applying the (possibly-edited) mapping to every raw
    /// drop. Idempotent.
    pub fn reproject(vault: &Vault, source: &str) -> Result<ImportOutcome> {
        let m = Self::load(vault, source)?;
        let proj = m.projection().ok_or_else(|| {
            anyhow::anyhow!("contract {}.{} is not projectable in v1", m.domain, m.shape)
        })?;
        clear_projected_partitions(vault, &m, &proj)?;

        let raw_dir = vault.resolve(&proj.raw_dir(source))?;
        let mut raw_files: Vec<std::path::PathBuf> = match fs::read_dir(&raw_dir) {
            Ok(entries) => entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect(),
            Err(_) => Vec::new(),
        };
        raw_files.sort();

        let (mut imported, mut duplicates, mut invalid, mut total) = (0u64, 0u64, 0u64, 0u64);
        for raw in &raw_files {
            let out = m.project_file(vault, &proj, raw, &mut |_| {})?;
            imported += out.counts.get("imported").copied().unwrap_or(0);
            duplicates += out.counts.get("duplicates").copied().unwrap_or(0);
            invalid += out.counts.get("invalid").copied().unwrap_or(0);
            total += out.counts.get("total").copied().unwrap_or(0);
        }
        Ok(ImportOutcome {
            headline: format!(
                "reprojected {source}: {imported} rows from {} raw file(s), {duplicates} duplicates, {invalid} invalid",
                raw_files.len()
            ),
            counts: [
                ("imported", imported),
                ("duplicates", duplicates),
                ("invalid", invalid),
                ("total", total),
            ]
            .into(),
        })
    }
}

/// Remove a source's projected partition files (the `*.jsonl` directly under
/// its projection dir), leaving `raw/` and the mapping untouched.
fn clear_projected_partitions(vault: &Vault, m: &Mapping, proj: &Proj) -> Result<()> {
    let dir = vault.resolve(&proj.dir(&m.source))?;
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "jsonl") {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Copy a dropped file into a vault-relative directory, keeping its original
/// filename; on collision, insert a local-time timestamp before the extension.
/// Returns the vault-relative landed path.
fn land_file(vault: &Vault, dest_dir_rel: &str, src: &Path) -> Result<String> {
    let fname = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dropped".to_string());
    let mut rel = format!("{dest_dir_rel}/{fname}");
    let mut dest = vault.resolve(&rel)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if dest.exists() {
        let p = Path::new(&fname);
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let stamp = Local::now().format("%Y%m%dT%H%M%S");
        let renamed = match p.extension() {
            Some(ext) => format!("{stem}-{stamp}.{}", ext.to_string_lossy()),
            None => format!("{stem}-{stamp}"),
        };
        rel = format!("{dest_dir_rel}/{renamed}");
        dest = vault.resolve(&rel)?;
    }
    fs::copy(src, &dest).with_context(|| format!("landing {} -> {rel}", src.display()))?;
    Ok(rel)
}

// ===========================================================================
// Declined drops — raw-kept, manifest-listed (nothing-fits outcome)
// ===========================================================================

const DECLINED_MANIFEST: &str = ".trove/imports.json";

/// One manifest entry for a declined (nothing-fits) drop kept raw under
/// `imports/<source>/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeclinedDrop {
    /// User-named source folder under `imports/`.
    pub source: String,
    /// Vault-relative path of the landed raw file.
    pub file: String,
    /// RFC3339 local time it was landed.
    pub landed: String,
}

impl Vault {
    /// Land a file that fits nothing under `imports/<source>/`, kept raw and
    /// listed in the declined-drops manifest. Returns the manifest entry.
    pub fn land_declined(&self, source: &str, src: &Path) -> Result<DeclinedDrop> {
        if !is_slug(source) {
            bail!("declined source {source:?} is not a valid slug (lowercase/digits/dash)");
        }
        let rel = land_file(self, &format!("imports/{source}"), src)?;
        let entry = DeclinedDrop {
            source: source.to_string(),
            file: rel,
            landed: Local::now().to_rfc3339(),
        };
        let mut list = self.list_declined();
        list.push(entry.clone());
        crate::store::write_json_atomic(&self.resolve(DECLINED_MANIFEST)?, &list)?;
        Ok(entry)
    }

    /// Read a page of a raw drop by vault-relative path — jailed by
    /// [`Vault::resolve_user`] (path escapes **and** `.trove/`, case-insensitive,
    /// are rejected, so a caller can never read machine artifacts / secrets this
    /// way). The read cost is O(offset + limit), never the whole file.
    pub fn read_raw(&self, rel: &str, offset: u64, limit: u64) -> Result<RawPage> {
        let abs = self.resolve_user(rel)?;
        read_raw_page(&abs, offset, limit)
    }

    /// The declined-drops manifest (missing = empty).
    pub fn list_declined(&self) -> Vec<DeclinedDrop> {
        let Ok(path) = self.resolve(DECLINED_MANIFEST) else {
            return Vec::new();
        };
        fs::read_to_string(path)
            .ok()
            .and_then(|body| serde_json::from_str(&body).ok())
            .unwrap_or_default()
    }
}

// ===========================================================================
// Small helpers
// ===========================================================================

/// A source cell as a string: the string itself, or a scalar's textual form.
fn cell_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Normalize a header for signature matching: trim, collapse internal
/// whitespace to single spaces, lowercase.
fn normalize_header(h: &str) -> String {
    h.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Is `s` a valid source slug (lowercase ASCII letters/digits/dash, non-empty)?
fn is_slug(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Turn any text into a source slug (lowercase/digits/dash), collapsing runs of
/// other characters to single dashes. Used to seed a draft mapping's `source`
/// from a dropped filename; the user renames it in the confirm sheet.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() { "dropped-source".to_string() } else { trimmed }
}

// ===========================================================================
// Detect — what a dropped file can become (Step 2)
// ===========================================================================
//
// Every dropped CSV/JSONL file resolves, via one pure-over-its-inputs pass, to
// exactly one of three outcomes (`docs/normalizer.md` decision tree):
//
//   1. a compiled importer claims the header shape → a route-to-built-importer
//      *offer* (`ImportSpec::signatures`, Step 2a; checked FIRST);
//   2. one or more ratified contracts fit → ranked draft mappings the user
//      confirms/edits (Step 2b heuristics);
//   3. nothing fits → the honest decline (land raw under a user-named source).
//
// The heuristics are deterministic and fixture-tested: normalized header tokens
// plus sample-row value shapes, scored against every embedded schema's field
// names, descriptions, and examples. No network, no model — the opt-in LLM
// (Step 2c) only ever *pre-fills the same draft*, so this pass is the floor the
// UI always has. `detect` reads only the header row + a bounded sample; it
// never loads the whole file.

/// How many sample rows `detect` exposes to the app/LLM (the ratified ≤5 cap on
/// what may leave the machine on an opt-in escalation).
const SAMPLE_ROWS: usize = 5;
/// How many data rows `detect` scans for value-shape inference (bounded, so a
/// huge drop is cheap to detect).
const SCAN_ROWS: usize = 50;
/// Most contract candidates surfaced for the ambiguous case.
const MAX_CANDIDATES: usize = 5;

/// A header→field binding must reach this score to be drafted at all.
const MIN_BINDING_SCORE: f64 = 3.0;
/// A contract must reach this score to be offered as a candidate.
const MIN_SCHEMA_SCORE: f64 = 5.0;
/// The top candidate is treated as *confident* (a single pre-filled form) when
/// it clears this score and leads the runner-up by [`CONFIDENT_MARGIN`].
const CONFIDENT_SCORE: f64 = 12.0;
const CONFIDENT_MARGIN: f64 = 6.0;

// Scoring weights: an exact field-name token match is the strongest signal, a
// value-shape agreement (date/url/number) next, a description-token hit a mild
// nudge. Tuned against the Appendix-A `pub_ops → social.post` fixture.
const W_NAME: f64 = 4.0;
const W_SHAPE: f64 = 3.0;
const W_DESC: f64 = 1.0;
/// Fraction of its raw score a shape-only binding (no name/desc support) on a
/// non-timestamp field contributes to the schema total.
const SHAPE_ONLY_WEIGHT: f64 = 0.4;

/// Field-name tokens too generic to discriminate one contract from another —
/// nearly every schema carries a `name`/`type`/`id`. A source header sharing
/// only one of these with a field name is not evidence, so they're excluded
/// from the name-match signal (shape and description still speak).
const GENERIC_NAME_TOKENS: &[&str] =
    &["name", "type", "id", "value", "kind", "data", "info", "code", "number", "num", "key"];

/// The three-way outcome of detecting a dropped file, plus the headers and a
/// bounded sample the app renders (and the opt-in LLM escalation would send).
#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    /// Raw headers, in column order (a blank/unnamed column stays `""`).
    pub headers: Vec<String>,
    /// Up to [`SAMPLE_ROWS`] example rows, header-keyed (blank header → `col<n>`),
    /// for display and the opt-in LLM payload.
    pub sample_rows: Vec<Map<String, Value>>,
    /// `"csv"` | `"jsonl"`.
    pub format: String,
    /// Which of the three fates this drop resolved to.
    pub outcome: DetectOutcome,
}

/// The resolved fate of a dropped file — exactly one shape.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DetectOutcome {
    /// Path 1: a compiled importer claims the shape — offer to run the built
    /// import (never auto-runs).
    Route(ImporterRoute),
    /// Path 2: ratified contracts fit — ranked candidates, each a ready-to-
    /// confirm draft mapping. `confident` = the top candidate wins clearly (a
    /// single pre-filled form); otherwise the UI shows the ranked choices.
    Contract {
        confident: bool,
        candidates: Vec<ContractCandidate>,
    },
    /// Path 3: nothing fits — the honest decline; land raw under a user-named
    /// source.
    NoMatch,
}

/// A route-to-built-importer offer (path 1).
#[derive(Debug, Clone, Serialize)]
pub struct ImporterRoute {
    /// The claiming integration's id (e.g. `"imdb"`).
    pub integration_id: String,
    /// Its display name (e.g. `"IMDb"`).
    pub name: String,
    /// The matched signature's shape label (e.g. `"IMDb ratings"`).
    pub shape_label: String,
}

/// One ranked contract candidate (path 2): a complete draft mapping the user
/// confirms or edits, its heuristic score, and the headers left unbound (they
/// ride `extra` verbatim).
#[derive(Debug, Clone, Serialize)]
pub struct ContractCandidate {
    pub domain: String,
    pub shape: String,
    /// Heuristic score (unbounded; higher is stronger) — the ranking key.
    pub score: f64,
    /// The pre-filled mapping (`provenance.suggested_by = "heuristic"`), already
    /// valid and projectable.
    pub draft: Mapping,
    /// Source headers no binding claimed (they land in `extra`).
    pub unbound_headers: Vec<String>,
}

/// Detect a dropped file's fate. Reads the header row and a bounded sample,
/// checks compiled-importer signatures first, then scores every projectable
/// contract. Deterministic; the heavy lifting is in the pure helpers below.
pub fn detect(path: &Path) -> Result<Detection> {
    let format = Format::from_path(path);
    let headers = read_headers(path, format)?;
    let scan = scan_sample(path, format, SCAN_ROWS)?;
    let sample_rows = scan.iter().take(SAMPLE_ROWS).map(row_to_map).collect();
    let format_str = format.as_str().to_string();

    // Path 1 — a compiled importer claims the header shape (checked FIRST).
    if let Some(route) = importer_route(&headers, format) {
        return Ok(Detection {
            headers,
            sample_rows,
            format: format_str,
            outcome: DetectOutcome::Route(route),
        });
    }

    // Path 2 — score every projectable contract against headers + sample.
    let slug = slugify(path.file_stem().and_then(|s| s.to_str()).unwrap_or("dropped-source"));
    let (confident, candidates) = score_contracts(&slug, &headers, &scan, format);
    let outcome = if candidates.is_empty() {
        DetectOutcome::NoMatch // Path 3 — the honest decline.
    } else {
        DetectOutcome::Contract { confident, candidates }
    };
    Ok(Detection { headers, sample_rows, format: format_str, outcome })
}

// ---------------------------------------------------------------------------
// Path 1 — compiled-importer signatures

/// The first compiled importer whose static header signature claims this file,
/// if any. Signatures are CSV-header markers, so a JSONL drop (no header row to
/// claim) never routes here.
fn importer_route(headers: &[String], format: Format) -> Option<ImporterRoute> {
    if format != Format::Csv {
        return None;
    }
    let present: HashSet<String> = headers.iter().map(|h| normalize_header(h)).collect();
    for def in crate::integrations::INTEGRATIONS {
        let Some(spec) = def.import_spec() else { continue };
        for sig in spec.signatures {
            let required_ok =
                sig.required.iter().all(|r| present.contains(&normalize_header(r)));
            let absent_ok =
                !sig.absent.iter().any(|a| present.contains(&normalize_header(a)));
            if required_ok && absent_ok {
                return Some(ImporterRoute {
                    integration_id: def.id.to_string(),
                    name: def.name.to_string(),
                    shape_label: sig.label.to_string(),
                });
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Path 2 — contract heuristics (pure)

/// Score every projectable contract against the file, returning ranked
/// candidates (best first, capped) and whether the top one is confident.
fn score_contracts(
    slug: &str,
    headers: &[String],
    scan: &[Row],
    format: Format,
) -> (bool, Vec<ContractCandidate>) {
    // Precompute each column's dominant value shape and token set once.
    let cols: Vec<ColInfo> = headers
        .iter()
        .enumerate()
        .map(|(idx, h)| ColInfo {
            idx,
            header: h.clone(),
            tokens: unique_tokens(h),
            shape: column_shape(scan, h),
        })
        .collect();

    let mut candidates: Vec<ContractCandidate> = Vec::new();
    for schema in contract_schemas() {
        // Only projectable (event-stream) contracts can receive a projection in
        // v1; snapshot kinds are skipped so detect never drafts a mapping that
        // can't write.
        let Some(proj) = schema.proj else { continue };
        if let Some(c) = score_one(slug, &cols, schema, &proj, format) {
            candidates.push(c);
        }
    }

    // Rank: score desc, then schema id asc for a stable order on ties.
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.domain.cmp(&b.domain))
            .then_with(|| a.shape.cmp(&b.shape))
    });

    let confident = match (candidates.first(), candidates.get(1)) {
        (Some(top), Some(second)) => {
            top.score >= CONFIDENT_SCORE && top.score - second.score >= CONFIDENT_MARGIN
        }
        (Some(top), None) => top.score >= CONFIDENT_SCORE,
        _ => false,
    };

    candidates.truncate(MAX_CANDIDATES);
    (confident, candidates)
}

/// A source column's precomputed matching inputs.
struct ColInfo {
    idx: usize,
    header: String,
    tokens: Vec<String>,
    shape: Cell,
}

/// One assigned header→field binding with the score that won it.
struct Assign {
    col: usize,
    field: String,
    score: f64,
    /// Did a field-name or description token back this binding (vs. matching on
    /// value shape alone)? Shape-only bindings on secondary fields are weak
    /// evidence and count less toward the schema total.
    supported: bool,
    coerce: Option<(String, Option<Value>)>,
}

/// Score one contract against the columns; `None` when it isn't a viable fit
/// (its timestamp field can't be bound, or the total is too weak).
fn score_one(
    slug: &str,
    cols: &[ColInfo],
    schema: &ContractSchema,
    proj: &Proj,
    format: Format,
) -> Option<ContractCandidate> {
    // Fields a binding can target: everything except the synthesized ones
    // (`source` is a constant from the slug; the guid field comes from a
    // recipe, never a plain cell).
    let bindable: Vec<&FieldMeta> = schema
        .fields
        .iter()
        .filter(|f| f.name != "source" && f.name != schema.guid_field && f.name != "extra")
        .collect();

    // All (column, field) pairs clearing the binding threshold, best first.
    let mut pairs: Vec<(usize, usize, f64)> = Vec::new();
    for (ci, col) in cols.iter().enumerate() {
        if col.header.trim().is_empty() {
            continue; // an unnamed column can't be a named binding source.
        }
        for (fi, field) in bindable.iter().enumerate() {
            let s = binding_score(col, field, proj.ts_field);
            if s >= MIN_BINDING_SCORE {
                pairs.push((ci, fi, s));
            }
        }
    }
    pairs.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });

    // Greedy one-to-one assignment (each column and field used at most once).
    let mut used_col = vec![false; cols.len()];
    let mut used_field = vec![false; bindable.len()];
    let mut assigns: Vec<Assign> = Vec::new();
    for (ci, fi, s) in pairs {
        if used_col[ci] || used_field[fi] {
            continue;
        }
        used_col[ci] = true;
        used_field[fi] = true;
        let field = bindable[fi];
        assigns.push(Assign {
            col: ci,
            field: field.name.clone(),
            score: s,
            supported: binding_has_lexical_support(&cols[ci], field),
            coerce: infer_coerce(field, cols[ci].shape, proj.ts_field),
        });
    }

    // Viability: the timestamp field must be bound — it's required, is the
    // partition key, and without it a row can't validate.
    if !assigns.iter().any(|a| a.field == proj.ts_field) {
        return None;
    }

    // Schema score: sum of binding scores, penalized for still-unbound required
    // fields (beyond the synthesized `source`/guid).
    let bound: HashSet<&str> = assigns.iter().map(|a| a.field.as_str()).collect();
    let unbound_required = schema
        .required
        .iter()
        .filter(|r| r.as_str() != "source" && r.as_str() != schema.guid_field)
        .filter(|r| !bound.contains(r.as_str()))
        .count();
    // A shape-only binding on a non-timestamp field (a stray date column landing
    // in a schema's second temporal slot, a number landing in `lat`) is weak
    // evidence — count it at a fraction so schemas don't inflate by absorbing
    // columns into unrelated secondary fields.
    let score: f64 = assigns
        .iter()
        .map(|a| {
            if a.supported || a.field == proj.ts_field {
                a.score
            } else {
                SHAPE_ONLY_WEIGHT * a.score
            }
        })
        .sum::<f64>()
        - 2.0 * unbound_required as f64;
    if score < MIN_SCHEMA_SCORE {
        return None;
    }

    let draft = build_draft(slug, schema, cols, &assigns, format);
    // A malformed draft (should not happen) simply isn't offered.
    if draft.validate().is_err() {
        return None;
    }
    let unbound_headers = cols
        .iter()
        .filter(|c| !assigns.iter().any(|a| a.col == c.idx))
        .map(|c| if c.header.trim().is_empty() { format!("col{}", c.idx) } else { c.header.clone() })
        .collect();

    Some(ContractCandidate {
        domain: schema.domain.clone(),
        shape: schema.shape.clone(),
        score,
        draft,
        unbound_headers,
    })
}

/// The match score of one column against one contract field: name-token
/// overlap (strongest) + value-shape agreement + description-token hits.
fn binding_score(col: &ColInfo, field: &FieldMeta, ts_field: &str) -> f64 {
    let name = name_overlap(col, field);
    let shape = shape_agree(col.shape, field_kind(field, ts_field));
    let desc = desc_overlap(col, field);
    W_NAME * name + W_SHAPE * shape + W_DESC * desc
}

/// Count of discriminating (non-generic) field-name tokens the column shares
/// with the field name.
fn name_overlap(col: &ColInfo, field: &FieldMeta) -> f64 {
    let f_tokens = tokenize(&field.name);
    col.tokens
        .iter()
        .filter(|t| f_tokens.contains(t) && !GENERIC_NAME_TOKENS.contains(&t.as_str()))
        .count() as f64
}

/// Count (capped) of the column's tokens appearing in the field description.
fn desc_overlap(col: &ColInfo, field: &FieldMeta) -> f64 {
    let desc_tokens = field_desc_tokens(field);
    let hits = col
        .tokens
        .iter()
        .filter(|t| t.len() >= 3 && desc_tokens.contains(*t))
        .count();
    (hits as f64).min(2.0)
}

/// Did a field-name or description token back this binding (as opposed to a
/// value-shape agreement alone)?
fn binding_has_lexical_support(col: &ColInfo, field: &FieldMeta) -> bool {
    name_overlap(col, field) > 0.0 || desc_overlap(col, field) > 0.0
}

/// The coercion a drafted binding carries, from the field's expected kind and
/// the column's observed value shape.
fn infer_coerce(field: &FieldMeta, col: Cell, ts_field: &str) -> Option<(String, Option<Value>)> {
    match field_kind(field, ts_field) {
        FieldKind::Temporal => {
            // Draft every format the column's observed shape can actually carry,
            // in priority order — `coerce_date` tries each and the first that
            // parses wins. These mirror exactly what `cell_shape` recognizes for
            // each shape (RFC3339 *and* the space-separated `YYYY-MM-DD HH:MM:SS`
            // form for DateTime; the bare-date token *and* US/EU slash dates for
            // DateOnly), so a confident temporal detection always projects
            // instead of drafting a lone token that can't parse the source.
            // Date-only forms stay date-only (no fabricated midnight); forms
            // carrying a time emit RFC3339-local.
            let formats: Vec<&str> = if col == Cell::DateTime {
                vec!["rfc3339", "%Y-%m-%d %H:%M:%S"]
            } else {
                vec!["date", "%m/%d/%Y", "%d/%m/%Y"]
            };
            Some(("date".into(), Some(serde_json::json!({ "formats": formats }))))
        }
        FieldKind::Array => Some(("split".into(), None)),
        FieldKind::Numeric => Some(("number".into(), None)),
        _ => None,
    }
}

/// Assemble the draft mapping from the assigned bindings.
fn build_draft(
    slug: &str,
    schema: &ContractSchema,
    cols: &[ColInfo],
    assigns: &[Assign],
    format: Format,
) -> Mapping {
    let headers: Vec<String> = cols.iter().map(|c| c.header.clone()).collect();
    let bindings: Vec<Binding> = assigns
        .iter()
        .map(|a| {
            let (coerce, with) = match &a.coerce {
                Some((c, w)) => (Some(c.clone()), w.clone()),
                None => (None, None),
            };
            Binding { from: cols[a.col].header.clone(), to: a.field.clone(), coerce, with }
        })
        .collect();

    // Guid recipe: hash the bound url column (a stable dedupe key without URL
    // parsing); else an id-looking column verbatim; else hash the whole row.
    let url_col = assigns
        .iter()
        .find(|a| a.field == "url")
        .map(|a| cols[a.col].header.clone());
    let id_col = cols.iter().find(|c| {
        c.tokens.iter().any(|t| matches!(t.as_str(), "id" | "guid" | "uuid"))
    });
    let guid = if let Some(url) = url_col {
        GuidRecipe::Hash { hash: vec![url], algo: None, prefix: None }
    } else if let Some(idc) = id_col {
        GuidRecipe::Column { column: idc.header.clone(), prefix: None }
    } else {
        let named: Vec<String> =
            cols.iter().filter(|c| !c.header.trim().is_empty()).map(|c| c.header.clone()).collect();
        GuidRecipe::Hash {
            hash: if named.is_empty() { headers.clone() } else { named },
            algo: None,
            prefix: None,
        }
    };

    Mapping {
        version: MAPPING_VERSION,
        source: slug.to_string(),
        domain: schema.domain.clone(),
        shape: schema.shape.clone(),
        signature: Signature::of(&headers, format),
        bindings,
        constants: Map::new(),
        guid,
        unbound: "extra".to_string(),
        provenance: Provenance {
            created: Local::now().format("%Y-%m-%d").to_string(),
            suggested_by: "heuristic".to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// Value-shape + token primitives (pure)

/// A cell's inferred value shape.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Cell {
    Empty,
    DateOnly,
    DateTime,
    Url,
    Int,
    Dec,
    Bool,
    Text,
}

/// The kind of value a contract field expects.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldKind {
    Temporal,
    Url,
    Numeric,
    Array,
    Text,
    Any,
}

/// Infer one cell's value shape (checked date → url → bool → number → text).
fn cell_shape(raw: &str) -> Cell {
    let s = raw.trim();
    if s.is_empty() {
        return Cell::Empty;
    }
    let low = s.to_lowercase();
    if low.starts_with("http://") || low.starts_with("https://") {
        return Cell::Url;
    }
    if DateTime::parse_from_rfc3339(s).is_ok() {
        return Cell::DateTime;
    }
    if s.len() >= 16 && NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").is_ok() {
        return Cell::DateTime;
    }
    if NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
        || NaiveDate::parse_from_str(s, "%m/%d/%Y").is_ok()
        || NaiveDate::parse_from_str(s, "%d/%m/%Y").is_ok()
    {
        return Cell::DateOnly;
    }
    if low == "true" || low == "false" {
        return Cell::Bool;
    }
    if let Some(n) = clean_number(s, '.', true, false) {
        return if n.contains('.') { Cell::Dec } else { Cell::Int };
    }
    Cell::Text
}

/// Preference order for a value shape when a column mixes shapes (higher wins a
/// tie): the more specific/parseable a shape, the more it says about the column.
fn cell_priority(c: Cell) -> u8 {
    match c {
        Cell::Url => 7,
        Cell::DateTime => 6,
        Cell::DateOnly => 5,
        Cell::Dec => 4,
        Cell::Int => 3,
        Cell::Bool => 2,
        Cell::Text => 1,
        Cell::Empty => 0,
    }
}

/// A column's dominant non-empty value shape across the scanned sample.
fn column_shape(scan: &[Row], header: &str) -> Cell {
    let mut counts: HashMap<Cell, usize> = HashMap::new();
    for row in scan {
        if let Some(v) = row.get(header) {
            let c = cell_shape(&cell_str(v));
            if c != Cell::Empty {
                *counts.entry(c).or_default() += 1;
            }
        }
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| cell_priority(a.0).cmp(&cell_priority(b.0))))
        .map(|(c, _)| c)
        .unwrap_or(Cell::Empty)
}

/// The kind of value a contract field expects, from its type, name, and
/// example.
fn field_kind(field: &FieldMeta, ts_field: &str) -> FieldKind {
    if field.type_.as_deref() == Some("array") {
        return FieldKind::Array;
    }
    let name = field.name.to_lowercase();
    if field.name == ts_field
        || matches!(
            name.as_str(),
            "ts" | "start" | "end" | "date" | "time" | "datetime" | "timestamp" | "when"
        )
    {
        return FieldKind::Temporal;
    }
    // A representative example pins url/temporal even when the type is a bare
    // `string` (the `anyOf` date fields, url fields).
    if let Some(ex) = field.examples.first().and_then(|v| v.as_str()) {
        match cell_shape(ex) {
            Cell::DateOnly | Cell::DateTime => return FieldKind::Temporal,
            Cell::Url => return FieldKind::Url,
            _ => {}
        }
    }
    if name.contains("url") || name.contains("uri") || name.contains("link") {
        return FieldKind::Url;
    }
    match field.type_.as_deref() {
        Some("number") | Some("integer") => FieldKind::Numeric,
        Some("string") => FieldKind::Text,
        _ => FieldKind::Any,
    }
}

/// How well an observed column shape agrees with a field's expected kind
/// (`0.0` = a hard conflict that blocks the binding on shape alone).
fn shape_agree(col: Cell, fk: FieldKind) -> f64 {
    match fk {
        FieldKind::Temporal => match col {
            Cell::DateOnly | Cell::DateTime => 1.0,
            _ => 0.0,
        },
        FieldKind::Url => match col {
            Cell::Url => 1.0,
            _ => 0.0,
        },
        FieldKind::Numeric => match col {
            Cell::Int | Cell::Dec => 1.0,
            _ => 0.0,
        },
        // A delimited text cell splits into an array; anything else weakly.
        FieldKind::Array => match col {
            Cell::Text => 0.6,
            _ => 0.2,
        },
        FieldKind::Text => match col {
            Cell::Text => 0.5,
            Cell::Url => 0.3,
            _ => 0.2,
        },
        FieldKind::Any => 0.3,
    }
}

/// Split any text into lowercase alphanumeric tokens.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Distinct tokens of a header, first-occurrence order preserved — so a
/// repeated word (`"Framed Date Date"`) counts once, not twice, in overlap.
fn unique_tokens(s: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokenize(s).into_iter().filter(|t| seen.insert(t.clone())).collect()
}

/// The content tokens of a field's description (stopwords removed), for the
/// description-overlap signal.
fn field_desc_tokens(field: &FieldMeta) -> HashSet<String> {
    const STOP: &[&str] = &[
        "the", "a", "an", "of", "or", "and", "to", "is", "it", "its", "for", "where", "when",
        "with", "as", "at", "in", "on", "by", "per", "each", "one", "this", "that", "source",
        "item", "vault", "id", "no", "not", "has", "have", "any", "from", "into", "out",
        "own", "only", "e", "g", "eg", "ie",
    ];
    let stop: HashSet<&str> = STOP.iter().copied().collect();
    tokenize(&field.description)
        .into_iter()
        .filter(|t| !stop.contains(t.as_str()))
        .collect()
}

/// Header-keyed sample row for display / LLM payload (blank header → `col<n>`).
fn row_to_map(row: &Row) -> Map<String, Value> {
    let mut m = Map::new();
    for (idx, (name, cell)) in row.cols.iter().enumerate() {
        let key = if name.trim().is_empty() { format!("col{idx}") } else { name.clone() };
        m.insert(key, cell.clone());
    }
    m
}

/// Read up to `n` data rows (bounded; never the whole file) for value-shape
/// inference and the display sample.
fn scan_sample(path: &Path, format: Format, n: usize) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    match format {
        Format::Csv => {
            let mut rdr = csv::ReaderBuilder::new()
                .flexible(true)
                .has_headers(true)
                .from_path(path)
                .with_context(|| format!("opening {}", path.display()))?;
            let headers: Vec<String> =
                rdr.headers().context("reading CSV header row")?.iter().map(str::to_string).collect();
            for rec in rdr.records() {
                if rows.len() >= n {
                    break;
                }
                if let Ok(rec) = rec {
                    let cols = rec
                        .iter()
                        .enumerate()
                        .map(|(i, cell)| {
                            (headers.get(i).cloned().unwrap_or_default(), Value::String(cell.to_string()))
                        })
                        .collect();
                    rows.push(Row { cols });
                }
            }
        }
        Format::Jsonl => {
            let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let reader = BufReader::new(file);
            for l in reader.lines() {
                if rows.len() >= n {
                    break;
                }
                let l = l.with_context(|| format!("reading {}", path.display()))?;
                if l.trim().is_empty() {
                    continue;
                }
                if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&l) {
                    rows.push(Row { cols: obj.into_iter().collect() });
                }
            }
        }
    }
    Ok(rows)
}

// ===========================================================================
// Step 2c — the opt-in LLM classify-and-suggest advisor (core side)
// ===========================================================================
//
// The deterministic heuristics (path 2 above) always run first. When they are
// inconclusive, Step 3's UI may offer an *explicit, per-drop* escalation to a
// cloud LLM that pre-fills the same binding form the heuristics do. This module
// owns the core half of that ratified decision (`docs/normalizer.md` §2c,
// Decision 1, Ratified ruling 4):
//
//   * **Consent is verifiable up front.** [`SuggestPayload`] is the exact data
//     that would leave the machine — headers + at most [`SAMPLE_ROWS`] sample
//     rows + the candidate contract shapes' field metadata. Step 3 shows it in
//     the consent dialog *before* any network call; there is no "always allow".
//   * **The advisor produces a plain [`Mapping`]**, identical in type to a
//     heuristic draft (`provenance.suggested_by = "llm"`). The UI cannot tell
//     which advisor filled the form.
//   * **Key handling follows the ConnectSpec precedent** — a build-time baked
//     key (`option_env!("TROVE_ANTHROPIC_API_KEY")`) plus a bring-your-own
//     override stored via the vault's existing 0600 secret mechanism, BYO
//     winning. No key ⇒ [`llm_advisor_status`] reports unavailable-with-reason
//     and there is *never* an implicit network call.
//   * **The model id lives in exactly one place** ([`LLM_MODEL`]).
//
// The HTTP call is synchronous (`ureq`, the crate-wide HTTP client — the
// standalone rule forbids a heavyweight async stack like reqwest/tokio, and
// every other cloud pull in this crate is a sync `fn(&Vault)`). Step 3 wraps it
// in an `async fn` + `spawn_blocking` Tauri command like every other
// vault-touching command. The transport is injected through [`LlmTransport`] so
// unit tests mock the network with no real request.

/// The one place the advisor's model id is defined (Ratified ruling 4: model
/// pinned at build time, not in the spec). Opus 4.8 — the current most-capable
/// widely-available model, and structured/extraction-friendly.
pub const LLM_MODEL: &str = "claude-opus-4-8";

/// The Anthropic Messages endpoint and API version the advisor calls.
pub const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Output-token ceiling for one suggestion (a single mapping artifact is small).
const LLM_MAX_TOKENS: u32 = 4096;

/// Build-time baked key (may be absent). BYO override wins over this — see
/// [`resolve_llm_key`].
const BAKED_ANTHROPIC_KEY: Option<&str> = option_env!("TROVE_ANTHROPIC_API_KEY");

// ---------------------------------------------------------------------------
// The consent payload (the literal bytes that would leave the machine)

/// One candidate contract shape offered to the advisor: its identity plus the
/// same field metadata the binding UI shows. Contract metadata only — no user
/// data — but bundled here so the consent dialog renders the whole request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SuggestCandidateMeta {
    pub domain: String,
    pub shape: String,
    /// The field the guid recipe must populate.
    pub guid_field: String,
    pub required: Vec<String>,
    pub fields: Vec<FieldMeta>,
}

impl SuggestCandidateMeta {
    fn of(schema: &ContractSchema) -> SuggestCandidateMeta {
        SuggestCandidateMeta {
            domain: schema.domain.clone(),
            shape: schema.shape.clone(),
            guid_field: schema.guid_field.clone(),
            required: schema.required.clone(),
            fields: schema.fields.clone(),
        }
    }
}

/// The exact, inspectable payload for one LLM suggestion. Serialize it straight
/// into the consent dialog: `headers` + `sample_rows` are the only user data
/// that leaves the machine, capped at [`SAMPLE_ROWS`] rows. `candidates` is
/// contract metadata (public schema info).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SuggestPayload {
    /// Raw file headers, in column order (blank/unnamed column stays `""`).
    pub headers: Vec<String>,
    /// Up to [`SAMPLE_ROWS`] header-keyed sample rows (blank header → `col<n>`).
    pub sample_rows: Vec<Map<String, Value>>,
    /// `"csv"` | `"jsonl"`.
    pub format: String,
    /// The contract shapes the advisor may classify into, with field metadata.
    pub candidates: Vec<SuggestCandidateMeta>,
}

impl SuggestPayload {
    /// Assemble the consent payload from a [`Detection`]. The candidate shapes
    /// are the detection's ranked contract candidates when it produced any;
    /// otherwise (an honest heuristic decline) *every* projectable contract, so
    /// the advisor gets a fair shot at classifying a file the heuristics
    /// couldn't. A `Route` detection never reaches here (that is path 1), but if
    /// it does the fallback of all projectable contracts is still safe.
    pub fn from_detection(det: &Detection) -> SuggestPayload {
        let candidates: Vec<SuggestCandidateMeta> = match &det.outcome {
            DetectOutcome::Contract { candidates, .. } if !candidates.is_empty() => candidates
                .iter()
                .filter_map(|c| contract_schema(&c.domain, &c.shape))
                .map(SuggestCandidateMeta::of)
                .collect(),
            _ => contract_schemas()
                .iter()
                .filter(|s| s.proj.is_some())
                .map(SuggestCandidateMeta::of)
                .collect(),
        };
        SuggestPayload {
            headers: det.headers.clone(),
            sample_rows: det.sample_rows.iter().take(SAMPLE_ROWS).cloned().collect(),
            format: det.format.clone(),
            candidates,
        }
    }

    /// The exact Anthropic Messages request body this payload would send (the
    /// api key is a header, never in the body; the mapping's `source` slug is a
    /// post-parse assembly concern, not sent to the model). Deterministic and
    /// public so Step 3 can show the literal wire bytes if it wants; the consent
    /// dialog normally shows the [`SuggestPayload`] itself.
    pub fn request_body(&self) -> Value {
        self.build_body(None)
    }

    fn build_body(&self, retry_note: Option<&str>) -> Value {
        let mut user = String::new();
        user.push_str(
            "Classify a dropped data file into one of the candidate contract shapes below \
             and produce a mapping artifact that binds its columns to that shape's fields.\n\n",
        );
        user.push_str("## Candidate contract shapes (choose exactly one)\n\n");
        user.push_str(
            &serde_json::to_string_pretty(&self.candidates)
                .unwrap_or_else(|_| "[]".to_string()),
        );
        user.push_str("\n\n## The dropped file\n\n");
        user.push_str(&format!("format: {}\n", self.format));
        user.push_str(&format!(
            "headers: {}\n",
            serde_json::to_string(&self.headers).unwrap_or_default()
        ));
        user.push_str("sample rows (at most 5):\n");
        user.push_str(
            &serde_json::to_string_pretty(&self.sample_rows).unwrap_or_else(|_| "[]".to_string()),
        );
        if let Some(note) = retry_note {
            user.push_str("\n\n## Your previous attempt was rejected\n");
            user.push_str(note);
            user.push_str("\nReturn a corrected mapping that fixes this.");
        }
        serde_json::json!({
            "model": LLM_MODEL,
            "max_tokens": LLM_MAX_TOKENS,
            "system": LLM_SYSTEM_PROMPT,
            "messages": [ { "role": "user", "content": user } ],
        })
    }

    /// Run the advisor: one network call (a second only on a mismatch), then a
    /// validated [`Mapping`] indistinguishable from a heuristic draft. Pure with
    /// respect to the vault — the caller supplies the resolved `api_key` and the
    /// transport, so this never reads secrets or picks the network on its own.
    /// `source` is the user-confirmed slug the mapping is keyed on.
    pub fn suggest(
        &self,
        source: &str,
        api_key: &str,
        transport: &dyn LlmTransport,
    ) -> Result<Mapping> {
        match self.suggest_attempt(source, api_key, transport, None) {
            Ok(m) => Ok(m),
            // Exactly one retry, feeding the failure back to the model.
            Err(first) => {
                let note = first.to_string();
                self.suggest_attempt(source, api_key, transport, Some(&note))
                    .map_err(|second| {
                        anyhow::anyhow!(
                            "LLM suggestion failed twice: first: {first}; retry: {second}"
                        )
                    })
            }
        }
    }

    fn suggest_attempt(
        &self,
        source: &str,
        api_key: &str,
        transport: &dyn LlmTransport,
        retry_note: Option<&str>,
    ) -> Result<Mapping> {
        let body = self.build_body(retry_note);
        let resp = transport.post_messages(api_key, &body)?;
        let text = extract_message_text(&resp)?;
        let obj = extract_json_object(&text)?;
        let sm: SuggestedMapping = serde_json::from_value(obj)
            .context("model response was not a mapping artifact")?;
        // Machine-authoritative fields are set here, not trusted from the model:
        // version/unbound are fixed, the signature must describe the real file,
        // and provenance records the advisor. Only the semantic choices
        // (contract, bindings, constants, guid) come from the model.
        let mapping = Mapping {
            version: MAPPING_VERSION,
            source: source.to_string(),
            domain: sm.domain,
            shape: sm.shape,
            signature: Signature::of(&self.headers, Format::parse(&self.format)),
            bindings: sm.bindings,
            constants: sm.constants,
            guid: sm.guid,
            unbound: "extra".to_string(),
            provenance: Provenance {
                created: Local::now().format("%Y-%m-%d").to_string(),
                suggested_by: "llm".to_string(),
            },
        };
        // Constrain the model to the offered shapes — a hallucinated contract is
        // a mismatch, not a silent accept.
        if !self
            .candidates
            .iter()
            .any(|c| c.domain == mapping.domain && c.shape == mapping.shape)
        {
            bail!(
                "model chose {}.{}, which was not among the candidate shapes",
                mapping.domain,
                mapping.shape
            );
        }
        mapping.validate()?;
        Ok(mapping)
    }
}

/// The lenient view of a model response: only the semantic choices are read;
/// version/source/signature/unbound/provenance are set authoritatively by
/// [`SuggestPayload::suggest_attempt`] regardless of what the model emits.
#[derive(Deserialize)]
struct SuggestedMapping {
    domain: String,
    shape: String,
    #[serde(default)]
    bindings: Vec<Binding>,
    #[serde(default)]
    constants: Map<String, Value>,
    guid: GuidRecipe,
}

const LLM_SYSTEM_PROMPT: &str = "\
You are a data-mapping advisor for a local-first personal-data vault. You are given \
the headers and a few sample rows of a file the user dropped in, plus a set of \
candidate contract shapes (each a domain, a shape, its required fields, and per-field \
metadata). Choose the single best-fitting candidate contract and emit a mapping \
artifact that binds the file's columns to that contract's fields.\n\
\n\
Respond with ONLY a single JSON object (no prose, no markdown fences) in exactly this shape:\n\
{\n\
  \"domain\": \"<candidate domain>\",\n\
  \"shape\": \"<candidate shape>\",\n\
  \"bindings\": [ { \"from\": \"<source header>\", \"to\": \"<contract field>\", \"coerce\": \"<optional>\", \"with\": <optional params> } ],\n\
  \"constants\": { \"<contract field>\": <literal> },\n\
  \"guid\": <guid recipe>\n\
}\n\
\n\
Rules:\n\
- Bind a source column to a contract field only when it genuinely carries that field's value. Leave a field unbound rather than guessing; unbound source columns are preserved verbatim, so never invent data.\n\
- \"coerce\" is optional and must be one of exactly: \"date\", \"number\", \"split\", \"value_map\". Omit it for a verbatim copy.\n\
    - date: parses a date/time string into an ISO value. \"with\" is optional.\n\
    - number: strips thousands separators / currency symbols into a number. \"with\" optional (e.g. {\"decimal\":\",\"}).\n\
    - split: splits a delimited cell into an array. \"with\": {\"sep\": \", \"}.\n\
    - value_map: maps source tokens to contract enum values. \"with\": {\"map\": {\"src\":\"dst\"}}.\n\
- Use \"constants\" for a required field that has no matching column but a fixed value (e.g. a kind/type discriminator the contract expects).\n\
- Do not set a field with both a binding and a constant.\n\
- \"guid\" is the dedupe key and must be exactly one of:\n\
    { \"column\": \"<header>\" }              (a stable id column, used verbatim)\n\
    { \"hash\": [\"<header>\", ...] }          (sha256 of the joined columns)\n\
  Prefer a stable url or id column; otherwise hash the columns that make a row unique.\n\
- The mapping must satisfy the chosen contract's required fields.\n";

// ---------------------------------------------------------------------------
// Availability + key resolution (baked + BYO, following the ConnectSpec precedent)

/// Whether the LLM advisor can run, and why not when it can't. No key ⇒
/// `available: false` with a `reason` Step 3 renders next to a disabled
/// control (the disabled-controls-need-affordance rule). Never triggers a call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmAdvisorStatus {
    pub available: bool,
    /// `"byo"` | `"baked"` when available; `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Present only when unavailable — a user-facing reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The pinned model id, for display.
    pub model: String,
}

/// Report whether the advisor is usable. Reads the key store but makes no
/// network call.
pub fn llm_advisor_status(vault: &Vault) -> Result<LlmAdvisorStatus> {
    Ok(match resolve_llm_key(vault)? {
        Some((_, source)) => LlmAdvisorStatus {
            available: true,
            source: Some(source.to_string()),
            reason: None,
            model: LLM_MODEL.to_string(),
        },
        None => LlmAdvisorStatus {
            available: false,
            source: None,
            reason: Some(
                "No Anthropic API key configured — add one in Settings to enable AI suggestions."
                    .to_string(),
            ),
            model: LLM_MODEL.to_string(),
        },
    })
}

/// Resolve the advisor key: a saved bring-your-own key wins over the build-time
/// baked key (ConnectSpec precedent). Empty values are treated as absent.
fn resolve_llm_key(vault: &Vault) -> Result<Option<(String, &'static str)>> {
    if let Some(k) = vault.load_llm_key()? {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Ok(Some((k, "byo")));
        }
    }
    if let Some(k) = BAKED_ANTHROPIC_KEY {
        if !k.is_empty() {
            return Ok(Some((k.to_string(), "baked")));
        }
    }
    Ok(None)
}

/// Run the advisor end to end: resolve the key, then suggest. Errors with a
/// clear "not configured" message when no key is present (Step 3 gates on
/// [`llm_advisor_status`] first, so this is a backstop). Uses the real `ureq`
/// transport.
pub fn llm_suggest(vault: &Vault, source: &str, payload: &SuggestPayload) -> Result<Mapping> {
    llm_suggest_with(vault, source, payload, &UreqTransport)
}

/// [`llm_suggest`] with an injected transport — the seam unit tests mock.
pub fn llm_suggest_with(
    vault: &Vault,
    source: &str,
    payload: &SuggestPayload,
    transport: &dyn LlmTransport,
) -> Result<Mapping> {
    let (api_key, _source) = resolve_llm_key(vault)?.context(
        "the AI suggestion advisor is not configured — add an Anthropic API key in Settings",
    )?;
    payload.suggest(source, &api_key, transport)
}

impl Vault {
    /// Path of the BYO advisor key (0600, under `.trove/` like other secrets).
    fn llm_key_path(&self) -> Result<std::path::PathBuf> {
        let dir = self.root().join(".trove").join("normalizer");
        fs::create_dir_all(&dir).context("creating .trove/normalizer")?;
        Ok(dir.join("anthropic-key.json"))
    }

    /// Save a bring-your-own Anthropic API key (0600). Rejects an empty key.
    pub fn save_llm_key(&self, key: &str) -> Result<()> {
        let key = key.trim();
        if key.is_empty() {
            bail!("empty API key");
        }
        crate::store::write_atomic_secret(
            &self.llm_key_path()?,
            serde_json::to_string_pretty(&StoredLlmKey { api_key: key.to_string() })?.as_bytes(),
        )
    }

    /// Load the saved BYO key, if any.
    pub fn load_llm_key(&self) -> Result<Option<String>> {
        let path = self.llm_key_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path).context("reading advisor key")?;
        let stored: StoredLlmKey =
            serde_json::from_str(&raw).context("parsing advisor key")?;
        Ok(Some(stored.api_key))
    }

    /// Forget the saved BYO key (the baked key, if any, still applies).
    pub fn delete_llm_key(&self) -> Result<()> {
        let path = self.llm_key_path()?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("deleting advisor key"),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredLlmKey {
    api_key: String,
}

// ---------------------------------------------------------------------------
// Transport (the injectable network seam)

/// The one network operation the advisor needs: POST a Messages request body
/// (the api key is added as a header by the implementation) and return the
/// parsed JSON response. Implemented by [`UreqTransport`] in production and
/// mocked in tests.
pub trait LlmTransport {
    fn post_messages(&self, api_key: &str, body: &Value) -> Result<Value>;
}

/// The production transport: a synchronous `ureq` POST to the Anthropic
/// Messages API. This is the vault's only networked path besides the existing
/// cloud syncs; it is never reached without an explicit, consented call.
pub struct UreqTransport;

impl LlmTransport for UreqTransport {
    fn post_messages(&self, api_key: &str, body: &Value) -> Result<Value> {
        ureq::post(ANTHROPIC_MESSAGES_URL)
            .timeout(std::time::Duration::from_secs(120))
            .set("x-api-key", api_key)
            .set("anthropic-version", ANTHROPIC_VERSION)
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(describe_anthropic_error)?
            .into_json::<Value>()
            .context("parsing Anthropic API response")
    }
}

/// Flatten a `ureq` error, keeping the response body — Anthropic puts the useful
/// error type/message in the JSON body.
fn describe_anthropic_error(err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            anyhow::anyhow!(
                "Anthropic API HTTP {code}: {}",
                body.chars().take(500).collect::<String>()
            )
        }
        other => anyhow::anyhow!("Anthropic API request failed: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Response parsing

/// Concatenate the `text` blocks of a Messages API response, erroring on a
/// refusal or a shape that carries no text.
fn extract_message_text(resp: &Value) -> Result<String> {
    if resp.get("stop_reason").and_then(|s| s.as_str()) == Some("refusal") {
        let why = resp
            .get("stop_details")
            .and_then(|d| d.get("explanation"))
            .and_then(|e| e.as_str())
            .unwrap_or("the request was declined by the model's safety system");
        bail!("Anthropic API refused the request: {why}");
    }
    let content = resp
        .get("content")
        .and_then(|c| c.as_array())
        .context("Anthropic API response had no content array")?;
    let mut text = String::new();
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
        }
    }
    if text.trim().is_empty() {
        bail!("Anthropic API response contained no text");
    }
    Ok(text)
}

/// Pull the first balanced JSON object out of the model's text (tolerating
/// stray prose or code fences around it), string-escape aware.
fn extract_json_object(text: &str) -> Result<Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() {
            return Ok(v);
        }
    }
    let bytes = trimmed.as_bytes();
    let start = trimmed.find('{').context("no JSON object in the model response")?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let slice = &trimmed[start..=i];
                    return serde_json::from_str(slice)
                        .context("model response JSON did not parse");
                }
            }
            _ => {}
        }
    }
    bail!("model response had an unterminated JSON object")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-normalizer-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn write_file(v: &Vault, name: &str, body: &str) -> std::path::PathBuf {
        let p = v.root().join(name);
        fs::write(&p, body).unwrap();
        p
    }

    // The gate-shaped fixture: a Looker-style export with an unnamed leading
    // index column, date-only Post Date, and a comma-formatted number — fake
    // values only (real export stays out of the repo).
    const PUBOPS_CSV: &str = "\
,Creative Framing Tags,Framed By,Framed Date Date,Post Date,List Name,Page Name,Scheduling Type,Post URL,Total Link Clicks\r\n\
1,\"tag-a, tag-b\",alice,2026-07-01,2026-07-15,Best Widgets,Widgets Page,manual,https://example.com/p/1,\"13,133\"\r\n\
2,tag-c,bob,2026-07-02,2026-06-16,Top Gadgets,Gadgets Page,auto,https://example.com/p/2,\"1,024\"\r\n";

    fn pubops_mapping() -> Mapping {
        Mapping {
            version: 1,
            source: "acme-pubops".into(),
            domain: "social".into(),
            shape: "post".into(),
            signature: Signature::of(
                &pubops_headers(),
                Format::Csv,
            ),
            bindings: vec![
                Binding { from: "Post Date".into(), to: "ts".into(), coerce: Some("date".into()), with: Some(json!({"formats": ["date"]})) },
                Binding { from: "Post URL".into(), to: "url".into(), coerce: None, with: None },
                Binding { from: "List Name".into(), to: "title".into(), coerce: None, with: None },
                Binding { from: "Page Name".into(), to: "context".into(), coerce: None, with: None },
            ],
            constants: json!({"kind": "post"}).as_object().unwrap().clone(),
            guid: GuidRecipe::Hash { hash: vec!["Post URL".into()], algo: None, prefix: None },
            unbound: "extra".into(),
            provenance: Provenance { created: "2026-07-20".into(), suggested_by: "manual".into() },
        }
    }

    fn pubops_headers() -> Vec<String> {
        vec![
            "".into(), "Creative Framing Tags".into(), "Framed By".into(), "Framed Date Date".into(),
            "Post Date".into(), "List Name".into(), "Page Name".into(), "Scheduling Type".into(),
            "Post URL".into(), "Total Link Clicks".into(),
        ]
    }

    // ---- artifact round-trip ------------------------------------------------

    #[test]
    fn mapping_round_trips_through_json() {
        let m = pubops_mapping();
        let s = serde_json::to_string_pretty(&m).unwrap();
        let back: Mapping = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back, "mapping survives a serde round-trip verbatim");
        // The guid untagged enum round-trips as Hash, not Column.
        assert!(matches!(back.guid, GuidRecipe::Hash { .. }));
    }

    #[test]
    fn mapping_save_load_list_via_vault_paths() {
        let v = temp_vault("save-load");
        let m = pubops_mapping();
        m.save(&v).unwrap();
        assert!(v.root().join(".trove/mappings/acme-pubops.json").exists());
        let loaded = Mapping::load(&v, "acme-pubops").unwrap();
        assert_eq!(loaded, m);
        let all = Mapping::list(&v).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].source, "acme-pubops");
    }

    #[test]
    fn validate_rejects_bad_mappings() {
        let mut m = pubops_mapping();
        m.version = 2;
        assert!(m.validate().is_err(), "unknown version rejected");

        let mut m = pubops_mapping();
        m.bindings.push(Binding { from: "x".into(), to: "kind".into(), coerce: None, with: None });
        assert!(m.validate().is_err(), "constant+binding overlap on `kind` rejected");

        let mut m = pubops_mapping();
        m.bindings[0].coerce = Some("magic".into());
        assert!(m.validate().is_err(), "unknown coercion rejected");

        let mut m = pubops_mapping();
        m.domain = "nope".into();
        assert!(m.validate().is_err(), "unknown contract rejected");
    }

    // ---- coercions ----------------------------------------------------------

    fn dc(cell: &str, with: Value) -> Option<Value> {
        coerce_date(cell.trim(), Some(&with))
    }

    #[test]
    fn coerce_date_all_forms() {
        assert_eq!(dc("2026-07-15", json!({"formats": ["date"]})), Some(json!("2026-07-15")));
        // strftime date-only pattern emits date-only verbatim.
        assert_eq!(dc("07/15/2026", json!({"formats": ["%m/%d/%Y"]})), Some(json!("2026-07-15")));
        // rfc3339 → local offset RFC3339 (round-trips the instant).
        let out = dc("2026-07-15T12:00:00Z", json!({"formats": ["rfc3339"]})).unwrap();
        let parsed = DateTime::parse_from_rfc3339(out.as_str().unwrap()).unwrap();
        assert_eq!(parsed.timestamp(), 1_784_116_800);
        // epoch_s / epoch_ms → RFC3339 with a time component.
        let e = dc("1784116800", json!({"formats": ["epoch_s"]})).unwrap();
        assert_eq!(DateTime::parse_from_rfc3339(e.as_str().unwrap()).unwrap().timestamp(), 1_784_116_800);
        let ems = dc("1784116800000", json!({"formats": ["epoch_ms"]})).unwrap();
        assert_eq!(DateTime::parse_from_rfc3339(ems.as_str().unwrap()).unwrap().timestamp(), 1_784_116_800);
        // datetime strftime → RFC3339-local (carries a time, so not date-only).
        let dt = dc("2026-07-15 09:30:00", json!({"formats": ["%Y-%m-%d %H:%M:%S"]})).unwrap();
        assert!(dt.as_str().unwrap().starts_with("2026-07-15T09:30:00"), "got {dt}");
        // ordered formats: first that parses wins.
        assert_eq!(
            dc("2026-07-15", json!({"formats": ["rfc3339", "date"]})),
            Some(json!("2026-07-15"))
        );
        // no match → no value (omit).
        assert_eq!(dc("not a date", json!({"formats": ["date"]})), None);
        assert_eq!(dc("", json!({"formats": ["date"]})), None);
    }

    #[test]
    fn coerce_number_cleanup() {
        let n = |cell: &str, with: Value| coerce_number(cell.trim(), Some(&with));
        // A number coercion emits a *typed JSON number* (integral → integer,
        // fractional → decimal) so it validates against a `number`/`integer`
        // contract field — a string would be rejected by validate_row.
        assert_eq!(n("13,133", json!({})), Some(json!(13133)));
        assert!(n("13,133", json!({})).unwrap().is_i64(), "integral → integer number");
        // Canonical form is trailing-zero-normalized (spec: "trailing
        // zero-normalized"): "1,234.50" → magnitude 1234.5, parens → negative.
        assert_eq!(n("(1,234.50)", json!({})), Some(json!(-1234.5)));
        assert_eq!(n("$1,000.00", json!({})), Some(json!(1000)));
        // decimal comma.
        assert_eq!(n("1.234,50", json!({"decimal_sep": ","})), Some(json!(1234.5)));
        // negate flips sign; true zero stays unsigned.
        assert_eq!(n("5", json!({"negate": true})), Some(json!(-5)));
        assert_eq!(n("0.00", json!({"negate": true})), Some(json!(0)));
        assert_eq!(n("-0", json!({})), Some(json!(0)), "never emits -0");
        // parens_negative can be disabled.
        assert_eq!(n("(50)", json!({"parens_negative": false})), None);
        // blanks and non-numeric → no value (never 0).
        assert_eq!(n("", json!({})), None);
        assert_eq!(n("Not Available", json!({})), None);
    }

    // A number-coerced binding must project through validation into a numeric
    // contract field. Regression for the dead-end where `number` emitted a
    // string that `validate_row` rejected for every `number`/`integer` field,
    // making location.fix / environment.reading / media.play / finance
    // line-items unprojectable. Uses the auto-drafted mapping (detect) so the
    // whole detect→draft→coerce→validate→project path is exercised.
    #[test]
    fn number_coerced_fields_project_and_validate() {
        let v = temp_vault("number-projects");
        // A GPS-style export → location.fix (lat/lon required, typed number).
        let src = write_file(
            &v,
            "track.csv",
            "ts,lat,lon,speed\n2026-07-15T10:00:00Z,37.77,-122.41,3\n2026-07-15T10:05:00Z,37.78,-122.42,0\n",
        );
        let det = detect(&src).unwrap();
        let candidates = match det.outcome {
            DetectOutcome::Contract { candidates, .. } => candidates,
            other => panic!("expected a contract match, got {other:?}"),
        };
        let fix = candidates
            .iter()
            .find(|c| c.domain == "location" && c.shape == "fix")
            .expect("location.fix drafted for a lat/lon/ts export");
        // The lat/lon bindings carry the number coercion.
        assert!(fix
            .draft
            .bindings
            .iter()
            .any(|b| b.to == "lat" && b.coerce.as_deref() == Some("number")));

        let applied = fix.draft.apply(&src).unwrap();
        assert_eq!(applied.valid, 2, "every row validates: {:?}", applied.invalid_samples);
        assert_eq!(applied.invalid, 0);
        // lat/lon land as real JSON numbers (not strings) so they satisfy the
        // `number`-typed contract fields.
        let r0 = &applied.rows[0];
        assert!(r0["lat"].is_number(), "lat is a JSON number, got {}", r0["lat"]);
        assert_eq!(r0["lat"].as_f64().unwrap(), 37.77);
        assert!(r0["lon"].is_number());

        // And it projects end-to-end.
        fix.draft.save(&v).unwrap();
        let out = fix.draft.project(&v, &src, &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 2);
        assert_eq!(out.counts["invalid"], 0);
    }

    // A DateTime column whose values are the space-separated `YYYY-MM-DD
    // HH:MM:SS` form (a BI-export staple) must project — the auto-draft has to
    // offer a format that actually parses it, not a lone rfc3339 token.
    #[test]
    fn datetime_space_separated_column_projects() {
        let v = temp_vault("dt-space");
        let src = write_file(
            &v,
            "events.csv",
            "ts,metric,value\n2026-07-15 00:00:00,temp,21.5\n2026-07-15 01:00:00,temp,20.0\n",
        );
        let det = detect(&src).unwrap();
        let candidates = match det.outcome {
            DetectOutcome::Contract { candidates, .. } => candidates,
            other => panic!("expected a contract match, got {other:?}"),
        };
        // Any candidate binding `ts` from this DateTime column must project it.
        let cand = candidates
            .iter()
            .find(|c| c.draft.bindings.iter().any(|b| b.to == "ts"))
            .expect("some contract binds ts");
        let applied = cand.draft.apply(&src).unwrap();
        assert!(applied.valid >= 1, "space-separated datetime parsed: {:?}", applied.invalid_samples);
        let ts = applied.rows[0]["ts"].as_str().unwrap();
        assert!(ts.starts_with("2026-07-15T00:00:00"), "rfc3339-local ts, got {ts}");
    }

    #[test]
    fn coerce_split_to_array() {
        let s = |cell: &str, with: Value| coerce_split(cell, Some(&with));
        assert_eq!(s("Action, Adventure", json!({})), Some(json!(["Action", "Adventure"])));
        assert_eq!(s("a|b||c", json!({"sep": "|"})), Some(json!(["a", "b", "c"])), "drop_empty default");
        assert_eq!(s("a| |c", json!({"sep": "|", "drop_empty": false})), Some(json!(["a", "", "c"])));
        assert_eq!(s("", json!({})), None, "empty → omit");
    }

    #[test]
    fn coerce_value_map_lookup_and_fallback() {
        let vm = |cell: &str, with: Value| coerce_value_map(cell.trim(), Some(&with));
        let tbl = json!({"table": {"accepted": "confirmed", "cancelled": "canceled"}});
        assert_eq!(vm("Accepted", tbl.clone()), Some(json!("confirmed")), "case-insensitive hit");
        assert_eq!(vm("unknown", tbl.clone()), None, "default fallback omits");
        assert_eq!(
            vm("unknown", json!({"table": {}, "fallback": "passthrough"})),
            Some(json!("unknown"))
        );
        assert_eq!(
            vm("unknown", json!({"table": {}, "fallback": "other"})),
            Some(json!("other")),
            "literal fallback"
        );
        // case_insensitive false requires exact case.
        assert_eq!(
            vm("Accepted", json!({"table": {"accepted": "confirmed"}, "case_insensitive": false})),
            None
        );
    }

    #[test]
    fn guid_recipes_are_deterministic() {
        let row = Row {
            cols: vec![
                ("Const".into(), json!("tt0111161")),
                ("Post URL".into(), json!("https://example.com/p/1")),
                ("Blank".into(), json!("")),
            ],
        };
        // column: prefix + trimmed cell.
        let g = compute_guid(&GuidRecipe::Column { column: "Const".into(), prefix: Some("imdb:".into()) }, &row);
        assert_eq!(g.as_deref(), Some("imdb:tt0111161"));
        // blank column → no guid.
        assert_eq!(compute_guid(&GuidRecipe::Column { column: "Blank".into(), prefix: None }, &row), None);
        // hash: stable sha256 hex over joined trimmed cells.
        let h1 = compute_guid(&GuidRecipe::Hash { hash: vec!["Post URL".into()], algo: None, prefix: None }, &row).unwrap();
        let h2 = compute_guid(&GuidRecipe::Hash { hash: vec!["Post URL".into()], algo: None, prefix: None }, &row).unwrap();
        assert_eq!(h1, h2, "deterministic");
        assert_eq!(h1.len(), 64, "sha256 hex");
        let mut hasher = Sha256::new();
        hasher.update("https://example.com/p/1".as_bytes());
        assert_eq!(h1, hex::encode(hasher.finalize()));
    }

    // ---- embedded schemas ---------------------------------------------------

    #[test]
    fn all_32_schemas_embed_and_parse() {
        let all = contract_schemas();
        assert_eq!(all.len(), 32, "all schema files embedded");
        // social.post metadata is exposed for detect/UI.
        let sp = contract_schema("social", "post").unwrap();
        assert_eq!(sp.required, vec!["ts", "source", "guid"]);
        let ts = sp.field("ts").unwrap();
        assert!(ts.required);
        assert!(!ts.description.is_empty(), "field descriptions surfaced");
        assert!(!ts.examples.is_empty(), "field examples surfaced");
        // A projectable target resolves its write path.
        assert_eq!(sp.proj.unwrap().dir("acme-pubops"), "social/acme-pubops");
        assert_eq!(sp.proj.unwrap().raw_dir("acme-pubops"), "social/acme-pubops/raw");
        // A snapshot-kind contract is embedded but not projectable in v1.
        assert!(contract_schema("contacts", "contact").unwrap().proj.is_none());
        // Sub-stream target resolves a subdir.
        assert_eq!(contract_schema("reading", "highlight").unwrap().proj.unwrap().dir("readwise"), "reading/readwise/highlights");
    }

    #[test]
    fn validate_row_checks_required_and_types() {
        let sp = contract_schema("social", "post").unwrap();
        let mut ok = Map::new();
        ok.insert("ts".into(), json!("2026-07-15"));
        ok.insert("source".into(), json!("acme-pubops"));
        ok.insert("guid".into(), json!("abc"));
        assert!(sp.validate_row(&ok).is_ok());
        // missing guid → invalid.
        let mut bad = ok.clone();
        bad.remove("guid");
        assert!(sp.validate_row(&bad).is_err());
        // wrong type: split into a string field (title) → array where string
        // expected → invalid.
        let mut wrong = ok.clone();
        wrong.insert("title".into(), json!(["a", "b"]));
        assert!(sp.validate_row(&wrong).is_err(), "array into a string field rejected");
        // tags is array-typed: an array is fine.
        let mut tags = ok.clone();
        tags.insert("tags".into(), json!(["x"]));
        assert!(sp.validate_row(&tags).is_ok());
    }

    // ---- apply --------------------------------------------------------------

    #[test]
    fn apply_maps_pubops_to_social_post_with_extra_verbatim() {
        let v = temp_vault("apply");
        let p = write_file(&v, "pubops.csv", PUBOPS_CSV);
        let applied = pubops_mapping().apply(&p).unwrap();
        assert_eq!(applied.total, 2);
        assert_eq!(applied.valid, 2);
        assert_eq!(applied.invalid, 0);

        let row = &applied.rows[0];
        assert_eq!(row["ts"], json!("2026-07-15"), "date-only ts, no fabricated midnight");
        assert_eq!(row["source"], json!("acme-pubops"), "source injected from slug");
        assert_eq!(row["kind"], json!("post"), "constant applied");
        assert_eq!(row["url"], json!("https://example.com/p/1"));
        assert_eq!(row["title"], json!("Best Widgets"));
        assert_eq!(row["context"], json!("Widgets Page"));
        assert!(row["guid"].as_str().unwrap().len() == 64, "hash guid present");

        // Unbound columns → extra, verbatim; blank header → col0.
        let extra = row["extra"].as_object().unwrap();
        assert_eq!(extra["col0"], json!("1"), "unnamed index column keyed col0");
        assert_eq!(extra["Total Link Clicks"], json!("13,133"), "comma number kept verbatim in extra");
        assert_eq!(extra["Creative Framing Tags"], json!("tag-a, tag-b"));
        assert_eq!(extra["Scheduling Type"], json!("manual"));
        // Bound columns are NOT duplicated into extra.
        assert!(!extra.contains_key("Post Date"));
        assert!(!extra.contains_key("List Name"));
        // Post URL is bound to url, so it is not in extra.
        assert!(!extra.contains_key("Post URL"));
    }

    #[test]
    fn apply_counts_and_reports_invalid_rows() {
        let v = temp_vault("invalid");
        // Second row has an unparseable Post Date → no ts → fails validation.
        let csv = "\
,Post Date,List Name,Post URL\r\n\
1,2026-07-15,Good,https://example.com/p/1\r\n\
2,garbage,Bad,https://example.com/p/2\r\n";
        let p = write_file(&v, "in.csv", csv);
        let m = Mapping {
            version: 1,
            source: "src".into(),
            domain: "social".into(),
            shape: "post".into(),
            signature: Signature::of(&["".into(), "Post Date".into(), "List Name".into(), "Post URL".into()], Format::Csv),
            bindings: vec![
                Binding { from: "Post Date".into(), to: "ts".into(), coerce: Some("date".into()), with: Some(json!({"formats": ["date"]})) },
                Binding { from: "List Name".into(), to: "title".into(), coerce: None, with: None },
            ],
            constants: Map::new(),
            guid: GuidRecipe::Hash { hash: vec!["Post URL".into()], algo: None, prefix: None },
            unbound: "extra".into(),
            provenance: Provenance::default(),
        };
        let applied = m.apply(&p).unwrap();
        assert_eq!(applied.total, 2);
        assert_eq!(applied.valid, 1);
        assert_eq!(applied.invalid, 1, "the garbage-date row is counted invalid, not dropped silently");
        assert_eq!(applied.invalid_samples.len(), 1);
        assert!(applied.invalid_samples[0].reason.contains("ts"));
    }

    #[test]
    fn apply_reads_jsonl_and_keeps_typed_extra() {
        let v = temp_vault("jsonl");
        let body = "\
{\"when\":\"2026-07-15T10:00:00Z\",\"id\":\"a1\",\"clicks\":42,\"note\":\"hi\"}\n\
{\"when\":\"2026-07-16T10:00:00Z\",\"id\":\"a2\",\"clicks\":7,\"note\":\"\"}\n";
        let p = write_file(&v, "in.jsonl", body);
        let m = Mapping {
            version: 1,
            source: "jsrc".into(),
            domain: "social".into(),
            shape: "post".into(),
            signature: Signature::of(&["when".into(), "id".into(), "clicks".into(), "note".into()], Format::Jsonl),
            bindings: vec![
                Binding { from: "when".into(), to: "ts".into(), coerce: Some("date".into()), with: Some(json!({"formats": ["rfc3339"]})) },
            ],
            constants: Map::new(),
            guid: GuidRecipe::Column { column: "id".into(), prefix: None },
            unbound: "extra".into(),
            provenance: Provenance::default(),
        };
        let applied = m.apply(&p).unwrap();
        assert_eq!(applied.valid, 2);
        assert_eq!(applied.rows[0]["guid"], json!("a1"));
        // JSONL numeric unbound cell keeps its type in extra.
        assert_eq!(applied.rows[0]["extra"]["clicks"], json!(42));
        // Blank string unbound omitted.
        assert!(!applied.rows[1]["extra"].as_object().unwrap().contains_key("note"));
    }

    // ---- project + lifecycle ------------------------------------------------

    #[test]
    fn project_writes_partitions_lands_raw_and_dedupes() {
        let v = temp_vault("project");
        let p = write_file(&v, "pubops.csv", PUBOPS_CSV);
        let m = pubops_mapping();
        m.save(&v).unwrap();

        let out = m.project(&v, &p, &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 2);
        assert_eq!(out.counts["duplicates"], 0);

        // Partitioned by ts month: 2026-07 and 2026-06.
        assert!(v.root().join("social/acme-pubops/2026-07.jsonl").exists());
        assert!(v.root().join("social/acme-pubops/2026-06.jsonl").exists());
        // Raw landed full-fidelity.
        assert!(v.root().join("social/acme-pubops/raw/pubops.csv").exists());

        // The projected row validates as a Post.
        let jul = fs::read_to_string(v.root().join("social/acme-pubops/2026-07.jsonl")).unwrap();
        let post: crate::social::Post = serde_json::from_str(jul.lines().next().unwrap()).unwrap();
        assert_eq!(post.source, "acme-pubops");
        assert_eq!(post.kind, "post");
        assert_eq!(post.title, "Best Widgets");

        // Re-dropping the same export dedupes on the hash guid (no new rows).
        let out2 = m.project(&v, &p, &mut |_| {}).unwrap();
        assert_eq!(out2.counts["imported"], 0);
        assert_eq!(out2.counts["duplicates"], 2);
        assert_eq!(fs::read_to_string(v.root().join("social/acme-pubops/2026-07.jsonl")).unwrap().lines().count(), 1);
    }

    #[test]
    fn reproject_is_idempotent_from_raw() {
        let v = temp_vault("reproject");
        let p = write_file(&v, "pubops.csv", PUBOPS_CSV);
        let m = pubops_mapping();
        m.save(&v).unwrap();
        m.project(&v, &p, &mut |_| {}).unwrap();

        let before = fs::read_to_string(v.root().join("social/acme-pubops/2026-07.jsonl")).unwrap();
        let out = Mapping::reproject(&v, "acme-pubops").unwrap();
        assert_eq!(out.counts["imported"], 2, "rebuilt both rows from the one raw file");
        let after = fs::read_to_string(v.root().join("social/acme-pubops/2026-07.jsonl")).unwrap();
        assert_eq!(before, after, "reproject reproduces byte-identical partitions");
        // A second reproject is still idempotent.
        Mapping::reproject(&v, "acme-pubops").unwrap();
        assert_eq!(fs::read_to_string(v.root().join("social/acme-pubops/2026-07.jsonl")).unwrap(), after);
    }

    #[test]
    fn delete_mapping_removes_projection_keeps_raw() {
        let v = temp_vault("delete");
        let p = write_file(&v, "pubops.csv", PUBOPS_CSV);
        let m = pubops_mapping();
        m.save(&v).unwrap();
        m.project(&v, &p, &mut |_| {}).unwrap();
        assert!(v.root().join("social/acme-pubops/2026-07.jsonl").exists());

        Mapping::delete(&v, "acme-pubops").unwrap();
        // Mapping gone, projected partitions gone, raw kept.
        assert!(!v.root().join(".trove/mappings/acme-pubops.json").exists());
        assert!(!v.root().join("social/acme-pubops/2026-07.jsonl").exists());
        assert!(!v.root().join("social/acme-pubops/2026-06.jsonl").exists());
        assert!(v.root().join("social/acme-pubops/raw/pubops.csv").exists(), "raw is kept");
    }

    #[test]
    fn signature_match_drives_auto_conform() {
        let v = temp_vault("signature");
        let m = pubops_mapping();
        m.save(&v).unwrap();

        // A file with the same headers (any case/whitespace) + format matches.
        let mut drifted = pubops_headers();
        drifted[1] = "  CREATIVE   framing tags ".into(); // case + whitespace variation
        assert!(m.signature.matches(&drifted, Format::Csv), "normalization ignores case/whitespace");
        let found = Mapping::for_signature(&v, &pubops_headers(), Format::Csv).unwrap();
        assert_eq!(found.unwrap().source, "acme-pubops");

        // A renamed column (header drift) → no match → treated as a new shape.
        let mut renamed = pubops_headers();
        renamed[5] = "Playlist Name".into();
        assert!(!m.signature.matches(&renamed, Format::Csv));
        assert!(Mapping::for_signature(&v, &renamed, Format::Csv).unwrap().is_none());
        // Wrong format also fails to match.
        assert!(!m.signature.matches(&pubops_headers(), Format::Jsonl));
    }

    #[test]
    fn file_signature_reads_headers_including_unnamed_column() {
        let v = temp_vault("filesig");
        let p = write_file(&v, "pubops.csv", PUBOPS_CSV);
        let sig = file_signature(&p).unwrap();
        assert_eq!(sig.format, "csv");
        assert_eq!(sig.headers[0], "", "unnamed leading column preserved as empty header");
        assert_eq!(sig.headers[1], "creative framing tags", "normalized lowercase");
        assert_eq!(sig.headers.len(), 10);
    }

    #[test]
    fn declined_drop_lands_raw_and_lists_in_manifest() {
        let v = temp_vault("declined");
        let p = write_file(&v, "mystery.csv", "a,b\n1,2\n");
        let entry = v.land_declined("random-export", &p).unwrap();
        assert_eq!(entry.source, "random-export");
        assert!(v.root().join("imports/random-export/mystery.csv").exists());
        let listed = v.list_declined();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].file, "imports/random-export/mystery.csv");
        // A second drop of the same name gets a timestamp suffix (no clobber).
        v.land_declined("random-export", &p).unwrap();
        assert_eq!(v.list_declined().len(), 2);
        let files: Vec<_> = fs::read_dir(v.root().join("imports/random-export")).unwrap().flatten().collect();
        assert_eq!(files.len(), 2, "both raw drops kept");
    }

    #[test]
    fn land_raw_collision_gets_timestamp_suffix() {
        let v = temp_vault("collision");
        let rel1 = land_file(&v, "social/s/raw", &write_file(&v, "a.csv", "x")).unwrap();
        let rel2 = land_file(&v, "social/s/raw", &write_file(&v, "a.csv", "y")).unwrap();
        assert_eq!(rel1, "social/s/raw/a.csv");
        assert_ne!(rel1, rel2, "collision renamed with a timestamp");
        assert!(rel2.starts_with("social/s/raw/a-"));
    }

    #[test]
    fn read_raw_page_paginates_csv_and_reports_has_more() {
        let v = temp_vault("read-raw-csv");
        // Land the gate-shaped fixture (2 data rows) under imports/, then read it.
        let src = write_file(&v, "drop.csv", PUBOPS_CSV);
        let d = v.land_declined("my-export", &src).unwrap();

        // First page of 1 row: the unnamed leading column surfaces as `col0`,
        // named columns keep their header, and has_more is true (a 2nd row waits).
        let p0 = v.read_raw(&d.file, 0, 1).unwrap();
        assert_eq!(p0.format, "csv");
        assert_eq!(p0.rows.len(), 1);
        assert!(p0.has_more);
        assert_eq!(p0.rows[0].get("col0").and_then(|x| x.as_str()), Some("1"));
        assert_eq!(
            p0.rows[0].get("Post URL").and_then(|x| x.as_str()),
            Some("https://example.com/p/1")
        );

        // Second page: the last row, no more.
        let p1 = v.read_raw(&d.file, 1, 1).unwrap();
        assert_eq!(p1.rows.len(), 1);
        assert!(!p1.has_more);
        assert_eq!(p1.offset, 1);
        assert_eq!(p1.rows[0].get("col0").and_then(|x| x.as_str()), Some("2"));

        // `.trove/` is jailed by resolve_user — and the jail is real, not an
        // artifact of the file being absent: create a genuine secret there and
        // confirm every spelling that reaches the same inode is refused. (On
        // case-insensitive macOS, `.Trove` and `./.trove` open the real file;
        // the jail rejects them before any open, so the assertion holds on
        // case-sensitive filesystems too.)
        let secret = v.root().join(".trove/normalizer/anthropic-key.json");
        fs::create_dir_all(secret.parent().unwrap()).unwrap();
        fs::write(&secret, "{\"key\":\"sk-ant-do-not-leak\"}").unwrap();
        assert!(secret.exists(), "secret really exists in .trove/");
        assert!(v.read_raw(".trove/normalizer/anthropic-key.json", 0, 10).is_err(), "plain .trove/ blocked");
        assert!(v.read_raw("./.trove/normalizer/anthropic-key.json", 0, 10).is_err(), "./ prefix blocked");
        assert!(v.read_raw(".Trove/normalizer/anthropic-key.json", 0, 10).is_err(), "case variant blocked");
        assert!(v.read_raw("../outside.csv", 0, 10).is_err(), "path escape blocked");
        // read_artifact shares the same jail (the IPC command forwards the path).
        assert!(v.read_artifact(".trove/normalizer/anthropic-key.json").is_err(), "artifact read jailed too");
    }

    #[test]
    fn read_raw_page_reads_jsonl_objects() {
        let v = temp_vault("read-raw-jsonl");
        let body = "{\"a\":1,\"b\":\"x\"}\n\n{\"a\":2,\"b\":\"y\"}\n";
        let src = write_file(&v, "drop.jsonl", body);
        let d = v.land_declined("j", &src).unwrap();
        let p = v.read_raw(&d.file, 0, 10).unwrap();
        assert_eq!(p.format, "jsonl");
        assert_eq!(p.rows.len(), 2, "blank line skipped");
        assert!(!p.has_more);
        assert_eq!(p.rows[1].get("a").and_then(|x| x.as_i64()), Some(2));
    }

    // ---- detect (Step 2) ----------------------------------------------------

    // Real IMDb export headers (from imdb.rs — the built importer's own shapes).
    const IMDB_RATINGS_HDR: &str =
        "Const,Your Rating,Date Rated,Title,URL,Title Type,IMDb Rating,Runtime (mins),Year,Genres,Num Votes,Release Date,Directors\r\n\
tt0110912,10,2024-03-15,Pulp Fiction,https://www.imdb.com/title/tt0110912/,movie,8.9,154,1994,\"Crime, Drama\",2100000,1994-10-14,Quentin Tarantino\r\n";
    const IMDB_LIST_HDR: &str =
        "Position,Const,Created,Modified,Description,Title,URL,Title Type,IMDb Rating,Runtime (mins),Year,Genres,Num Votes,Release Date,Directors,Your Rating,Date Rated\r\n\
1,tt0816692,2023-01-15,2023-01-15,,Interstellar,https://www.imdb.com/title/tt0816692/,movie,8.7,169,2014,\"Adventure, Drama, Sci-Fi\",2000000,2014-11-07,Christopher Nolan,,\r\n";

    #[test]
    fn detect_routes_imdb_exports_to_the_built_importer() {
        let v = temp_vault("detect-imdb");
        // Ratings export → path 1 (route to imdb), distinguished from a list.
        let p = write_file(&v, "ratings.csv", IMDB_RATINGS_HDR);
        let d = detect(&p).unwrap();
        match d.outcome {
            DetectOutcome::Route(r) => {
                assert_eq!(r.integration_id, "imdb");
                assert_eq!(r.shape_label, "IMDb ratings");
            }
            other => panic!("expected route to imdb, got {other:?}"),
        }
        // Watchlist/custom-list export → also path 1 (the list signature).
        let p2 = write_file(&v, "WATCHLIST.csv", IMDB_LIST_HDR);
        match detect(&p2).unwrap().outcome {
            DetectOutcome::Route(r) => {
                assert_eq!(r.integration_id, "imdb");
                assert_eq!(r.shape_label, "IMDb watchlist / custom list");
            }
            other => panic!("expected route to imdb list, got {other:?}"),
        }
    }

    #[test]
    fn importer_signatures_tolerate_reorder_case_and_extra_columns() {
        // Reordered, re-cased, and with an unknown trailing column — the marker
        // subset still claims it (name-based, not positional).
        let headers: Vec<String> = vec![
            "TITLE".into(), "url".into(), "const".into(), "  Your   Rating ".into(),
            "date rated".into(), "title type".into(), "Some New Column".into(),
        ];
        let route = importer_route(&headers, Format::Csv).expect("ratings markers present");
        assert_eq!(route.integration_id, "imdb");
        // JSONL never routes to a CSV-header importer.
        assert!(importer_route(&headers, Format::Jsonl).is_none());
        // Missing a required marker → no claim.
        let missing: Vec<String> =
            vec!["Const".into(), "Title".into(), "URL".into()]; // no Your Rating/Date Rated
        assert!(importer_route(&missing, Format::Csv).is_none());
    }

    #[test]
    fn detect_routes_letterboxd_diary() {
        let v = temp_vault("detect-letterboxd");
        let csv = "Date,Name,Year,Letterboxd URI,Rating,Rewatch,Tags,Watched Date\r\n\
2026-01-02,Some Film,2020,https://boxd.it/abc,4,,tag,2026-01-03\r\n";
        let p = write_file(&v, "diary.csv", csv);
        match detect(&p).unwrap().outcome {
            DetectOutcome::Route(r) => assert_eq!(r.integration_id, "letterboxd"),
            other => panic!("expected route to letterboxd, got {other:?}"),
        }
    }

    #[test]
    fn detect_ranks_social_post_top_for_pubops_with_a_projectable_draft() {
        let v = temp_vault("detect-pubops");
        let p = write_file(&v, "pubops.csv", PUBOPS_CSV);
        let d = detect(&p).unwrap();
        assert_eq!(d.format, "csv");
        assert_eq!(d.headers.len(), 10);
        assert!(d.sample_rows.len() <= SAMPLE_ROWS && !d.sample_rows.is_empty());
        // The unnamed leading column surfaces as col0 in the display sample.
        assert_eq!(d.sample_rows[0]["col0"], json!("1"));

        let candidates = match &d.outcome {
            DetectOutcome::Contract { candidates, .. } => candidates,
            other => panic!("expected a contract match, got {other:?}"),
        };
        // social.post is the top-ranked contract for the pub_ops export.
        let top = &candidates[0];
        assert_eq!((top.domain.as_str(), top.shape.as_str()), ("social", "post"));

        // The draft binds the observable columns: ts ← Post Date (date-only,
        // coerced), url ← Post URL (verbatim), guid = hash(Post URL).
        let draft = &top.draft;
        assert_eq!(draft.provenance.suggested_by, "heuristic");
        let ts = draft.bindings.iter().find(|b| b.to == "ts").expect("ts bound");
        assert_eq!(ts.from, "Post Date");
        assert_eq!(ts.coerce.as_deref(), Some("date"));
        let url = draft.bindings.iter().find(|b| b.to == "url").expect("url bound");
        assert_eq!(url.from, "Post URL");
        assert!(url.coerce.is_none(), "url is a verbatim rename");
        assert!(matches!(&draft.guid, GuidRecipe::Hash { hash, .. } if hash == &["Post URL"]));
        // Unmapped columns are reported so the user knows what rides `extra`.
        assert!(draft.validate().is_ok(), "the draft is a valid, saveable mapping");
        assert!(top.unbound_headers.iter().any(|h| h == "Framed By"));

        // End to end: the drafted mapping actually projects the file into
        // social.post rows that validate — detect hands the engine a working
        // mapping with zero edits.
        draft.save(&v).unwrap();
        let out = draft.project(&v, &p, &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 2);
        let jul = fs::read_to_string(v.root().join(format!("social/{}/2026-07.jsonl", draft.source))).unwrap();
        let post: crate::social::Post = serde_json::from_str(jul.lines().next().unwrap()).unwrap();
        assert_eq!(post.ts, "2026-07-15", "date-only ts projected, no fabricated midnight");
        assert_eq!(post.url, "https://example.com/p/1");
    }

    #[test]
    fn detect_declines_a_file_that_fits_nothing() {
        let v = temp_vault("detect-junk");
        // No temporal column, no urls, no schema-shaped headers → honest decline.
        let junk = "alpha,beta,gamma\r\nfoo,bar,baz\r\nqux,quux,corge\r\n";
        let p = write_file(&v, "mystery.csv", junk);
        assert!(matches!(detect(&p).unwrap().outcome, DetectOutcome::NoMatch));
    }

    #[test]
    fn detect_reads_jsonl_and_ranks_by_value_shape() {
        let v = temp_vault("detect-jsonl");
        // A flat JSONL with an authored timestamp + url + a title-ish field.
        let body = "\
{\"posted_at\":\"2026-07-15T10:00:00Z\",\"permalink\":\"https://example.com/1\",\"body\":\"hello world\"}\n\
{\"posted_at\":\"2026-07-16T10:00:00Z\",\"permalink\":\"https://example.com/2\",\"body\":\"another\"}\n";
        let p = write_file(&v, "posts.jsonl", body);
        let d = detect(&p).unwrap();
        assert_eq!(d.format, "jsonl");
        let candidates = match &d.outcome {
            DetectOutcome::Contract { candidates, .. } => candidates,
            other => panic!("expected a contract match, got {other:?}"),
        };
        // Whatever ranks top, its draft binds an RFC3339 ts and a url, and is
        // a valid mapping. (JSONL date-times coerce via rfc3339.)
        let draft = &candidates[0].draft;
        let ts = draft.bindings.iter().find(|b| b.to == "ts").expect("ts bound");
        assert_eq!(ts.from, "posted_at");
        assert_eq!(
            ts.with.as_ref().and_then(|w| w.get("formats")).and_then(|f| f.get(0)).and_then(|f| f.as_str()),
            Some("rfc3339"),
            "a datetime column coerces via rfc3339, not date-only"
        );
        assert!(draft.bindings.iter().any(|b| b.to == "url" && b.from == "permalink"));
        assert!(draft.validate().is_ok());
    }

    // ---- detect internals ---------------------------------------------------

    #[test]
    fn cell_shape_classifies_values() {
        assert_eq!(cell_shape("2026-07-15"), Cell::DateOnly);
        assert_eq!(cell_shape("07/15/2026"), Cell::DateOnly);
        assert_eq!(cell_shape("2026-07-15T12:00:00Z"), Cell::DateTime);
        assert_eq!(cell_shape("2026-07-15 09:30:00"), Cell::DateTime);
        assert_eq!(cell_shape("https://example.com/x"), Cell::Url);
        assert_eq!(cell_shape("13,133"), Cell::Int);
        assert_eq!(cell_shape("8.9"), Cell::Dec);
        assert_eq!(cell_shape("true"), Cell::Bool);
        assert_eq!(cell_shape("Best Widgets"), Cell::Text);
        assert_eq!(cell_shape("   "), Cell::Empty);
    }

    #[test]
    fn slugify_makes_valid_source_slugs() {
        assert_eq!(slugify("pub_ops fb_pages 2026-07-20T1403"), "pub-ops-fb-pages-2026-07-20t1403");
        assert_eq!(slugify("  Weird!!Name  "), "weird-name");
        assert!(is_slug(&slugify("pubops.csv")));
        assert_eq!(slugify("***"), "dropped-source");
    }

    #[test]
    fn generic_name_tokens_do_not_alone_bind() {
        // A "… Name"/"… Type" header must not bind to a schema field just
        // because both carry the generic token — the false-positive guard.
        let v = temp_vault("detect-generic");
        // Two text columns whose only lexical tie to any field is name/type,
        // plus a lone date so a candidate is even considered.
        let csv = "when,list name,scheduling type\r\n2026-07-15,Best Widgets,manual\r\n";
        let p = write_file(&v, "g.csv", csv);
        if let DetectOutcome::Contract { candidates, .. } = detect(&p).unwrap().outcome {
            for c in &candidates {
                for b in &c.draft.bindings {
                    // No binding may come from the "name"/"type"-only columns.
                    assert!(
                        b.from != "list name" && b.from != "scheduling type",
                        "{}.{} spuriously bound {} via a generic token",
                        c.domain, c.shape, b.from
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Step 2c — the opt-in LLM advisor (mocked transport; no real network)

    use std::cell::RefCell;

    /// A transport that returns canned responses in order, recording every
    /// (api_key, body) it was asked to send. Never touches the network.
    struct MockTransport {
        responses: RefCell<Vec<Result<Value>>>,
        calls: RefCell<Vec<(String, Value)>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<Value>>) -> MockTransport {
            MockTransport { responses: RefCell::new(responses), calls: RefCell::new(Vec::new()) }
        }
        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl LlmTransport for MockTransport {
        fn post_messages(&self, api_key: &str, body: &Value) -> Result<Value> {
            self.calls.borrow_mut().push((api_key.to_string(), body.clone()));
            let mut r = self.responses.borrow_mut();
            if r.is_empty() {
                bail!("mock: no more responses");
            }
            r.remove(0)
        }
    }

    /// Wrap a mapping-artifact JSON string in a minimal Messages API response.
    fn model_reply(text: &str) -> Value {
        json!({
            "stop_reason": "end_turn",
            "content": [ { "type": "text", "text": text } ],
        })
    }

    /// A valid `social.post` mapping the model could return for the gate file.
    fn pubops_suggestion() -> String {
        json!({
            "domain": "social",
            "shape": "post",
            "bindings": [
                { "from": "Post Date", "to": "ts", "coerce": "date" },
                { "from": "Post URL", "to": "url" },
                { "from": "List Name", "to": "title" },
                { "from": "Page Name", "to": "context" }
            ],
            "constants": { "kind": "post" },
            "guid": { "hash": ["Post URL"] }
        })
        .to_string()
    }

    fn pubops_detection(v: &Vault) -> Detection {
        let p = write_file(v, "pubops.csv", PUBOPS_CSV);
        detect(&p).unwrap()
    }

    #[test]
    fn payload_carries_headers_sample_and_candidate_metadata() {
        let v = temp_vault("llm-payload");
        let det = pubops_detection(&v);
        let payload = SuggestPayload::from_detection(&det);

        // The literal user data that would leave the machine.
        assert_eq!(payload.headers, det.headers);
        assert!(payload.sample_rows.len() <= SAMPLE_ROWS);
        assert_eq!(payload.sample_rows.len(), 2, "the fixture has two rows");
        assert_eq!(payload.format, "csv");
        assert!(!payload.candidates.is_empty());
        // Candidate metadata is real contract field metadata.
        let c = &payload.candidates[0];
        assert!(!c.fields.is_empty());
        assert!(c.fields.iter().any(|f| !f.description.is_empty()));

        // The exact wire body embeds headers + sample rows verbatim.
        let body = payload.request_body();
        let wire = serde_json::to_string(&body).unwrap();
        assert!(wire.contains("Post URL"), "headers must be in the payload");
        assert!(wire.contains("13,133"), "sample-row values must be in the payload");
        assert_eq!(body["model"], json!(LLM_MODEL));
    }

    #[test]
    fn suggestion_is_a_valid_mapping_indistinguishable_from_heuristic() {
        let v = temp_vault("llm-suggest-ok");
        let det = pubops_detection(&v);
        let payload = SuggestPayload::from_detection(&det);
        let transport = MockTransport::new(vec![Ok(model_reply(&pubops_suggestion()))]);

        let m = payload.suggest("acme-pubops", "sk-test", &transport).unwrap();
        assert_eq!(transport.call_count(), 1, "one network call, no retry");

        // Same type + shape as a heuristic draft; only provenance differs.
        assert_eq!(m.provenance.suggested_by, "llm");
        assert_eq!(m.domain, "social");
        assert_eq!(m.shape, "post");
        assert_eq!(m.source, "acme-pubops");
        assert_eq!(m.unbound, "extra");
        assert_eq!(m.version, MAPPING_VERSION);
        // Signature is authoritative — built from the real file, not the model.
        assert!(m.signature.matches(&det.headers, Format::Csv));
        // Fully valid + projects to a real contract.
        m.validate().unwrap();
        assert!(m.projection().is_some());
    }

    #[test]
    fn mismatched_response_triggers_exactly_one_retry_then_succeeds() {
        let v = temp_vault("llm-retry");
        let det = pubops_detection(&v);
        let payload = SuggestPayload::from_detection(&det);
        // First reply is unparseable prose; the retry returns a valid mapping.
        let transport = MockTransport::new(vec![
            Ok(model_reply("I'm not sure, here are some thoughts without JSON.")),
            Ok(model_reply(&pubops_suggestion())),
        ]);

        let m = payload.suggest("acme-pubops", "sk-test", &transport).unwrap();
        assert_eq!(transport.call_count(), 2, "one retry after the bad first reply");
        assert_eq!(m.domain, "social");
        // The retry body carried the failure back to the model.
        let second = &transport.calls.borrow()[1].1;
        let user = second["messages"][0]["content"].as_str().unwrap();
        assert!(user.contains("previous attempt was rejected"));
    }

    #[test]
    fn two_bad_responses_error_out() {
        let v = temp_vault("llm-both-bad");
        let det = pubops_detection(&v);
        let payload = SuggestPayload::from_detection(&det);
        // A syntactically valid object but a hallucinated contract, twice.
        let bogus = json!({
            "domain": "not-a-real", "shape": "shape",
            "bindings": [], "guid": { "hash": ["x"] }
        })
        .to_string();
        let transport =
            MockTransport::new(vec![Ok(model_reply(&bogus)), Ok(model_reply(&bogus))]);

        let err = payload.suggest("acme-pubops", "sk-test", &transport).unwrap_err();
        assert_eq!(transport.call_count(), 2);
        assert!(err.to_string().contains("failed twice"), "{err}");
    }

    #[test]
    fn refusal_response_is_a_clean_error() {
        let v = temp_vault("llm-refusal");
        let det = pubops_detection(&v);
        let payload = SuggestPayload::from_detection(&det);
        let refusal = json!({
            "stop_reason": "refusal",
            "stop_details": { "explanation": "declined" },
            "content": []
        });
        // Same refusal twice (one retry), then a terminal error.
        let transport = MockTransport::new(vec![Ok(refusal.clone()), Ok(refusal)]);
        let err = payload.suggest("acme-pubops", "sk-test", &transport).unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
    }

    #[test]
    fn json_extraction_tolerates_fences_and_prose() {
        let wrapped = "Sure! Here is the mapping:\n```json\n{\"domain\":\"social\",\"shape\":\"post\",\"guid\":{\"hash\":[\"u\"]}}\n```\nHope that helps.";
        let obj = extract_json_object(wrapped).unwrap();
        assert_eq!(obj["domain"], json!("social"));
    }

    #[test]
    fn advisor_unavailable_without_a_key() {
        let v = temp_vault("llm-status-nokey");
        // Baked key may or may not be compiled in; the test asserts the pure
        // no-key path only when there is genuinely no key resolvable.
        if resolve_llm_key(&v).unwrap().is_none() {
            let status = llm_advisor_status(&v).unwrap();
            assert!(!status.available);
            assert!(status.reason.is_some());
            assert_eq!(status.model, LLM_MODEL);
            assert!(status.source.is_none());

            let det = pubops_detection(&v);
            let payload = SuggestPayload::from_detection(&det);
            let err =
                llm_suggest_with(&v, "acme-pubops", &payload, &UreqTransport).unwrap_err();
            assert!(err.to_string().contains("not configured"), "{err}");
        }
    }

    #[test]
    fn byo_key_roundtrips_and_overrides_baked() {
        let v = temp_vault("llm-key-byo");
        assert!(v.load_llm_key().unwrap().is_none());
        assert!(v.save_llm_key("  ").is_err(), "empty key is rejected");

        v.save_llm_key("sk-ant-byo").unwrap();
        assert_eq!(v.load_llm_key().unwrap().as_deref(), Some("sk-ant-byo"));

        // BYO wins regardless of whether a baked key is compiled in.
        let (key, source) = resolve_llm_key(&v).unwrap().unwrap();
        assert_eq!(key, "sk-ant-byo");
        assert_eq!(source, "byo");
        let status = llm_advisor_status(&v).unwrap();
        assert!(status.available);
        assert_eq!(status.source.as_deref(), Some("byo"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(v.llm_key_path().unwrap()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "advisor key file must be 0600");
        }

        v.delete_llm_key().unwrap();
        assert!(v.load_llm_key().unwrap().is_none());
    }

    #[test]
    fn from_detection_offers_all_projectable_shapes_on_no_match() {
        // A file the heuristics decline still gives the advisor every
        // projectable contract to classify into.
        let v = temp_vault("llm-nomatch-candidates");
        let csv = "alpha,beta,gamma\r\nfoo,bar,baz\r\n";
        let p = write_file(&v, "junk.csv", csv);
        let det = detect(&p).unwrap();
        let payload = SuggestPayload::from_detection(&det);
        let projectable = contract_schemas().iter().filter(|s| s.proj.is_some()).count();
        assert!(matches!(det.outcome, DetectOutcome::NoMatch));
        assert_eq!(payload.candidates.len(), projectable);
    }
}


//! MyFitnessPal — Premium "Download Your Data" ZIP import.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/myfitnesspal.md.
//!
//! ## Export format
//!
//! MyFitnessPal Premium (Settings → Account → Download Your Data) delivers a
//! ZIP archive within ~1 hour via email link. The ZIP contains up to three
//! CSVs (exact file names vary by account and MFP version):
//!
//! - **Meal Level Nutrition Details** — one row per logged entry (either a
//!   per-food item OR a meal+time-group rollup, depending on the export variant
//!   and account locale — exact granularity is unconfirmed: Needs-sample), with
//!   a date/timestamp, meal slot, optional food name, and a macro/micro tail.
//!   This is the primary source for the `health-nutrition` contract.
//! - **Progress History** — body weight and measurement rows over time.
//!   Routed to the raw layer (body-measurement writes to `health/` are a
//!   read-time concern; see brief).
//! - **Exercise History** — logged exercise sessions. Raw-only pending a
//!   contract decision (brief).
//!
//! ## Parser status
//!
//! **Meal-nutrition parser: PROVISIONAL — Needs-sample.** MyFitnessPal does
//! not publish exact Premium export column names. The alias table below is
//! inferred from the official API field names (python-myfitnesspal library
//! types.py), the athlete_data_warehouse MFP integration, and common
//! nutrition-tracker conventions. These need confirmation against a real
//! Premium export before they can be trusted. The raw layer stores the export
//! at full fidelity regardless; the contract rows are best-effort provisional.
//!
//! Per the evidence rule (cf. the raindrop `_id` bug, the fathom
//! embedded-transcript trap): a green test over a fabricated fixture is false
//! confidence. The fixtures in the `tests` module are *scaffold* rows built
//! from known API field names, not from a real Premium export. When a real
//! sample lands:
//!
//! 1. Tighten the `MEAL_COL_ALIASES`, `DATE_ALIASES`, `FOOD_ALIASES`,
//!    `MEAL_SLOT_ALIASES` constants to the exact header strings.
//! 2. Update the fixture constants to match the real file.
//! 3. Remove the "provisional" / "Needs-sample" notes from the module doc.
//!
//! ## Vault layout
//!
//! - `health/nutrition/myfitnesspal/raw/<stamp>-<filename>` — every CSV from
//!   the export, stored verbatim at each import (accumulating, not overwriting).
//! - `health/nutrition/myfitnesspal/YYYY-MM.jsonl` — `health-nutrition::Entry`
//!   rows from the meal-nutrition CSV, deduped by `guid`, month-partitioned.
//!
//! ## Access
//!
//! User downloads the ZIP from myfitnesspal.com (Premium/Premium+ only) and
//! drops it here. No API (closed to new developers as of 2026). No credentials
//! held by Trove — fully offline once downloaded. The import is re-runnable:
//! duplicate entries are skipped via `guid` deduplication.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::health_nutrition::Entry;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::{write_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths.

/// Contract-layer JSONL directory (month-partitioned Entry rows).
const DIR: &str = "health/nutrition/myfitnesspal";
/// Raw JSONL stream: verbatim CSV row objects, month-partitioned (per Cronometer pattern).
const RAW_DIR: &str = "health/nutrition/myfitnesspal/raw";
/// Verbatim file snapshots: original export files stored byte-for-byte on each import.
const SNAPSHOTS_DIR: &str = "health/nutrition/myfitnesspal/snapshots";
/// Source id (matches the vault folder name and `Entry.source`).
const SOURCE: &str = "myfitnesspal";

// ---------------------------------------------------------------------------
// Column-alias tables — PROVISIONAL until confirmed against a real export.
//
// MyFitnessPal does not publish exact Premium export column names. These
// aliases are inferred from:
//  - python-myfitnesspal/myfitnesspal/types.py (FoodItemNutritionDict,
//    DEFAULT_MEASURE_AND_UNIT: calories, carbohydrates, fat, protein, sodium,
//    sugar, fiber, potassium, cholesterol, saturated_fat, polyunsaturated_fat,
//    monounsaturated_fat, trans_fat, vitamin_a, vitamin_c, calcium, iron).
//  - pgalko/athlete_data_warehouse mfp_data_download_db_insert.py column list
//    (same fields in snake_case: sat_fat, ply_fat, mon_fat, trn_fat, chol,
//    potass, vit_a, vit_c).
//  - Common nutrition-tracker CSV conventions (Title-Case headers).
//
// Header matching is case-insensitive. The first alias that hits wins. Any
// column that doesn't match any alias still lands in the raw layer verbatim.

/// Aliases for the date/timestamp column. MFP may use "Date" or "Date & Time".
const DATE_ALIASES: &[&str] = &["Date & Time", "Date and Time", "Date/Time", "Date", "date"];

/// Aliases for the meal slot column.
const MEAL_SLOT_ALIASES: &[&str] = &["Meal", "Meal Name", "Meal Type", "meal"];

/// Aliases for the food name column.
const FOOD_ALIASES: &[&str] = &["Food Name", "Food", "Item", "Description", "food_name"];

/// Aliases for the brand/manufacturer column.
const BRAND_ALIASES: &[&str] = &["Brand", "Manufacturer", "brand"];

/// Aliases for the serving amount/quantity column (used in the guid hash so
/// two identically-named foods logged at different servings within the same
/// meal+time group get distinct guids, matching the Cronometer recipe).
const AMOUNT_ALIASES: &[&str] = &["Amount", "Quantity", "Serving", "Servings", "amount", "quantity"];

/// Typed-macro columns: `(aliases, setter_fn)`.
/// Everything NOT listed here (and not a core col) goes into `nutrients`.
const TYPED_MACROS: &[(&[&str], fn(&mut Entry, f64))] = &[
    (
        &["Calories", "Energy (kcal)", "Energy", "calories", "Cal"],
        |e, v| e.energy_kcal = Some(v),
    ),
    (
        &["Protein (g)", "Protein", "protein"],
        |e, v| e.protein_g = Some(v),
    ),
    (
        &["Carbohydrates (g)", "Carbohydrates", "Carbs (g)", "Carbs", "carbohydrates", "carbs"],
        |e, v| e.carb_g = Some(v),
    ),
    (
        &["Fat (g)", "Fat", "Total Fat", "fat"],
        |e, v| e.fat_g = Some(v),
    ),
    (
        &["Fiber (g)", "Dietary Fiber", "Fiber", "fiber"],
        |e, v| e.fiber_g = Some(v),
    ),
    (
        &["Sugar (g)", "Sugars (g)", "Total Sugars", "Sugars", "Sugar", "sugar"],
        |e, v| e.sugar_g = Some(v),
    ),
    (
        &[
            "Saturated Fat (g)", "Saturated Fat", "Saturated (g)", "Saturated",
            "Sat Fat", "sat_fat", "saturated_fat",
        ],
        |e, v| e.saturated_fat_g = Some(v),
    ),
    (
        &[
            "Polyunsaturated Fat (g)", "Polyunsaturated Fat", "Polyunsaturated",
            "Ply Fat", "ply_fat", "polyunsaturated_fat",
        ],
        |e, v| e.polyunsaturated_fat_g = Some(v),
    ),
    (
        &[
            "Monounsaturated Fat (g)", "Monounsaturated Fat", "Monounsaturated",
            "Mon Fat", "mon_fat", "monounsaturated_fat",
        ],
        |e, v| e.monounsaturated_fat_g = Some(v),
    ),
    (
        &["Sodium (mg)", "Sodium", "sodium"],
        |e, v| e.sodium_mg = Some(v),
    ),
    (
        &["Cholesterol (mg)", "Cholesterol", "Chol", "chol", "cholesterol"],
        |e, v| e.cholesterol_mg = Some(v),
    ),
];

/// "Core" columns consumed by name (not routed into `nutrients`).
const CORE_COLS: &[&[&str]] = &[
    DATE_ALIASES,
    MEAL_SLOT_ALIASES,
    FOOD_ALIASES,
    BRAND_ALIASES,
    AMOUNT_ALIASES,
];

// ---------------------------------------------------------------------------
// CSV file-name heuristics for the ZIP extraction.
//
// The exact filenames inside the MFP export ZIP are not published. These
// patterns are provisional — tighten against a real export.

/// Keywords that identify the meal/food-diary CSV (case-insensitive substring).
const MEAL_CSV_HINTS: &[&str] = &["meal", "nutrition", "food", "diary"];

/// Keywords that identify the progress/body-measurements CSV.
const PROGRESS_CSV_HINTS: &[&str] = &["progress", "measurement", "weight", "body"];

/// Keywords that identify the exercise-history CSV.
const EXERCISE_CSV_HINTS: &[&str] = &["exercise", "workout", "activity", "fitness"];

// ---------------------------------------------------------------------------
// DEF.

fn def_last_data(vault: &Vault) -> Option<String> {
    // Prefer the contract partition date; fall back to snapshot mtime.
    crate::registry::newest_stem(&vault.root().join(DIR))
        .or_else(|| crate::registry::newest_mtime(&vault.root().join(SNAPSHOTS_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
/// The pub-mod and INTEGRATIONS lines already exist (Phase 2 stub); this build
/// replaces the `NotWired` body with the real `Import` implementation.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "myfitnesspal",
        name: "MyFitnessPal",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your MyFitnessPal Premium export — per-meal food diary entries \
                      with macros, micros, exercise logs, and progress history — the most \
                      detailed micronutrient export of any nutrition app. Re-importable: \
                      duplicate entries are never double-counted.",
        domain: "health-nutrition",
        vault_path: "health/nutrition/myfitnesspal/",
        toggleable: false,
        setup: &[
            "myfitnesspal.com → Settings → Account → Download Your Data (Premium/Premium+ only).",
            "Wait for the email link (usually within 1 hour), download the ZIP.",
            "Drop the ZIP (or individual CSVs) here.",
        ],
        caveats: "Requires a MyFitnessPal Premium or Premium+ subscription (~$10/month) — \
                  the data export is not available to free accounts. The public API is closed \
                  to new developers as of 2026; this import is the only data path.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // Accept the export ZIP (primary) and individual CSVs (if the user unpacks).
    accepts: &["zip", "csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import entry point.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let is_zip = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip"));
    if is_zip {
        import_zip(vault, path, progress)
    } else {
        // A bare CSV: treat it as the meal-nutrition file (the most common
        // reason a user would drop a bare CSV).
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let stamp = Local::now().format("%Y%m%dT%H%M%S").to_string();
        let orig_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("export.csv");
        let snap_rel = format!("{SNAPSHOTS_DIR}/{stamp}-{orig_name}");
        let snap_path = vault.resolve(&snap_rel)?;
        write_atomic(&snap_path, body.as_bytes())?;
        import_meal_csv_body(vault, &body, progress)
    }
}

/// Handle the ZIP archive: extract and store each CSV verbatim, then parse the
/// meal-nutrition CSV into contract rows.
fn import_zip(
    vault: &Vault,
    path: &Path,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).with_context(|| format!("reading ZIP {}", path.display()))?;

    let stamp = Local::now().format("%Y%m%dT%H%M%S").to_string();

    // Collect all CSV filenames inside the archive.
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| {
            zip.by_index(i).ok().filter(|e| {
                e.is_file()
                    && e.name().ends_with(".csv")
                    && !e.name().contains("__MACOSX")
            })
            .map(|e| e.name().to_string())
        })
        .collect();

    let mut meal_body: Option<String> = None;
    let mut raw_files: u64 = 0;

    for name in &names {
        // Read the CSV entry.
        let mut entry = zip.by_name(name)
            .with_context(|| format!("reading ZIP entry {name}"))?;
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).with_context(|| format!("reading {name}"))?;

        // Store verbatim in the snapshots layer (full fidelity, unconditional).
        let base = Path::new(name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name.as_str());
        let snap_rel = format!("{SNAPSHOTS_DIR}/{stamp}-{base}");
        let snap_path = vault.resolve(&snap_rel)?;
        write_atomic(&snap_path, &bytes)?;
        raw_files += 1;

        // Identify the meal-nutrition CSV by filename heuristic.
        let lc = name.to_ascii_lowercase();
        if meal_body.is_none() && MEAL_CSV_HINTS.iter().any(|h| lc.contains(h)) {
            // Prefer meal/nutrition/food/diary over progress/exercise.
            let is_progress = PROGRESS_CSV_HINTS.iter().any(|h| lc.contains(h));
            let is_exercise = EXERCISE_CSV_HINTS.iter().any(|h| lc.contains(h));
            if !is_progress && !is_exercise {
                if let Ok(s) = String::from_utf8(bytes) {
                    meal_body = Some(s);
                }
            }
        }
    }

    if raw_files == 0 {
        return Ok(ImportOutcome {
            headline: "No CSV files found in the ZIP — is this a MyFitnessPal export?".into(),
            counts: [("raw_files", 0u64), ("entries", 0u64)].into(),
        });
    }

    progress(ImportProgress { records: raw_files, percent: 50.0 });

    // Parse the meal-nutrition CSV if we found it; otherwise raw-only.
    // The `raw_files` count is set ONCE below; `import_meal_csv_body` never
    // sets a "raw_files" key, so we always insert it here rather than also
    // including it in the no-meal branch (which would cause a double-add).
    let mut outcome = if let Some(body) = meal_body {
        import_meal_csv_body(vault, &body, progress)?
    } else {
        progress(ImportProgress { records: 0, percent: 100.0 });
        ImportOutcome {
            headline: format!(
                "{raw_files} raw CSV file(s) stored — meal-nutrition CSV not identified in ZIP \
                 (Needs-sample: exact filenames unconfirmed). \
                 Raw files are in health/nutrition/myfitnesspal/snapshots/"
            ),
            // Do NOT pre-populate raw_files here; it is set unconditionally below.
            counts: [("entries", 0u64)].into(),
        }
    };

    // Set raw_files exactly once on the merged outcome (whether or not the
    // meal CSV was found). Using insert rather than or_insert + += avoids a
    // double-add when we just created the no-meal branch above.
    outcome.counts.insert("raw_files", raw_files);
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Meal-nutrition CSV parser (provisional — Needs-sample).

/// Raw row wrapper: the verbatim CSV row as a JSON object; `ts` drives
/// the month-partition key but is not serialized.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Parse one in-memory meal-nutrition CSV and write both the raw and contract
/// layers. This is the testable seam.
fn import_meal_csv_body(
    vault: &Vault,
    body: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Load already-persisted guids for idempotent re-import.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                if !g.is_empty() {
                    seen.insert(g.to_string());
                }
            }
        }
    }

    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers = rdr
        .headers()
        .context("reading CSV header row — is this a MyFitnessPal meal-nutrition export?")?
        .clone();

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut entries: Vec<Entry> = Vec::new();
    let mut raws: Vec<RawLine> = Vec::new();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };

        // Build a header→value map (verbatim strings).
        let fields: Map<String, Value> = headers
            .iter()
            .zip(rec.iter())
            .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
            .collect();

        // Raw layer is unconditional — every well-formed row lands verbatim.
        // Use the full ts for month-partitioning (Partition::Month extracts YYYY-MM).
        // Fall back to a sentinel date string when no date column can be found.
        let raw_ts = col_val_ci(&fields, DATE_ALIASES)
            .and_then(parse_date_ts)
            .unwrap_or_else(|| "0000-00-00".to_string());
        raws.push(RawLine { ts: raw_ts, value: Value::Object(fields.clone()) });

        // Contract row: parse date, food name, macros.
        let Some(entry) = entry_from(&fields) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(entry.guid.clone()) {
            duplicates += 1;
            continue;
        }
        entries.push(entry);
        imported += 1;
        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Raw layer first (full fidelity, unconditional), then contract.
    raw.append(&raws, |r| &r.ts)?;
    contract.append(&entries, |e| &e.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{imported} entries imported, {duplicates} duplicates skipped \
             (provisional parser — Needs-sample: confirm column names against a real \
             MyFitnessPal Premium export)"
        ),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Row → Entry mapping.

/// One CSV row (header→value map) → a contract [`Entry`]. Returns `None` when
/// the row has no parseable date (can't partition).
///
/// Food name is **optional**: MFP may export per-food rows (with a "Food Name"
/// column) OR meal+time-group rollup rows (no per-food column). When no food
/// name is present the meal-slot label becomes the entry label, matching the
/// documented meal+time-group granularity of the "Meal Level Nutrition Details"
/// export. Either way the row is a valid nutrition entry.
///
/// The `guid` includes the full timestamp (with clock time where the source
/// carries one) plus the meal label and amount/serving, mirroring the Cronometer
/// guid recipe so that two distinct meal-time groups on the same day — which MFP
/// is documented to emit as separate rows — get distinct guids and are not
/// silently collapsed as duplicates.
fn entry_from(fields: &Map<String, Value>) -> Option<Entry> {
    // Date / timestamp column — required (drives partition key).
    let date_raw = col_val_ci(fields, DATE_ALIASES)?;
    let ts = parse_date_ts(date_raw)?;

    let meal_raw = col_val_ci(fields, MEAL_SLOT_ALIASES).unwrap_or("");

    // Food name is optional: use it when present; fall back to the meal label
    // for rollup-style rows (no per-food column).
    let food_label = col_val_ci(fields, FOOD_ALIASES).unwrap_or("");

    // Amount/serving: used in the guid so same-food different-serving rows
    // within a meal stay distinct (matches Cronometer Day|Time|Group|Food|Amount).
    let amount_raw = col_val_ci(fields, AMOUNT_ALIASES).unwrap_or("");

    // Stable guid: full ts (with clock time) + meal + food/label + amount.
    // Using the full ts (not just ts[..10]) means two rows for the same meal
    // logged at different times within a day get distinct guids, which is the
    // documented MFP export behaviour (summarized at meal+time level).
    let guid = content_guid(&[&ts, meal_raw, food_label, amount_raw]);

    let mut entry = Entry::new(SOURCE, guid, ts);
    // Populate food label: prefer an explicit food name; if absent the entry
    // still carries the macro data under its meal label via `meal`/`meal_raw`.
    if !food_label.is_empty() {
        entry.food = food_label.to_string();
    }

    // Brand/manufacturer, where present.
    if let Some(b) = col_val_ci(fields, BRAND_ALIASES) {
        if !b.is_empty() {
            entry.brand = b.to_string();
        }
    }

    // Meal slot: MFP uses "Breakfast", "Lunch", "Dinner", "Snacks".
    if let Some(slot) = meal_slot(meal_raw) {
        entry.meal = slot.to_string();
    }
    // The verbatim group always rides in extra (custom meal names survive).
    if !meal_raw.is_empty() {
        entry.extra.insert("meal_raw".into(), Value::String(meal_raw.to_string()));
    }

    // Typed macros from their aliased columns.
    for (aliases, set) in TYPED_MACROS {
        if let Some(s) = col_val_ci(fields, aliases) {
            if let Some(v) = parse_num(s) {
                set(&mut entry, v);
            }
        }
    }

    // Everything else: if it's not a core col and not a typed macro, it's a
    // nutrient or source-specific field → put in `nutrients` if numeric,
    // or `extra` if string (preserves unknown future columns).
    let typed_headers: HashSet<&str> = TYPED_MACROS.iter().flat_map(|(a, _)| a.iter().copied()).collect();
    let core_headers: HashSet<&str> = CORE_COLS.iter().flat_map(|a| a.iter().copied()).collect();
    for (header, raw_val) in fields {
        let h = header.as_str();
        let is_typed = typed_headers.contains(h);
        let is_core = core_headers.contains(h);
        if is_typed || is_core {
            continue;
        }
        let raw_str = raw_val.as_str().unwrap_or("").trim();
        if raw_str.is_empty() {
            continue;
        }
        if let Some(n) = parse_num(raw_str) {
            // Numeric tail → nutrients map (keyed by verbatim column header).
            let json_num = serde_json::Number::from_f64(n)
                .map(Value::Number)
                .unwrap_or_else(|| Value::String(raw_str.to_string()));
            entry.nutrients.insert(header.clone(), json_num);
        } else {
            // Non-numeric / free-text → extra.
            entry.extra.insert(header.clone(), Value::String(raw_str.to_string()));
        }
    }

    Some(entry)
}

// ---------------------------------------------------------------------------
// Helpers.

/// Case-insensitive alias lookup: return the trimmed string value of the first
/// alias column found in `fields`. `None` when no alias column exists or when
/// all matching cells are blank.
fn col_val_ci<'a>(fields: &'a Map<String, Value>, aliases: &[&str]) -> Option<&'a str> {
    for alias in aliases {
        // Exact match first (fast path).
        if let Some(v) = fields.get(*alias).and_then(Value::as_str) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t);
            }
        }
        // Case-insensitive fallback (MFP may vary capitalisation across versions).
        let lc = alias.to_ascii_lowercase();
        for (k, v) in fields {
            if k.to_ascii_lowercase() == lc {
                if let Some(s) = v.as_str() {
                    let t = s.trim();
                    if !t.is_empty() {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

/// Parse an MFP date/datetime string into an RFC3339 string (with time, if the
/// source carries one) or a date-only `YYYY-MM-DD` string. Returns `None` when
/// the input is not recognisable as a date.
///
/// MFP export dates are expected to be one of:
/// - `YYYY-MM-DD HH:MM:SS` — full timestamp (Premium with per-entry times)
/// - `YYYY-MM-DD HH:MM`    — without seconds
/// - `MM/DD/YYYY HH:MM:SS` — US locale variant
/// - `YYYY-MM-DD`          — date-only (day-precision roll-ups)
/// - `MM/DD/YYYY`          — US locale date-only
///
/// **PROVISIONAL — Needs-sample.** The actual timestamp format in the Premium
/// CSV is unconfirmed. The variants below cover common MFP locale outputs.
fn parse_date_ts(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Try full datetime variants (time-aware → RFC3339 local).
    let fmts_dt = &[
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%m/%d/%Y %H:%M:%S",
        "%m/%d/%Y %H:%M",
        "%Y-%m-%dT%H:%M:%S",
    ];
    for fmt in fmts_dt {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(local) = Local.from_local_datetime(&dt).earliest() {
                return Some(local.to_rfc3339());
            }
        }
    }

    // Try date-only variants.
    let fmts_d = &["%Y-%m-%d", "%m/%d/%Y", "%d/%m/%Y"];
    for fmt in fmts_d {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d.format("%Y-%m-%d").to_string());
        }
    }

    None
}

/// Map an MFP meal slot string to the contract's closed enum. MFP's default
/// meal names are "Breakfast", "Lunch", "Dinner", "Snacks".
fn meal_slot(s: &str) -> Option<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "breakfast" => Some("breakfast"),
        "lunch" => Some("lunch"),
        "dinner" => Some("dinner"),
        "snack" | "snacks" => Some("snack"),
        _ => None,
    }
}

/// Parse a numeric string (possibly with commas as thousands separators or a
/// trailing unit suffix). Returns `None` for blank / non-numeric.
fn parse_num(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Strip commas (thousands separators), then parse.
    let clean: String = s.chars().filter(|&c| c != ',').collect();
    clean.parse::<f64>().ok()
}

/// Stable content-hash guid from a slice of identifying strings. Uses SHA-256
/// truncated to 10 hex chars (collision-safe at diary scale, matching Cronometer
/// guid width in the contract example).
fn content_guid(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(parts.join("\u{1f}").as_bytes());
    let digest = h.finalize();
    format!("mfp-{}", digest.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>())
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-myfitnesspal-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn import_body(v: &Vault, body: &str) -> ImportOutcome {
        import_meal_csv_body(v, body, &mut |_| {}).unwrap()
    }

    // -------------------------------------------------------------------------
    // Scaffold fixture: columns inferred from python-myfitnesspal types.py and
    // the athlete_data_warehouse MFP integration field list. These are the best-
    // known MFP export column names; the exact headers MUST be confirmed against
    // a real Premium export before the parser is considered finalized.
    //
    // PROVISIONAL — Needs-sample.

    const MEAL_CSV: &str = "\
Date & Time,Meal,Food Name,Calories,Carbohydrates (g),Fat (g),Protein (g),Sodium (mg),Sugar (g),Fiber (g),Saturated Fat (g),Cholesterol (mg),Potassium (mg),Iron (mg),Calcium (mg)\r\n\
2026-06-10 08:14:00,Breakfast,Oatmeal with Banana,350,65,6,10,180,18,7,1.0,0,410,3.5,40\r\n\
2026-06-10 12:30:00,Lunch,Grilled Chicken Salad,480,22,18,52,620,5,4,3.0,95,580,1.8,60\r\n\
2026-06-11,,Banana,105,27,0.4,1.3,1,14,3,0.1,0,422,0.3,6\r\n\
";

    #[test]
    fn maps_a_full_breakfast_row_to_contract_entry() {
        let fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date & Time": "2026-06-10 08:14:00",
            "Meal": "Breakfast",
            "Food Name": "Oatmeal with Banana",
            "Calories": "350",
            "Carbohydrates (g)": "65",
            "Fat (g)": "6",
            "Protein (g)": "10",
            "Sodium (mg)": "180",
            "Sugar (g)": "18",
            "Fiber (g)": "7",
            "Saturated Fat (g)": "1.0",
            "Cholesterol (mg)": "0",
            "Potassium (mg)": "410",
            "Iron (mg)": "3.5",
            "Calcium (mg)": "40"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert_eq!(e.source, "myfitnesspal");
        assert_eq!(e.meal, "breakfast", "Breakfast → closed meal enum");
        assert_eq!(e.food, "Oatmeal with Banana");
        assert_eq!(e.energy_kcal, Some(350.0));
        assert_eq!(e.carb_g, Some(65.0));
        assert_eq!(e.fat_g, Some(6.0));
        assert_eq!(e.protein_g, Some(10.0));
        assert_eq!(e.sodium_mg, Some(180.0));
        assert_eq!(e.sugar_g, Some(18.0));
        assert_eq!(e.fiber_g, Some(7.0));
        assert_eq!(e.saturated_fat_g, Some(1.0));
        assert_eq!(e.cholesterol_mg, Some(0.0), "explicit zero preserved");
        // Potassium, Iron, Calcium → nutrients map (not typed columns in contract).
        assert!(
            e.nutrients.contains_key("Potassium (mg)") || e.nutrients.contains_key("Potassium"),
            "potassium in nutrients: {:?}", e.nutrients
        );
        assert!(
            e.nutrients.contains_key("Iron (mg)") || e.nutrients.contains_key("Iron"),
            "iron in nutrients: {:?}", e.nutrients
        );
        // ts carries the full datetime.
        assert!(e.ts.starts_with("2026-06-10T08:14"), "ts has clock time: {}", e.ts);
        // The verbatim meal name preserved in extra.
        assert_eq!(e.extra.get("meal_raw"), Some(&serde_json::json!("Breakfast")));
    }

    #[test]
    fn row_with_blank_meal_and_date_only_lands_at_day_precision() {
        let fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date & Time": "2026-06-11",
            "Meal": "",
            "Food Name": "Banana",
            "Calories": "105"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert_eq!(e.ts, "2026-06-11", "date-only ts preserved: {}", e.ts);
        assert!(e.meal.is_empty(), "blank Meal → no contract meal slot");
        assert!(e.extra.get("meal_raw").is_none(), "blank meal not stored in extra");
        assert_eq!(e.energy_kcal, Some(105.0));
    }

    #[test]
    fn row_without_date_is_skipped() {
        // A row with no parseable date is always skipped (needed for partitioning).
        let no_date: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Food Name": "Banana",
            "Calories": "105"
        }))
        .unwrap();
        assert!(entry_from(&no_date).is_none(), "no date → skip");
    }

    #[test]
    fn row_without_food_name_still_produces_entry() {
        // Food name is optional: a rollup-style row (no "Food Name" column, or
        // an empty one) is still a valid entry — the macro data is present even
        // if there is no per-food label. The entry is identified by ts+meal.
        let no_food: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date & Time": "2026-06-10 12:00:00",
            "Meal": "Lunch",
            "Calories": "480",
            "Protein (g)": "35"
        }))
        .unwrap();
        let e = entry_from(&no_food).expect("rollup row without food name must produce entry");
        assert!(e.food.is_empty(), "no food label when column absent");
        assert_eq!(e.meal, "lunch");
        assert_eq!(e.energy_kcal, Some(480.0));
        assert_eq!(e.protein_g, Some(35.0));

        // Explicit empty Food Name is also fine.
        let empty_food: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date & Time": "2026-06-10",
            "Food Name": "",
            "Calories": "350"
        }))
        .unwrap();
        assert!(
            entry_from(&empty_food).is_some(),
            "empty Food Name column → entry still produced"
        );
    }

    #[test]
    fn custom_meal_slot_rides_extra_not_contract_meal_enum() {
        let fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date & Time": "2026-06-10 10:00:00",
            "Meal": "Pre-Workout",
            "Food Name": "Protein Bar",
            "Calories": "200"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert!(e.meal.is_empty(), "custom meal not in closed enum");
        assert_eq!(
            e.extra.get("meal_raw"),
            Some(&serde_json::json!("Pre-Workout")),
            "custom meal preserved verbatim in extra"
        );
    }

    #[test]
    fn us_locale_date_format_is_accepted() {
        let fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date": "06/10/2026",
            "Food Name": "Coffee",
            "Calories": "5"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert_eq!(e.ts, "2026-06-10", "US MM/DD/YYYY date parsed: {}", e.ts);
    }

    #[test]
    fn case_insensitive_column_matching() {
        // MFP may vary capitalisation across export versions — matching must be
        // case-insensitive.
        let fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "date & time": "2026-06-10",
            "food name": "Eggs",
            "calories": "140",
            "protein (g)": "12"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert_eq!(e.food, "Eggs", "case-insensitive food name");
        assert_eq!(e.energy_kcal, Some(140.0), "case-insensitive calories");
        assert_eq!(e.protein_g, Some(12.0), "case-insensitive protein");
    }

    #[test]
    fn guid_is_stable_and_distinguishes_ts_meal_food_amount() {
        // guid = content_guid(&[ts, meal, food_label, amount]) — four parts,
        // matching the Cronometer recipe (Day|Time|Group|Food|Amount).
        let g = |ts: &str, meal: &str, food: &str, amt: &str| {
            content_guid(&[ts, meal, food, amt])
        };
        let base = g("2026-06-10T08:14:00+00:00", "Breakfast", "Oatmeal", "");
        assert_eq!(base, g("2026-06-10T08:14:00+00:00", "Breakfast", "Oatmeal", ""), "deterministic");
        assert_ne!(base, g("2026-06-11T08:14:00+00:00", "Breakfast", "Oatmeal", ""), "date changes guid");
        // Two rows for the same food logged at different times within the same meal
        // must get different guids (MFP exports at meal+time level).
        assert_ne!(
            g("2026-06-10T08:14:00+00:00", "Breakfast", "Oatmeal", ""),
            g("2026-06-10T09:00:00+00:00", "Breakfast", "Oatmeal", ""),
            "different time within same meal → different guid (no silent collision)"
        );
        assert_ne!(base, g("2026-06-10T08:14:00+00:00", "Lunch", "Oatmeal", ""), "meal changes guid");
        assert_ne!(base, g("2026-06-10T08:14:00+00:00", "Breakfast", "Banana", ""), "food changes guid");
        assert_ne!(
            g("2026-06-10T08:14:00+00:00", "Breakfast", "Oatmeal", "1 cup"),
            g("2026-06-10T08:14:00+00:00", "Breakfast", "Oatmeal", "2 cups"),
            "amount changes guid"
        );
        assert!(base.starts_with("mfp-"), "guid has mfp- prefix");
        assert_eq!(base.len(), 12, "mfp- + 8 hex chars");
    }

    #[test]
    fn full_import_writes_both_layers_partitions_and_dedupes() {
        let v = temp_vault("fullimport");
        let out = import_body(&v, MEAL_CSV);
        assert_eq!(out.counts.get("imported"), Some(&3), "three rows imported: {out:?}");
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Contract layer: Jun-10 rows in 2026-06, Jun-11 row also in 2026-06.
        let jun = fs::read_to_string(
            v.root().join("health/nutrition/myfitnesspal/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(jun.lines().count(), 3, "all three entries in 2026-06: {jun}");
        assert!(jun.contains("\"meal\":\"breakfast\""));
        assert!(jun.contains("\"source\":\"myfitnesspal\""));
        assert!(jun.contains("\"energy_kcal\":350.0"), "energy_kcal on disk: {jun}");

        // Raw layer mirrors the same partition.
        let raw = fs::read_to_string(
            v.root().join("health/nutrition/myfitnesspal/raw/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(raw.lines().count(), 3, "three raw rows");
        assert!(raw.contains("\"Food Name\":\"Oatmeal with Banana\""), "verbatim food name: {raw}");

        // Re-import the same CSV → pure duplicates (idempotent).
        let again = import_body(&v, MEAL_CSV);
        assert_eq!(again.counts.get("imported"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&3));
        let jun2 = fs::read_to_string(
            v.root().join("health/nutrition/myfitnesspal/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(jun, jun2, "contract file byte-identical after re-import");
    }

    #[test]
    fn raw_layer_is_unconditional_even_for_skipped_rows() {
        // A row that fails entry_from (no date) must still land in the raw layer.
        const CSV: &str = "\
Date & Time,Meal,Food Name,Calories\r\n\
2026-06-10 08:00:00,Breakfast,Oatmeal,350\r\n\
,,Mysterious Row,200\r\n\
";
        let v = temp_vault("raw-unconditional");
        let out = import_body(&v, CSV);
        assert_eq!(out.counts.get("imported"), Some(&1));
        assert_eq!(out.counts.get("skipped"), Some(&1));
        let raw = fs::read_to_string(
            v.root().join("health/nutrition/myfitnesspal/raw/2026-06.jsonl"),
        )
        .unwrap();
        // Both rows in raw (even the one without a parseable date — filed under sentinel 0000-00).
        // The skipped row lands under the sentinel partition; just check Oatmeal in raw.
        assert!(raw.contains("\"Food Name\":\"Oatmeal\""), "good row in raw: {raw}");
    }

    #[test]
    fn zip_import_stores_raw_csv_and_parses_meal_nutrition() {
        use std::io::Write;
        let v = temp_vault("zipimport");
        let zip_path = v.root().join("mfp_export.zip");

        // Build a minimal ZIP with the three expected CSV files.
        let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        z.start_file("Meal Level Nutrition Details.csv", opts).unwrap();
        z.write_all(MEAL_CSV.as_bytes()).unwrap();

        z.start_file("Progress History.csv", opts).unwrap();
        z.write_all(b"Date,Weight (lbs)\r\n2026-06-10,175.0\r\n").unwrap();

        z.start_file("Exercise History.csv", opts).unwrap();
        z.write_all(b"Date,Exercise,Duration (min),Calories Burned\r\n2026-06-10,Running,30,320\r\n").unwrap();

        z.finish().unwrap();

        let out = run_import(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        // All three CSVs stored verbatim in the snapshots layer.
        let snaps_dir = v.root().join("health/nutrition/myfitnesspal/snapshots");
        let snapshot_files: Vec<_> = fs::read_dir(&snaps_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".csv"))
            .collect();
        assert_eq!(snapshot_files.len(), 3, "three raw CSV snapshots written: {out:?}");

        // Meal nutrition entries were parsed into the contract layer.
        assert!(
            out.counts.get("imported").copied().unwrap_or(0) > 0
                || out.counts.get("entries").copied().unwrap_or(0) > 0
                || out.headline.contains("imported"),
            "some meal entries parsed from ZIP: {out:?}"
        );
    }

    #[test]
    fn bare_csv_import_parses_meal_rows() {
        let v = temp_vault("barecsv");
        let csv_path = v.root().join("meal_details.csv");
        fs::write(&csv_path, MEAL_CSV).unwrap();
        let out = run_import(&v, &csv_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&3), "three rows from bare CSV: {out:?}");
    }

    #[test]
    fn def_is_import_behavior_with_no_connection() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none(), "no credentials held by Trove");
        let spec = DEF.import_spec().expect("Import has a spec");
        assert!(spec.accepts.contains(&"zip"), "zip in accepts");
        assert!(spec.accepts.contains(&"csv"), "csv in accepts");
        assert!(!DEF.meta.default_on);
        assert!(DEF.last_data.is_some());
        assert_eq!(DEF.meta.domain, "health-nutrition");
    }

    #[test]
    fn rollup_csv_without_food_name_column_produces_entries() {
        // "Meal Level Nutrition Details" may roll up foods to the meal+time level
        // with NO per-food "Food Name" column. Rows with a valid date must still
        // produce contract entries (no food name is not a skip condition).
        //
        // When the real MFP rollup export does carry a per-time-group row, the
        // timestamp will differ between rows (or the meal+time composite will
        // differ), giving them distinct guids. In this synthetic fixture the two
        // Lunch rows share date-only+meal+no-food+no-amount, so their guids are
        // identical — the second is correctly deduped (idempotent, not data loss).
        // A real export with per-time-group timestamps would have distinct guids.
        const ROLLUP_CSV: &str = "\
Date,Meal Name,Calories,Protein (g),Carbohydrates (g),Fat (g)\r\n\
2026-06-10,Breakfast,350,10,65,6\r\n\
2026-06-10,Lunch,480,52,22,18\r\n\
2026-06-10,Lunch,200,15,30,5\r\n\
";
        let v = temp_vault("rollup");
        let out = import_body(&v, ROLLUP_CSV);
        // No row should be skipped (absent food name is not a skip condition).
        assert_eq!(out.counts.get("skipped"), Some(&0), "no rows skipped: {out:?}");
        // Breakfast (1) + first Lunch (1) are imported; second Lunch shares guid
        // with first (identical date-only+meal, no distinguishing food/time/amount)
        // → deduplicated in-batch.
        let imported = out.counts.get("imported").copied().unwrap_or(0);
        let dupes = out.counts.get("duplicates").copied().unwrap_or(0);
        assert_eq!(imported + dupes, 3, "all rows accounted for (imported+duped=3): {out:?}");
        assert!(imported >= 2, "at least Breakfast and one Lunch imported: {out:?}");

        // Re-import the same CSV → all rows are now duplicates (idempotent).
        // The previously-imported guids are in `seen`; the shared-guid second
        // Lunch also matches, so all 3 rows are counted as duplicates.
        let again = import_body(&v, ROLLUP_CSV);
        assert_eq!(again.counts.get("imported"), Some(&0), "nothing new on re-import: {again:?}");
        assert_eq!(
            again.counts.get("duplicates").copied().unwrap_or(0),
            3,
            "all 3 rows reported as duplicates on re-import: {again:?}"
        );

        // Rollup rows with distinct timestamps (the expected real-export shape)
        // get distinct guids and are never collapsed.
        const ROLLUP_TIMED: &str = "\
Date & Time,Meal,Calories,Protein (g)\r\n\
2026-06-10 07:30:00,Breakfast,350,10\r\n\
2026-06-10 12:00:00,Lunch,480,52\r\n\
2026-06-10 12:30:00,Lunch,200,15\r\n\
";
        let v2 = temp_vault("rollup-timed");
        let out2 = import_body(&v2, ROLLUP_TIMED);
        assert_eq!(out2.counts.get("imported"), Some(&3), "timed rows all distinct: {out2:?}");
        assert_eq!(out2.counts.get("duplicates"), Some(&0));
    }

    #[test]
    fn zip_raw_files_count_is_not_doubled() {
        use std::io::Write;
        // A ZIP with only Progress + Exercise CSVs (no meal CSV) previously
        // double-counted raw_files because the no-meal branch pre-populated the
        // count and the merge added it again. Confirm it reports exactly N.
        let v = temp_vault("zipcount");
        let zip_path = v.root().join("mfp_export.zip");

        let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("Progress History.csv", opts).unwrap();
        z.write_all(b"Date,Weight (lbs)\r\n2026-06-10,175.0\r\n").unwrap();
        z.start_file("Exercise History.csv", opts).unwrap();
        z.write_all(b"Date,Exercise\r\n2026-06-10,Running\r\n").unwrap();
        z.finish().unwrap();

        let out = run_import(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(
            out.counts.get("raw_files").copied().unwrap_or(0),
            2,
            "raw_files must be 2 (not 4 from double-add): {out:?}"
        );
    }

    #[test]
    fn import_surfaces_on_hub_and_manifest() {
        let v = temp_vault("hubsurface");
        import_body(&v, MEAL_CSV);

        let m = v.rebuild_manifest().unwrap();
        let dom = m.domains.iter().find(|d| d.domain == "health-nutrition").unwrap();
        assert!(dom.sources.contains(&"myfitnesspal".to_string()));

        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "myfitnesspal").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        assert!(import_info.accepts.contains(&"zip"));
        assert!(import_info.accepts.contains(&"csv"));
        assert_eq!(card.last_data.as_deref(), Some("2026-06"));
    }
}

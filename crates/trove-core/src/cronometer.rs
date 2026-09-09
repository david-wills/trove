//! Cronometer nutrition-log CSV import — the food diary's per-serving entries
//! with their full micronutrient tail, into the bound [`crate::health_nutrition`]
//! contract. Catalogued in the Phase 2 pass; brief: docs/integrations/cronometer.md.
//! **First collector in the `health-nutrition` domain** — this build binds the
//! contract (see `crate::health_nutrition` / `crate::contracts`).
//!
//! Cronometer's free web export (Profile → Account → Export Data) produces clean
//! CSVs; the **Servings** export is the per-entry food log and the one this
//! importer reads. Its columns are header-name matched (never positional — the
//! export carries 80+ nutrient columns and the set evolves), confirmed against
//! the documented format via the community `gocronometer` parser:
//!
//! - Core: `Day` (`YYYY-MM-DD`), `Time` (`HH:MM AM/PM`, **blank on the free
//!   tier** — per-entry timestamps are Gold-only), `Group` (the meal slot),
//!   `Food Name`, `Amount` (quantity+unit combined, e.g. `"80.00 g"`),
//!   `Category` (the food category).
//! - Typed macros: `Energy (kcal)`, `Protein (g)`, `Carbs (g)`, `Fat (g)`,
//!   `Fiber (g)`, `Sugars (g)`, `Added Sugars (g)`, `Saturated (g)`,
//!   `Monounsaturated (g)`, `Polyunsaturated (g)`, `Sodium (mg)`,
//!   `Cholesterol (mg)` → the contract's typed columns.
//! - Everything else (all vitamins/minerals/amino-acids/individual-sugars, e.g.
//!   `Iron (mg)`, `Omega-3 (g)`, `Tryptophan (g)`) → the `nutrients` map, keyed
//!   by the verbatim Cronometer label-with-unit. Name normalization is a
//!   read-time job.
//!
//! Each Servings row becomes one [`crate::health_nutrition::Entry`] under
//! `health/nutrition/cronometer/YYYY-MM.jsonl`. `ts` is the eaten/logged local
//! time where the export gives one (`Day`+`Time`), or the date-only `Day` where
//! it does not (free tier) — never a fabricated clock time. The CSV carries no
//! row ids, so `guid` is a stable content hash of `Day|Time|Group|Food
//! Name|Amount`; re-importing an overlapping range is idempotent (the
//! letterboxd pattern). Two layers, unconditionally: the **raw** CSV row as a
//! header→value JSON object under `health/nutrition/cronometer/raw/YYYY-MM.jsonl`
//! (full fidelity), and the normalized **contract** rows deduped by `guid`.
//!
//! Behavior is [`Behavior::Import`] (a repeatable retrospective import; no
//! scheduled export exists and no credentials are held — Trove never touches
//! cronometer.com). Cronometer's *other* exports route by shape and are out of
//! scope here: Biometrics (weight/BP/glucose) belong to `health/`, Exercises and
//! Notes stay per-source raw — this importer reads only the Servings food log.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::health_nutrition::Entry;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer entry stream; raw rows nest under `raw/`.
const DIR: &str = "health/nutrition/cronometer";
const RAW_DIR: &str = "health/nutrition/cronometer/raw";

const SOURCE: &str = "cronometer";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "cronometer",
        name: "Cronometer",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your food diary from Cronometer's free CSV export (the Servings \
                      export) — over 80 nutrient columns per entry, the most detailed nutrition \
                      export of any tracker, into the unified nutrition store. Re-runnable: \
                      newer exports never duplicate.",
        domain: "health-nutrition",
        vault_path: "health/nutrition/cronometer/",
        toggleable: false,
        setup: &[
            "cronometer.com → Profile → Account → Export Data → Export Servings.",
            "Import the downloaded servings CSV here.",
        ],
        caveats: "Only the Servings (food diary) export is imported; Biometrics (weight/glucose) \
                  belong to the health metrics store and Exercises/Notes are not yet imported. \
                  Per-entry timestamps require a Cronometer Gold subscription — free-tier rows \
                  land at day precision (the diary date, no fabricated clock time).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Column names — verbatim Cronometer Servings headers (header-name matched).

/// `(header, Entry-field setter)` for the macros that earn a typed column.
/// Anything NOT listed here (and not a core column) rides in the `nutrients`
/// map under its verbatim header.
const TYPED_MACROS: &[(&str, fn(&mut Entry, f64))] = &[
    ("Energy (kcal)", |e, v| e.energy_kcal = Some(v)),
    ("Protein (g)", |e, v| e.protein_g = Some(v)),
    ("Carbs (g)", |e, v| e.carb_g = Some(v)),
    ("Fat (g)", |e, v| e.fat_g = Some(v)),
    ("Fiber (g)", |e, v| e.fiber_g = Some(v)),
    ("Sugars (g)", |e, v| e.sugar_g = Some(v)),
    ("Added Sugars (g)", |e, v| e.added_sugar_g = Some(v)),
    ("Saturated (g)", |e, v| e.saturated_fat_g = Some(v)),
    ("Polyunsaturated (g)", |e, v| e.polyunsaturated_fat_g = Some(v)),
    ("Monounsaturated (g)", |e, v| e.monounsaturated_fat_g = Some(v)),
    ("Sodium (mg)", |e, v| e.sodium_mg = Some(v)),
    ("Cholesterol (mg)", |e, v| e.cholesterol_mg = Some(v)),
];

/// The core (non-nutrient) columns — neither a typed macro nor a `nutrients`
/// entry; consumed individually into `ts`/`meal`/`food`/`amount`/`extra`.
const CORE_COLS: &[&str] = &["Day", "Time", "Group", "Food Name", "Amount", "Category"];

// ---------------------------------------------------------------------------
// Raw row shape: the verbatim CSV row as a header→value object, tagged with the
// contract month purely so the month-partition writer files it correctly. Only
// `value` is serialized.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// The import.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;
    import_body(vault, &body, progress)
}

/// The import body over an in-memory CSV string — the testable seam.
fn import_body(
    vault: &Vault,
    body: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Already-stored guids, for re-runnable imports: a re-pull of an overlapping
    // window never duplicates (the letterboxd/readwise pattern).
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
        .context("reading CSV header row — is this a Cronometer Servings export?")?
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
        // Header→value map for this row (verbatim values).
        let fields: Map<String, Value> = headers
            .iter()
            .zip(rec.iter())
            .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
            .collect();
        let Some(entry) = entry_from(&fields) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(entry.guid.clone()) {
            duplicates += 1;
            continue;
        }
        raws.push(RawLine { ts: entry.ts.clone(), value: Value::Object(fields) });
        entries.push(entry);
        imported += 1;
        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Raw layer first (unconditional full fidelity), then the contract rows.
    raw.append(&raws, |r| &r.ts)?;
    contract.append(&entries, |e| &e.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} entries imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// A trimmed string field by header name; "" when absent.
fn field<'a>(fields: &'a Map<String, Value>, key: &str) -> &'a str {
    fields.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("")
}

/// One Servings row (header→value) → a contract [`Entry`]. `None` when the row
/// has no parseable `Day` (can't partition) or no `Food Name` (not a food entry).
fn entry_from(fields: &Map<String, Value>) -> Option<Entry> {
    let day = field(fields, "Day");
    // Must yield a date; a Servings export uses ISO `YYYY-MM-DD`.
    let date = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let food = field(fields, "Food Name");
    if food.is_empty() {
        return None;
    }

    // ts: combine Day + Time (local) where the export carries a time (Gold);
    // else the date-only Day (free tier). Never invent a clock time.
    let time_raw = field(fields, "Time");
    let ts = match parse_time(time_raw) {
        Some(t) => Local
            .from_local_datetime(&date.and_time(t))
            .earliest()?
            .to_rfc3339(),
        None => date.format("%Y-%m-%d").to_string(),
    };

    // guid: a stable content hash (the CSV has no row ids). Includes Time so two
    // identical foods logged at different times stay distinct on Gold exports.
    let guid = content_guid(&[
        day,
        time_raw,
        field(fields, "Group"),
        food,
        field(fields, "Amount"),
    ]);

    let mut entry = Entry::new(SOURCE, guid, ts);
    entry.food = food.to_string();

    // Group → the closed meal enum where it matches; the verbatim group always
    // rides in extra so a custom/renamed group survives.
    let group = field(fields, "Group");
    if let Some(meal) = meal_slot(group) {
        entry.meal = meal.to_string();
    }
    if !group.is_empty() {
        entry.extra.insert("group".into(), Value::String(group.to_string()));
    }

    // Amount: "80.00 g" → amount 80.0 + unit "g". Keep the verbatim string too.
    let amount_raw = field(fields, "Amount");
    if !amount_raw.is_empty() {
        let (qty, unit) = split_amount(amount_raw);
        entry.amount = qty;
        entry.unit = unit;
        entry.extra.insert("amount_raw".into(), Value::String(amount_raw.to_string()));
    }

    // Food category (Cronometer's own taxonomy) → extra.
    let category = field(fields, "Category");
    if !category.is_empty() {
        entry.extra.insert("category".into(), Value::String(category.to_string()));
    }

    // Typed macros from their named columns.
    for (header, set) in TYPED_MACROS {
        if let Some(v) = num_field(fields, header) {
            set(&mut entry, v);
        }
    }

    // Everything else that isn't a core column or a typed macro → nutrients,
    // keyed by the verbatim Cronometer label-with-unit. Omit blanks (a blank
    // cell is "not measured", not zero); keep explicit zeros the export wrote.
    for (header, raw) in fields {
        if CORE_COLS.contains(&header.as_str())
            || TYPED_MACROS.iter().any(|(h, _)| h == header)
        {
            continue;
        }
        if let Some(v) = num_value(raw.as_str().unwrap_or("")) {
            entry.nutrients.insert(header.clone(), v);
        }
    }

    Some(entry)
}

/// Map a Cronometer diary `Group` to the contract's closed meal enum, or `None`
/// for a custom/renamed group (which still rides verbatim in `extra.group`).
/// Cronometer's default groups are Breakfast / Lunch / Dinner / Snacks.
fn meal_slot(group: &str) -> Option<&'static str> {
    match group.trim().to_ascii_lowercase().as_str() {
        "breakfast" => Some("breakfast"),
        "lunch" => Some("lunch"),
        "dinner" => Some("dinner"),
        "snack" | "snacks" => Some("snack"),
        _ => None,
    }
}

/// Parse a Cronometer `Time` cell (`HH:MM AM/PM`, possibly `H:MM AM/PM`). `None`
/// for an empty cell (free tier) — the caller then files the row at day
/// precision rather than inventing a clock time.
fn parse_time(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 12-hour with AM/PM is the documented export format; fall back to 24-hour
    // and second-precision variants defensively.
    NaiveTime::parse_from_str(s, "%I:%M %p")
        .or_else(|_| NaiveTime::parse_from_str(s, "%I:%M:%S %p"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .ok()
}

/// Split a combined `Amount` cell (`"80.00 g"`, `"1.00 serving"`, `"1.5 cup"`)
/// into a numeric quantity and a unit string. The leading token is the number;
/// the rest is the unit (verbatim, joined). A bare number yields no unit; an
/// unparseable leading token yields no quantity (the raw string is preserved by
/// the caller regardless).
fn split_amount(s: &str) -> (Option<f64>, String) {
    let s = s.trim();
    let mut parts = s.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let unit = parts.next().unwrap_or("").trim().to_string();
    (head.parse::<f64>().ok(), unit)
}

/// A numeric cell by header name → `f64`, or `None` for blank/non-numeric.
fn num_field(fields: &Map<String, Value>, key: &str) -> Option<f64> {
    fields.get(key).and_then(Value::as_str).and_then(|s| s.trim().parse::<f64>().ok())
}

/// A numeric cell string → a JSON number (preserving integer-ness), or `None`
/// for blank/non-numeric. Used for the `nutrients` map values.
fn num_value(s: &str) -> Option<Value> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Prefer an integer where the cell is integral, else an f64.
    if let Ok(i) = s.parse::<i64>() {
        return Some(Value::Number(i.into()));
    }
    s.parse::<f64>().ok().and_then(Number::from_f64).map(Value::Number)
}

/// A stable short hex digest over the join of the identifying fields, the dedupe
/// key for a CSV that carries no row ids. SHA-256 truncated to 10 hex chars
/// (matching the contract's example guid width) — collision-safe at diary scale.
fn content_guid(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(parts.join("\u{1f}").as_bytes()); // unit separator: unambiguous join
    let digest = h.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-cronometer-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // A Servings export with the documented columns: two Gold rows (with a
    // `Time`) and one free-tier row (blank `Time`). Header set abbreviated to
    // the typed macros + a representative micronutrient tail; real exports carry
    // 80+ columns, all header-matched the same way. Confirmed against the
    // gocronometer parser's verbatim header strings.
    const SERVINGS: &str = "\
Day,Time,Group,Food Name,Amount,Energy (kcal),Protein (g),Carbs (g),Fat (g),Fiber (g),Sugars (g),Saturated (g),Sodium (mg),Cholesterol (mg),Iron (mg),Magnesium (mg),Omega-3 (g),Tryptophan (g),Vitamin C (mg),Category
2026-06-10,08:14 AM,Breakfast,\"Oats, Rolled, Dry\",80.00 g,311,10.7,54.8,5.3,8.0,0.8,0.9,4,0,3.5,138.0,0.08,0.18,0.0,Cereal Grains
2026-06-10,01:02 PM,Lunch,\"Chicken Burrito Bowl\",1.00 serving,625,42,58,22,,,,,,,,,,,Restaurant Foods
2026-06-11,,Snacks,Banana,1.00 medium,105,1.3,27,0.4,3.1,14.4,0.1,1,0,0.3,32.0,0.03,0.01,10.3,Fruits
";

    fn import(v: &Vault, body: &str) -> ImportOutcome {
        import_body(v, body, &mut |_| {}).unwrap()
    }

    #[test]
    fn maps_a_gold_row_with_typed_macros_meal_amount_and_nutrient_tail() {
        let fields: Map<String, Value> = serde_json::from_value(json!({
            "Day": "2026-06-10",
            "Time": "08:14 AM",
            "Group": "Breakfast",
            "Food Name": "Oats, Rolled, Dry",
            "Amount": "80.00 g",
            "Energy (kcal)": "311",
            "Protein (g)": "10.7",
            "Carbs (g)": "54.8",
            "Fat (g)": "5.3",
            "Fiber (g)": "8.0",
            "Sugars (g)": "0.8",
            "Saturated (g)": "0.9",
            "Sodium (mg)": "4",
            "Cholesterol (mg)": "0",
            "Iron (mg)": "3.5",
            "Magnesium (mg)": "138.0",
            "Omega-3 (g)": "0.08",
            "Tryptophan (g)": "0.18",
            "Vitamin C (mg)": "0.0",
            "Category": "Cereal Grains"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert_eq!(e.source, "cronometer");
        assert_eq!(e.meal, "breakfast", "Group → closed meal enum");
        assert_eq!(e.food, "Oats, Rolled, Dry");
        assert_eq!(e.amount, Some(80.0), "Amount number split out");
        assert_eq!(e.unit, "g", "Amount unit split out");
        // Typed macros land in their columns.
        assert_eq!(e.energy_kcal, Some(311.0));
        assert_eq!(e.protein_g, Some(10.7));
        assert_eq!(e.sodium_mg, Some(4.0));
        assert_eq!(e.cholesterol_mg, Some(0.0), "explicit zero kept");
        // The tail rides in nutrients under the verbatim label-with-unit key;
        // integers stay integers, floats stay floats.
        assert_eq!(e.nutrients.get("Iron (mg)"), Some(&json!(3.5)));
        assert_eq!(e.nutrients.get("Magnesium (mg)"), Some(&json!(138.0)));
        assert_eq!(e.nutrients.get("Omega-3 (g)"), Some(&json!(0.08)));
        // Typed macros are NOT duplicated into nutrients.
        assert!(e.nutrients.get("Protein (g)").is_none());
        assert!(e.nutrients.get("Energy (kcal)").is_none());
        // Core cols never leak into nutrients.
        assert!(e.nutrients.get("Food Name").is_none());
        assert!(e.nutrients.get("Category").is_none());
        // Category + raw Amount + verbatim group preserved in extra.
        assert_eq!(e.extra.get("category"), Some(&json!("Cereal Grains")));
        assert_eq!(e.extra.get("amount_raw"), Some(&json!("80.00 g")));
        assert_eq!(e.extra.get("group"), Some(&json!("Breakfast")));
        // ts = Day+Time at the local instant of 08:14 that day.
        assert!(e.ts.starts_with("2026-06-10T08:14:00"), "Gold row carries a clock time: {}", e.ts);
    }

    #[test]
    fn free_tier_blank_time_lands_at_day_precision() {
        let fields: Map<String, Value> = serde_json::from_value(json!({
            "Day": "2026-06-11",
            "Time": "",
            "Group": "Snacks",
            "Food Name": "Banana",
            "Amount": "1.00 medium",
            "Energy (kcal)": "105"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        // No fabricated clock time — the ts is the date-only diary day.
        assert_eq!(e.ts, "2026-06-11", "blank Time → date-only ts, no invented clock");
        assert_eq!(e.meal, "snack", "Snacks → snack");
        assert_eq!(e.amount, Some(1.0));
        assert_eq!(e.unit, "medium");
        // A date-only ts still partitions to its month.
        assert_eq!(Partition::Month.key(&e.ts), Some("2026-06"));
    }

    #[test]
    fn custom_group_rides_extra_and_yields_no_meal_enum() {
        let fields: Map<String, Value> = serde_json::from_value(json!({
            "Day": "2026-06-10",
            "Time": "03:00 PM",
            "Group": "Pre-Workout",
            "Food Name": "Whey Shake",
            "Amount": "1.00 scoop"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert!(e.meal.is_empty(), "a non-default group is not a contract meal slot");
        assert_eq!(e.extra.get("group"), Some(&json!("Pre-Workout")), "but survives verbatim in extra");
    }

    #[test]
    fn rows_without_food_or_with_bad_day_are_skipped() {
        // No Food Name → not a food entry.
        let no_food: Map<String, Value> =
            serde_json::from_value(json!({"Day": "2026-06-10", "Food Name": ""})).unwrap();
        assert!(entry_from(&no_food).is_none());
        // Unparseable Day → can't partition.
        let bad_day: Map<String, Value> =
            serde_json::from_value(json!({"Day": "not-a-date", "Food Name": "X"})).unwrap();
        assert!(entry_from(&bad_day).is_none());
    }

    #[test]
    fn split_amount_handles_decimals_words_and_bare_numbers() {
        assert_eq!(split_amount("80.00 g"), (Some(80.0), "g".into()));
        assert_eq!(split_amount("1.00 serving"), (Some(1.0), "serving".into()));
        assert_eq!(split_amount("1.5 fl oz"), (Some(1.5), "fl oz".into()), "multi-word unit joined");
        assert_eq!(split_amount("100"), (Some(100.0), "".into()), "bare number, no unit");
        assert_eq!(split_amount("n/a"), (None, "".into()), "unparseable head → no qty");
    }

    #[test]
    fn guid_is_stable_and_distinguishes_time_and_food() {
        let g = |day, time, grp, food, amt| content_guid(&[day, time, grp, food, amt]);
        let base = g("2026-06-10", "08:14 AM", "Breakfast", "Oats", "80.00 g");
        // Deterministic.
        assert_eq!(base, g("2026-06-10", "08:14 AM", "Breakfast", "Oats", "80.00 g"));
        // Time, food, and amount each change the guid.
        assert_ne!(base, g("2026-06-10", "12:00 PM", "Breakfast", "Oats", "80.00 g"));
        assert_ne!(base, g("2026-06-10", "08:14 AM", "Breakfast", "Rice", "80.00 g"));
        assert_ne!(base, g("2026-06-10", "08:14 AM", "Breakfast", "Oats", "90.00 g"));
        // The expected hex width (10 chars, matching the contract example).
        assert_eq!(base.len(), 10);
    }

    #[test]
    fn full_import_writes_both_layers_partitions_and_dedupes() {
        let v = temp_vault("fullimport");
        let out = import(&v, SERVINGS);
        assert_eq!(out.counts.get("imported"), Some(&3));
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Contract rows: the two June-10 Gold rows land in 2026-06; the June-11
        // free-tier row also lands in 2026-06 (same month).
        let jun = fs::read_to_string(v.root().join("health/nutrition/cronometer/2026-06.jsonl")).unwrap();
        assert_eq!(jun.lines().count(), 3, "all three entries in the June file");
        assert!(jun.contains("\"meal\":\"breakfast\""));
        assert!(jun.contains("\"energy_kcal\":311"));
        // The micronutrient tail is on disk under verbatim keys.
        assert!(jun.contains("\"Iron (mg)\":3.5"), "nutrient tail persisted: {jun}");
        // The free-tier row carries a date-only ts (no clock).
        assert!(jun.contains("\"ts\":\"2026-06-11\""), "date-only free-tier ts on disk: {jun}");

        // Raw layer mirrors the partitioning, verbatim CSV cells (incl. the
        // combined Amount string and the food category the contract relocates).
        let raw = fs::read_to_string(v.root().join("health/nutrition/cronometer/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 3);
        assert!(raw.contains("\"Amount\":\"80.00 g\""), "raw keeps the combined Amount: {raw}");
        assert!(raw.contains("\"Category\":\"Cereal Grains\""));

        // Re-import the same export → pure duplicates, files byte-identical.
        let again = import(&v, SERVINGS);
        assert_eq!(again.counts.get("imported"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&3));
        let jun2 = fs::read_to_string(v.root().join("health/nutrition/cronometer/2026-06.jsonl")).unwrap();
        assert_eq!(jun, jun2, "contract file byte-identical after re-import");
        let raw2 = fs::read_to_string(v.root().join("health/nutrition/cronometer/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw, raw2, "raw file byte-identical after re-import");
    }

    #[test]
    fn import_joins_every_generic_surface() {
        let v = temp_vault("surfaces");
        import(&v, SERVINGS);

        // The manifest indexes it as a health-nutrition source.
        let m = v.rebuild_manifest().unwrap();
        let dom = m.domains.iter().find(|d| d.domain == "health-nutrition").unwrap();
        assert!(dom.sources.contains(&"cronometer".to_string()));
        assert_eq!(dom.first.as_deref(), Some("2026-06"));
        assert!(!dom.spec.is_empty());

        // The hub knows it with zero UI code: a card with an import box.
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "cronometer").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2026-06"));
    }

    #[test]
    fn run_import_reads_from_a_csv_file() {
        let v = temp_vault("fileio");
        let path = v.root().join("servings.csv");
        fs::write(&path, SERVINGS).unwrap();
        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&3));
    }
}

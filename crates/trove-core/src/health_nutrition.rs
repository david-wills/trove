//! The `health-nutrition` domain contract: what the user ate — one record per
//! logged food entry, from every nutrition tracker — in one normalized,
//! source-agnostic store.
//!
//! One record shape ([`Entry`]) under `health/nutrition/<source>/YYYY-MM.jsonl`
//! (`<source>` is the collector id and the folder name; the month is the month
//! of [`Entry::ts`]). Cronometer, MyFitnessPal, MacroFactor, Lifesum, and the
//! food-log portion of Levels write this shape; readers see one diet timeline
//! regardless of which app produced it.
//!
//! The stream is **append-only** — a logged meal happens once — and collectors
//! skip guids they already hold (`guid` is the dedupe key). Only
//! `ts`/`source`/`guid` are required; everything else is omit-empty, so a bare
//! day-rollup writes a handful of fields while a rich Cronometer row carries
//! typed macros plus a `nutrients` map of micros. The core macros
//! (`energy_kcal`, `protein_g`, `carb_g`, `fat_g`, …) are typed columns so the
//! common query never digs into a sub-object, while the long micronutrient tail
//! — Cronometer alone exposes 80+ nutrients per food — rides a nested
//! [`Entry::nutrients`] map so that fidelity survives without 80 columns.
//! Source-specific fields the normalized columns don't carry ride verbatim under
//! [`Entry::extra`] rather than being dropped.
//!
//! `ts` is the most identity-bearing time the source exposes: the eaten/logged
//! time where the export has one (MyFitnessPal Premium, Cronometer Gold,
//! Levels), or the date-only `YYYY-MM-DD` of the diary day where it does not
//! (Cronometer free tier, MacroFactor per-day rows) — a date-only value is a
//! lexical prefix of a full timestamp, so it still partitions and sorts; never
//! invent a clock time. Cross-source overlap (the same meal via a direct export
//! and an Apple Health passthrough) is reconciled at *read* time — each source
//! keeps its own folder and stable guids; nothing is merged or dropped at write
//! time. Only food logs live here: a nutrition app's *other* exports route by
//! shape — weight/body-measurement rows go to `health/`, Levels' raw CGM glucose
//! readings go to `health/`, exercise logs stay per-source raw — never into this
//! contract.
//!
//! See [`docs/vault-spec/domains/health-nutrition.md`] for the field-level spec;
//! the schema field descriptions there are authoritative for names/units/meanings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One logged food entry — one line of `health/nutrition/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/`guid`
/// are required; everything else is omit-empty. Matches
/// `health-nutrition.entry.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Entry {
    /// RFC3339 local time the food was eaten/logged, **or** a date-only
    /// `YYYY-MM-DD` for a day-precision diary row the source never timestamped.
    /// Always serialized; its month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`cronometer`,
    /// `myfitnesspal`, `macrofactor`). Always serialized.
    pub source: String,
    /// Source-unique id, the dedupe key: a per-entry log id where the source has
    /// one, else a content hash of date + meal + food + amount (most nutrition
    /// CSV exports carry no row ids). Always serialized.
    pub guid: String,
    /// The meal slot, where the source records one:
    /// `"breakfast"` | `"lunch"` | `"dinner"` | `"snack"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub meal: String,
    /// Food / item name, verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub food: String,
    /// Brand or manufacturer, where the source carries one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub brand: String,
    /// Quantity consumed, paired with `unit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    /// Unit for `amount`, verbatim (`"g"`, `"oz"`, `"cup"`, `"serving"`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// Food energy, in kilocalories (the consumer "Calories").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_kcal: Option<f64>,
    /// Protein, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protein_g: Option<f64>,
    /// Total carbohydrate, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carb_g: Option<f64>,
    /// Total fat, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fat_g: Option<f64>,
    /// Dietary fiber, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fiber_g: Option<f64>,
    /// Total sugars, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sugar_g: Option<f64>,
    /// Added sugars, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_sugar_g: Option<f64>,
    /// Saturated fat, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saturated_fat_g: Option<f64>,
    /// Polyunsaturated fat, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polyunsaturated_fat_g: Option<f64>,
    /// Monounsaturated fat, in grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monounsaturated_fat_g: Option<f64>,
    /// Sodium, in milligrams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sodium_mg: Option<f64>,
    /// Cholesterol, in milligrams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cholesterol_mg: Option<f64>,
    /// The long micronutrient / extended-macro tail: source-native nutrient
    /// label (unit in the key, e.g. `"Vitamin C (mg)"`) → number. Keys are the
    /// source's own labels — normalizing nutrient names across apps is a
    /// read-time job.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub nutrients: Map<String, Value>,
    /// Everything source-specific the normalized fields don't carry (Levels Zone
    /// score, MacroFactor estimated expenditure / trend weight, food notes,
    /// source-native food/recipe ids, …) — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Entry {
    /// A minimal record with only the three required fields set.
    pub fn new(source: impl Into<String>, guid: impl Into<String>, ts: impl Into<String>) -> Self {
        Entry {
            ts: ts.into(),
            source: source.into(),
            guid: guid.into(),
            meal: String::new(),
            food: String::new(),
            brand: String::new(),
            amount: None,
            unit: String::new(),
            energy_kcal: None,
            protein_g: None,
            carb_g: None,
            fat_g: None,
            fiber_g: None,
            sugar_g: None,
            added_sugar_g: None,
            saturated_fat_g: None,
            polyunsaturated_fat_g: None,
            monounsaturated_fat_g: None,
            sodium_mg: None,
            cholesterol_mg: None,
            nutrients: Map::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_entry_serializes_only_required_fields() {
        // Omit-empty: a sparse line is exactly the three required keys (a
        // day-precision MacroFactor-style rollup with nothing else set).
        let e = Entry::new("cronometer", "33f1c0a7e2", "2026-06-11");
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"ts": "2026-06-11", "source": "cronometer", "guid": "33f1c0a7e2"})
        );
    }

    #[test]
    fn full_entry_round_trips_with_typed_macros_and_nutrients_map() {
        let line = json!({
            "ts": "2026-06-10T08:14:00-07:00",
            "source": "cronometer",
            "guid": "33f1c0a7e2",
            "meal": "breakfast",
            "food": "Oats, Rolled, Dry",
            "brand": "Bob's Red Mill",
            "amount": 80,
            "unit": "g",
            "energy_kcal": 311,
            "protein_g": 10.7,
            "carb_g": 54.8,
            "fat_g": 5.3,
            "fiber_g": 8.0,
            "sugar_g": 0.8,
            "saturated_fat_g": 0.9,
            "sodium_mg": 4,
            "cholesterol_mg": 0,
            "nutrients": {"Vitamin C (mg)": 0.0, "Iron (mg)": 3.5, "Magnesium (mg)": 138.0, "Omega-3 (g)": 0.08, "Tryptophan (g)": 0.18}
        });
        let e: Entry = serde_json::from_value(line).unwrap();
        assert_eq!(e.meal, "breakfast");
        assert_eq!(e.food, "Oats, Rolled, Dry");
        assert_eq!(e.amount, Some(80.0));
        assert_eq!(e.energy_kcal, Some(311.0));
        assert_eq!(e.cholesterol_mg, Some(0.0), "explicit zero preserved");
        assert_eq!(e.nutrients.get("Iron (mg)"), Some(&json!(3.5)));
        // Typed numeric columns are `Option<f64>`, so an integer input (311)
        // re-serializes as a float-typed JSON number (311.0) — semantically
        // equal and still schema-valid (`number`), which the spec suite proves.
        let re = serde_json::to_value(&e).unwrap();
        assert_eq!(re["energy_kcal"].as_f64(), Some(311.0));
        assert_eq!(re["meal"], json!("breakfast"));
        assert_eq!(re["nutrients"]["Iron (mg)"], json!(3.5));
        assert!(re.get("unit").is_some() && re["unit"] == json!("g"));
    }

    #[test]
    fn day_precision_rollup_with_extra_round_trips() {
        // A MacroFactor-style day rollup: date-only ts, no per-food `food`,
        // expenditure/trend-weight in extra.
        let line = json!({
            "ts": "2026-06-11",
            "source": "macrofactor",
            "guid": "mf-2026-06-11-total",
            "energy_kcal": 2180,
            "protein_g": 158,
            "carb_g": 201,
            "fat_g": 74,
            "extra": {"expenditure_kcal": 2640, "trend_weight_kg": 81.4}
        });
        let e: Entry = serde_json::from_value(line).unwrap();
        assert!(e.food.is_empty(), "a day rollup has no per-food name");
        assert_eq!(e.ts, "2026-06-11", "date-only ts preserved verbatim");
        // `extra` is a free Value map (not typed numbers), so it round-trips
        // byte-for-byte — integer expenditure stays an integer.
        assert_eq!(e.extra.get("expenditure_kcal"), Some(&json!(2640)));
        assert_eq!(e.extra.get("trend_weight_kg"), Some(&json!(81.4)));
        let re = serde_json::to_value(&e).unwrap();
        assert_eq!(re["extra"], json!({"expenditure_kcal": 2640, "trend_weight_kg": 81.4}));
        assert_eq!(re["energy_kcal"].as_f64(), Some(2180.0));
    }

    #[test]
    fn unknown_fields_tolerated_and_empty_optionals_omitted() {
        // Forward-compat: an unknown top-level field is ignored on re-serialize;
        // empty optionals (amount/meal/nutrients) are omitted.
        let line = json!({
            "ts": "2026-06-10T13:02:00-07:00",
            "source": "myfitnesspal",
            "guid": "mfp-log-7781201",
            "food": "Chicken Burrito Bowl",
            "future_field": "ignored"
        });
        let e: Entry = serde_json::from_value(line).unwrap();
        assert_eq!(e.food, "Chicken Burrito Bowl");
        assert!(e.amount.is_none() && e.meal.is_empty());
        let re = serde_json::to_value(&e).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert!(re.get("amount").is_none() && re.get("nutrients").is_none(), "empty optionals omitted");
    }
}

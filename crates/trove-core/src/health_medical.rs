//! The `health-medical` domain contract: clinical records — lab results,
//! vital signs, medications, and conditions — normalized out of FHIR (and the
//! non-FHIR fallbacks) into flat, source-agnostic rows that merge at read time.
//!
//! Three record shapes share the domain (the `tasks`/`reading`/`environment`
//! precedent of one DOMAINS entry covering several shapes):
//!
//! - **[`Observation`]** — one lab result or vital sign, under
//!   `health/medical/<source>/observations/YYYY-MM.jsonl` (`<source>` is the
//!   collector id and the folder name; the month is the month of
//!   [`Observation::ts`]). FHIR `Observation`/`DiagnosticReport`, a lab PDF
//!   parse, or — the **pioneer** writer — a Dexcom CGM estimated-glucose reading
//!   ([`crate::dexcom`]) write this shape.
//! - **medication** — one prescribed/reported/filled medication
//!   (`MedicationRequest`/`MedicationStatement`, a Part D claim, or a pharmacy
//!   PDF), under `health/medical/<source>/medications/YYYY-MM.jsonl`.
//! - **condition** — one problem/diagnosis (`Condition` or a claim diagnosis),
//!   under `health/medical/<source>/conditions/YYYY-MM.jsonl`.
//!
//! Only [`Observation`] is bound as a Rust type so far (first collector
//! `dexcom`). The sibling medication and condition shapes stay Phase-3 drafts
//! until a collector writes them (the `environment`/`home` precedent: one
//! entry, bind only what you write) — the draft schemas + example fixtures
//! already exist and are validated by the contract suite.
//!
//! All three streams are **append-only** — an observation/med/condition is
//! recorded once — and collectors skip guids they already hold (`guid` is the
//! dedupe key). Only the required core is mandatory (`ts`/`source`/`guid`/`test`
//! for an observation); everything else is omit-empty, so a sparse PDF parse
//! writes little more than the core while a coded FHIR result fills many fields.
//! Coded terminologies ride as an optional `code` + `code_system` pair (LOINC
//! for labs/vitals); a source without a code omits both. Source-specific fields
//! the normalized columns don't carry ride verbatim under `extra` rather than
//! being dropped.
//!
//! This domain is **privacy-sensitive (medical)**: every collector ships opt-in
//! with explicit acknowledgement, and full-fidelity raw resources live under
//! each source's own `raw/` folder regardless of the contract.
//!
//! Cross-source overlap (a Quest result that also arrives inside an Epic
//! bundle) is reconciled at *read* time — each source keeps its own folder and
//! stable guids; nothing is merged or dropped at write time.
//!
//! See [`docs/vault-spec/domains/health-medical.md`] for the field-level spec;
//! the schema field descriptions there are authoritative for
//! names/units/meanings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One lab result or vital sign — one line of
/// `health/medical/<source>/observations/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Numeric results carry
/// `value` (+`unit`); qualitative results ("Non-Reactive", "Positive") carry
/// `value_text`; a row uses whichever applies. Only `ts`/`source`/`guid`/`test`
/// are required; everything else is omit-empty. Matches
/// `health-medical.observation.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Observation {
    /// Effective/collection time: RFC3339 local, or a date-only `YYYY-MM-DD`
    /// when that is all the source gives (clinical data is routinely
    /// date-granular). Always serialized; its month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`quest-diagnostics`,
    /// `epic-mychart`, `dexcom`). Always serialized.
    pub source: String,
    /// Source-unique id, the dedupe key (FHIR `Observation.id` org-scoped, the
    /// Dexcom EGV `recordId`, or `hash+test+date` for a parsed PDF). Always
    /// serialized.
    pub guid: String,
    /// Test/measurement name (FHIR `code.text` or `coding.display`). Always
    /// serialized.
    pub test: String,
    /// Terminology code (FHIR `coding.code`) — LOINC for labs/vitals; omitted by
    /// a PDF-parsed row.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    /// Which system the code is from: `"loinc"` | `"snomed"` | …
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code_system: String,
    /// Numeric result (FHIR `valueQuantity.value`), paired with `unit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Qualitative result (FHIR `valueString`: `"Positive"`, `"Non-Reactive"`,
    /// …) for non-numeric tests.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value_text: String,
    /// Unit of `value`, UCUM where FHIR-sourced (`"mg/dL"`, `"mm[Hg]"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// Normal range as text (`"70-99"`, `"<5.7"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reference_range: String,
    /// Abnormal interpretation (`"H"`, `"L"`, `"A"`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub flag: String,
    /// Parent `DiagnosticReport` / order name (`"Comprehensive Metabolic
    /// Panel"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub panel: String,
    /// Ordering provider or lab, as a display name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Everything source-specific the normalized columns don't carry (specimen,
    /// status, trend, full resource, confidence) — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Observation {
    /// A minimal record with only the four required fields set.
    pub fn new(
        source: impl Into<String>,
        guid: impl Into<String>,
        ts: impl Into<String>,
        test: impl Into<String>,
    ) -> Self {
        Observation {
            ts: ts.into(),
            source: source.into(),
            guid: guid.into(),
            test: test.into(),
            code: String::new(),
            code_system: String::new(),
            value: None,
            value_text: String::new(),
            unit: String::new(),
            reference_range: String::new(),
            flag: String::new(),
            panel: String::new(),
            provider: String::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_observation_serializes_only_required_fields() {
        let o = Observation::new(
            "lab-pdf",
            "f3a9c1-hiv-ab",
            "2025-11-03",
            "HIV 1/2 Antibody Screen",
        );
        // Omit-empty: a sparse line is exactly the four required keys.
        assert_eq!(
            serde_json::to_value(&o).unwrap(),
            json!({
                "ts": "2025-11-03",
                "source": "lab-pdf",
                "guid": "f3a9c1-hiv-ab",
                "test": "HIV 1/2 Antibody Screen"
            })
        );
    }

    #[test]
    fn full_numeric_observation_round_trips() {
        let line = json!({
            "ts": "2026-04-02T09:30:10-07:00",
            "source": "quest-diagnostics",
            "guid": "obs-15074-8-7a3f",
            "test": "Glucose [Mass/volume] in Blood",
            "code": "15074-8",
            "code_system": "loinc",
            "value": 113,
            "unit": "mg/dL",
            "reference_range": "70-99",
            "flag": "H",
            "panel": "Comprehensive Metabolic Panel",
            "provider": "Quest Diagnostics"
        });
        let o: Observation = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(o.value, Some(113.0), "numeric result");
        assert_eq!(o.code, "15074-8");
        assert_eq!(o.code_system, "loinc");
        assert_eq!(o.flag, "H");
        // Round-trips with every field preserved. `value` is typed `f64`, so it
        // re-serializes as the JSON number `113.0`; the input wrote the integer
        // literal `113`. Both are the *same* number under the schema (type
        // "number"), but `serde_json::Value` equality is representation-sensitive
        // (Number(113.0) != Number(113)), so compare `value` numerically and the
        // rest of the object structurally.
        let re = serde_json::to_value(&o).unwrap();
        assert_eq!(re["value"].as_f64(), Some(113.0), "numeric value round-trips");
        let (mut re_rest, mut line_rest) = (re.clone(), line.clone());
        re_rest.as_object_mut().unwrap().remove("value");
        line_rest.as_object_mut().unwrap().remove("value");
        assert_eq!(re_rest, line_rest, "every non-numeric field round-trips unchanged");
    }

    #[test]
    fn qualitative_observation_uses_value_text_not_value() {
        // A non-numeric result writes value_text and omits value; an unknown
        // top-level field is tolerated (forward-compat) and dropped on
        // re-serialize.
        let line = json!({
            "ts": "2025-11-03",
            "source": "lab-pdf",
            "guid": "f3a9c1-hiv-ab",
            "test": "HIV 1/2 Antibody Screen",
            "value_text": "Non-Reactive",
            "extra": {"confidence": "0.82"},
            "future_field": "ignored"
        });
        let o: Observation = serde_json::from_value(line).unwrap();
        assert_eq!(o.value_text, "Non-Reactive");
        assert!(o.value.is_none(), "no numeric value on a qualitative result");
        assert_eq!(o.extra.get("confidence"), Some(&json!("0.82")));
        let re = serde_json::to_value(&o).unwrap();
        assert!(re.get("value").is_none(), "empty value omitted");
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
    }

    #[test]
    fn fractional_value_is_preserved() {
        // The schema types `value` as a number, not an integer (HbA1c 5.6%,
        // a 36.6 °C temperature) — prove a fractional value round-trips.
        let o = Observation {
            value: Some(5.6),
            unit: "%".into(),
            ..Observation::new("epic-mychart", "Observation/eA1c", "2026-03-01", "Hemoglobin A1c")
        };
        let re = serde_json::to_value(&o).unwrap();
        assert_eq!(re["value"], json!(5.6));
        let back: Observation = serde_json::from_value(re).unwrap();
        assert_eq!(back.value, Some(5.6));
    }
}

//! The `habits` domain contract: the routines the user deliberately tracks —
//! and the per-day record of whether each one happened — in one normalized,
//! source-agnostic store.
//!
//! Two record shapes share the domain, mirroring `tasks/` (snapshot + events):
//!
//! - **[`Habit`]** — one current habit *definition*, rewritten whole on every
//!   sync, under `habits/<source>/habits.jsonl` (`<source>` is the collector id
//!   and the folder name). Habitica (habits + dailies), Habitify, and TickTick's
//!   habits feature write this shape over their APIs; Way of Life writes it from
//!   a CSV/Excel export. Only `source`/`id`/`title` are required — a yes/no
//!   tracker writes just those three, a measurable habit fills `goal`+`unit`.
//! - **[`Checkin`]** — one habit-day *event*: the day a habit was marked and how
//!   it went, under `habits/<source>/checkins/YYYY-MM.jsonl` (month of
//!   [`Checkin::date`]). Append-only. Only `date`/`source`/`habit`/`status` are
//!   required; a measurable habit adds `value`.
//!
//! The snapshot is keyed by `id`; check-ins dedupe on `source` + `habit` +
//! `date` (one row per habit per day), with `guid` carrying the source-native id
//! when there is one. `date` (not `ts`) is the check-in key because the calendar
//! day is the one thing every source agrees on (Way of Life records no time of
//! day); a source that knows the exact moment adds `ts`.
//!
//! `status` is a coarse normalization of each app's own vocabulary (TickTick's
//! `0`/`1`/`2`, Habitica's per-day history values, Way of Life's yes/no/skip);
//! the raw flag stays in `extra`. A measurable habit that logged a partial
//! amount is still `"done"` unless the source says otherwise — `value` carries
//! the shortfall, `status` is never guessed from it. A habit-day the source
//! never recorded is *absent*, not a `"missed"` row.
//!
//! Cross-source overlap (the same routine logged in two apps) is reconciled at
//! *read* time — each source keeps its own folder and stable ids; nothing is
//! merged or dropped at write time. A habit's full raw payload (RPG progression,
//! reminders, the per-day history blob) stays at full fidelity in
//! `habits/<source>/raw/`; only the definition and the normalized check-ins join
//! the contract.
//!
//! See [`docs/vault-spec/domains/habits.md`] for the field-level spec; the
//! schema field descriptions there are authoritative for names/units/meanings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One current habit definition — one line of `habits/<source>/habits.jsonl`,
/// rewritten whole per sync.
///
/// A *snapshot* record (no `ts`), keyed by `id`. Only `source`/`id`/`title` are
/// required; everything else is omit-empty. Matches `habits.habit.schema.json`
/// field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Habit {
    /// Collector id, identical to the source folder name (`habitica`,
    /// `ticktick`, `way-of-life`). Always serialized.
    pub source: String,
    /// Source-native habit id, the snapshot key (Way of Life: a stable hash of
    /// the habit name). Always serialized.
    pub id: String,
    /// Habit name. Always serialized.
    pub title: String,
    /// Cadence, verbatim from the source: an RRULE, `"daily"`, `"weekly"`,
    /// `"Mon,Wed,Fri"` — raw, not interpreted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schedule: String,
    /// Per-period target for a measurable habit (`8` glasses, `30` minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<f64>,
    /// The unit `goal` is counted in (`"Glass"`, `"min"`, `"page"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// Display color, where the source has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    /// Habit is archived / paused / no longer active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// RFC3339 local time the habit was created.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created: String,
    /// RFC3339 local time the habit was last modified.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub modified: String,
    /// Everything source-specific the normalized fields don't carry (streak
    /// counts, RPG XP/level/gold, reminders, icon, target days, …) — full
    /// fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Habit {
    /// A minimal record with only the three required fields set.
    pub fn new(source: impl Into<String>, id: impl Into<String>, title: impl Into<String>) -> Self {
        Habit {
            source: source.into(),
            id: id.into(),
            title: title.into(),
            schedule: String::new(),
            goal: None,
            unit: String::new(),
            color: String::new(),
            archived: None,
            created: String::new(),
            modified: String::new(),
            extra: Map::new(),
        }
    }
}

/// One habit-day — one line of `habits/<source>/checkins/YYYY-MM.jsonl`,
/// append-only.
///
/// An *event* record keyed on the calendar `date` (not `ts`); the dedupe key is
/// `source` + `habit` + `date`. Only `date`/`source`/`habit`/`status` are
/// required; a measurable habit adds `value`, a source that knows the exact
/// moment adds `ts`. Matches `habits.checkin.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Checkin {
    /// The day checked, date-only `YYYY-MM-DD` (local); the dedupe key (with
    /// `source` + `habit`). Always serialized; its month is the partition key.
    pub date: String,
    /// Collector id, identical to the source folder name. Always serialized.
    pub source: String,
    /// The habit `id` this check-in belongs to. Always serialized.
    pub habit: String,
    /// Coarse outcome: `"done"` | `"skipped"` (deliberately excused — a vacation
    /// day) | `"missed"`; never guessed from a partial value. Always serialized.
    pub status: String,
    /// RFC3339 local moment of the check-in, when the source records one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ts: String,
    /// Logged amount for a measurable habit (`8` glasses, `30` minutes) — the
    /// per-day total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Free-form note attached to the check-in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Source-native check-in id, where one exists.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guid: String,
    /// Everything source-specific the normalized fields don't carry
    /// (goal-at-the-time, raw stamp, mood, …) — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Checkin {
    /// A minimal record with only the four required fields set.
    pub fn new(
        source: impl Into<String>,
        habit: impl Into<String>,
        date: impl Into<String>,
        status: impl Into<String>,
    ) -> Self {
        Checkin {
            date: date.into(),
            source: source.into(),
            habit: habit.into(),
            status: status.into(),
            ts: String::new(),
            value: None,
            note: String::new(),
            guid: String::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_habit_serializes_only_required_fields() {
        let h = Habit::new("way-of-life", "f3a9c1b27e", "No alcohol");
        // Omit-empty: a sparse line is exactly the three required keys.
        assert_eq!(
            serde_json::to_value(&h).unwrap(),
            json!({"source": "way-of-life", "id": "f3a9c1b27e", "title": "No alcohol"})
        );
    }

    #[test]
    fn minimal_checkin_serializes_only_required_fields() {
        let c = Checkin::new("way-of-life", "f3a9c1b27e", "2026-06-10", "skipped");
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({"date": "2026-06-10", "source": "way-of-life", "habit": "f3a9c1b27e", "status": "skipped"})
        );
    }

    #[test]
    fn full_habit_round_trips_with_numeric_goal() {
        let line = json!({
            "source": "ticktick",
            "id": "6247e8f0b3a1c2",
            "title": "Drink water",
            "schedule": "RRULE:FREQ=DAILY;INTERVAL=1",
            "goal": 8,
            "unit": "Glass",
            "color": "#97E38B",
            "archived": false,
            "created": "2026-01-04T08:00:00-08:00",
            "extra": {"totalCheckIns": 118, "targetDays": 21}
        });
        let h: Habit = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(h.goal, Some(8.0), "goal is a number");
        assert_eq!(h.unit, "Glass");
        assert_eq!(h.archived, Some(false));
        // Re-serialize and re-parse: every field survives the round-trip. (A
        // `goal:8` integer comes back as the float `8.0` because the contract
        // types it `number`/`f64` — fractional goals are real — so compare the
        // reparsed value, not the integer-vs-float byte form.)
        let re: Habit = serde_json::from_value(serde_json::to_value(&h).unwrap()).unwrap();
        assert_eq!(re, h, "round-trips through serde");
    }

    #[test]
    fn full_checkin_round_trips_with_value() {
        let line = json!({
            "date": "2026-06-10",
            "source": "ticktick",
            "habit": "6247e8f0b3a1c2",
            "status": "done",
            "ts": "2026-06-10T21:14:00-07:00",
            "value": 8,
            "extra": {"goal": 8, "stamp": 20260610}
        });
        let c: Checkin = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(c.status, "done");
        assert_eq!(c.value, Some(8.0));
        // Re-serialize and re-parse: every field survives (a `value:8` integer
        // returns as the float `8.0` — `value` is typed `number`/`f64`).
        let re: Checkin = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
        assert_eq!(re, c, "round-trips through serde");
    }

    #[test]
    fn unknown_fields_tolerated_and_archived_omitted_when_unset() {
        // Forward-compat: an unknown top-level field is ignored; an unset
        // `archived` (a non-archivable source) omits the key entirely.
        let line = json!({
            "source": "habitica",
            "id": "a1f4c0de-2b77-4f3e-9c11-8d6e0f5a2b34",
            "title": "Meditate",
            "schedule": "weekly",
            "archived": true,
            "future_field": "ignored",
            "extra": {"frequency": "daily", "streak": 0}
        });
        let h: Habit = serde_json::from_value(line).unwrap();
        assert_eq!(h.title, "Meditate");
        assert_eq!(h.archived, Some(true));
        let re = serde_json::to_value(&h).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert!(re.get("goal").is_none(), "unset goal omitted");
        assert!(re.get("unit").is_none(), "empty unit omitted");
    }
}

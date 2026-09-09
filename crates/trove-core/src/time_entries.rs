//! The `time-entries` domain contract: **user-asserted** time entries — what
//! the owner *says* they spent time on, logged by hand against projects,
//! clients, and tasks in a manual time tracker (Toggl Track, Clockify,
//! Harvest; Timery is a Toggl frontend with no store of its own, so it lands
//! here under `toggl-track`). Readers see one stream of how the owner
//! accounted for their hours, regardless of tracker.
//!
//! This is the deliberate counterpart to `activity/` — **observed**
//! auto-trackers (RescueTime, Timing) are passive measurements and write
//! `activity/<source>/`, never here. The two merge only at *read* time, where
//! a view can compare asserted hours against observed ones.
//!
//! One record shape ([`TimeEntry`]) under `time-entries/<source>/YYYY-MM.jsonl`
//! (`<source>` is the collector id and the folder name; the month is the
//! **local** month of [`TimeEntry::start`]). The stream is **append-only** —
//! an entry is logged once — and collectors skip ids they already hold (`id`
//! is the dedupe key). Because it is append-only, the *first* observed state of
//! an `id` is the one that lands: an entry first seen while its timer is still
//! running is written with no `end`/`duration_secs`, and a later poll that
//! catches it stopped is dropped at the contract layer (the `id` is already
//! held) — the final `end`/`duration_secs` survive only in the per-source
//! `raw/` snapshot. (A collector that prefers no open rows can let an
//! in-progress timer settle and emit it once stopped.)
//!
//! Only `source`/`id`/`start` are required; everything else is omit-empty, so
//! a sparse free-tier row carries just a description while a rich one fills
//! project/client/task/tags/billable. A running timer is a [`TimeEntry`] with
//! a `start` and no `end`/`duration_secs` (both omitted until it stops).
//! Source-specific fields the normalized columns don't carry (workspace id,
//! hourly rate, rounding, invoice id, color, …) ride verbatim under `extra`
//! rather than being dropped.
//!
//! See [`docs/vault-spec/domains/time-entries.md`] for the field-level spec;
//! the schema field descriptions there are authoritative for names/units/
//! meanings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One user-asserted time entry — one line of
/// `time-entries/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `start`), not a snapshot. Only
/// `source`/`id`/`start` are required; everything else is omit-empty. Matches
/// `time-entries.entry.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct TimeEntry {
    /// Collector id, identical to the source folder name (`toggl-track`,
    /// `clockify`, `harvest`). Always serialized.
    pub source: String,
    /// Source-native entry id, the dedupe key. Always serialized.
    pub id: String,
    /// When the entry began: RFC3339 **local** time, **or** a date-only
    /// `YYYY-MM-DD` for a duration-only entry the source never timestamped
    /// (Harvest "X hours on this day"). Always serialized; its (local) month
    /// is the partition key. A date-only value is a lexical prefix of a full
    /// timestamp, so mixed rows still sort and partition correctly.
    pub start: String,
    /// RFC3339 local time the entry stopped; omitted while a timer runs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end: String,
    /// Seconds tracked; omitted while a timer runs. Present for duration-only
    /// entries that have no `end`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<i64>,
    /// The user's note for the entry.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Project name, verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    /// Client name, verbatim (Harvest/Clockify; absent on trackers without
    /// clients).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client: String,
    /// Task / sub-activity name within the project, verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    /// Source-native tag labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Whether the entry is marked billable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billable: Option<bool>,
    /// Everything source-specific the normalized columns don't carry
    /// (workspace/account id, hourly rate, rounding, invoice id, color, …) —
    /// full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl TimeEntry {
    /// A minimal record with only the three required fields set.
    pub fn new(
        source: impl Into<String>,
        id: impl Into<String>,
        start: impl Into<String>,
    ) -> Self {
        TimeEntry {
            source: source.into(),
            id: id.into(),
            start: start.into(),
            end: String::new(),
            duration_secs: None,
            description: String::new(),
            project: String::new(),
            client: String::new(),
            task: String::new(),
            tags: Vec::new(),
            billable: None,
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
        let e = TimeEntry::new("toggl-track", "3691827456", "2026-06-10T09:00:00-07:00");
        // Omit-empty: a sparse line is exactly the three required keys.
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"source": "toggl-track", "id": "3691827456", "start": "2026-06-10T09:00:00-07:00"})
        );
    }

    #[test]
    fn full_entry_round_trips() {
        let line = json!({
            "source": "toggl-track",
            "id": "3691827456",
            "start": "2026-06-10T09:00:00-07:00",
            "end": "2026-06-10T10:30:00-07:00",
            "duration_secs": 5400,
            "description": "Quarterly traffic report",
            "project": "Editorial",
            "tags": ["deep-work"],
            "billable": true,
            "extra": {"workspace_id": 1234567}
        });
        let e: TimeEntry = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(e.id, "3691827456");
        assert_eq!(e.duration_secs, Some(5400));
        assert_eq!(e.billable, Some(true));
        assert_eq!(e.tags, vec!["deep-work"]);
        assert_eq!(serde_json::to_value(&e).unwrap(), line);
    }

    #[test]
    fn running_timer_omits_end_and_duration() {
        // A running timer: a start with no end/duration_secs (both omitted).
        let e = TimeEntry::new("toggl-track", "9999", "2026-06-15T08:00:00-07:00");
        let re = serde_json::to_value(&e).unwrap();
        assert!(re.get("end").is_none(), "running timer omits end");
        assert!(re.get("duration_secs").is_none(), "running timer omits duration_secs");
    }

    #[test]
    fn date_only_duration_entry_round_trips() {
        // A Harvest-style duration-only entry: a date-only `start`, a
        // `duration_secs`, no `end`.
        let line = json!({
            "source": "harvest",
            "id": "636709355",
            "start": "2026-06-09",
            "duration_secs": 7740,
            "description": "On-site client workshop",
            "project": "Website Redesign",
            "client": "Acme Co",
            "billable": true
        });
        let e: TimeEntry = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(e.start, "2026-06-09", "date-only start kept verbatim");
        assert!(e.end.is_empty());
        assert_eq!(e.duration_secs, Some(7740));
        assert_eq!(e.client, "Acme Co");
        assert_eq!(serde_json::to_value(&e).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_back_compat() {
        // Forward-compat: an unknown top-level field is ignored on re-serialize.
        // A sparse Clockify row with only a description (no client/billable).
        let line = json!({
            "source": "clockify",
            "id": "657f1a9b2c3d4e5f6a7b8c9d",
            "start": "2026-06-11T13:15:00-07:00",
            "description": "Pairing on the import parser",
            "project": "Trove",
            "task": "time-entries collector",
            "future_field": "ignored"
        });
        let e: TimeEntry = serde_json::from_value(line).unwrap();
        assert_eq!(e.task, "time-entries collector");
        assert!(e.client.is_empty());
        assert!(e.billable.is_none());
        let re = serde_json::to_value(&e).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert!(re.get("client").is_none(), "empty client omitted");
        assert!(re.get("billable").is_none(), "absent billable omitted");
    }
}

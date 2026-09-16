//! App activity: the read side of the `activity/` stream.
//!
//! Source of truth is one JSONL file per local day: `activity/YYYY-MM-DD.jsonl`,
//! one merged event per line:
//!
//! ```json
//! {"start":"2026-06-10T14:03:01-07:00","end":"2026-06-10T14:09:22-07:00",
//!  "seconds":381,"app":"Code","bundle_id":"","title":"activity.rs","afk":false}
//! ```
//!
//! AFK ("away from keyboard") spans are recorded as events with `afk: true`
//! and an empty app, so per-app totals never count idle time. Reads aggregate
//! these on the fly — activity volume is tiny next to health, so no derived
//! index is needed yet.
//!
//! The stream is **written by the external `trove-collector` program** (the
//! CoreGraphics sampler and the span-merging state machine live there;
//! `docs/vault-spec/domains/activity.md` is the contract both sides follow).
//! This crate only reads it, plus [`Vault::append_activity_events`] as the
//! byte-parity reference writer used by tests and imports.

use std::collections::HashMap;
use std::fs;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};
use crate::vault::Vault;

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join("activity"))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "activity",
        name: "App activity",
        kind: IntegrationKind::Live,
        default_on: true,
        description: "Tracks the frontmost app and window title with idle detection, 24/7 via the trove-collector background process. Window titles of other apps need Screen Recording.",
        domain: "activity",
        vault_path: "activity/",
        toggleable: true,
        setup: &[
            "Install trove-collector (github.com/david-wills/trove-collector): scripts/build.sh builds, signs, and registers the launch agent.",
            "App-level tracking works immediately, no permission needed.",
            "For window titles: run `trove-collector permission` (or System Settings → Privacy & Security → Screen Recording → add trove-collector).",
        ],
        caveats: "Screen Recording grants are per-binary and tied to the binary's signature — the collector's build script signs with a stable identity so you grant once.",
    },
    behavior: Behavior::External { collector: "trove-collector" },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// One contiguous span of using a single app/window — or being away.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ActivityEvent {
    /// RFC3339 local time, e.g. "2026-06-10T14:03:01-07:00".
    pub start: String,
    pub end: String,
    pub seconds: u64,
    /// Owning app; empty for AFK spans.
    pub app: String,
    #[serde(default)]
    pub bundle_id: String,
    #[serde(default)]
    pub title: String,
    pub afk: bool,
}

/// Per-app time over a date range (active time only).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AppUsage {
    pub app: String,
    pub seconds: u64,
}

/// Aggregate of a date range for the Activity view.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ActivitySummary {
    pub active_seconds: u64,
    pub afk_seconds: u64,
    /// Apps by active time, descending.
    pub apps: Vec<AppUsage>,
}

impl Vault {
    /// Append merged events to their day's JSONL log (keyed by start day).
    pub fn append_activity_events(&self, events: &[ActivityEvent]) -> Result<()> {
        self.stream("activity", crate::store::Partition::Day)
            .append(events, |e| &e.start)
    }

    /// All events for one local day (YYYY-MM-DD), in file order.
    pub fn activity_timeline(&self, date: &str) -> Result<Vec<ActivityEvent>> {
        let path = self.resolve(&format!("activity/{date}.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path).with_context(|| format!("reading activity/{date}.jsonl"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<ActivityEvent>(l).ok())
            .collect())
    }

    /// Per-app active time plus active/AFK totals over an inclusive date range.
    pub fn activity_summary(&self, from: &str, to: &str) -> Result<ActivitySummary> {
        let mut apps: HashMap<String, u64> = HashMap::new();
        let mut active = 0u64;
        let mut afk = 0u64;
        for date in days(from, to)? {
            for e in self.activity_timeline(&date)? {
                if e.afk {
                    afk += e.seconds;
                } else {
                    active += e.seconds;
                    *apps.entry(e.app).or_default() += e.seconds;
                }
            }
        }
        let mut apps: Vec<AppUsage> = apps
            .into_iter()
            .map(|(app, seconds)| AppUsage { app, seconds })
            .collect();
        apps.sort_by(|a, b| b.seconds.cmp(&a.seconds).then_with(|| a.app.cmp(&b.app)));
        Ok(ActivitySummary {
            active_seconds: active,
            afk_seconds: afk,
            apps,
        })
    }

    /// Active hours per day over an inclusive range — a trend series for the
    /// chart. Days with no data are omitted.
    pub fn activity_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut out = Vec::new();
        for date in days(from, to)? {
            let secs: u64 = self
                .activity_timeline(&date)?
                .iter()
                .filter(|e| !e.afk)
                .map(|e| e.seconds)
                .sum();
            if secs > 0 {
                out.push(SeriesPoint {
                    date,
                    value: secs as f64 / 3600.0,
                });
            }
        }
        Ok(out)
    }
}

/// Inclusive list of YYYY-MM-DD strings from `from` to `to`.
pub(crate) fn days(from: &str, to: &str) -> Result<Vec<String>> {
    let start = NaiveDate::parse_from_str(from, "%Y-%m-%d").context("bad from date")?;
    let end = NaiveDate::parse_from_str(to, "%Y-%m-%d").context("bad to date")?;
    let mut out = Vec::new();
    let mut d = start;
    while d <= end {
        out.push(d.format("%Y-%m-%d").to_string());
        match d.succ_opt() {
            Some(n) => d = n,
            None => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Local, TimeZone};

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 10, h, m, s).unwrap()
    }

    fn span(start: DateTime<Local>, end: DateTime<Local>, app: &str, title: &str) -> ActivityEvent {
        ActivityEvent {
            start: start.to_rfc3339(),
            end: end.to_rfc3339(),
            seconds: (end - start).num_seconds() as u64,
            app: app.into(),
            bundle_id: String::new(),
            title: title.into(),
            afk: app.is_empty(),
        }
    }

    /// Byte-parity contract for the append path — the exact line shape the
    /// external collector writes and this reader expects.
    #[test]
    fn append_writes_byte_identical_jsonl() {
        let dir = std::env::temp_dir().join(format!("trove-activity-parity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();
        v.append_activity_events(&[span(at(9, 0, 0), at(9, 1, 0), "Code", "a.rs")]).unwrap();
        let raw = fs::read_to_string(v.root().join("activity/2026-06-10.jsonl")).unwrap();
        assert_eq!(
            raw,
            format!(
                "{{\"start\":\"{}\",\"end\":\"{}\",\"seconds\":60,\"app\":\"Code\",\
                 \"bundle_id\":\"\",\"title\":\"a.rs\",\"afk\":false}}\n",
                at(9, 0, 0).to_rfc3339(),
                at(9, 1, 0).to_rfc3339()
            )
        );
    }

    #[test]
    fn vault_round_trip_and_summary() {
        let dir = std::env::temp_dir().join(format!("trove-activity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let v = Vault::open_or_create(dir).unwrap();

        v.append_activity_events(&[
            span(at(9, 0, 0), at(9, 1, 0), "Code", "a.rs"),
            span(at(9, 1, 0), at(9, 1, 30), "Safari", "news"),
            span(at(9, 1, 30), at(9, 5, 30), "", ""),
        ])
        .unwrap();

        let day = v.activity_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3);

        let s = v.activity_summary("2026-06-10", "2026-06-10").unwrap();
        assert_eq!(s.active_seconds, 90); // 60 Code + 30 Safari
        assert_eq!(s.afk_seconds, 240, "afk spans never count toward apps");
        assert_eq!(s.apps[0].app, "Code");
        assert_eq!(s.apps[0].seconds, 60);

        let daily = v.activity_daily("2026-06-09", "2026-06-11").unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].date, "2026-06-10");
        assert!((daily[0].value - 90.0 / 3600.0).abs() < 1e-9);
    }
}

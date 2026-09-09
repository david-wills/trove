//! Calendar collector — Apple Calendar via EventKit, which reads *every*
//! synced account in one shot (iCloud, Google/Gmail, Google Workspace,
//! Exchange, subscriptions) — plus the Apple Reminders half, which plugs into
//! the normalized `tasks/` contract (see [`crate::tasks`]).
//!
//! Store (the snapshot+events shape of `tasks`/`music_library`, sharded by
//! month because a calendar is naturally a timeline):
//!
//! - `calendar/events/YYYY-MM.jsonl` — occurrence snapshot keyed by the
//!   occurrence's *start* month, one occurrence per line, recurrences
//!   expanded. Each sync rewrites only the months whose content changed
//!   (atomic temp+rename); months older than the sync window freeze and are
//!   never touched again.
//! - `calendar/changes/YYYY-MM.jsonl` — append-only diff stream keyed by
//!   *observation* month: `added` / `changed` / `removed`, where `changed`
//!   carries per-field `{field, old, new}`. **This is the unrecoverable
//!   part**: the event store always reflects the present, so reschedule and
//!   cancellation history exists only because each sync diffs against the
//!   stored snapshot. The occurrences themselves are fully backfillable.
//!
//! **The first-ever snapshot is a silent baseline** (years of events are
//! state, not "added" events), followed by a one-time history backfill in
//! year chunks — EventKit predicates cap at ~4 years — walking backwards
//! until [`BACKFILL_EMPTY_STOP`] consecutive empty years.
//!
//! **The store is shared** with the Google Calendar pull
//! ([`crate::google_calendar`]): rows written by that collector carry a
//! [`GOOGLE_ROW_PREFIX`]-prefixed id, and each collector's snapshot pass
//! diffs and rewrites only the rows it owns
//! ([`Vault::calendar_snapshot_scoped`]) — so neither pass can wipe or
//! mass-"remove" the other's rows.
//!
//! Sync scope: each pass re-queries `[today − WINDOW_PAST_DAYS, today +
//! WINDOW_FUTURE_DAYS]` and diffs only inside that window. A reschedule of an
//! event further out is caught when it enters the window; an event moved
//! *beyond* the window edge logs as `removed` (documented trade-off).
//!
//! Both collectors gate on TCC full access — checked cheaply via
//! authorization status, requested (prompt) once when not-determined, and
//! *silently skipped* while denied so the owner loop's logs don't fill with
//! permission noise (the Safari-history precedent). The diff/store half is
//! OS-free and tested with injected data; EventKit lives in
//! [`crate::eventkit`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{Context, Result};
use chrono::{Duration, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::eventkit::{self, AuthStatus};
use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind};
use crate::music_library::FieldChange;
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::tasks::{TaskFate, TasksSyncStats};
use crate::vault::Vault;

// Skips itself silently while Calendar access is missing (it requests once
// when undetermined); queries are near-instant, and nothing is written when
// nothing changed. Only event-bearing passes (and the one-time backfill) log.
fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    use crate::registry::CollectOutcome;
    let s = vault.collect_calendar()?;
    Ok(if s.baseline || s.backfilled_months > 0 {
        CollectOutcome::note(format!(
            "calendar baseline — {} events in window, {} months backfilled",
            s.events, s.backfilled_months
        ))
    } else if s.added + s.removed + s.changed > 0 {
        CollectOutcome::note(format!(
            "calendar synced — {} added, {} changed, {} removed",
            s.added, s.changed, s.removed
        ))
    } else {
        CollectOutcome::quiet()
    })
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "calendars",
        granted: Some(eventkit::events_auth_status() == AuthStatus::Granted),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_calendar_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "calendar",
        name: "Calendar",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Syncs every calendar the Mac knows — iCloud, Google, Exchange, subscriptions — via EventKit every 15 minutes, with full history backfilled once and a change stream for reschedules and cancellations.",
        domain: "calendar",
        vault_path: "calendar/",
        toggleable: true,
        setup: &[
            "Approve the Calendar access prompt the first time the app (and separately the troved daemon) asks for Full Access.",
            "If the prompt was declined: System Settings → Privacy & Security → Calendars → set Trove and troved to Full Access.",
            "Google/Exchange calendars are covered as long as the account is added to macOS (System Settings → Internet Accounts) with Calendars on.",
        ],
        caveats: "Events themselves can be re-backfilled anytime, but the change stream (what got rescheduled or cancelled) only accrues while the sync runs. Changes are only watched 60 days back / 1 year forward; events past their month freeze. There is no manual-grant path: the Calendars settings pane only lists apps after they ask.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(crate::browser::BROWSER_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// How far back each sync pass re-queries and diffs. Anything older freezes.
pub const WINDOW_PAST_DAYS: i64 = 60;
/// How far forward — wide, so reschedules of upcoming events are seen.
pub const WINDOW_FUTURE_DAYS: i64 = 366;
/// One-time backfill walks backwards in chunks of this many days…
const BACKFILL_CHUNK_DAYS: i64 = 365;
/// …stopping after this many consecutive empty chunks…
const BACKFILL_EMPTY_STOP: u32 = 2;
/// …or this many chunks total, whichever first.
const BACKFILL_MAX_CHUNKS: u32 = 30;
/// Completed reminders are pulled this far back to resolve disappearances.
const REMINDERS_COMPLETED_LOOKBACK_DAYS: i64 = 90;
/// How long a TCC-prompt answer is awaited before deferring to the next pass
/// (an already-decided request resolves instantly).
const ACCESS_WAIT_SECS: u64 = 5;

const SYNC_FILE: &str = ".trove/calendar-sync.json";
const EVENTS_DIR: &str = "calendar/events";

/// Id prefix marking occurrences written by the Google Calendar collector
/// ([`crate::google_calendar`]), which shares this store. Ownership is keyed
/// off it: the EventKit snapshot owns every row *without* the prefix, the
/// Google pull owns the rows scoped under it
/// (`gcal:{sub}/{calendarId}/{eventId}`).
pub(crate) const GOOGLE_ROW_PREFIX: &str = "gcal:";

/// Id prefix marking occurrences written by the Outlook Calendar collector
/// ([`crate::outlook_calendar`]). Same shared-store ownership scheme as
/// Google: `mcal:{account_id}/{calendar_id}/{event_id}`.
pub(crate) const MICROSOFT_ROW_PREFIX: &str = "mcal:";

/// One event occurrence — a line in `calendar/events/YYYY-MM.jsonl`. Times
/// are RFC3339 local. Only `id` is required to deserialize, so old lines keep
/// parsing as fields are added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CalendarOccurrence {
    /// EventKit's event identifier — shared by all occurrences of a
    /// recurring event, so the diff key is `id` + `occurrence`.
    pub id: String,
    /// The occurrence's original slot (stable across a reschedule of this
    /// instance, which is exactly what makes it a usable key).
    #[serde(default)]
    pub occurrence: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub title: String,
    /// Calendar name ("Work", "HEALTH", …).
    #[serde(default)]
    pub calendar: String,
    /// Account the calendar syncs from (EKSource title: "iCloud", "Gmail",
    /// "dwills@example.com", …).
    #[serde(default)]
    pub account: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<String>,
    /// "confirmed" / "tentative" / "canceled" / "" (none reported).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default)]
    pub recurring: bool,
}

impl CalendarOccurrence {
    /// The diff key: `id` + `occurrence` (shared with [`diff_occurrences`]
    /// and the Google collector's working sets).
    pub(crate) fn key(&self) -> String {
        format!("{}/{}", self.id, self.occurrence)
    }

    /// Start day (YYYY-MM-DD) and month (YYYY-MM) — RFC3339 prefixes.
    pub(crate) fn day(&self) -> &str {
        &self.start[..10.min(self.start.len())]
    }

    fn month(&self) -> &str {
        &self.start[..7.min(self.start.len())]
    }
}

/// One line of `calendar/changes/YYYY-MM.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CalendarChange {
    /// RFC3339 local time of the sync that observed the change.
    pub ts: String,
    /// "added", "changed", or "removed".
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub occurrence: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub calendar: String,
    /// Only on `changed`: exactly the fields that drifted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<FieldChange>,
}

/// Result of one calendar sync pass, for logging/status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CalendarSyncStats {
    /// Occurrences currently in the sync window.
    pub events: u64,
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
    /// True when this was the first-ever snapshot (baseline, no events).
    pub baseline: bool,
    /// Months written by the one-time history backfill (0 after day one).
    pub backfilled_months: u64,
    /// True when the pass was skipped for lack of TCC access.
    pub skipped_permission: bool,
}

/// Persisted in `.trove/calendar-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CalendarSyncState {
    /// RFC3339 local time of the last successful sync.
    pub updated: String,
    /// The one-time history backfill has completed.
    #[serde(default)]
    pub backfill_done: bool,
    /// Earliest day the backfill reached (YYYY-MM-DD), for display.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backfilled_to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Headline numbers for the Calendar view.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CalendarSummary {
    pub events: u64,
    pub all_day: u64,
    /// Scheduled hours (non-all-day events only).
    pub hours: f64,
    pub calendars: Vec<CalendarCount>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CalendarCount {
    pub calendar: String,
    pub account: String,
    pub count: u64,
}

/// The fields whose drift is an *event in your schedule* (reschedules, moves,
/// renames, cancellations). Notes/attendee edits update the snapshot
/// silently — corrections, not history.
fn field_changes(old: &CalendarOccurrence, new: &CalendarOccurrence) -> Vec<FieldChange> {
    let mut out = Vec::new();
    let mut push = |field: &str, o: &str, n: &str| {
        if o != n {
            out.push(FieldChange {
                field: field.into(),
                old: Value::from(o),
                new: Value::from(n),
            });
        }
    };
    push("title", &old.title, &new.title);
    push("start", &old.start, &new.start);
    push("end", &old.end, &new.end);
    push("location", &old.location, &new.location);
    push("calendar", &old.calendar, &new.calendar);
    push("status", &old.status, &new.status);
    if old.all_day != new.all_day {
        out.push(FieldChange {
            field: "all_day".into(),
            old: old.all_day.into(),
            new: new.all_day.into(),
        });
    }
    out
}

/// The pure diff: occurrences keyed by `id/occurrence`, events stamped `ts`.
/// Deterministic order — added/changed in new order, removed in old order.
pub fn diff_occurrences(
    old: &[CalendarOccurrence],
    new: &[CalendarOccurrence],
    ts: &str,
) -> Vec<CalendarChange> {
    let old_by_key: BTreeMap<String, &CalendarOccurrence> =
        old.iter().map(|o| (o.key(), o)).collect();
    let new_keys: BTreeSet<String> = new.iter().map(|o| o.key()).collect();
    let mut out = Vec::new();
    let change = |kind: &str, o: &CalendarOccurrence, changes: Vec<FieldChange>| CalendarChange {
        ts: ts.to_string(),
        kind: kind.into(),
        id: o.id.clone(),
        occurrence: o.occurrence.clone(),
        title: o.title.clone(),
        start: o.start.clone(),
        calendar: o.calendar.clone(),
        changes,
    };
    for occ in new {
        match old_by_key.get(&occ.key()) {
            None => out.push(change("added", occ, Vec::new())),
            Some(before) => {
                let changes = field_changes(before, occ);
                if !changes.is_empty() {
                    out.push(change("changed", occ, changes));
                }
            }
        }
    }
    for occ in old {
        if !new_keys.contains(&occ.key()) {
            out.push(change("removed", occ, Vec::new()));
        }
    }
    out
}

impl Vault {
    /// One snapshot pass over the sync window: diff `fresh` (everything the
    /// event store reports with a start in `[window_start, window_end]`,
    /// both YYYY-MM-DD) against the stored occurrences in the same window,
    /// append the changes, and rewrite exactly the month files whose content
    /// changed. Rows outside the window in straddling months are preserved
    /// untouched. The first-ever snapshot writes the baseline silently.
    /// OS-free — tests drive it with injected data.
    ///
    /// This is the EventKit-owned pass: it diffs and replaces only rows
    /// *without* the [`GOOGLE_ROW_PREFIX`] (see
    /// [`Vault::calendar_snapshot_scoped`]).
    pub fn calendar_snapshot(
        &self,
        fresh: &[CalendarOccurrence],
        window_start: &str,
        window_end: &str,
        ts: &str,
    ) -> Result<CalendarSyncStats> {
        self.calendar_snapshot_scoped(fresh, window_start, window_end, ts, &|o| {
            !o.id.starts_with(GOOGLE_ROW_PREFIX)
        })
    }

    /// [`Vault::calendar_snapshot`] with an ownership scope: only stored
    /// rows for which `owns` returns true are diffed against `fresh` and
    /// replaced; everything else in the touched months — other collectors'
    /// rows, rows outside the window — is preserved untouched. This is what
    /// lets EventKit and the Google Calendar pull share one store without
    /// wiping (or mass-"removing") each other's rows. Contract: `fresh` must
    /// be entirely inside the window and entirely owned by `owns`.
    pub(crate) fn calendar_snapshot_scoped(
        &self,
        fresh: &[CalendarOccurrence],
        window_start: &str,
        window_end: &str,
        ts: &str,
        owns: &dyn Fn(&CalendarOccurrence) -> bool,
    ) -> Result<CalendarSyncStats> {
        let baseline = !self.resolve(EVENTS_DIR)?.exists();
        let in_scope =
            |o: &CalendarOccurrence| o.day() >= window_start && o.day() <= window_end && owns(o);

        // Stored months that can hold in-window rows.
        let months: Vec<String> = self
            .calendar_months()?
            .into_iter()
            .filter(|m| m.as_str() >= &window_start[..7] && m.as_str() <= &window_end[..7])
            .collect();
        let mut stored_by_month: BTreeMap<String, Vec<CalendarOccurrence>> = BTreeMap::new();
        for m in &months {
            stored_by_month.insert(m.clone(), self.load_calendar_month(m)?);
        }
        let stored_in_scope: Vec<CalendarOccurrence> = stored_by_month
            .values()
            .flatten()
            .filter(|o| in_scope(o))
            .cloned()
            .collect();

        let events = if baseline {
            Vec::new()
        } else {
            diff_occurrences(&stored_in_scope, fresh, ts)
        };
        self.append_calendar_changes(&events)?;

        // Rebuild affected months: out-of-scope rows stay, in-scope rows
        // are replaced by the fresh set.
        let mut next: BTreeMap<String, Vec<CalendarOccurrence>> = BTreeMap::new();
        for (m, rows) in &stored_by_month {
            let kept: Vec<CalendarOccurrence> =
                rows.iter().filter(|o| !in_scope(o)).cloned().collect();
            next.insert(m.clone(), kept);
        }
        for occ in fresh {
            next.entry(occ.month().to_string()).or_default().push(occ.clone());
        }
        for (m, mut rows) in next {
            rows.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.key().cmp(&b.key())));
            if stored_by_month.get(&m) != Some(&rows) {
                self.write_calendar_month(&m, &rows)?;
            }
        }

        let count = |kind: &str| events.iter().filter(|e| e.kind == kind).count() as u64;
        Ok(CalendarSyncStats {
            events: fresh.len() as u64,
            added: count("added"),
            removed: count("removed"),
            changed: count("changed"),
            baseline,
            ..Default::default()
        })
    }

    /// Merge historical occurrences into their month files without emitting
    /// change events (the one-time backfill). Existing rows win on key
    /// collision — the window snapshot is fresher than a backfill query.
    pub fn calendar_backfill(&self, occs: &[CalendarOccurrence]) -> Result<u64> {
        let mut by_month: BTreeMap<String, Vec<&CalendarOccurrence>> = BTreeMap::new();
        for o in occs {
            by_month.entry(o.month().to_string()).or_default().push(o);
        }
        let mut written = 0u64;
        for (m, rows) in by_month {
            let mut existing = self.load_calendar_month(&m)?;
            let have: BTreeSet<String> = existing.iter().map(|o| o.key()).collect();
            let before = existing.len();
            existing.extend(rows.into_iter().filter(|o| !have.contains(&o.key())).cloned());
            if existing.len() != before {
                existing.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.key().cmp(&b.key())));
                self.write_calendar_month(&m, &existing)?;
                written += 1;
            }
        }
        Ok(written)
    }

    /// One full calendar sync pass (runtime; rides the owner loop's slow
    /// tick): TCC gate → window snapshot → one-time history backfill →
    /// record state. Silently a no-op while access is denied.
    pub fn collect_calendar(&self) -> Result<CalendarSyncStats> {
        match eventkit::events_auth_status() {
            AuthStatus::Granted => {}
            AuthStatus::NotDetermined => {
                if !eventkit::request_events_access(ACCESS_WAIT_SECS) {
                    return Ok(CalendarSyncStats {
                        skipped_permission: true,
                        ..Default::default()
                    });
                }
            }
            AuthStatus::Denied => {
                return Ok(CalendarSyncStats {
                    skipped_permission: true,
                    ..Default::default()
                })
            }
        }
        let now = Local::now();
        let window_start = now - Duration::days(WINDOW_PAST_DAYS);
        let window_end = now + Duration::days(WINDOW_FUTURE_DAYS);
        let fresh = eventkit::fetch_occurrences(window_start, window_end)?;
        // The fetch window is datetime-bounded while the diff window is
        // day-bounded — clamp so edge occurrences don't flap in and out.
        let (ws, we) = (
            window_start.format("%Y-%m-%d").to_string(),
            window_end.format("%Y-%m-%d").to_string(),
        );
        let fresh: Vec<CalendarOccurrence> =
            fresh.into_iter().filter(|o| o.day() >= ws.as_str() && o.day() <= we.as_str()).collect();
        // Hiccup guard: an empty fetch against a window the store knows has
        // events would diff as a mass removal. That's never plausible drift —
        // skip the pass and retry next tick (fates are never guessed). Only
        // EventKit-owned rows count: a vault fed solely by the Google pull
        // must not trip this guard on every pass.
        let has_eventkit_rows = self
            .calendar_range(&ws, &we)?
            .iter()
            .any(|o| !o.id.starts_with(GOOGLE_ROW_PREFIX));
        if fresh.is_empty() && has_eventkit_rows {
            anyhow::bail!(
                "EventKit returned no events for a window with stored events — skipping this pass"
            );
        }
        let mut stats = self.calendar_snapshot(&fresh, &ws, &we, &now.to_rfc3339())?;

        let mut state = self.read_calendar_sync().unwrap_or_default();
        if !state.backfill_done {
            let (months, reached) = self.backfill_history(window_start)?;
            stats.backfilled_months = months;
            state.backfill_done = true;
            state.backfilled_to = reached;
        }
        state.updated = now.to_rfc3339();
        state.error = None;
        self.write_calendar_sync(&state)?;
        Ok(stats)
    }

    /// Walk history backwards from `until` in year chunks, merging each into
    /// the store, until enough consecutive empty chunks or the cap.
    fn backfill_history(
        &self,
        until: chrono::DateTime<Local>,
    ) -> Result<(u64, String)> {
        let mut months = 0u64;
        let mut empty_streak = 0u32;
        let mut hi = until;
        let mut reached = hi;
        for _ in 0..BACKFILL_MAX_CHUNKS {
            let lo = hi - Duration::days(BACKFILL_CHUNK_DAYS);
            let occs = eventkit::fetch_occurrences(lo, hi)?;
            // Keep strictly-before-window rows; the window snapshot owns the rest.
            let cutoff = until.format("%Y-%m-%d").to_string();
            let occs: Vec<CalendarOccurrence> =
                occs.into_iter().filter(|o| o.day() < cutoff.as_str()).collect();
            if occs.is_empty() {
                empty_streak += 1;
                if empty_streak >= BACKFILL_EMPTY_STOP {
                    break;
                }
            } else {
                empty_streak = 0;
                months += self.calendar_backfill(&occs)?;
            }
            reached = lo;
            hi = lo;
        }
        Ok((months, reached.format("%Y-%m-%d").to_string()))
    }

    /// One Reminders sync pass: TCC gate → fetch lists/open/completions →
    /// diff through the shared tasks engine into `tasks/apple-reminders/`.
    /// Disappeared reminders resolve against the completed-reminders query
    /// (real completion times — better than a fate guess); not completed
    /// means deleted.
    pub fn collect_reminders(&self) -> Result<TasksSyncStats> {
        match eventkit::reminders_auth_status() {
            AuthStatus::Granted => {}
            AuthStatus::NotDetermined => {
                if !eventkit::request_reminders_access(ACCESS_WAIT_SECS) {
                    return Ok(TasksSyncStats::default());
                }
            }
            AuthStatus::Denied => return Ok(TasksSyncStats::default()),
        }
        let since = Local::now() - Duration::days(REMINDERS_COMPLETED_LOOKBACK_DAYS);
        let pull = eventkit::fetch_reminders(since)?;
        // Same hiccup guard as the calendar: a pull with no lists at all
        // against a non-empty snapshot would mass-delete; skip and retry.
        if pull.lists.is_empty() && !self.load_tasks_snapshot("apple-reminders")?.is_empty() {
            anyhow::bail!(
                "EventKit returned no reminder lists while the snapshot has tasks — skipping this pass"
            );
        }
        self.apply_tasks_sync("apple-reminders", &pull.lists, pull.open, |t| {
            match pull.completed.get(&t.id) {
                Some(time) if !time.is_empty() => TaskFate::Completed(Some(time.clone())),
                Some(_) => TaskFate::Completed(None),
                // Not among recent completions: deleted. (A completion older
                // than the lookback can't be one we missed — it was open at
                // the last sync, minutes-to-hours ago.)
                None => TaskFate::Deleted,
            }
        })
    }

    // ---- reads ----

    /// Occurrences starting on `date` (YYYY-MM-DD), all-day first then by
    /// start time.
    pub fn calendar_timeline(&self, date: &str) -> Result<Vec<CalendarOccurrence>> {
        let mut out: Vec<CalendarOccurrence> = self
            .load_calendar_month(&date[..7.min(date.len())])?
            .into_iter()
            .filter(|o| o.day() == date)
            .collect();
        out.sort_by(|a, b| {
            b.all_day
                .cmp(&a.all_day)
                .then_with(|| a.start.cmp(&b.start))
        });
        Ok(out)
    }

    /// Headline numbers over an inclusive day range.
    pub fn calendar_summary(&self, from: &str, to: &str) -> Result<CalendarSummary> {
        let mut summary = CalendarSummary::default();
        let mut per_cal: BTreeMap<(String, String), u64> = BTreeMap::new();
        for occ in self.calendar_range(from, to)? {
            summary.events += 1;
            if occ.all_day {
                summary.all_day += 1;
            } else {
                summary.hours += occurrence_hours(&occ);
            }
            *per_cal
                .entry((occ.calendar.clone(), occ.account.clone()))
                .or_default() += 1;
        }
        summary.hours = (summary.hours * 10.0).round() / 10.0;
        summary.calendars = per_cal
            .into_iter()
            .map(|((calendar, account), count)| CalendarCount {
                calendar,
                account,
                count,
            })
            .collect();
        summary
            .calendars
            .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.calendar.cmp(&b.calendar)));
        Ok(summary)
    }

    /// Scheduled hours per day (non-all-day events) — the trend series.
    pub fn calendar_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut per_day: BTreeMap<String, f64> = BTreeMap::new();
        for occ in self.calendar_range(from, to)? {
            if !occ.all_day {
                *per_day.entry(occ.day().to_string()).or_default() += occurrence_hours(&occ);
            }
        }
        Ok(per_day
            .into_iter()
            .map(|(date, value)| SeriesPoint {
                date,
                value: (value * 100.0).round() / 100.0,
            })
            .collect())
    }

    pub fn read_calendar_sync(&self) -> Option<CalendarSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_calendar_sync(&self, state: &CalendarSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    // ---- store plumbing ----

    /// Every stored occurrence over an inclusive day range, by start —
    /// shared with the Google collector (coverage scan + mirror loads).
    pub(crate) fn calendar_range(&self, from: &str, to: &str) -> Result<Vec<CalendarOccurrence>> {
        let (from_month, to_month) = (&from[..7.min(from.len())], &to[..7.min(to.len())]);
        let mut out = Vec::new();
        for m in self.calendar_months()? {
            if m.as_str() < from_month || m.as_str() > to_month {
                continue;
            }
            out.extend(
                self.load_calendar_month(&m)?
                    .into_iter()
                    .filter(|o| o.day() >= from && o.day() <= to),
            );
        }
        out.sort_by(|a, b| a.start.cmp(&b.start));
        Ok(out)
    }

    /// Month stems present in the events store, ascending.
    fn calendar_months(&self) -> Result<Vec<String>> {
        let dir = self.root().join(EVENTS_DIR);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        out.sort();
        Ok(out)
    }

    fn load_calendar_month(&self, month: &str) -> Result<Vec<CalendarOccurrence>> {
        let path = self.resolve(&format!("{EVENTS_DIR}/{month}.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)
            .with_context(|| format!("reading {EVENTS_DIR}/{month}.jsonl"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<CalendarOccurrence>(l).ok())
            .collect())
    }

    /// Atomic month rewrite (temp + rename) so readers never see a torn file.
    fn write_calendar_month(&self, month: &str, rows: &[CalendarOccurrence]) -> Result<()> {
        self.write_snapshot(&format!("{EVENTS_DIR}/{month}.jsonl"), rows)
    }

    /// Append change events to their observation month's log.
    fn append_calendar_changes(&self, events: &[CalendarChange]) -> Result<()> {
        self.stream("calendar/changes", crate::store::Partition::Month).append(events, |e| &e.ts)
    }

    /// Change events over an inclusive day range, chronological.
    pub fn calendar_changes(&self, from: &str, to: &str) -> Result<Vec<CalendarChange>> {
        let (from_month, to_month) = (&from[..7.min(from.len())], &to[..7.min(to.len())]);
        let dir = self.root().join("calendar/changes");
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(out);
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(month) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if month < from_month || month > to_month {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            out.extend(
                body.lines()
                    .filter_map(|l| serde_json::from_str::<CalendarChange>(l).ok())
                    .filter(|e| {
                        let day = &e.ts[..10.min(e.ts.len())];
                        day >= from && day <= to
                    }),
            );
        }
        out.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(out)
    }
}

/// Duration of a non-all-day occurrence in hours (0 when unparseable).
fn occurrence_hours(o: &CalendarOccurrence) -> f64 {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (parse(&o.start), parse(&o.end)) {
        (Some(s), Some(e)) => ((e - s).num_seconds().max(0) as f64) / 3600.0,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-calendar-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn occ(id: &str, start: &str, end: &str, title: &str) -> CalendarOccurrence {
        CalendarOccurrence {
            id: id.into(),
            occurrence: start.into(),
            start: start.into(),
            end: end.into(),
            all_day: false,
            title: title.into(),
            calendar: "Work".into(),
            account: "iCloud".into(),
            location: String::new(),
            notes: String::new(),
            attendees: Vec::new(),
            status: "confirmed".into(),
            recurring: false,
        }
    }

    const TS: &str = "2026-06-11T12:00:00-07:00";

    #[test]
    fn diff_detects_added_changed_removed() {
        let kept = occ("A", "2026-06-10T09:00:00-07:00", "2026-06-10T10:00:00-07:00", "Standup");
        let mut moved = kept.clone();
        moved.start = "2026-06-10T11:00:00-07:00".into();
        moved.end = "2026-06-10T12:00:00-07:00".into();
        let old = vec![kept.clone(), occ("B", "2026-06-12T09:00:00-07:00", "2026-06-12T10:00:00-07:00", "Cancelled thing")];
        let new = vec![moved, occ("C", "2026-06-13T09:00:00-07:00", "2026-06-13T10:00:00-07:00", "New thing")];

        let events = diff_occurrences(&old, &new, TS);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, "changed");
        assert_eq!(events[0].id, "A");
        assert_eq!(
            events[0].changes.iter().map(|c| c.field.as_str()).collect::<Vec<_>>(),
            vec!["start", "end"]
        );
        assert_eq!(events[1].kind, "added");
        assert_eq!(events[1].id, "C");
        assert_eq!(events[2].kind, "removed");
        assert_eq!(events[2].id, "B");
    }

    #[test]
    fn recurring_occurrences_diff_independently() {
        // Same event id, two occurrence slots: rescheduling one instance
        // must not touch the other.
        let a1 = occ("R", "2026-06-10T09:00:00-07:00", "2026-06-10T09:30:00-07:00", "Daily");
        let mut a2 = occ("R", "2026-06-11T09:00:00-07:00", "2026-06-11T09:30:00-07:00", "Daily");
        a2.occurrence = "2026-06-11T09:00:00-07:00".into();
        let mut a2_moved = a2.clone();
        a2_moved.start = "2026-06-11T14:00:00-07:00".into(); // occurrence stays

        let events = diff_occurrences(&[a1.clone(), a2], &[a1, a2_moved], TS);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "changed");
        assert_eq!(events[0].changes[0].field, "start");
    }

    #[test]
    fn first_snapshot_is_a_silent_baseline() {
        let v = temp_vault("baseline");
        let fresh = vec![
            occ("A", "2026-06-10T09:00:00-07:00", "2026-06-10T10:00:00-07:00", "One"),
            occ("B", "2026-07-01T09:00:00-07:00", "2026-07-01T10:00:00-07:00", "Two"),
        ];
        let stats = v.calendar_snapshot(&fresh, "2026-06-01", "2026-12-31", TS).unwrap();
        assert!(stats.baseline);
        assert_eq!(stats.events, 2);
        assert_eq!(stats.added + stats.removed + stats.changed, 0);
        assert!(v.root().join("calendar/events/2026-06.jsonl").exists());
        assert!(v.root().join("calendar/events/2026-07.jsonl").exists());
        assert!(!v.root().join("calendar/changes").exists(), "no changes on day one");
    }

    #[test]
    fn second_snapshot_diffs_within_window_and_freezes_the_past() {
        let v = temp_vault("window");
        let frozen = occ("OLD", "2026-01-05T09:00:00-08:00", "2026-01-05T10:00:00-08:00", "Ancient");
        let kept = occ("A", "2026-06-10T09:00:00-07:00", "2026-06-10T10:00:00-07:00", "Kept");
        let doomed = occ("B", "2026-06-12T09:00:00-07:00", "2026-06-12T10:00:00-07:00", "Doomed");
        v.calendar_snapshot(
            &[frozen.clone(), kept.clone(), doomed],
            "2026-01-01",
            "2026-12-31",
            "2026-06-01T08:00:00-07:00",
        )
        .unwrap();

        // Second pass: narrower window — the January event is outside it and
        // must neither diff as removed nor be dropped from its month file.
        let mut renamed = kept.clone();
        renamed.title = "Kept (renamed)".into();
        let stats = v
            .calendar_snapshot(&[renamed.clone()], "2026-06-01", "2026-12-31", TS)
            .unwrap();
        assert!(!stats.baseline);
        assert_eq!((stats.added, stats.removed, stats.changed), (0, 1, 1));

        let jan = v.load_calendar_month("2026-01").unwrap();
        assert_eq!(jan, vec![frozen], "frozen month untouched");
        let june = v.load_calendar_month("2026-06").unwrap();
        assert_eq!(june, vec![renamed], "window rows replaced");

        let changes = v.calendar_changes("2026-06-11", "2026-06-11").unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].kind, "changed");
        assert_eq!(changes[0].changes[0].field, "title");
        assert_eq!(changes[1].kind, "removed");
        assert_eq!(changes[1].title, "Doomed");
    }

    /// Byte-parity contract for the three write paths (month snapshot
    /// rewrite, change append, sync-state file): exact bytes, pinned before
    /// the port onto `store` and unchanged by it.
    #[test]
    fn writes_are_byte_identical() {
        let v = temp_vault("parity");
        let a = occ("A", "2026-06-10T09:00:00-07:00", "2026-06-10T10:00:00-07:00", "One");
        let b = occ("B", "2026-06-12T09:00:00-07:00", "2026-06-12T10:00:00-07:00", "Two");
        v.calendar_snapshot(&[a.clone()], "2026-06-01", "2026-06-30", "2026-06-10T08:00:00-07:00")
            .unwrap();
        v.calendar_snapshot(&[a.clone(), b.clone()], "2026-06-01", "2026-06-30", TS).unwrap();
        v.calendar_snapshot(&[a], "2026-06-01", "2026-06-30", "2026-06-12T12:00:00-07:00")
            .unwrap();

        assert_eq!(
            fs::read_to_string(v.root().join("calendar/events/2026-06.jsonl")).unwrap(),
            "{\"id\":\"A\",\"occurrence\":\"2026-06-10T09:00:00-07:00\",\"start\":\"2026-06-10T09:00:00-07:00\",\"end\":\"2026-06-10T10:00:00-07:00\",\"all_day\":false,\"title\":\"One\",\"calendar\":\"Work\",\"account\":\"iCloud\",\"status\":\"confirmed\",\"recurring\":false}\n",
            "month snapshot rewrite"
        );
        let b_fields = "\"id\":\"B\",\"occurrence\":\"2026-06-12T09:00:00-07:00\",\"title\":\"Two\",\"start\":\"2026-06-12T09:00:00-07:00\",\"calendar\":\"Work\"";
        assert_eq!(
            fs::read_to_string(v.root().join("calendar/changes/2026-06.jsonl")).unwrap(),
            format!(
                "{{\"ts\":\"{TS}\",\"kind\":\"added\",{b_fields}}}\n\
                 {{\"ts\":\"2026-06-12T12:00:00-07:00\",\"kind\":\"removed\",{b_fields}}}\n"
            ),
            "change append extends across calls"
        );

        v.write_calendar_sync(&CalendarSyncState {
            updated: TS.into(),
            backfill_done: true,
            backfilled_to: "2024-01-01".into(),
            error: None,
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join(".trove/calendar-sync.json")).unwrap(),
            "{\n  \"updated\": \"2026-06-11T12:00:00-07:00\",\n  \"backfill_done\": true,\n  \"backfilled_to\": \"2024-01-01\"\n}",
            "sync state pretty json"
        );
    }

    #[test]
    fn unchanged_snapshot_writes_nothing() {
        let v = temp_vault("nochange");
        let fresh = vec![occ("A", "2026-06-10T09:00:00-07:00", "2026-06-10T10:00:00-07:00", "One")];
        v.calendar_snapshot(&fresh, "2026-06-01", "2026-06-30", "2026-06-10T08:00:00-07:00")
            .unwrap();
        let path = v.root().join("calendar/events/2026-06.jsonl");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let stats = v.calendar_snapshot(&fresh, "2026-06-01", "2026-06-30", TS).unwrap();
        assert_eq!(stats.added + stats.removed + stats.changed, 0);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            mtime,
            "unchanged month not rewritten"
        );
    }

    #[test]
    fn backfill_merges_without_events_and_existing_rows_win() {
        let v = temp_vault("backfill");
        // Window snapshot first (the real order): June row exists.
        let windowed = occ("A", "2026-06-10T09:00:00-07:00", "2026-06-10T10:00:00-07:00", "Window copy");
        v.calendar_snapshot(&[windowed.clone()], "2026-06-01", "2026-06-30", TS).unwrap();

        // Backfill brings an older month plus a duplicate key for June.
        let mut dup = windowed.clone();
        dup.title = "Backfill copy".into();
        let old = occ("Z", "2024-03-05T09:00:00-07:00", "2024-03-05T10:00:00-07:00", "History");
        let written = v.calendar_backfill(&[old.clone(), dup]).unwrap();
        assert_eq!(written, 1, "only the new month was written");
        assert_eq!(v.load_calendar_month("2024-03").unwrap(), vec![old]);
        assert_eq!(
            v.load_calendar_month("2026-06").unwrap()[0].title,
            "Window copy",
            "existing row wins on key collision"
        );
        assert!(!v.root().join("calendar/changes").exists(), "backfill emits no events");
    }

    #[test]
    fn reads_timeline_summary_daily() {
        let v = temp_vault("reads");
        let mut allday = occ("D", "2026-06-10T00:00:00-07:00", "2026-06-11T00:00:00-07:00", "Trip");
        allday.all_day = true;
        let fresh = vec![
            occ("A", "2026-06-10T09:00:00-07:00", "2026-06-10T10:30:00-07:00", "Meeting"),
            occ("B", "2026-06-10T13:00:00-07:00", "2026-06-10T14:00:00-07:00", "Lunch"),
            allday,
            occ("C", "2026-06-11T09:00:00-07:00", "2026-06-11T11:00:00-07:00", "Workshop"),
        ];
        v.calendar_snapshot(&fresh, "2026-06-01", "2026-06-30", TS).unwrap();

        let day = v.calendar_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3);
        assert_eq!(day[0].title, "Trip", "all-day first");
        assert_eq!(day[1].title, "Meeting");

        let summary = v.calendar_summary("2026-06-10", "2026-06-11").unwrap();
        assert_eq!(summary.events, 4);
        assert_eq!(summary.all_day, 1);
        assert_eq!(summary.hours, 4.5);
        assert_eq!(summary.calendars.len(), 1);
        assert_eq!(summary.calendars[0].count, 4);

        let daily = v.calendar_daily("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0].value, 2.5);
        assert_eq!(daily[1].value, 2.0);
    }

    #[test]
    fn minimal_old_style_line_still_parses() {
        let o: CalendarOccurrence = serde_json::from_str(r#"{"id":"ABC"}"#).unwrap();
        assert_eq!(o.id, "ABC");
        assert!(o.title.is_empty() && !o.all_day && o.attendees.is_empty());
        // Empty optionals are skipped on write, keeping lines lean.
        let body = serde_json::to_string(&o).unwrap();
        assert!(!body.contains("location") && !body.contains("notes") && !body.contains("attendees"));
    }

    #[test]
    fn sync_state_round_trip() {
        let v = temp_vault("state");
        assert!(v.read_calendar_sync().is_none());
        v.write_calendar_sync(&CalendarSyncState {
            updated: TS.into(),
            backfill_done: true,
            backfilled_to: "2010-06-15".into(),
            error: None,
        })
        .unwrap();
        let s = v.read_calendar_sync().unwrap();
        assert!(s.backfill_done);
        assert_eq!(s.backfilled_to, "2010-06-15");
    }
}

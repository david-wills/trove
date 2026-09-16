//! EventKit bridge — the OS half of the Calendar and Reminders collectors.
//!
//! Everything here turns EventKit objects into plain data (`CalendarOccurrence`,
//! [`crate::tasks::Task`]) and is only exercised at runtime by the collector
//! host; the store/diff logic in [`crate::calendar`] is OS-free and tested with
//! injected data, per the `Watcher`/`Scrobbler` pattern. Spike findings
//! (binding quirks, TCC behavior, the 4-year predicate cap) live in
//! `docs/notes/eventkit-spike.md`.
//!
//! TCC: Calendar and Reminders are per-service prompts keyed to the
//! *responsible process*. The prompt only renders when that process carries
//! usage strings (`NSCalendarsFullAccessUsageDescription` /
//! `NSRemindersFullAccessUsageDescription`) — the Tauri app gets them via its
//! Info.plist). There is no manual-grant fallback: the System
//! Settings panes for these services have no "+" button, an app appears there
//! only after requesting. Completion blocks arrive on an internal EventKit
//! queue — no run-loop pumping needed (unlike distributed notifications).

use anyhow::Result;
use chrono::{DateTime, Local};

use crate::calendar::CalendarOccurrence;
use crate::tasks::{ProjectInfo, Task};

/// Authorization for one EventKit entity type, as the collector cares about
/// it. `granted` means full access; `WriteOnly` (events) counts as denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    NotDetermined,
    Denied,
    Granted,
}

impl AuthStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthStatus::NotDetermined => "not-determined",
            AuthStatus::Denied => "denied",
            AuthStatus::Granted => "granted",
        }
    }
}

/// One Reminders pull: the lists, every open reminder (normalized), and the
/// recently-completed map (`item id → RFC3339 completion time`) used to
/// resolve the fates of reminders that disappeared since the last sync.
#[derive(Debug, Default)]
pub struct RemindersFetch {
    pub lists: Vec<ProjectInfo>,
    pub open: Vec<Task>,
    pub completed: std::collections::BTreeMap<String, String>,
}

pub fn events_auth_status() -> AuthStatus {
    imp::auth_status(true)
}

pub fn reminders_auth_status() -> AuthStatus {
    imp::auth_status(false)
}

/// Ask for full access (fires the TCC prompt when the status is
/// not-determined and the process carries a usage string). Waits up to
/// `wait_secs` for the answer — an already-decided request resolves
/// instantly; a live prompt usually outlives the wait, in which case the
/// caller skips this pass and picks the grant up on the next one.
pub fn request_events_access(wait_secs: u64) -> bool {
    imp::request_access(true, wait_secs)
}

pub fn request_reminders_access(wait_secs: u64) -> bool {
    imp::request_access(false, wait_secs)
}

/// Every event occurrence with a start inside `[start, end]`, recurrences
/// expanded. EventKit caps a single predicate at ~4 years — callers chunk.
pub fn fetch_occurrences(
    start: DateTime<Local>,
    end: DateTime<Local>,
) -> Result<Vec<CalendarOccurrence>> {
    imp::fetch_occurrences(start, end)
}

/// All reminder lists, open reminders, and completions since `completed_since`.
pub fn fetch_reminders(completed_since: DateTime<Local>) -> Result<RemindersFetch> {
    imp::fetch_reminders(completed_since)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use block2::RcBlock;
    use chrono::{DateTime, Local};
    use objc2::rc::Retained;
    use objc2::runtime::Bool;
    use objc2_event_kit::{
        EKAuthorizationStatus, EKCalendar, EKEntityType, EKEvent, EKEventStatus, EKEventStore,
        EKReminder,
    };
    use objc2_foundation::{NSArray, NSDate, NSError, NSString};

    use super::{AuthStatus, RemindersFetch};
    use crate::calendar::CalendarOccurrence;
    use crate::tasks::{ProjectInfo, Task};

    fn entity(events: bool) -> EKEntityType {
        if events {
            EKEntityType::Event
        } else {
            EKEntityType::Reminder
        }
    }

    /// The process-wide event store, created once and never released.
    /// Two reasons: Apple documents EKEventStore as expensive to create and
    /// meant to be long-lived, and — observed while validating on this
    /// machine — a process that repeatedly creates and drops stores starts
    /// getting *empty* query results from fresh ones. (Same never-teardown
    /// shape as the music listener's process-wide observer.) EKEventStore
    /// itself is documented thread-safe; only the store crosses threads
    /// here, fetched objects are consumed on the calling thread.
    fn shared_store() -> &'static EKEventStore {
        use std::sync::OnceLock;
        struct StorePtr(*const EKEventStore);
        unsafe impl Send for StorePtr {}
        unsafe impl Sync for StorePtr {}
        static STORE: OnceLock<StorePtr> = OnceLock::new();
        let p = STORE.get_or_init(|| unsafe {
            StorePtr(Retained::into_raw(EKEventStore::new()).cast_const())
        });
        unsafe { &*p.0 }
    }

    pub fn auth_status(events: bool) -> AuthStatus {
        let status = unsafe { EKEventStore::authorizationStatusForEntityType(entity(events)) };
        match status {
            EKAuthorizationStatus::NotDetermined => AuthStatus::NotDetermined,
            EKAuthorizationStatus::FullAccess => AuthStatus::Granted,
            // Restricted / Denied / WriteOnly: full read access is not coming
            // without user action in System Settings.
            _ => AuthStatus::Denied,
        }
    }

    pub fn request_access(events: bool, wait_secs: u64) -> bool {
        let store = shared_store();
        let (tx, rx) = mpsc::channel::<bool>();
        let block = RcBlock::new(move |granted: Bool, _err: *mut NSError| {
            let _ = tx.send(granted.as_bool());
        });
        unsafe {
            if events {
                store.requestFullAccessToEventsWithCompletion(&*block as *const _ as *mut _);
            } else {
                store.requestFullAccessToRemindersWithCompletion(&*block as *const _ as *mut _);
            }
        }
        rx.recv_timeout(Duration::from_secs(wait_secs)).unwrap_or(false)
    }

    fn nsdate(t: DateTime<Local>) -> Retained<NSDate> {
        NSDate::dateWithTimeIntervalSince1970(t.timestamp() as f64)
    }

    fn rfc3339(date: &NSDate) -> String {
        DateTime::from_timestamp_millis((date.timeIntervalSince1970() * 1000.0) as i64)
            .map(|t| t.with_timezone(&Local).to_rfc3339())
            .unwrap_or_default()
    }

    fn opt_string(s: Option<Retained<NSString>>) -> String {
        s.map(|s| s.to_string()).unwrap_or_default()
    }

    /// Calendar title and account (EKSource) title of a calendar item.
    fn calendar_names(cal: Option<Retained<EKCalendar>>) -> (String, String) {
        match cal {
            Some(c) => unsafe {
                let account = c.source().map(|s| s.title().to_string()).unwrap_or_default();
                (c.title().to_string(), account)
            },
            None => (String::new(), String::new()),
        }
    }

    pub fn fetch_occurrences(
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Result<Vec<CalendarOccurrence>> {
        let store = shared_store();
        let events: Retained<NSArray<EKEvent>> = unsafe {
            let pred = store.predicateForEventsWithStartDate_endDate_calendars(
                &nsdate(start),
                &nsdate(end),
                None,
            );
            store.eventsMatchingPredicate(&pred)
        };
        let mut out = Vec::with_capacity(events.len());
        for ev in events.iter() {
            unsafe {
                // No identifier = nothing stable to diff on; skip (rare —
                // typically an event mid-deletion).
                let Some(id) = ev.eventIdentifier() else {
                    continue;
                };
                let start = rfc3339(&ev.startDate());
                let (calendar, account) = calendar_names(ev.calendar());
                let attendees = ev
                    .attendees()
                    .map(|ps| {
                        ps.iter()
                            .map(|p| {
                                p.name()
                                    .map(|n| n.to_string())
                                    .or_else(|| p.URL().absoluteString().map(|u| u.to_string()))
                                    .unwrap_or_default()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(CalendarOccurrence {
                    id: id.to_string(),
                    occurrence: ev
                        .occurrenceDate()
                        .map(|d| rfc3339(&d))
                        .unwrap_or_else(|| start.clone()),
                    start,
                    end: rfc3339(&ev.endDate()),
                    all_day: ev.isAllDay(),
                    title: ev.title().to_string(),
                    calendar,
                    account,
                    location: opt_string(ev.location()),
                    notes: opt_string(ev.notes()),
                    attendees,
                    status: match ev.status() {
                        EKEventStatus::Confirmed => "confirmed".into(),
                        EKEventStatus::Tentative => "tentative".into(),
                        EKEventStatus::Canceled => "canceled".into(),
                        _ => String::new(),
                    },
                    recurring: ev.hasRecurrenceRules() || ev.isDetached(),
                })
            }
        }
        Ok(out)
    }

    /// EventKit reminder priority (0 none, 1 highest … 9 lowest) → the
    /// normalized TickTick scale (0 none, 1 low, 3 medium, 5 high).
    fn priority(ek: usize) -> i64 {
        match ek {
            1..=4 => 5,
            5 => 3,
            6..=9 => 1,
            _ => 0,
        }
    }

    fn fetch_reminder_array(
        store: &EKEventStore,
        pred: &objc2_foundation::NSPredicate,
    ) -> Result<Vec<Retained<EKReminder>>> {
        let (tx, rx) = mpsc::channel::<Vec<Retained<EKReminder>>>();
        let block = RcBlock::new(move |arr: *mut NSArray<EKReminder>| {
            let items = if arr.is_null() {
                Vec::new()
            } else {
                unsafe { (*arr).iter().collect() }
            };
            let _ = tx.send(items);
        });
        let _handle = unsafe { store.fetchRemindersMatchingPredicate_completion(pred, &block) };
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| anyhow!("EventKit reminder fetch did not complete in 30s"))
    }

    pub fn fetch_reminders(completed_since: DateTime<Local>) -> Result<RemindersFetch> {
        let store = shared_store();
        let lists: Vec<ProjectInfo> = unsafe {
            store
                .calendarsForEntityType(EKEntityType::Reminder)
                .iter()
                .map(|c| ProjectInfo {
                    id: c.calendarIdentifier().to_string(),
                    name: c.title().to_string(),
                })
                .collect()
        };

        let open_pred = unsafe {
            store.predicateForIncompleteRemindersWithDueDateStarting_ending_calendars(
                None, None, None,
            )
        };
        let open = fetch_reminder_array(&store, &open_pred)?
            .into_iter()
            .map(|r| unsafe {
                let (project, _account) = calendar_names(r.calendar());
                let mut extra = serde_json::Map::new();
                if let Some(c) = r.calendar() {
                    extra.insert(
                        "list_id".into(),
                        c.calendarIdentifier().to_string().into(),
                    );
                }
                Task {
                    source: "apple-reminders".into(),
                    id: r.calendarItemIdentifier().to_string(),
                    title: r.title().to_string(),
                    project,
                    notes: opt_string(r.notes()),
                    status: "open".into(),
                    priority: priority(r.priority()),
                    due: r
                        .dueDateComponents()
                        .and_then(|c| c.date())
                        .map(|d| rfc3339(&d)),
                    start: r
                        .startDateComponents()
                        .and_then(|c| c.date())
                        .map(|d| rfc3339(&d)),
                    all_day: false,
                    recurrence: r.hasRecurrenceRules().then(|| "recurring".into()),
                    tags: Vec::new(),
                    subtasks: Vec::new(),
                    created: r.creationDate().map(|d| rfc3339(&d)),
                    modified: r.lastModifiedDate().map(|d| rfc3339(&d)),
                    completed: None,
                    extra,
                }
            })
            .collect();

        let done_pred = unsafe {
            store.predicateForCompletedRemindersWithCompletionDateStarting_ending_calendars(
                Some(&nsdate(completed_since)),
                None,
                None,
            )
        };
        let completed = fetch_reminder_array(&store, &done_pred)?
            .into_iter()
            .map(|r| unsafe {
                (
                    r.calendarItemIdentifier().to_string(),
                    r.completionDate().map(|d| rfc3339(&d)).unwrap_or_default(),
                )
            })
            .collect();

        Ok(RemindersFetch {
            lists,
            open,
            completed,
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use anyhow::{bail, Result};
    use chrono::{DateTime, Local};

    use super::{AuthStatus, RemindersFetch};
    use crate::calendar::CalendarOccurrence;

    pub fn auth_status(_events: bool) -> AuthStatus {
        AuthStatus::Denied
    }

    pub fn request_access(_events: bool, _wait_secs: u64) -> bool {
        false
    }

    pub fn fetch_occurrences(
        _start: DateTime<Local>,
        _end: DateTime<Local>,
    ) -> Result<Vec<CalendarOccurrence>> {
        bail!("EventKit is only available on macOS");
    }

    pub fn fetch_reminders(_completed_since: DateTime<Local>) -> Result<RemindersFetch> {
        bail!("EventKit is only available on macOS");
    }
}

//! Call / FaceTime history collector — an M3 ("copy-then-read another app's
//! database") source over `~/Library/Application Support/CallHistoryDB/
//! CallHistory.storedata`, modeled on [`crate::imessage`]. One Core Data
//! table (`ZCALLRECORD`) holds iPhone calls relayed via Continuity plus
//! FaceTime video/audio, with multi-year retention (this machine: back to
//! 2022). Needs **Full Disk Access** (same per-binary grant as Messages);
//! silently skipped while the DB is unreadable.
//!
//! Writes the unified correspondence stream (see [`crate::correspondence`]):
//! `correspondence/calls/YYYY-MM.jsonl`, one call per line as `kind:"call"`
//! with the connected time in `duration_secs` — so calls sit in the same
//! timeline as the messages around them without counting as message volume.
//! Everything is kept, spam-flagged and missed calls included (full fidelity
//! at write time; reads decide what matters), with the spam flag noted in
//! `text` so later views can filter.
//!
//! Incremental sync cursors on **Z_PK** (the Core Data primary key —
//! monotonic for appended rows, the same role ROWID plays in chat.db):
//! iCloud sync can land rows whose date is old, and those still get fresh
//! keys. Every JSONL line carries its `rowid`, so a lost
//! `.trove/calls-sync.json` is rebuilt by scanning the logs.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::browser::import_via_copy;
use crate::correspondence::{months, Message};
use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;

const SYNC_FILE: &str = ".trove/calls-sync.json";

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_calls()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_calls > 0, || {
        format!("imported {} calls", s.new_calls)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(calls_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_calls_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "calls",
        name: "Calls & FaceTime",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Syncs phone, FaceTime video, and FaceTime audio call history every 15 minutes; the first sync backfills the full retained log (often years).",
        domain: "correspondence",
        vault_path: "correspondence/calls/",
        toggleable: true,
        setup: &["Uses the same Full Disk Access grant as Messages and Safari — nothing extra once that's done."],
        caveats: "iPhone calls reach this Mac via iCloud/Continuity sync, so recent calls can lag until the devices talk. Callers are raw numbers unless the system cached a contact name — the planned Contacts source will map them to people. Spam-flagged and missed calls are kept, marked in the text.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(crate::browser::BROWSER_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Seconds between the Unix epoch and the Apple/Core Data epoch
/// (2001-01-01). `ZCALLRECORD.ZDATE` is float seconds since 2001, UTC.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

/// Incremental-sync state, persisted in `.trove/calls-sync.json`.
/// Rebuildable from the JSONL logs (see [`Vault::rebuild_calls_sync`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CallsSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest imported `ZCALLRECORD.Z_PK`.
    pub cursor: i64,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Serialize)]
pub struct CallsSyncStats {
    /// False when the call history DB is unreadable (no Full Disk Access) —
    /// nothing was attempted.
    pub available: bool,
    pub new_calls: u64,
}

/// Call volume for one caller over a range, for the Calls view.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CallerUsage {
    /// The other party's handle (number or FaceTime address).
    pub contact: String,
    /// Display name when any record carried a cached contact name.
    pub contact_name: String,
    pub calls: u64,
    /// Total connected seconds with this caller.
    pub talk_secs: u64,
}

/// Aggregate of a date range for the Calls view.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CallsSummary {
    pub calls: u64,
    pub outgoing: u64,
    /// Incoming and connected.
    pub incoming: u64,
    /// Incoming, never connected — declined and spam-filtered included.
    pub missed: u64,
    /// Total connected seconds across the range.
    pub talk_secs: u64,
    /// Callers by call count, descending.
    pub callers: Vec<CallerUsage>,
}

fn calls_db_path() -> Option<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join("Library/Application Support/CallHistoryDB/CallHistory.storedata"))
}

/// Whether this process can read the call history database. False means
/// Full Disk Access hasn't been granted to this binary. There is no API to
/// prompt for FDA — the UI deep-links to System Settings, same as Messages.
pub fn calls_permission_ok() -> bool {
    calls_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// ZCALLRECORD semantics

/// `ZDATE` (float seconds since 2001, UTC) → local.
fn core_data_time_to_local(secs: f64) -> Option<DateTime<Local>> {
    if secs <= 0.0 || !secs.is_finite() {
        return None;
    }
    let unix = secs + APPLE_EPOCH_OFFSET_S as f64;
    let t = DateTime::from_timestamp(unix.trunc() as i64, (unix.fract() * 1e9) as u32)?;
    Some(t.with_timezone(&Local))
}

/// `ZCALLTYPE` + `ZSERVICE_PROVIDER` → transport label for `service`.
/// Observed values: type 1 with com.apple.Telephony, types 8 (video) and 16
/// (audio) with com.apple.FaceTime. Unknown types fall back to the provider
/// id with its reverse-DNS prefix dropped.
fn call_service(calltype: i64, provider: &str) -> String {
    match calltype {
        1 => "Phone".into(),
        8 => "FaceTime Video".into(),
        16 => "FaceTime Audio".into(),
        _ => provider.strip_prefix("com.apple.").unwrap_or(provider).to_string(),
    }
}

/// Seconds → "1h 5m 23s" / "29m 23s" / "45s".
fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m {s}s"),
    }
}

/// Human summary for `text`. Outgoing rows never set ZANSWERED, so outcome
/// is read off the duration; incoming rows distinguish answered from missed.
/// Spam-flagged calls are imported like everything else, marked here so
/// views can filter without a schema change.
fn call_text(from_me: bool, answered: bool, duration_secs: u64, junk: bool) -> String {
    let base = match (from_me, answered, duration_secs) {
        (true, _, 0) => "Outgoing call, no answer".to_string(),
        (true, _, d) => format!("Outgoing call, {}", fmt_duration(d)),
        (false, true, d) => format!("Incoming call, {}", fmt_duration(d)),
        (false, false, _) => "Missed call".to_string(),
    };
    if junk {
        format!("{base} (flagged spam)")
    } else {
        base
    }
}

/// Text out of a column that some macOS versions store as TEXT and others
/// as BLOB (ZADDRESS is the known offender). Lossy on bad UTF-8, empty on
/// NULL — never an error.
fn column_text(row: &rusqlite::Row, idx: usize) -> String {
    match row.get_ref(idx) {
        Ok(rusqlite::types::ValueRef::Text(t)) | Ok(rusqlite::types::ValueRef::Blob(t)) => {
            String::from_utf8_lossy(t).into_owned()
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// the import

/// Read calls with Z_PK > `cursor` out of a CallHistory.storedata (or a
/// copy) and append them to the vault. Returns (rows imported, new cursor).
/// Split from the copy step so tests can run it on a synthetic DB.
fn import_calls_db(vault: &Vault, db: &Path, cursor: i64) -> Result<(u64, i64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening call history copy {}", db.display()))?;

    let mut stmt = conn.prepare(
        "SELECT Z_PK, COALESCE(ZUNIQUE_ID, ''), COALESCE(ZDATE, 0),
                COALESCE(ZDURATION, 0), COALESCE(ZORIGINATED, 0),
                COALESCE(ZANSWERED, 0), COALESCE(ZCALLTYPE, 0),
                ZADDRESS, COALESCE(ZNAME, ''),
                COALESCE(ZSERVICE_PROVIDER, ''), COALESCE(ZJUNKCONFIDENCE, 0)
         FROM ZCALLRECORD
         WHERE Z_PK > ?1
         ORDER BY Z_PK",
    )?;
    let mut rows = stmt.query([cursor])?;
    let mut calls = Vec::new();
    let mut max = cursor;
    while let Some(row) = rows.next()? {
        let pk: i64 = row.get(0)?;
        max = max.max(pk);
        let Some(local) = core_data_time_to_local(row.get(2)?) else {
            continue;
        };
        let duration_secs = row.get::<_, f64>(3)?.round().max(0.0) as u64;
        let from_me = row.get::<_, i64>(4)? != 0;
        let answered = row.get::<_, i64>(5)? != 0;
        let junk = row.get::<_, i64>(10)? > 0;
        let address = column_text(row, 7);
        let name: String = row.get(8)?;

        let mut m = Message::new("calls", local.to_rfc3339());
        m.kind = "call".into();
        m.guid = row.get(1)?;
        m.rowid = pk;
        m.from_me = from_me;
        m.chat = address.clone();
        m.chat_name = name.clone();
        if !from_me {
            m.sender = address;
            m.sender_name = name;
        }
        m.service = call_service(row.get(6)?, &row.get::<_, String>(9)?);
        m.duration_secs = duration_secs;
        m.text = call_text(from_me, answered, duration_secs, junk);
        calls.push(m);
    }
    vault.append_messages(&calls)?;
    Ok((calls.len() as u64, max))
}

impl Vault {
    /// One incremental sync pass over the call history database. Silently a
    /// no-op (with `available:false`) while the DB is unreadable — the UI
    /// surfaces the permission state; logging every pass would be noise.
    pub fn collect_calls(&self) -> Result<CallsSyncStats> {
        if !calls_permission_ok() {
            return Ok(CallsSyncStats {
                available: false,
                new_calls: 0,
            });
        }
        let db = calls_db_path().expect("permission_ok implies path");
        let mut state = match self.read_calls_sync() {
            Some(s) => s,
            None => self.rebuild_calls_sync(),
        };
        let stem = format!("trove-calls-{}", std::process::id());
        let (n, max) = import_via_copy(&db, &stem, |tmp| {
            import_calls_db(self, tmp, state.cursor)
        })?;
        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_calls_sync(&state)?;
        Ok(CallsSyncStats {
            available: true,
            new_calls: n,
        })
    }

    /// The persisted sync state, if a sync has ever run.
    pub fn read_calls_sync(&self) -> Option<CallsSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_calls_sync(&self, state: &CallsSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Every call of an inclusive local date range, in file order. The shared
    /// filter behind the range reads below.
    fn calls_in_range(&self, from: &str, to: &str) -> Result<Vec<Message>> {
        let mut out = Vec::new();
        for month in months(from, to)? {
            out.extend(
                self.read_correspondence_month("calls", &month)?
                    .into_iter()
                    .filter(|m| m.kind == "call")
                    .filter(|m| {
                        let day = &m.ts[..10.min(m.ts.len())];
                        day >= from && day <= to
                    }),
            );
        }
        Ok(out)
    }

    /// Counts, talk time, and per-caller volume over an inclusive date range.
    /// Outcomes are read off the stored shape, not the text: `from_me` =
    /// outgoing, connected incoming has `duration_secs > 0`, the rest is
    /// missed (declined and spam-filtered included).
    pub fn calls_summary(&self, from: &str, to: &str) -> Result<CallsSummary> {
        let mut summary = CallsSummary {
            calls: 0,
            outgoing: 0,
            incoming: 0,
            missed: 0,
            talk_secs: 0,
            callers: Vec::new(),
        };
        let mut callers: HashMap<String, CallerUsage> = HashMap::new();
        for m in self.calls_in_range(from, to)? {
            summary.calls += 1;
            summary.talk_secs += m.duration_secs;
            if m.from_me {
                summary.outgoing += 1;
            } else if m.duration_secs > 0 {
                summary.incoming += 1;
            } else {
                summary.missed += 1;
            }
            let entry = callers.entry(m.chat.clone()).or_insert_with(|| CallerUsage {
                contact: m.chat.clone(),
                contact_name: String::new(),
                calls: 0,
                talk_secs: 0,
            });
            entry.calls += 1;
            entry.talk_secs += m.duration_secs;
            if entry.contact_name.is_empty() && !m.chat_name.is_empty() {
                entry.contact_name = m.chat_name.clone();
            }
        }
        summary.callers = callers.into_values().collect();
        summary
            .callers
            .sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.contact.cmp(&b.contact)));
        Ok(summary)
    }

    /// Calls per day over an inclusive range — the trend series for the
    /// chart. Days with no calls are omitted.
    pub fn calls_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut per_day: BTreeMap<String, u64> = BTreeMap::new();
        for m in self.calls_in_range(from, to)? {
            if m.ts.len() >= 10 {
                *per_day.entry(m.ts[..10].to_string()).or_default() += 1;
            }
        }
        Ok(per_day
            .into_iter()
            .map(|(date, n)| SeriesPoint {
                date,
                value: n as f64,
            })
            .collect())
    }

    /// The newest `limit` calls of an inclusive range, most recent first —
    /// the call log list for the Calls view.
    pub fn calls_recent(&self, from: &str, to: &str, limit: usize) -> Result<Vec<Message>> {
        let mut calls = self.calls_in_range(from, to)?;
        calls.sort_by(|a, b| b.ts.cmp(&a.ts));
        calls.truncate(limit);
        Ok(calls)
    }

    /// Reconstruct the cursor from the JSONL logs — used when the sync file
    /// is missing so a resync appends only genuinely new calls.
    fn rebuild_calls_sync(&self) -> CallsSyncState {
        let mut cursor = 0i64;
        let dir = self.root().join("correspondence/calls");
        let Ok(entries) = fs::read_dir(&dir) else {
            return CallsSyncState::default();
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            for line in body.lines() {
                if let Ok(m) = serde_json::from_str::<Message>(line) {
                    cursor = cursor.max(m.rowid);
                }
            }
        }
        CallsSyncState {
            updated: String::new(),
            cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-calls-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// `ZDATE` (float seconds since 2001) for a fixed local datetime.
    fn core_data_secs(d: u32, h: u32) -> f64 {
        let t = Local.with_ymd_and_hms(2026, 6, d, h, 0, 0).unwrap();
        (t.timestamp() - APPLE_EPOCH_OFFSET_S) as f64
    }

    fn fake_calls_db(name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = std::env::temp_dir().join(format!(
            "trove-fakecallsdb-{}-{name}.db",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZCALLRECORD (Z_PK INTEGER PRIMARY KEY, ZUNIQUE_ID VARCHAR,
                ZDATE TIMESTAMP, ZDURATION FLOAT, ZORIGINATED INTEGER DEFAULT 0,
                ZANSWERED INTEGER DEFAULT 0, ZCALLTYPE INTEGER DEFAULT 0,
                ZADDRESS VARCHAR, ZNAME VARCHAR, ZSERVICE_PROVIDER VARCHAR,
                ZJUNKCONFIDENCE INTEGER);",
        )
        .unwrap();
        (path, conn)
    }

    #[test]
    fn core_data_time_conversion() {
        // 2026-06-10T00:00:00Z in Core Data seconds.
        let secs = (1_781_049_600 - APPLE_EPOCH_OFFSET_S) as f64;
        assert_eq!(core_data_time_to_local(secs).unwrap().timestamp(), 1_781_049_600);
        // Sub-second precision survives.
        let t = core_data_time_to_local(secs + 0.5).unwrap();
        assert_eq!(t.timestamp_subsec_millis(), 500);
        assert!(core_data_time_to_local(0.0).is_none());
        assert!(core_data_time_to_local(-1.0).is_none());
        assert!(core_data_time_to_local(f64::NAN).is_none());
    }

    #[test]
    fn service_and_text_labels() {
        assert_eq!(call_service(1, "com.apple.Telephony"), "Phone");
        assert_eq!(call_service(8, "com.apple.FaceTime"), "FaceTime Video");
        assert_eq!(call_service(16, "com.apple.FaceTime"), "FaceTime Audio");
        assert_eq!(call_service(99, "com.apple.Telephony"), "Telephony");
        assert_eq!(call_service(99, ""), "");

        assert_eq!(fmt_duration(45), "45s");
        assert_eq!(fmt_duration(1763), "29m 23s");
        assert_eq!(fmt_duration(4675), "1h 17m 55s");

        assert_eq!(call_text(true, false, 1763, false), "Outgoing call, 29m 23s");
        assert_eq!(call_text(true, false, 0, false), "Outgoing call, no answer");
        assert_eq!(call_text(false, true, 394, false), "Incoming call, 6m 34s");
        assert_eq!(call_text(false, false, 0, false), "Missed call");
        assert_eq!(call_text(false, false, 0, true), "Missed call (flagged spam)");
    }

    #[test]
    fn import_maps_rows_and_is_incremental() {
        let v = temp_vault("import");
        let (db, conn) = fake_calls_db("import");
        // Outgoing phone call; incoming answered FaceTime audio with a cached
        // contact name; incoming missed call flagged spam; a dateless row
        // (skipped, cursor still advances).
        conn.execute_batch(&format!(
            "INSERT INTO ZCALLRECORD VALUES
                (1, 'U1', {d1}, 1763.4, 1, 0, 1, '+15551234567', '', 'com.apple.Telephony', 0),
                (2, 'U2', {d2}, 394.0, 0, 1, 16, '+15557654321', 'Jane Doe', 'com.apple.FaceTime', 0),
                (3, 'U3', {d3}, 0.0, 0, 0, 1, '+15559999999', '', 'com.apple.Telephony', 1),
                (4, 'U4', 0, 0.0, 0, 0, 1, '+15550000000', '', 'com.apple.Telephony', 0);",
            d1 = core_data_secs(9, 10),
            d2 = core_data_secs(10, 9),
            d3 = core_data_secs(10, 11),
        ))
        .unwrap();

        let (n, cursor) = import_calls_db(&v, &db, 0).unwrap();
        assert_eq!(n, 3, "dateless row is skipped");
        assert_eq!(cursor, 4, "cursor advances past skipped rows");

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].kind, "call");
        assert_eq!(day[0].source, "calls");
        assert!(!day[0].from_me);
        assert_eq!(day[0].sender, "+15557654321");
        assert_eq!(day[0].sender_name, "Jane Doe");
        assert_eq!(day[0].chat, "+15557654321");
        assert_eq!(day[0].chat_name, "Jane Doe");
        assert_eq!(day[0].service, "FaceTime Audio");
        assert_eq!(day[0].duration_secs, 394);
        assert_eq!(day[0].text, "Incoming call, 6m 34s");
        assert_eq!(day[1].text, "Missed call (flagged spam)");
        assert_eq!(day[1].duration_secs, 0);

        let prev = v.correspondence_timeline("2026-06-09").unwrap();
        assert_eq!(prev.len(), 1);
        assert!(prev[0].from_me);
        assert_eq!(prev[0].sender, "", "vault owner is implicit on outgoing");
        assert_eq!(prev[0].chat, "+15551234567");
        assert_eq!(prev[0].service, "Phone");
        assert_eq!(prev[0].duration_secs, 1763, "duration rounds to whole seconds");
        assert_eq!(prev[0].text, "Outgoing call, 29m 23s");

        // Re-import from the cursor: nothing new, nothing duplicated.
        let (n2, cursor2) = import_calls_db(&v, &db, cursor).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(cursor2, cursor);

        // Cursor survives losing the sync file.
        assert_eq!(v.rebuild_calls_sync().cursor, 3, "rebuild sees max stored rowid");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn blob_address_decodes() {
        // Some macOS versions store ZADDRESS as a BLOB; the import must read
        // it as text either way.
        let v = temp_vault("blob");
        let (db, conn) = fake_calls_db("blob");
        conn.execute(
            "INSERT INTO ZCALLRECORD VALUES
                (1, 'B1', ?1, 60.0, 0, 1, 1, CAST('+15551112222' AS BLOB), '',
                 'com.apple.Telephony', 0)",
            [core_data_secs(10, 12)],
        )
        .unwrap();

        let (n, _) = import_calls_db(&v, &db, 0).unwrap();
        assert_eq!(n, 1);
        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day[0].chat, "+15551112222");
        assert_eq!(day[0].sender, "+15551112222");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn calls_are_stored_but_not_message_volume() {
        // kind:"call" rides the correspondence stream without inflating
        // message counts — same contract as reactions and group events.
        let v = temp_vault("volume");
        let (db, conn) = fake_calls_db("volume");
        conn.execute(
            "INSERT INTO ZCALLRECORD VALUES
                (1, 'V1', ?1, 120.0, 1, 0, 1, '+15553334444', '', 'com.apple.Telephony', 0)",
            [core_data_secs(10, 14)],
        )
        .unwrap();
        import_calls_db(&v, &db, 0).unwrap();

        let s = v.correspondence_summary("2026-06-10", "2026-06-10").unwrap();
        assert_eq!(s.messages, 0);
        assert_eq!(v.correspondence_timeline("2026-06-10").unwrap().len(), 1);

        let _ = fs::remove_file(db);
    }

    #[test]
    fn summary_daily_and_recent_reads() {
        let v = temp_vault("reads");
        let (db, conn) = fake_calls_db("reads");
        // Two callers across two days (one spanning a month boundary back
        // into May), mixed outcomes: outgoing, answered incoming, missed.
        conn.execute_batch(&format!(
            "INSERT INTO ZCALLRECORD VALUES
                (1, 'R1', {may}, 600.0, 1, 0, 1, '+15551111111', 'Alice', 'com.apple.Telephony', 0),
                (2, 'R2', {d1}, 300.0, 1, 0, 1, '+15551111111', 'Alice', 'com.apple.Telephony', 0),
                (3, 'R3', {d2}, 120.0, 0, 1, 8, '+15551111111', '', 'com.apple.FaceTime', 0),
                (4, 'R4', {d3}, 0.0, 0, 0, 1, '+15552222222', '', 'com.apple.Telephony', 0);",
            may = (Local.with_ymd_and_hms(2026, 5, 31, 12, 0, 0).unwrap().timestamp()
                - APPLE_EPOCH_OFFSET_S) as f64,
            d1 = core_data_secs(9, 10),
            d2 = core_data_secs(10, 9),
            d3 = core_data_secs(10, 11),
        ))
        .unwrap();
        import_calls_db(&v, &db, 0).unwrap();

        // June-only range excludes the May call.
        let s = v.calls_summary("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(s.calls, 3);
        assert_eq!(s.outgoing, 1);
        assert_eq!(s.incoming, 1);
        assert_eq!(s.missed, 1);
        assert_eq!(s.talk_secs, 420);
        assert_eq!(s.callers.len(), 2);
        assert_eq!(s.callers[0].contact, "+15551111111");
        assert_eq!(s.callers[0].contact_name, "Alice", "name backfills from any record");
        assert_eq!(s.callers[0].calls, 2);
        assert_eq!(s.callers[0].talk_secs, 420);

        // Range spanning the month boundary picks up the May file too.
        let wide = v.calls_summary("2026-05-01", "2026-06-30").unwrap();
        assert_eq!(wide.calls, 4);
        assert_eq!(wide.talk_secs, 1020);

        let daily = v.calls_daily("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0].date, "2026-06-09");
        assert_eq!(daily[0].value, 1.0);
        assert_eq!(daily[1].value, 2.0);

        let recent = v.calls_recent("2026-06-01", "2026-06-30", 2).unwrap();
        assert_eq!(recent.len(), 2, "limit applies");
        assert_eq!(recent[0].chat, "+15552222222", "most recent first");
        assert_eq!(recent[1].service, "FaceTime Video");

        let _ = fs::remove_file(db);
    }

    /// Runs the real import against this machine's CallHistory.storedata
    /// into a temp vault — verifies the production schema still matches.
    /// Needs Full Disk Access. Run with:
    /// cargo test -p trove-core calls -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_db_smoke() {
        if !calls_permission_ok() {
            eprintln!("skipped: no Full Disk Access in this process");
            return;
        }
        let v = temp_vault("real");
        let stats = v.collect_calls().unwrap();
        assert!(stats.available);
        eprintln!("imported {} calls from the real database", stats.new_calls);
        assert!(stats.new_calls > 0, "a used Mac should have call history");
        // Second pass is incremental: nothing new, nothing duplicated.
        let again = v.collect_calls().unwrap();
        assert_eq!(again.new_calls, 0);
    }

    #[test]
    fn sync_state_round_trip() {
        let v = temp_vault("state");
        assert!(v.read_calls_sync().is_none());
        let state = CallsSyncState {
            updated: "2026-06-11T10:00:00-07:00".into(),
            cursor: 3082,
        };
        v.write_calls_sync(&state).unwrap();
        let read = v.read_calls_sync().unwrap();
        assert_eq!(read.cursor, 3082);
        assert_eq!(read.updated, "2026-06-11T10:00:00-07:00");
    }
}

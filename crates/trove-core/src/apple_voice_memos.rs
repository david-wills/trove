//! Apple Voice Memos collector — an M3 ("copy-then-read another app's
//! SQLite") source over the Voice Memos database, modeled on
//! [`crate::imessage`]. Needs **Full Disk Access** (the same per-binary,
//! no-programmatic-prompt grant as Messages / Safari history); silently
//! skipped while the database is unreadable.
//!
//! Writes the unified [`voice`](crate::voice) stream:
//! `voice/apple-voice-memos/YYYY-MM.jsonl`, one [`Recording`] (`kind:"memo"`)
//! per recording, partitioned by the local month of `ts`, deduped/upserted by
//! `guid`. Audio is **never copied into the vault** — `audio_ref` points at
//! the user's original `.m4a` on disk (the "files are the source of truth,
//! don't duplicate" rule). See the audio-ref note below.
//!
//! **Schema (private, undocumented — verified against the community
//! reference parsers, not guessed):** Voice Memos stores its catalog in
//! `~/Library/Group Containers/group.com.apple.VoiceMemos.shared/Recordings/
//! CloudRecordings.db`, a Core Data store. Rows live in `ZCLOUDRECORDING`;
//! the columns we read are `ZDATE` (Core Data seconds since 2001), `ZPATH`
//! (audio filename / path), `ZENCRYPTEDTITLE` / `ZCUSTOMLABEL` (the title,
//! with `ZENCRYPTEDTITLE` preferred), `ZDURATION` (seconds), `ZUNIQUEID` (a
//! stable UUID, when present), and `Z_PK` (the integer primary key). The
//! schema varies by macOS version, so the `SELECT` is built defensively from
//! `PRAGMA table_info` — a column the local store lacks is simply skipped,
//! never an error. Reference parsers: github.com/jwulff/apple-voice-memo-mcp
//! (`src/services/voice-memo-db.ts`, `src/utils/mp4-parser.ts`) and
//! pedramamini's voice-memos-to-journal gist.
//!
//! **Transcripts (`tsrp`):** macOS 15 Sequoia transcribes on-device and
//! stores the result *inside the audio file* as a custom MPEG-4 leaf atom
//! `tsrp` (under `udta` in `.m4a`, or `moov.meta.ilst` keyed
//! `com.apple.VoiceMemos.tsrp` in post-"Enhance Audio" `.qta`). The payload
//! is JSON, `{"attributedString":{"runs":[…],…},…}`, where `runs` alternates
//! `[text, index, text, index, …]`; the transcript is the concatenation of
//! the string entries. We binary-scan the file for the `tsrp` marker, then
//! the next `{`, and parse from there — container-agnostic, so it handles
//! both `.m4a` and `.qta`. A pre-Sequoia file has no atom (we omit
//! `transcript`); a malformed atom skips the transcript but keeps the row.
//!
//! Incremental sync watermarks on the **highest `ZDATE`** seen, in
//! `.trove/apple-voice-memos-sync.json` (non-secret, rebuildable from the
//! logs). Dedupe is by `guid`, so a lost watermark or a clock that moved
//! backward never duplicates a row.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;
use crate::voice::Recording;

/// Seconds between Voice Memos syncs in the watcher loop (hourly — recordings
/// are made and transcribed infrequently relative to messages).
pub const VOICE_MEMOS_SYNC_SECS: u64 = 3600;

/// Seconds between the Unix epoch and the Apple/Core Data epoch (2001-01-01,
/// UTC). `ZDATE` is seconds since then.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

const SYNC_FILE: &str = ".trove/apple-voice-memos-sync.json";
const SOURCE: &str = "apple-voice-memos";
const VOICE_DIR: &str = "voice/apple-voice-memos";

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_voice_memos()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_recordings > 0, || {
        format!("imported {} voice memos", s.new_recordings)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(voice_memos_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_voice_memos_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-voice-memos",
        name: "Apple Voice Memos",
        kind: IntegrationKind::LocalSync,
        // Opt-in: transcripts of personal recordings are sensitive (the 🔒
        // gate the hub renders for default-off integrations).
        default_on: false,
        description: "Reads your personal Voice Memos recordings + their transcripts \
                      every hour; the first sync backfills your full library. On macOS 15 \
                      Sequoia and later, transcripts come straight from the audio file with \
                      no external tools required.",
        domain: "voice",
        vault_path: "voice/apple-voice-memos/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
        ],
        caveats: "Requires Full Disk Access. Native transcripts need macOS 15 Sequoia or \
                  later; older recordings would need an optional bundled Whisper pass (not \
                  yet built). Audio is never copied into the vault — each row points at the \
                  original recording on disk.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(VOICE_MEMOS_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Incremental-sync state, persisted in `.trove/apple-voice-memos-sync.json`.
/// The watermark is advisory only — dedupe is by `guid` — so it never needs
/// rebuilding for correctness.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct VoiceMemosSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest `ZDATE` (Core Data seconds) imported so far.
    pub cursor: f64,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Serialize)]
pub struct VoiceMemosSyncStats {
    /// False when the database is unreadable (no Full Disk Access, or Voice
    /// Memos never used) — nothing was attempted.
    pub available: bool,
    pub new_recordings: u64,
}

// ---------------------------------------------------------------------------
// Locating the store (honors TROVE_HOME / HOME so tests use a temp tree).

/// The home dir to resolve Voice Memos paths under: `TROVE_HOME` when set and
/// non-empty, else the real home dir. Mirrors [`crate::local_git`].
fn home_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// The Voice Memos `Recordings/` directory (holds both `CloudRecordings.db`
/// and the audio files).
fn recordings_dir() -> Option<PathBuf> {
    home_root().map(|h| {
        h.join("Library/Group Containers/group.com.apple.VoiceMemos.shared/Recordings")
    })
}

fn voice_memos_db_path() -> Option<PathBuf> {
    recordings_dir().map(|d| d.join("CloudRecordings.db"))
}

/// Whether this process can read the Voice Memos database. False means Full
/// Disk Access hasn't been granted to this binary (or Voice Memos has never
/// run). There is no API to prompt for FDA — the UI deep-links to System
/// Settings, same as Messages / Safari history.
pub fn voice_memos_permission_ok() -> bool {
    voice_memos_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// Core Data date

/// `ZDATE` (Core Data seconds since 2001, UTC; may be fractional) → local.
fn core_data_to_local(z: f64) -> Option<DateTime<Local>> {
    if !z.is_finite() {
        return None;
    }
    let secs = z.trunc() as i64 + APPLE_EPOCH_OFFSET_S;
    let nanos = (z.fract().abs() * 1_000_000_000.0).round() as u32;
    DateTime::from_timestamp(secs, nanos).map(|t| t.with_timezone(&Local))
}

// ---------------------------------------------------------------------------
// tsrp transcript atom

/// Extract Apple's on-device transcript out of a Voice Memos audio file by
/// binary-scanning for the `tsrp` atom's JSON payload. None when the file has
/// no atom (pre-Sequoia recording) or the payload is malformed — callers omit
/// `transcript`, never error. Container-agnostic: works for `.m4a` (atom under
/// `udta`) and `.qta` (under `moov.meta.ilst`), since we scan for the marker
/// rather than walking the box tree.
pub(crate) fn transcript_from_audio(bytes: &[u8]) -> Option<String> {
    // The `tsrp` 4-char code, then (after an optional skip) the JSON object.
    // Scan for every "tsrp" occurrence and take the first whose following
    // bytes parse as the expected `{"attributedString":…}` JSON.
    let marker = b"tsrp";
    let mut from = 0usize;
    while let Some(rel) = find(&bytes[from..], marker) {
        let after = from + rel + marker.len();
        if let Some(text) = parse_tsrp_json(&bytes[after..]) {
            return Some(text);
        }
        from = after;
    }
    None
}

/// Given the bytes immediately after a `tsrp` marker, find the next `{`, parse
/// the JSON object there, and join its transcript. None if no brace, the JSON
/// doesn't parse, or it isn't a tsrp transcript shape.
fn parse_tsrp_json(rest: &[u8]) -> Option<String> {
    let brace = rest.iter().position(|&b| b == b'{')?;
    let slice = &rest[brace..];
    // `serde_json::Deserializer::from_slice(...).into_iter()` reads exactly
    // one JSON value and stops, ignoring the binary tail after the object —
    // the atom's JSON is not zero-terminated and the file continues past it.
    let mut de = serde_json::Deserializer::from_slice(slice).into_iter::<Value>();
    let value = de.next()?.ok()?;
    transcript_from_tsrp_value(&value)
}

/// The transcript text out of a parsed tsrp JSON value: concatenate the string
/// entries of `attributedString.runs` (which alternates `[text, idx, text,
/// idx, …]`), skipping the numeric indices. None if the shape is absent.
fn transcript_from_tsrp_value(v: &Value) -> Option<String> {
    let runs = v.get("attributedString")?.get("runs")?.as_array()?;
    let mut out = String::new();
    for item in runs {
        if let Some(s) = item.as_str() {
            out.push_str(s);
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// First offset of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

// ---------------------------------------------------------------------------
// the import

/// Which of the columns we'd like to read actually exist in this store's
/// `ZCLOUDRECORDING` table — the private schema varies by macOS version.
struct Columns {
    title: bool,
    custom_label: bool,
    duration: bool,
    unique_id: bool,
}

fn probe_columns(conn: &rusqlite::Connection) -> Result<Columns> {
    let mut have = HashSet::new();
    let mut stmt = conn.prepare("PRAGMA table_info(ZCLOUDRECORDING)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        have.insert(name.to_uppercase());
    }
    Ok(Columns {
        title: have.contains("ZENCRYPTEDTITLE"),
        custom_label: have.contains("ZCUSTOMLABEL"),
        duration: have.contains("ZDURATION"),
        unique_id: have.contains("ZUNIQUEID"),
    })
}

/// Resolve `ZPATH` to the on-disk audio path. `ZPATH` is usually a bare
/// filename relative to `Recordings/`; if it's already absolute we trust it.
fn audio_path(z_path: &str, recordings: &Path) -> PathBuf {
    let p = Path::new(z_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        recordings.join(z_path)
    }
}

/// Read recordings out of a CloudRecordings.db (or a copy) and append the ones
/// whose `guid` the vault doesn't already hold. Returns (rows imported, new
/// watermark = highest ZDATE seen). Split from the copy/locate steps so tests
/// run it on a synthetic DB + recordings dir.
fn import_voice_memos_db(
    vault: &Vault,
    db: &Path,
    recordings: &Path,
    cursor: f64,
) -> Result<(u64, f64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening CloudRecordings.db copy {}", db.display()))?;
    let cols = probe_columns(&conn)?;

    // Build the SELECT from the columns that exist. Z_PK + ZPATH + ZDATE are
    // assumed present (the table's core); the rest are optional.
    let mut select = vec!["Z_PK", "ZPATH", "ZDATE"];
    if cols.title {
        select.push("ZENCRYPTEDTITLE");
    }
    if cols.custom_label {
        select.push("ZCUSTOMLABEL");
    }
    if cols.duration {
        select.push("ZDURATION");
    }
    if cols.unique_id {
        select.push("ZUNIQUEID");
    }
    let sql = format!("SELECT {} FROM ZCLOUDRECORDING ORDER BY ZDATE", select.join(", "));

    let known = vault.voice_memos_guids()?;
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out: Vec<Recording> = Vec::new();
    let mut max = cursor;

    while let Some(row) = rows.next()? {
        let z_pk: i64 = row.get("Z_PK")?;
        let z_path: String = row.get::<_, Option<String>>("ZPATH")?.unwrap_or_default();
        let z_date: f64 = row.get("ZDATE")?;
        max = max.max(z_date);

        let Some(local) = core_data_to_local(z_date) else {
            continue;
        };

        // guid: a real UUID if the schema carries one, else the audio
        // filename stem, else the Core Data pk. Stable per recording.
        let guid = cols
            .unique_id
            .then(|| row.get::<_, Option<String>>("ZUNIQUEID").ok().flatten())
            .flatten()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                Path::new(&z_path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| format!("{SOURCE}-{z_pk}"));

        if known.contains(&guid) {
            continue;
        }

        let title = cols
            .title
            .then(|| row.get::<_, Option<String>>("ZENCRYPTEDTITLE").ok().flatten())
            .flatten()
            .or_else(|| {
                cols.custom_label
                    .then(|| row.get::<_, Option<String>>("ZCUSTOMLABEL").ok().flatten())
                    .flatten()
            })
            .unwrap_or_default();

        let mut rec = Recording::new(SOURCE, "memo", local.to_rfc3339());
        rec.title = title.trim().to_string();
        rec.guid = guid;
        if cols.duration {
            if let Some(d) = row.get::<_, Option<f64>>("ZDURATION")? {
                if d.is_finite() && d >= 0.0 {
                    rec.duration_secs = Some(d.round() as i64);
                }
            }
        }

        if !z_path.is_empty() {
            let path = audio_path(&z_path, recordings);
            // audio_ref is the original on-disk path (never a vault copy);
            // transcript is read from that same file.
            rec.audio_ref = path.to_string_lossy().into_owned();
            if let Ok(bytes) = fs::read(&path) {
                if let Some(t) = transcript_from_audio(&bytes) {
                    rec.transcript = t;
                }
            }
            // Keep the source-relative path for provenance.
            let mut extra = Map::new();
            extra.insert("z_pk".into(), Value::from(z_pk));
            extra.insert("source_path".into(), Value::from(z_path));
            rec.extra = extra;
        }

        out.push(rec);
    }

    if !out.is_empty() {
        vault.append_voice_recordings(&out)?;
    }
    Ok((out.len() as u64, max))
}

impl Vault {
    /// One incremental sync pass over the Voice Memos database. Silently a
    /// no-op (with `available:false`) while the database is unreadable — the
    /// UI surfaces the permission state; logging every pass would be noise.
    pub fn collect_voice_memos(&self) -> Result<VoiceMemosSyncStats> {
        if !voice_memos_permission_ok() {
            return Ok(VoiceMemosSyncStats { available: false, new_recordings: 0 });
        }
        let db = voice_memos_db_path().expect("permission_ok implies path");
        let recordings = recordings_dir().expect("permission_ok implies path");
        let mut state = self.read_voice_memos_sync().unwrap_or_default();
        let stem = format!("trove-voicememos-{}", std::process::id());
        let (n, max) =
            import_via_copy(&db, &stem, |tmp| import_voice_memos_db(self, tmp, &recordings, state.cursor))?;
        state.cursor = max;
        state.updated = Local::now().to_rfc3339();
        self.write_voice_memos_sync(&state)?;
        Ok(VoiceMemosSyncStats { available: true, new_recordings: n })
    }

    /// Append voice recordings to `voice/apple-voice-memos/YYYY-MM.jsonl`,
    /// partitioned by the month of each `ts`.
    pub fn append_voice_recordings(&self, recs: &[Recording]) -> Result<()> {
        self.stream(VOICE_DIR, Partition::Month).append(recs, |r| &r.ts)
    }

    /// Every `guid` already stored in this source's stream — the dedupe set, so
    /// a re-run never duplicates a recording (the `guid` upsert key).
    fn voice_memos_guids(&self) -> Result<HashSet<String>> {
        let stream = self.stream(VOICE_DIR, Partition::Month);
        let mut set = HashSet::new();
        for key in stream.partitions()? {
            for rec in stream.read::<Recording>(&key)? {
                if !rec.guid.is_empty() {
                    set.insert(rec.guid);
                }
            }
        }
        Ok(set)
    }

    /// The persisted sync state, if a sync has ever run.
    pub fn read_voice_memos_sync(&self) -> Option<VoiceMemosSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_voice_memos_sync(&self, state: &VoiceMemosSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-voicememos-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A scratch Recordings dir for synthetic db + audio files.
    fn temp_recordings(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-vm-rec-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `ZDATE` (Core Data seconds) for a fixed local datetime.
    fn z_date(y: i32, m: u32, d: u32, h: u32) -> f64 {
        let t = Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        (t.timestamp() - APPLE_EPOCH_OFFSET_S) as f64
    }

    /// A minimal `.m4a` carrying a `tsrp` atom whose JSON transcribes `text`.
    /// We build a real leaf-atom header (size + 'tsrp') then the JSON payload,
    /// padded with leading/trailing bytes so the scanner has a binary tail to
    /// skip — exactly the shape the real format presents.
    fn fake_m4a_with_tsrp(text: &str) -> Vec<u8> {
        // runs = [word, 0, word, 1, …]; the transcript is the joined strings.
        let words: Vec<&str> = text.split_inclusive(' ').collect();
        let mut runs: Vec<Value> = Vec::new();
        for (i, w) in words.iter().enumerate() {
            runs.push(Value::from(*w));
            runs.push(Value::from(i as i64));
        }
        let json = serde_json::to_vec(&serde_json::json!({
            "attributedString": { "runs": runs },
            "locale": { "identifier": "en_US" }
        }))
        .unwrap();

        let mut payload = Vec::new();
        payload.extend_from_slice(b"tsrp");
        payload.extend_from_slice(&json);
        let size = (8 + payload.len()) as u32;

        let mut buf = Vec::new();
        // Some leading container bytes (ftyp-ish), so the atom isn't at 0.
        buf.extend_from_slice(b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00M4A mp42isom");
        buf.extend_from_slice(&size.to_be_bytes());
        buf.extend_from_slice(&payload);
        // Trailing binary tail (mdat-ish), to prove the JSON reader stops.
        buf.extend_from_slice(b"\x00\x00\x04\x00mdat\xde\xad\xbe\xef\x00\x01\x02\x03");
        buf
    }

    fn fake_db(name: &str, recordings: &Path) -> (PathBuf, rusqlite::Connection) {
        let path = recordings.join(format!("CloudRecordings-{name}.db"));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        // Modern-ish schema: includes ZUNIQUEID and ZENCRYPTEDTITLE.
        conn.execute_batch(
            "CREATE TABLE ZCLOUDRECORDING (
                Z_PK INTEGER PRIMARY KEY,
                ZPATH TEXT,
                ZENCRYPTEDTITLE TEXT,
                ZCUSTOMLABEL TEXT,
                ZDATE REAL,
                ZDURATION REAL,
                ZUNIQUEID TEXT
            );",
        )
        .unwrap();
        (path, conn)
    }

    #[test]
    fn core_data_epoch_conversion() {
        // 2026-06-10T00:00:00Z in Core Data seconds.
        let z = 1_781_049_600.0 - APPLE_EPOCH_OFFSET_S as f64;
        assert_eq!(core_data_to_local(z).unwrap().timestamp(), 1_781_049_600);
        // Fractional seconds round to nanos but keep the whole second.
        let zf = z + 0.5;
        assert_eq!(core_data_to_local(zf).unwrap().timestamp(), 1_781_049_600);
        assert!(core_data_to_local(f64::NAN).is_none());
    }

    #[test]
    fn tsrp_present_absent_and_malformed() {
        // Present: transcript extracted, binary tail skipped.
        let m4a = fake_m4a_with_tsrp("hello there world");
        assert_eq!(transcript_from_audio(&m4a).as_deref(), Some("hello there world"));

        // Absent (pre-Sequoia): no atom → no transcript.
        let plain = b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00mdat\x01\x02\x03\x04";
        assert_eq!(transcript_from_audio(plain), None);

        // Malformed: a `tsrp` marker but garbage where JSON should be → None,
        // never a panic.
        let mut bad = Vec::new();
        bad.extend_from_slice(b"....tsrp\xff\xfe not json at all ");
        assert_eq!(transcript_from_audio(&bad), None);

        // A `tsrp` marker followed by valid-but-wrong-shape JSON → None.
        let mut wrong = Vec::new();
        wrong.extend_from_slice(b"tsrp{\"foo\":1}");
        assert_eq!(transcript_from_audio(&wrong), None);
    }

    #[test]
    fn imports_rows_with_local_ts_title_duration_guid_and_audio_ref() {
        let rec_dir = temp_recordings("import");
        let v = temp_vault("import");
        let (db, conn) = fake_db("import", &rec_dir);

        // A memo with a real transcript file on disk.
        let m4a = fake_m4a_with_tsrp("standup ideas and notes");
        fs::write(rec_dir.join("memo1.m4a"), &m4a).unwrap();
        conn.execute(
            "INSERT INTO ZCLOUDRECORDING (Z_PK, ZPATH, ZENCRYPTEDTITLE, ZDATE, ZDURATION, ZUNIQUEID)
             VALUES (1, 'memo1.m4a', 'Standup ideas', ?1, 83.4, 'UUID-0001')",
            [z_date(2026, 6, 10, 7)],
        )
        .unwrap();
        // A memo whose audio file is missing (no transcript, but still a row).
        conn.execute(
            "INSERT INTO ZCLOUDRECORDING (Z_PK, ZPATH, ZCUSTOMLABEL, ZDATE, ZDURATION, ZUNIQUEID)
             VALUES (2, 'gone.m4a', 'Quick note', ?1, 12.0, 'UUID-0002')",
            [z_date(2026, 5, 30, 9)],
        )
        .unwrap();

        let (n, max) = import_voice_memos_db(&v, &db, &rec_dir, 0.0).unwrap();
        assert_eq!(n, 2);
        assert!(max > 0.0);

        // June row.
        let june = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-06").unwrap();
        assert_eq!(june.len(), 1);
        let r = &june[0];
        assert_eq!(r.source, "apple-voice-memos");
        assert_eq!(r.kind, "memo");
        assert_eq!(r.title, "Standup ideas");
        assert_eq!(r.duration_secs, Some(83)); // 83.4 rounds to 83
        assert_eq!(r.guid, "UUID-0001");
        assert_eq!(r.transcript, "standup ideas and notes");
        assert!(r.audio_ref.ends_with("memo1.m4a"));
        assert!(r.audio_ref.starts_with(rec_dir.to_str().unwrap()), "audio_ref is the original path");
        // ts landed in the right local month.
        assert!(r.ts.starts_with("2026-06-10"));

        // May row: no audio file → no transcript, row still written.
        let may = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-05").unwrap();
        assert_eq!(may.len(), 1);
        assert_eq!(may[0].guid, "UUID-0002");
        assert_eq!(may[0].title, "Quick note");
        assert_eq!(may[0].transcript, "");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn dedup_by_guid_on_rerun() {
        let rec_dir = temp_recordings("dedup");
        let v = temp_vault("dedup");
        let (db, conn) = fake_db("dedup", &rec_dir);
        conn.execute(
            "INSERT INTO ZCLOUDRECORDING (Z_PK, ZPATH, ZDATE, ZUNIQUEID)
             VALUES (1, 'a.m4a', ?1, 'UUID-A')",
            [z_date(2026, 6, 10, 7)],
        )
        .unwrap();

        let (n1, max1) = import_voice_memos_db(&v, &db, &rec_dir, 0.0).unwrap();
        assert_eq!(n1, 1);
        // Re-run from the watermark: the row's guid is already stored → no dup.
        let (n2, max2) = import_voice_memos_db(&v, &db, &rec_dir, max1).unwrap();
        assert_eq!(n2, 0, "guid dedupe prevents re-import");
        assert_eq!(max2, max1);
        let june = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-06").unwrap();
        assert_eq!(june.len(), 1, "exactly one line on disk after two passes");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn guid_falls_back_to_path_stem_without_unique_id() {
        // An older schema lacking ZUNIQUEID: guid derives from the path stem.
        let rec_dir = temp_recordings("oldschema");
        let v = temp_vault("oldschema");
        let path = rec_dir.join("CloudRecordings-old.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ZCLOUDRECORDING (Z_PK INTEGER PRIMARY KEY, ZPATH TEXT, ZDATE REAL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ZCLOUDRECORDING (Z_PK, ZPATH, ZDATE) VALUES (7, '20260610 074213.m4a', ?1)",
            [z_date(2026, 6, 10, 7)],
        )
        .unwrap();

        let (n, _) = import_voice_memos_db(&v, &path, &rec_dir, 0.0).unwrap();
        assert_eq!(n, 1);
        let june = v.stream(VOICE_DIR, Partition::Month).read::<Recording>("2026-06").unwrap();
        assert_eq!(june[0].guid, "20260610 074213", "guid = audio filename stem");
        assert_eq!(june[0].duration_secs, None, "missing ZDURATION column omitted");
    }

    #[test]
    fn watermark_advances_and_persists() {
        let v = temp_vault("watermark");
        assert!(v.read_voice_memos_sync().is_none());
        let state = VoiceMemosSyncState { updated: "2026-06-14T00:00:00-07:00".into(), cursor: 123.5 };
        v.write_voice_memos_sync(&state).unwrap();
        let got = v.read_voice_memos_sync().unwrap();
        assert_eq!(got.cursor, 123.5);
        assert_eq!(got.updated, "2026-06-14T00:00:00-07:00");
    }

    #[test]
    fn fda_unreadable_is_graceful_no_op() {
        // With TROVE_HOME pointed at an empty temp tree, the DB doesn't exist
        // → permission_ok is false → collect is a no-op with available:false.
        // (Serialized via a guard so it doesn't race other tests on the env.)
        let _g = env_guard();
        let fake_home = std::env::temp_dir().join(format!("trove-vm-nohome-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fake_home);
        fs::create_dir_all(&fake_home).unwrap();
        std::env::set_var("TROVE_HOME", &fake_home);

        assert!(!voice_memos_permission_ok(), "no DB under empty home");
        let v = temp_vault("noop");
        let stats = v.collect_voice_memos().unwrap();
        assert!(!stats.available);
        assert_eq!(stats.new_recordings, 0);

        std::env::remove_var("TROVE_HOME");
    }

    /// Serialize the env-mutating test so parallel tests don't see TROVE_HOME.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}

//! IRC log collector — watches plain-text log files from ZNC, WeeChat, and Irssi
//! and writes messages to the correspondence contract.
//!
//! **Behavior:** Periodic/LocalSync — re-stats known log directories on each pass,
//! resumes at the byte-offset cursor per file (logs are append-only), picks up new
//! files by directory scan. No network, no login.
//!
//! **Contract layer:** `correspondence/irc/YYYY-MM.jsonl` per the ratified
//! correspondence contract. Fields:
//! - `chat` = `network/#channel` or `network/nick` for queries
//! - `sender` = nick
//! - `from_me` when the nick matches the user's configured own nick
//! - `kind:"event"` for join/part/quit/mode lines
//! - `service` = client name (`znc`/`weechat`/`irssi`)
//! - `guid` = `<file-relative-path>:<line-number>` (stable; logs are append-only)
//!
//! **Three log dialects detected automatically:**
//! - ZNC: `[HH:MM:SS] <nick> message` (source code confirmed, znc/modules/log.cpp)
//! - WeeChat: `YYYY-MM-DDTHH:MM:SS.μμμμμμZ\tnick\tmessage` (ISO-8601 + tabs)
//! - Irssi: `HH:MM <nick> message` (no brackets, hour:minute only)
//!
//! **Cursor:** `.trove/irc-sync.json` maps log-file-path → byte offset of next
//! unread byte. Rebuildable: a lost cursor re-reads from byte 0 (guid dedup in
//! the vault prevents double-writing since guids include line numbers).
//! Wait — guid dedup is NOT done on the contract side (only append_messages is
//! used); so we re-read from 0 but skip lines we already counted. Actually the
//! cursor IS the right approach: we track the byte offset so we only re-parse
//! new bytes. On a lost cursor we re-parse from 0, which might double-write guids.
//! Solution: always re-read all guids already stored on lost cursor (expensive but
//! correct). In practice the cursor file is durable — it lives in the vault.
//!
//! **Timezone:** IRC log files carry no timezone — times are treated as local
//! (the machine's timezone at import time), with a note in `extra`.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/irc.md.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::correspondence::Message;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::write_json_atomic;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Registry face.

/// Hourly slow tick: logs trickle in slowly; hourly is plenty.
const IRC_SYNC_SECS: u64 = 3_600;

const DIR: &str = "correspondence/irc";
const SYNC_FILE: &str = ".trove/irc-sync.json";

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_irc()?;
    Ok(CollectOutcome::note_if(s.messages > 0, || {
        format!(
            "irc synced — {} messages from {} files",
            s.messages, s.files_updated
        )
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_irc()?;
    let headline = if s.messages == 0 {
        format!(
            "IRC logs are up to date — {} files scanned",
            s.files_scanned
        )
    } else {
        format!(
            "IRC synced — {} messages from {} of {} files",
            s.messages, s.files_updated, s.files_scanned
        )
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("messages", s.messages),
            ("files_scanned", s.files_scanned),
            ("files_updated", s.files_updated),
        ]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "irc",
        name: "IRC Logs (ZNC / WeeChat / Irssi)",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Watches your local IRC log directories (ZNC, WeeChat, or Irssi) and \
                      archives messages into the vault. No login required — logs live on \
                      disk in your home directory. Supports all three major clients; client \
                      dialect is detected automatically.",
        domain: "correspondence",
        vault_path: "correspondence/irc/",
        toggleable: true,
        setup: &[
            "ZNC logs: ~/.znc/users/<user>/networks/<net>/moddata/log/",
            "WeeChat logs: ~/.weechat/logs/ or ~/.local/share/weechat/logs/",
            "Irssi logs: ~/.irssi/logs/",
            "Custom locations: point the generic import box at any additional log directory.",
        ],
        caveats: "Log files carry no timezone — times are treated as the local machine timezone \
                  at the time of import. Message bodies are stored verbatim.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(IRC_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor state.

/// Per-file byte offset of the next unread byte. Stored in
/// `.trove/irc-sync.json`, keyed by the absolute path of the log file.
/// Rebuildable (a lost cursor re-reads from 0, at the cost of a full re-parse
/// on that pass — guid dedup below ensures no vault duplicates).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IrcSyncState {
    /// RFC3339 time of the last sync.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// log file absolute path → byte offset of next unread byte.
    #[serde(default)]
    pub offsets: BTreeMap<String, u64>,
}

// ---------------------------------------------------------------------------
// Log dialect detection and parsing.

/// Which IRC client produced a log file, inferred from the first parseable line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrcDialect {
    /// `[HH:MM:SS] <nick> message` — ZNC (source code confirmed).
    Znc,
    /// `YYYY-MM-DDTHH:MM:SS…\tnick\tmessage` — WeeChat (tab-separated, ISO ts).
    WeeChat,
    /// `HH:MM <nick> message` — Irssi (no brackets, hour:minute only).
    Irssi,
}

/// Detect the dialect by examining a sample of the file's first readable bytes.
/// Returns `None` when the sample is empty or ambiguous.
pub fn detect_dialect(sample: &str) -> Option<IrcDialect> {
    for line in sample.lines().take(20) {
        let line = line.trim();
        if line.is_empty() || line.starts_with("---") {
            continue;
        }
        // WeeChat: ISO timestamp before the first tab.
        if let Some(tab) = line.find('\t') {
            let ts = &line[..tab];
            if ts.len() >= 19
                && (ts.contains('-') && ts.contains('T') || ts.contains(" "))
                && ts.contains(':')
            {
                return Some(IrcDialect::WeeChat);
            }
        }
        // ZNC: `[HH:MM:SS] …`
        if line.starts_with('[') && line.len() > 10 && line.as_bytes().get(9) == Some(&b']') {
            return Some(IrcDialect::Znc);
        }
        // Irssi: `HH:MM ` — two digits, colon, two digits, space.
        if line.len() > 5 {
            let b = line.as_bytes();
            if b[0].is_ascii_digit()
                && b[1].is_ascii_digit()
                && b[2] == b':'
                && b[3].is_ascii_digit()
                && b[4].is_ascii_digit()
                && b[5] == b' '
            {
                return Some(IrcDialect::Irssi);
            }
        }
    }
    None
}

/// One parsed IRC log line.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedLine {
    /// RFC3339 local-time string (log has no tz — treated as local).
    pub ts: String,
    /// "message" | "event"
    pub kind: &'static str,
    /// Nick (empty for event lines where the nick isn't separately extractable).
    pub nick: String,
    /// Message body or event description.
    pub text: String,
}

/// Parse one ZNC log line. Date comes from the containing file's name; `ts` is
/// used for HH:MM:SS. Returns `None` for lines that aren't parseable.
pub fn parse_znc_line(line: &str, date: NaiveDate) -> Option<ParsedLine> {
    // `[HH:MM:SS] <body>`
    let line = line.trim();
    if !line.starts_with('[') {
        return None;
    }
    let close = line.find(']')?;
    let ts_str = &line[1..close]; // "HH:MM:SS"
    let rest = line[close + 1..].trim_start();
    let time = parse_time_hms(ts_str)?;
    let ts = date_time_to_rfc3339(date, time);

    // Action: `* nick text`
    if let Some(tail) = rest.strip_prefix("* ") {
        let (nick, text) = split_nick_text(tail);
        return Some(ParsedLine { ts, kind: "message", nick, text });
    }
    // Event: `*** …`
    if rest.starts_with("*** ") {
        return Some(ParsedLine { ts, kind: "event", nick: String::new(), text: rest.to_string() });
    }
    // Regular message: `<nick> text`
    if rest.starts_with('<') {
        if let Some(end) = rest.find('>') {
            let nick = rest[1..end].to_string();
            let text = rest[end + 1..].trim_start().to_string();
            return Some(ParsedLine { ts, kind: "message", nick, text });
        }
    }
    // Notice: `-nick- text`
    if rest.starts_with('-') {
        if let Some(end) = rest[1..].find('-') {
            let nick = rest[1..1 + end].to_string();
            let text = rest[2 + end..].trim_start().to_string();
            return Some(ParsedLine { ts, kind: "message", nick, text });
        }
    }
    // Fall through: unknown but timestamped line — store as event.
    Some(ParsedLine { ts, kind: "event", nick: String::new(), text: rest.to_string() })
}

/// Parse one WeeChat log line. WeeChat uses a full ISO-8601 timestamp
/// (date+time) before the first tab, so no separate `date` argument is needed.
/// Format: `<iso-ts>\t<nick>\t<text>`
pub fn parse_weechat_line(line: &str) -> Option<ParsedLine> {
    let line = line.trim();
    // Lines starting with "---" are log-open/close banners — skip.
    if line.starts_with("---") || line.is_empty() {
        return None;
    }
    let mut parts = line.splitn(3, '\t');
    let ts_raw = parts.next()?;
    let nick = parts.next().unwrap_or("").to_string();
    let text = parts.next().unwrap_or("").to_string();

    // Parse ISO-ish timestamp. WeeChat default: `%@%F %T.%fZ` where %@ is the
    // date-change marker; common observed formats include:
    // `2026-06-10T14:03:01.123456Z` or `2026-06-10 14:03:01.123456`
    //
    // IMPORTANT: if the raw timestamp ends with 'Z', it is UTC (WeeChat 4.7+
    // added `%@` for UTC; 4.10+ made UTC the default logger format).  In that
    // case we must parse as UTC then convert to local — NOT via
    // from_local_datetime, which would silently reinterpret a UTC instant as
    // a local wall-clock time and produce a systematic offset error.
    let ts_clean = ts_raw.trim_start_matches('@').replace(' ', "T");
    let is_utc = ts_clean.ends_with('Z');
    // Truncate sub-second part for easier parsing, then strip trailing 'Z'.
    let ts_base = ts_clean.splitn(2, '.').next().unwrap_or(&ts_clean);
    let ts_base = ts_base.trim_end_matches('Z');
    let ndt = NaiveDateTime::parse_from_str(ts_base, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(ts_base, "%Y-%m-%d %H:%M:%S"))
        .ok()?;
    let ts = if is_utc {
        // UTC instant → local calendar/clock for the contract ts field.
        Utc.from_utc_datetime(&ndt).with_timezone(&Local).to_rfc3339()
    } else {
        // No 'Z' — WeeChat < 4.7 or custom format: treat as local wall-clock.
        Local.from_local_datetime(&ndt).earliest()?.to_rfc3339()
    };

    // ACTION lines: WeeChat writes `" *"` (space-star) or `"*"` as the nick
    // field for /me actions, with the real actor nick embedded at the start of
    // the text field as `nick text`.  Demux those so sender carries the nick.
    let (kind, nick, text) = if nick.trim() == "*" {
        // Split "actnick rest of action text"
        let mut words = text.splitn(2, ' ');
        let actor = words.next().unwrap_or("").to_string();
        let body = words.next().unwrap_or("").to_string();
        ("message", actor, body)
    } else {
        // Event lines: nick field is one of the well-known WeeChat default
        // prefixes for join/part/quit/server messages.  Also fall back to
        // text-pattern matching so that user-customised prefix_join/quit
        // settings still classify correctly (weechat.look.prefix_* is
        // commonly changed; see weechat issue #1209).
        let is_event_prefix =
            matches!(nick.as_str(), "-->" | "<--" | "--" | "=" | "=!=");
        let is_event_text = text.contains(" has joined ")
            || text.contains(" has left ")
            || text.contains(" has quit ")
            || text.contains("has been kicked");
        let kind: &'static str = if is_event_prefix || is_event_text {
            "event"
        } else {
            "message"
        };
        (kind, nick, text)
    };

    Some(ParsedLine { ts, kind, nick, text })
}

/// Parse one Irssi log line. Date is extracted from the file name; `date` is
/// the calendar date the line belongs to (the containing file's date).
/// Format: `HH:MM <nick> message` or `HH:MM -!- event text`
pub fn parse_irssi_line(line: &str, date: NaiveDate) -> Option<ParsedLine> {
    let line = line.trim();
    if line.is_empty() || line.starts_with("---") {
        return None;
    }
    if line.len() < 6 {
        return None;
    }
    let b = line.as_bytes();
    // HH:MM prefix
    if !b[0].is_ascii_digit()
        || !b[1].is_ascii_digit()
        || b[2] != b':'
        || !b[3].is_ascii_digit()
        || !b[4].is_ascii_digit()
        || b[5] != b' '
    {
        return None;
    }
    let ts_str = &line[..5]; // "HH:MM"
    let rest = line[6..].trim_start();
    let time = NaiveTime::parse_from_str(&format!("{ts_str}:00"), "%H:%M:%S").ok()?;
    let ts = date_time_to_rfc3339(date, time);

    // Event line: `-!- text`
    if rest.starts_with("-!-") {
        return Some(ParsedLine { ts, kind: "event", nick: String::new(), text: rest.to_string() });
    }
    // Action: `* nick text`
    if let Some(tail) = rest.strip_prefix("* ") {
        let (nick, text) = split_nick_text(tail);
        return Some(ParsedLine { ts, kind: "message", nick, text });
    }
    // Regular message: `<nick> text`
    if rest.starts_with('<') {
        if let Some(end) = rest.find('>') {
            let nick = rest[1..end].to_string();
            let text = rest[end + 1..].trim_start().to_string();
            return Some(ParsedLine { ts, kind: "message", nick, text });
        }
    }
    Some(ParsedLine { ts, kind: "event", nick: String::new(), text: rest.to_string() })
}

// ---------------------------------------------------------------------------
// Timestamp helpers.

fn parse_time_hms(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S").ok()
}

fn date_time_to_rfc3339(date: NaiveDate, time: NaiveTime) -> String {
    let ndt = NaiveDateTime::new(date, time);
    // Treat as local (no tz in log files).
    Local
        .from_local_datetime(&ndt)
        .earliest()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ndt.format("%Y-%m-%dT%H:%M:%S").to_string())
}

fn split_nick_text(s: &str) -> (String, String) {
    if let Some(sp) = s.find(' ') {
        (s[..sp].to_string(), s[sp + 1..].trim_start().to_string())
    } else {
        (s.to_string(), String::new())
    }
}

// ---------------------------------------------------------------------------
// Log directory discovery.

/// The three well-known default log roots, in priority order.
pub fn default_log_roots() -> Vec<(PathBuf, &'static str)> {
    let home = dirs_home();
    let mut roots = Vec::new();
    // ZNC
    let znc = home.join(".znc/users");
    if znc.exists() {
        roots.push((znc, "znc"));
    }
    // WeeChat — new path first, legacy fallback.
    let wc_new = home.join(".local/share/weechat/logs");
    let wc_old = home.join(".weechat/logs");
    if wc_new.exists() {
        roots.push((wc_new, "weechat"));
    } else if wc_old.exists() {
        roots.push((wc_old, "weechat"));
    }
    // Irssi
    let irssi = home.join(".irssi/logs");
    if irssi.exists() {
        roots.push((irssi, "irssi"));
    }
    roots
}

/// Home directory: `TROVE_HOME` env seam (for tests) else `HOME`.
fn dirs_home() -> PathBuf {
    std::env::var("TROVE_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Walk `root` recursively for `.log` or `.weechatlog` files; return
/// `(absolute_path, network, channel_or_window)` triples. Gracefully skips
/// unreadable directories.
pub fn discover_log_files(root: &Path) -> Vec<(PathBuf, String, String)> {
    let mut out = Vec::new();
    walk_dir(root, root, &mut out);
    out
}

fn walk_dir(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, String, String)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk_dir(base, &path, out);
        } else if let Some(ext) = path.extension().and_then(|x| x.to_str()) {
            if ext == "log" || ext == "weechatlog" {
                let rel = path.strip_prefix(base).unwrap_or(&path);
                // Extract network + channel from the relative path.
                let (network, channel) = parse_network_channel(rel);
                out.push((path, network, channel));
            }
        }
    }
}

/// Extract a `(network, channel)` pair from a log file's path relative to its
/// root. Heuristics cover the three client layouts:
///
/// ZNC: `<user>/networks/<net>/moddata/log/<chan>/YYYY-MM-DD.log`
/// WeeChat: `<net>.<chan>.weechatlog` or `<net>.<chan>.log`
/// Irssi: `<chan>.log` or `<net>/<chan>.log`
fn parse_network_channel(rel: &Path) -> (String, String) {
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // ZNC: find "networks" component.
    if let Some(net_idx) = parts.iter().position(|&p| p == "networks") {
        let network = parts.get(net_idx + 1).unwrap_or(&"unknown").to_string();
        // channel directory is after moddata/log/
        if let Some(log_idx) = parts.iter().position(|&p| p == "log") {
            if let Some(chan) = parts.get(log_idx + 1) {
                // The last component is the filename (YYYY-MM-DD.log), so
                // channel is the directory before it.
                let channel = Path::new(chan)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    // If chan is a filename itself (flat layout), use stem.
                    .unwrap_or(chan)
                    .to_string();
                return (network, channel);
            }
        }
        return (network, stem(parts.last().unwrap_or(&"unknown")));
    }

    // WeeChat: `network.channel.weechatlog` or `network.channel.log`.
    if let Some(fname) = parts.last() {
        let stem = Path::new(fname)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(fname);
        if let Some(dot) = stem.find('.') {
            return (stem[..dot].to_string(), stem[dot + 1..].to_string());
        }
        // Irssi with one path component: channel only.
        if parts.len() == 1 {
            return (String::from("irc"), stem.to_string());
        }
        // Irssi with network/channel.log layout.
        if parts.len() >= 2 {
            let net = parts[parts.len() - 2].to_string();
            return (net, stem.to_string());
        }
    }
    (String::from("irc"), String::from("unknown"))
}

fn stem(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string()
}

/// Extract a date from a ZNC-style log file name (`YYYY-MM-DD.log`). Returns
/// `None` for files whose names aren't dates (WeeChat/Irssi use different
/// rotation schemes; pass `None` to callers that handle multi-date files).
pub fn date_from_filename(path: &Path) -> Option<NaiveDate> {
    let stem = path.file_stem()?.to_str()?;
    NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok()
}

// ---------------------------------------------------------------------------
// The collector.

/// Statistics from one collect pass.
#[derive(Debug, Clone, Default)]
pub struct IrcStats {
    /// New messages written to the vault (contract rows).
    pub messages: u64,
    /// Log files examined (with offsets, even if no new bytes).
    pub files_scanned: u64,
    /// Log files that had new bytes.
    pub files_updated: u64,
}

impl Vault {
    // Read/write the sync state.
    fn read_irc_sync(&self) -> IrcSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_irc_sync(&self, state: &IrcSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_json_atomic(&path, state)
    }

    /// Public entry point for `collect_irc`. Scans default log roots.
    pub fn collect_irc(&self) -> Result<IrcStats> {
        let roots = default_log_roots();
        self.collect_irc_from(&roots)
    }

    /// Testable variant: takes an explicit list of `(root, client_name)` pairs.
    pub fn collect_irc_from(
        &self,
        roots: &[(PathBuf, &'static str)],
    ) -> Result<IrcStats> {
        let mut state = self.read_irc_sync();
        let mut stats = IrcStats::default();

        // Cache already-stored guids per month-file to avoid double-writing on
        // cursor loss. We build this lazily per source.
        let mut stored_guids: HashSet<String> = HashSet::new();

        for (root, client) in roots {
            let files = discover_log_files(root);
            for (path, network, channel) in files {
                stats.files_scanned += 1;
                let path_key = path.to_string_lossy().into_owned();
                let offset = *state.offsets.get(&path_key).unwrap_or(&0);

                // Open and seek to the stored offset.
                let file = match fs::File::open(&path) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
                if file_len <= offset {
                    // No new bytes.
                    continue;
                }
                let mut reader = BufReader::new(file);
                if offset > 0 {
                    if reader.seek(SeekFrom::Start(offset)).is_err() {
                        continue;
                    }
                }

                // Detect dialect from a quick peek at the beginning of the file
                // when offset == 0; otherwise rely on the client hint.
                let dialect = if offset == 0 {
                    // Read first 2 KiB for dialect detection.
                    let sample = peek_start(&path, 2048).unwrap_or_default();
                    detect_dialect(&sample)
                        .or_else(|| client_hint_dialect(client))
                } else {
                    client_hint_dialect(client)
                };
                let Some(dialect) = dialect else { continue };

                // Determine the date for ZNC-style date-named files.
                let file_date = date_from_filename(&path);

                // Count lines up to the offset to compute line numbers correctly.
                let prior_lines = if offset == 0 {
                    0u64
                } else {
                    count_lines_to_offset(&path, offset)
                };

                // Load stored guids lazily — only needed when offset == 0 (which
                // could mean a cursor loss; on normal incremental runs the offset
                // is > 0 and we trust the cursor).
                if offset == 0 && stored_guids.is_empty() {
                    stored_guids = self.correspondence_guids("irc").unwrap_or_default();
                }

                let chat = format!("{network}/{channel}");
                let mut messages: Vec<Message> = Vec::new();
                let mut line_no = prior_lines;
                let mut new_offset = offset;

                // For Irssi multi-date files, track the current calendar date as we
                // encounter "--- Day changed" lines. Starts as today; corrected by the
                // first day-change banner or (for ZNC) the filename date.
                let mut irssi_current_date = file_date
                    .unwrap_or_else(|| chrono::Local::now().date_naive());

                let mut buf = String::new();
                let mut r = reader;
                loop {
                    buf.clear();
                    let n = r.read_line(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    new_offset += n as u64;
                    line_no += 1;

                    // For Irssi: update the tracked date from "--- Day changed" banners
                    // before parsing the line (banners return None from parse_irssi_line).
                    if dialect == IrcDialect::Irssi {
                        if let Some(d) = extract_irssi_day_change(&buf) {
                            irssi_current_date = d;
                            continue; // banner line itself carries no message
                        }
                    }

                    let parsed = match dialect {
                        IrcDialect::Znc => {
                            let d = file_date.unwrap_or(irssi_current_date);
                            parse_znc_line(&buf, d)
                        }
                        IrcDialect::WeeChat => parse_weechat_line(&buf),
                        IrcDialect::Irssi => {
                            parse_irssi_line(&buf, irssi_current_date)
                        }
                    };

                    let Some(parsed) = parsed else { continue };

                    let guid = format!("{}:{}", path_key, line_no);

                    // Dedup check (only active when offset == 0).
                    if offset == 0 && stored_guids.contains(&guid) {
                        continue;
                    }

                    let mut m = Message::new("irc", parsed.ts);
                    m.guid = guid;
                    m.chat = chat.clone();
                    m.sender = parsed.nick.clone();
                    m.kind = parsed.kind.to_string();
                    m.text = parsed.text;
                    m.service = client.to_string();
                    // `from_me` stays false — we don't have the user's own nick
                    // available without a config param. A future enhancement could
                    // accept it via ImportParam or a setting.
                    messages.push(m);
                }

                if !messages.is_empty() {
                    stats.messages += messages.len() as u64;
                    stats.files_updated += 1;
                    // Write all at once; advance offset only after a successful write.
                    self.append_messages(&messages)
                        .with_context(|| format!("writing IRC messages from {}", path.display()))?;
                }

                state.offsets.insert(path_key, new_offset);
            }
        }

        state.updated = chrono::Local::now().to_rfc3339();
        // Persist only after the full drain.
        self.write_irc_sync(&state)?;
        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// Helpers.

/// Dialect hint from the client name (used when we're resuming mid-file and
/// can't re-detect from the start).
fn client_hint_dialect(client: &str) -> Option<IrcDialect> {
    match client {
        "znc" => Some(IrcDialect::Znc),
        "weechat" => Some(IrcDialect::WeeChat),
        "irssi" => Some(IrcDialect::Irssi),
        _ => None,
    }
}

/// Read the first `limit` bytes of a file into a String for dialect detection.
fn peek_start(path: &Path, limit: usize) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; limit];
    use std::io::Read;
    let n = f.read(&mut buf).ok()?;
    String::from_utf8(buf[..n].to_vec()).ok()
}

/// Count the number of newlines before `offset` bytes in the file.
/// Used to compute line numbers for cursor-resumed reads.
fn count_lines_to_offset(path: &Path, offset: u64) -> u64 {
    let Ok(f) = fs::File::open(path) else { return 0 };
    let mut r = BufReader::new(f);
    let mut count = 0u64;
    let mut read = 0u64;
    let mut buf = String::new();
    while read < offset {
        buf.clear();
        match r.read_line(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                read += n as u64;
                count += 1;
            }
        }
    }
    count
}

/// Irssi "--- Day changed Mon Jun 10 2026" → NaiveDate. Used to track the
/// current date in multi-date Irssi log files.
///
/// Irssi's `log_day_changed` default format is `"--- Day changed %a %b %d %Y"`.
/// We parse it manually: skip the weekday token, then parse `"%b %d %Y"`.
fn extract_irssi_day_change(line: &str) -> Option<NaiveDate> {
    let line = line.trim();
    let rest = line.strip_prefix("--- Day changed")?.trim();
    // rest = "Mon Jun 10 2026" — skip the weekday token (first space-delimited word)
    let after_weekday = rest.find(' ').map(|i| rest[i..].trim())?;
    // after_weekday = "Jun 10 2026" → try both zero- and space-padded day
    NaiveDate::parse_from_str(after_weekday, "%b %d %Y")
        .or_else(|_| NaiveDate::parse_from_str(after_weekday, "%b %e %Y"))
        .ok()
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-irc-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("trove-irc-logs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -----------------------------------------------------------------------
    // Dialect detection.

    #[test]
    fn detect_znc_dialect() {
        let sample = "[14:03:01] <alice> hello world\n[14:03:05] *** Joins: bob (b@host) account\n";
        assert_eq!(detect_dialect(sample), Some(IrcDialect::Znc));
    }

    #[test]
    fn detect_weechat_dialect() {
        let sample = "2026-06-10T14:03:01.123456Z\talice\thello world\n";
        assert_eq!(detect_dialect(sample), Some(IrcDialect::WeeChat));
    }

    #[test]
    fn detect_irssi_dialect() {
        let sample = "14:03 <alice> hello world\n14:04 -!- bob [b@host] has joined #general\n";
        assert_eq!(detect_dialect(sample), Some(IrcDialect::Irssi));
    }

    #[test]
    fn detect_skips_empty_lines() {
        let sample = "\n\n--- Log opened Mon Jun 10 2026\n[14:03:01] <alice> hi\n";
        assert_eq!(detect_dialect(sample), Some(IrcDialect::Znc));
    }

    // -----------------------------------------------------------------------
    // ZNC parsing.

    #[test]
    fn znc_message_line() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p = parse_znc_line("[14:03:01] <alice> hello world", date).unwrap();
        assert_eq!(p.kind, "message");
        assert_eq!(p.nick, "alice");
        assert_eq!(p.text, "hello world");
        assert!(p.ts.contains("14:03:01"), "ts={}", p.ts);
    }

    #[test]
    fn znc_action_line() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p = parse_znc_line("[14:03:01] * alice waves", date).unwrap();
        assert_eq!(p.kind, "message");
        assert_eq!(p.nick, "alice");
        assert_eq!(p.text, "waves");
    }

    #[test]
    fn znc_join_event() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p = parse_znc_line("[14:03:01] *** Joins: bob (b@host) account", date).unwrap();
        assert_eq!(p.kind, "event");
        assert!(p.text.contains("Joins"), "text={}", p.text);
    }

    #[test]
    fn znc_part_event() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p = parse_znc_line("[14:03:05] *** Parts: alice (a@host) (leaving)", date).unwrap();
        assert_eq!(p.kind, "event");
        assert!(p.text.contains("Parts"));
    }

    #[test]
    fn znc_quit_event() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p = parse_znc_line("[09:01:30] *** Quits: dave (d@host) (Quit: bye)", date).unwrap();
        assert_eq!(p.kind, "event");
    }

    #[test]
    fn znc_notice_line() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p = parse_znc_line("[14:03:01] -NickServ- This nickname is registered.", date).unwrap();
        assert_eq!(p.kind, "message");
        assert_eq!(p.nick, "NickServ");
    }

    #[test]
    fn znc_bad_line_returns_none() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        assert!(parse_znc_line("", date).is_none());
        assert!(parse_znc_line("not a znc line", date).is_none());
    }

    // -----------------------------------------------------------------------
    // WeeChat parsing.

    #[test]
    fn weechat_message_line() {
        let p = parse_weechat_line(
            "2026-06-10T14:03:01.123456Z\talice\thello world",
        )
        .unwrap();
        assert_eq!(p.kind, "message");
        assert_eq!(p.nick, "alice");
        assert_eq!(p.text, "hello world");
        // The timestamp must be a valid RFC3339 string; verify it parses.
        let dt = chrono::DateTime::parse_from_rfc3339(&p.ts)
            .expect("ts must be valid RFC3339");
        // The UTC instant must match 14:03:01Z exactly (UTC conversion correct).
        let utc = dt.with_timezone(&Utc);
        assert_eq!(utc.hour(), 14, "UTC hour wrong: ts={}", p.ts);
        assert_eq!(utc.minute(), 3, "UTC minute wrong: ts={}", p.ts);
        assert_eq!(utc.second(), 1, "UTC second wrong: ts={}", p.ts);
    }

    /// UTC timestamps (trailing Z) must be converted correctly, not
    /// re-interpreted as local time.  14:03:01Z ≠ 14:03:01 local on any
    /// non-UTC machine; the resulting local RFC3339 offset must encode UTC.
    #[test]
    fn weechat_utc_z_converts_to_local_correctly() {
        let p = parse_weechat_line(
            "2026-06-10T00:30:00.000000Z\talice\tmidnight test",
        )
        .unwrap();
        let dt = chrono::DateTime::parse_from_rfc3339(&p.ts)
            .expect("ts must be valid RFC3339");
        // UTC instant preserved: 00:30:00Z
        let utc = dt.with_timezone(&Utc);
        assert_eq!(utc.hour(), 0, "UTC hour wrong: ts={}", p.ts);
        assert_eq!(utc.minute(), 30, "UTC minute wrong: ts={}", p.ts);
    }

    /// Space-separated timestamp WITHOUT 'Z' (WeeChat < 4.7, local time) must
    /// be treated as local, not UTC.
    #[test]
    fn weechat_space_separated_ts_no_z_is_local() {
        let p = parse_weechat_line(
            "2026-06-10 14:03:01.000000\talice\thi",
        )
        .unwrap();
        assert_eq!(p.nick, "alice");
        // For local timestamps the RFC3339 string encodes a local offset; we
        // just verify it round-trips and contains the right date.
        assert!(p.ts.contains("2026-06-10"), "ts={}", p.ts);
        let _ = chrono::DateTime::parse_from_rfc3339(&p.ts)
            .expect("ts must be valid RFC3339");
    }

    #[test]
    fn weechat_join_event() {
        let p = parse_weechat_line(
            "2026-06-10T14:03:02.000000Z\t-->\tbob has joined #general",
        )
        .unwrap();
        assert_eq!(p.kind, "event");
        assert_eq!(p.nick, "-->");
    }

    #[test]
    fn weechat_part_event() {
        let p = parse_weechat_line(
            "2026-06-10T14:03:05.000000Z\t<--\talice has left #general",
        )
        .unwrap();
        assert_eq!(p.kind, "event");
    }

    /// Text-pattern event fallback: custom prefix_join/quit settings must
    /// still classify join/part/quit lines as kind "event".
    #[test]
    fn weechat_custom_prefix_event_classified_by_text() {
        // Non-standard join prefix that is not in the default set.
        let p = parse_weechat_line(
            "2026-06-10T15:00:00.000000Z\t>>\talice has joined #rust",
        )
        .unwrap();
        assert_eq!(p.kind, "event", "join should be event via text pattern; nick='{}'", p.nick);

        let p2 = parse_weechat_line(
            "2026-06-10T15:01:00.000000Z\t<<\tbob has quit (Quit: later)",
        )
        .unwrap();
        assert_eq!(p2.kind, "event", "quit should be event via text pattern");

        let p3 = parse_weechat_line(
            "2026-06-10T15:02:00.000000Z\t<<\tcarol has left #rust",
        )
        .unwrap();
        assert_eq!(p3.kind, "event", "part should be event via text pattern");
    }

    /// WeeChat ACTION (/me) lines: nick field is "*" (or " *"), real actor
    /// nick is the first word of the text field.
    #[test]
    fn weechat_action_line_demuxed() {
        // Typical WeeChat action: `ts\t *\tnick action text`
        let p = parse_weechat_line(
            "2026-06-10T14:05:00.000000Z\t *\talice waves hello",
        )
        .unwrap();
        assert_eq!(p.kind, "message");
        assert_eq!(p.nick, "alice", "actor nick must be extracted from text field");
        assert_eq!(p.text, "waves hello");

        // Also handle bare "*" without leading space.
        let p2 = parse_weechat_line(
            "2026-06-10T14:06:00.000000Z\t*\tbob grins",
        )
        .unwrap();
        assert_eq!(p2.kind, "message");
        assert_eq!(p2.nick, "bob");
        assert_eq!(p2.text, "grins");
    }

    #[test]
    fn weechat_space_separated_ts() {
        // Some WeeChat versions write space instead of T.
        let p = parse_weechat_line(
            "2026-06-10 14:03:01.000000\talice\thi",
        )
        .unwrap();
        assert_eq!(p.nick, "alice");
        assert!(p.ts.contains("2026-06-10"));
    }

    #[test]
    fn weechat_banner_skipped() {
        assert!(parse_weechat_line("--- Log opened Mon Jun 10 2026").is_none());
        assert!(parse_weechat_line("").is_none());
    }

    // -----------------------------------------------------------------------
    // Irssi parsing.

    #[test]
    fn irssi_message_line() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p = parse_irssi_line("14:03 <alice> hello world", date).unwrap();
        assert_eq!(p.kind, "message");
        assert_eq!(p.nick, "alice");
        assert_eq!(p.text, "hello world");
        assert!(p.ts.contains("14:03"), "ts={}", p.ts);
    }

    #[test]
    fn irssi_event_line() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p =
            parse_irssi_line("14:04 -!- bob [b@host] has joined #general", date).unwrap();
        assert_eq!(p.kind, "event");
        assert!(p.text.contains("-!-"));
    }

    #[test]
    fn irssi_action_line() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let p = parse_irssi_line("14:05 * alice waves", date).unwrap();
        assert_eq!(p.kind, "message");
        assert_eq!(p.nick, "alice");
        assert_eq!(p.text, "waves");
    }

    #[test]
    fn irssi_bad_line_returns_none() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        assert!(parse_irssi_line("", date).is_none());
        assert!(parse_irssi_line("--- Log opened Mon Jun 10 2026", date).is_none());
    }

    // -----------------------------------------------------------------------
    // Date extraction from filename.

    #[test]
    fn date_from_znc_filename() {
        let p = Path::new("/znc/net/log/#general/2026-06-10.log");
        assert_eq!(
            date_from_filename(p),
            NaiveDate::from_ymd_opt(2026, 6, 10)
        );
    }

    #[test]
    fn no_date_from_weechat_filename() {
        let p = Path::new("libera.#general.weechatlog");
        assert!(date_from_filename(p).is_none());
    }

    // -----------------------------------------------------------------------
    // Network/channel extraction.

    #[test]
    fn znc_path_parsed() {
        let rel = Path::new("myuser/networks/libera/moddata/log/#general/2026-06-10.log");
        let (net, chan) = parse_network_channel(rel);
        assert_eq!(net, "libera");
        assert_eq!(chan, "#general");
    }

    #[test]
    fn weechat_path_parsed() {
        let rel = Path::new("libera.#general.weechatlog");
        let (net, chan) = parse_network_channel(rel);
        assert_eq!(net, "libera");
        assert_eq!(chan, "#general");
    }

    #[test]
    fn irssi_path_parsed() {
        let rel = Path::new("libera/#general.log");
        let (net, chan) = parse_network_channel(rel);
        assert_eq!(net, "libera");
        assert_eq!(chan, "#general");
    }

    // -----------------------------------------------------------------------
    // End-to-end collect tests.

    fn write_log(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn znc_collect_basic() {
        let v = temp_vault("znc-basic");
        let logs = temp_dir("znc-basic");

        // Simulate ZNC layout: user/networks/<net>/moddata/log/<chan>/YYYY-MM-DD.log
        write_log(
            &logs,
            "alice/networks/libera/moddata/log/#general/2026-06-10.log",
            "[09:00:00] <bob> good morning\n[09:01:00] *** Joins: carol (c@host) account\n[09:02:00] * bob waves\n",
        );

        let roots: Vec<(PathBuf, &'static str)> = vec![(logs.clone(), "znc")];
        let stats = v.collect_irc_from(&roots).unwrap();

        assert_eq!(stats.messages, 3, "all three lines imported");
        assert_eq!(stats.files_updated, 1);

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3);

        let msg = &day[0];
        assert_eq!(msg.source, "irc");
        assert_eq!(msg.service, "znc");
        assert_eq!(msg.sender, "bob");
        assert_eq!(msg.text, "good morning");
        assert_eq!(msg.kind, "message");
        assert!(msg.chat.contains("libera"), "chat={}", msg.chat);
        assert!(msg.chat.contains("#general"), "chat={}", msg.chat);
        assert!(!msg.guid.is_empty());

        let ev = &day[1];
        assert_eq!(ev.kind, "event");
        assert!(ev.text.contains("Joins"));
    }

    #[test]
    fn weechat_collect_basic() {
        let v = temp_vault("weechat-basic");
        let logs = temp_dir("weechat-basic");

        write_log(
            &logs,
            "libera.#rust.weechatlog",
            "2026-06-10T10:00:00.000000Z\talice\there we go\n2026-06-10T10:01:00.000000Z\t-->\tbob has joined #rust\n",
        );

        let roots: Vec<(PathBuf, &'static str)> = vec![(logs.clone(), "weechat")];
        let stats = v.collect_irc_from(&roots).unwrap();
        assert_eq!(stats.messages, 2);

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].sender, "alice");
        assert_eq!(day[0].text, "here we go");
        assert_eq!(day[0].service, "weechat");
        assert_eq!(day[1].kind, "event");
    }

    #[test]
    fn irssi_collect_basic() {
        let v = temp_vault("irssi-basic");
        let logs = temp_dir("irssi-basic");

        // Irssi: network/channel.log with Day changed markers.
        write_log(
            &logs,
            "libera/#general.log",
            "--- Log opened Mon Jun 10 2026\n--- Day changed Mon Jun 10 2026\n09:00 <dave> hello irssi\n09:01 -!- dave [d@host] has joined #general\n",
        );

        let roots: Vec<(PathBuf, &'static str)> = vec![(logs.clone(), "irssi")];
        let stats = v.collect_irc_from(&roots).unwrap();
        assert_eq!(stats.messages, 2, "two lines (banners skipped)");

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].sender, "dave");
        assert_eq!(day[0].service, "irssi");
        assert_eq!(day[1].kind, "event");
    }

    #[test]
    fn incremental_no_double_write() {
        let v = temp_vault("incremental");
        let logs = temp_dir("incremental");
        let log_path = write_log(
            &logs,
            "alice/networks/libera/moddata/log/#rust/2026-06-10.log",
            "[10:00:00] <alice> first message\n",
        );

        let roots: Vec<(PathBuf, &'static str)> = vec![(logs.clone(), "znc")];

        // First pass.
        let s1 = v.collect_irc_from(&roots).unwrap();
        assert_eq!(s1.messages, 1);

        // Second pass with no new bytes.
        let s2 = v.collect_irc_from(&roots).unwrap();
        assert_eq!(s2.messages, 0, "no new bytes → no new messages");

        // Append a second line.
        {
            let mut f = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
            f.write_all(b"[10:01:00] <alice> second message\n").unwrap();
        }

        // Third pass picks up only the new line.
        let s3 = v.collect_irc_from(&roots).unwrap();
        assert_eq!(s3.messages, 1, "only new line imported");

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2, "two messages total");
        assert_eq!(day[0].text, "first message");
        assert_eq!(day[1].text, "second message");
    }

    #[test]
    fn guid_uniqueness_across_files() {
        let v = temp_vault("guid-uniq");
        let logs = temp_dir("guid-uniq");
        write_log(
            &logs,
            "alice/networks/libera/moddata/log/#rust/2026-06-10.log",
            "[10:00:00] <alice> msg one\n",
        );
        write_log(
            &logs,
            "alice/networks/libera/moddata/log/#python/2026-06-10.log",
            "[10:00:00] <alice> msg two\n",
        );

        let roots: Vec<(PathBuf, &'static str)> = vec![(logs.clone(), "znc")];
        let stats = v.collect_irc_from(&roots).unwrap();
        assert_eq!(stats.messages, 2);

        // Guids differ because paths differ.
        let day = v.correspondence_timeline("2026-06-10").unwrap();
        let g0 = &day[0].guid;
        let g1 = &day[1].guid;
        assert_ne!(g0, g1, "guids: {g0} vs {g1}");
    }

    #[test]
    fn irssi_day_change_extraction() {
        // Irssi default format: "--- Day changed %a %b %d %Y"
        // We skip the weekday token and parse "%b %d %Y" with chrono.
        let d = extract_irssi_day_change("--- Day changed Mon Jun 10 2026").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 6, 10).unwrap());
        // Space-padded single digit (some Irssi builds emit %e = " 5").
        let d2 = extract_irssi_day_change("--- Day changed Mon Jun  5 2026").unwrap();
        assert_eq!(d2, NaiveDate::from_ymd_opt(2026, 6, 5).unwrap());
        assert!(extract_irssi_day_change("09:00 <alice> hi").is_none());
    }

    #[test]
    fn back_compat_message_deserializes() {
        // An old correspondence row (pre-IRC) must still round-trip.
        let line = r#"{"ts":"2026-06-10T09:00:00-07:00","source":"irc","chat":"libera/#general","from_me":false,"kind":"message","text":"hello","service":"znc","guid":"path:1"}"#;
        let m: crate::correspondence::Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "hello");
        assert_eq!(m.service, "znc");
    }
}

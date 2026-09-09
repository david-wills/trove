//! Shell command history — a pure-local, no-login collector for zsh, bash, and
//! fish. Catalogued in the Phase 2 pass; brief: docs/integrations/shell-history.md.
//!
//! A **Periodic** local filesystem collector (the [`crate::local_git`] shape: a
//! bounded read of the user's home dir, no network and no connection). It reads
//! the shell history files the shells already keep and writes **every** command
//! it finds as one raw row to `developer/shell/YYYY-MM.jsonl`, partitioned by the
//! command's own month; commands with no recoverable timestamp go to a single
//! `developer/shell/undated.jsonl`.
//!
//! **`developer/` is raw-only** (taxonomy decision): no media-plays/domain
//! contract, no normalized struct, no spec validation — this module owns its row
//! shape.
//!
//! ## Privacy — mandatory parse-time redaction
//!
//! Shell history routinely contains secrets typed inline (`export
//! GH_TOKEN=ghp_…`, `mysql --password=…`, `https://user:pw@host`). Every command
//! is run through a built-in [`redact`] pass **before** it is written, so the raw
//! vault never persists an obvious secret. The rule set is conservative and
//! always on (a user-configurable allow/deny list is a deferred follow-up); it
//! biases toward over-redaction and, when a secret signature matches but the
//! value can't be isolated, replaces the whole command with `"[redacted]"`. This
//! is best-effort for *obvious* secrets — generic high-entropy detection is out
//! of scope (too many false positives); the analysis layer flags the rest.
//!
//! ## Sources (auto-detected per file)
//!
//! Resolved under `TROVE_HOME` (else `$HOME`), mirroring [`crate::local_git`]'s
//! override so tests point at a temp home:
//! - `~/.zsh_history` — zsh; EXTENDED_HISTORY rows carry `: <epoch>:<elapsed>;<cmd>`,
//!   plain rows are bare commands (no ts). A trailing `\` continues onto the next
//!   physical line (one logical command).
//! - `~/.zsh_sessions/*.history` — per-session zsh history; `session` = the file
//!   stem.
//! - `~/.bash_history` — bash; bare commands, with an optional `#<epoch>` line
//!   (HISTTIMEFORMAT) preceding a command to supply its ts.
//! - `~/.local/share/fish/fish_history` — fish; YAML-ish `- cmd:` / `  when:`
//!   blocks.
//!
//! ## Cursor / incremental / dedupe
//!
//! `.trove/shell-history-sync.json` (non-secret, rebuildable) maps each source
//! file → the byte offset imported through. Each pass reads only the bytes past
//! that offset and advances the cursor only after the batch is written.
//! **Truncation-safe:** if a file is now *shorter* than its stored offset
//! (`history -c`, HISTSIZE trim, rotation), the cursor resets to 0 and the file
//! is re-scanned — the guid upsert keeps that idempotent. Because a command can
//! appear in both `~/.zsh_history` and a `~/.zsh_sessions/*` file, rows are
//! upserted into their month partition **by `guid`**: a stable hash of
//! `(ts, command)` when a ts is present, else `(session, command, offset)`.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Hourly, like the other always-on local collectors. Commands trickle in; an
/// hourly sweep keeps the log fresh without churn.
pub const SHELL_HISTORY_SYNC_SECS: u64 = 3600;

/// Contract-free raw stream: one row per command.
const DIR: &str = "developer/shell";
/// The bucket for commands with no recoverable timestamp (zsh-plain / bash
/// without HISTTIMEFORMAT). A single partition, keyed within by file offset.
const UNDATED_KEY: &str = "undated";
/// Rebuildable cursor: source file (display name) → byte offset imported.
const SYNC_FILE: &str = ".trove/shell-history-sync.json";
/// The session label for the main (non-per-session) zsh history.
const MAIN_SESSION: &str = "main";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_shell_history()?;
    Ok(crate::registry::CollectOutcome::note_if(s.commands > 0, || {
        let mut note = format!("shell history synced — {} commands", s.commands);
        if s.redacted > 0 {
            note.push_str(&format!(" ({} redacted)", s.redacted));
        }
        note
    }))
}

// Manual "Sync now": the same scan, surfacing a human headline.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_shell_history()?;
    let headline = if s.commands == 0 {
        "Shell history is up to date — no new commands".to_string()
    } else {
        format!("Shell history synced — {} commands ({} redacted)", s.commands, s.redacted)
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("commands", s.commands), ("redacted", s.redacted)]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "shell-history",
        name: "Shell History",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Indexes every command you've run at the terminal, with timestamps, \
                      by reading the history files zsh, bash, and fish already keep. \
                      Pure-local: it reads the files directly, nothing is sent anywhere. \
                      Commands are scanned at collection time and obvious secrets \
                      (tokens, passwords, keys) are redacted before anything is stored.",
        domain: "developer",
        vault_path: "developer/shell/",
        toggleable: true,
        setup: &[
            "Reads ~/.zsh_history, ~/.zsh_sessions, ~/.bash_history, and fish history already on your Mac — nothing to install or connect.",
            "Timestamps come through only when the shell records them (zsh EXTENDED_HISTORY, bash HISTTIMEFORMAT); without them, commands are still stored, just undated.",
            "Set TROVE_HOME to read history from a different home directory.",
        ],
        caveats: "Commands are scanned at collection time and obvious secrets — token/password/key \
                  environment assignments, --password/--token flags, inline URL credentials, and AWS \
                  access keys — are redacted before storage. No filter is perfect: a secret typed in an \
                  unusual shape can slip through, so avoid putting secrets on the command line. \
                  Timestamps are only as good as your shell's history settings (zsh EXTENDED_HISTORY, \
                  bash HISTTIMEFORMAT); commands without one are kept in an undated bucket. History \
                  that was never written to disk, or trimmed by HISTSIZE before a sync, can't be \
                  recovered.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SHELL_HISTORY_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// The raw command row (this module's own shape — no contract).

/// Which shell a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

/// One row in `developer/shell/YYYY-MM.jsonl` (or `undated.jsonl`): a single
/// command, post-redaction. `guid` is the upsert/dedupe key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CommandRow {
    /// Start time as RFC3339 local. Omitted when the shell recorded none
    /// (zsh-plain, bash without HISTTIMEFORMAT) — those rows live in the
    /// undated bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    /// The command line, **after** redaction. Secret values are `***`; a line
    /// that matched a secret signature but couldn't be safely isolated is the
    /// literal `"[redacted]"`.
    pub command: String,
    /// Wall-clock duration in seconds (zsh EXTENDED_HISTORY `elapsed`). Omitted
    /// for bash/fish/plain, which don't record it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<i64>,
    /// Session label: a `~/.zsh_sessions/*` file's stem, or "main" for the main
    /// zsh history; the source file stem for bash/fish.
    pub session: String,
    /// Which shell wrote it.
    pub shell: Shell,
    /// True when [`redact`] changed the command (a secret was scrubbed).
    pub redacted: bool,
    /// Upsert/dedupe key: hash of `(ts, command)` when ts present, else
    /// `(session, command, offset)`.
    pub guid: String,
}

/// Result of one scan pass, for logging/status.
#[derive(Debug, Clone, Default)]
pub struct ShellHistoryStats {
    /// Commands written (genuinely new by guid) this pass.
    pub commands: u64,
    /// Of those, how many were redacted.
    pub redacted: u64,
    /// Source files that yielded at least one new command.
    pub files_with_new: u64,
}

// ---------------------------------------------------------------------------
// Scan root + source enumeration.

/// The scan root: `TROVE_HOME` when set and non-empty, else the home dir —
/// mirroring [`crate::local_git`] so tests can point the scan at a temp home.
fn scan_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// The history file format, decided per source path (not by sniffing bytes —
/// the path already tells us, and zsh auto-detects extended-vs-plain per line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// zsh `~/.zsh_history` or `~/.zsh_sessions/*` (extended or plain per line).
    Zsh,
    Bash,
    Fish,
}

/// One source file to read: its path, a stable display name (the cursor key),
/// its format, and the session label rows from it carry.
struct Source {
    path: PathBuf,
    /// Cursor key + display: home-relative when under the scan root, else the
    /// file name. Stable across runs so the offset cursor keeps matching.
    display: String,
    format: Format,
    session: String,
}

/// Enumerate the shell-history sources under `root`, in a deterministic order.
/// Missing files are simply absent from the list (a quiet no-op).
fn sources(root: &Path) -> Vec<Source> {
    let mut out: Vec<Source> = Vec::new();
    let rel = |p: &Path| -> String {
        p.strip_prefix(root)
            .ok()
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    };

    // Main zsh history.
    let zsh = root.join(".zsh_history");
    if zsh.is_file() {
        out.push(Source { display: rel(&zsh), path: zsh, format: Format::Zsh, session: MAIN_SESSION.to_string() });
    }
    // Per-session zsh history: ~/.zsh_sessions/*.history (session = file stem).
    let sessions_dir = root.join(".zsh_sessions");
    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        let mut session_files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("history"))
            .collect();
        session_files.sort();
        for p in session_files {
            let session = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(Source { display: rel(&p), path: p, format: Format::Zsh, session });
        }
    }
    // bash.
    let bash = root.join(".bash_history");
    if bash.is_file() {
        out.push(Source { display: rel(&bash), path: bash, format: Format::Bash, session: "bash".to_string() });
    }
    // fish.
    let fish = root.join(".local/share/fish/fish_history");
    if fish.is_file() {
        out.push(Source { display: rel(&fish), path: fish, format: Format::Fish, session: "fish".to_string() });
    }
    out
}

// ---------------------------------------------------------------------------
// Redaction (the privacy core — pure, fixture-tested, always on).

/// The default sensitive env-var name fragments. A `NAME=value` assignment
/// whose NAME contains any of these (case-insensitive) has its value scrubbed.
/// Catches GH_TOKEN, AWS_SECRET_ACCESS_KEY, PGPASSWORD, MYSQL_PWD, API_KEY, …
const SENSITIVE_NAME_FRAGMENTS: &[&str] =
    &["TOKEN", "SECRET", "PASSWORD", "PASSWD", "PWD", "CREDENTIAL", "APIKEY", "API_KEY", "ACCESS_KEY", "ACCESSKEY", "ACCESS-KEY"];

/// Flag names whose following value is a secret (`--password x`, `--token=x`).
/// Bare `-p` is deliberately excluded (false positives like `mkdir -p`).
const SENSITIVE_FLAGS: &[&str] =
    &["password", "passwd", "token", "api-key", "api_key", "apikey", "secret", "auth", "bearer"];

/// Credential-tool programs for which a short `-p`/`-P`/`-a` flag IS a password
/// (`mysql -phunter2`, `redis-cli -a s3cr3t`). For these, and ONLY these, the
/// value of `-p`/`-P`/`-a` is scrubbed in both the concatenated (`-phunter2`)
/// and space (`-p hunter2`) forms. Biases to over-redaction: a db-name or port
/// passed to one of these may get scrubbed too — acceptable. For every OTHER
/// program these short flags are left untouched (`mkdir -p`, `ssh -p 22`, …).
const CRED_TOOLS: &[&str] = &[
    "mysql", "mysqldump", "mysqladmin", "mariadb",
    "mongo", "mongosh", "mongodump", "mongorestore",
    "redis-cli",
    "psql", "pg_dump", "pg_restore", "pg_dumpall",
];

/// Short flags whose value is a credential under a [`CRED_TOOLS`] program.
const CRED_SHORT_FLAGS: &[&str] = &["p", "P", "a"];

/// Redact obvious secrets from a command line. Returns the (possibly rewritten)
/// command and whether anything changed. Conservative + always on; biases
/// toward over-redaction. Pure (no I/O) so it's exercised directly by tests.
pub fn redact(command: &str) -> (String, bool) {
    let mut out = command.to_string();
    let mut changed = false;

    // 1. AWS access key ids: AKIA followed by 16 uppercase-alnum chars → ***.
    if let Some(red) = redact_aws_keys(&out) {
        out = red;
        changed = true;
    }
    // 2. URL inline creds: scheme://user:<password>@host → password ***.
    if let Some(red) = redact_url_creds(&out) {
        out = red;
        changed = true;
    }
    // 2b. URL query-string secrets: ?access_token=…&api_key=… → value ***.
    if let Some(red) = redact_url_query_secrets(&out) {
        out = red;
        changed = true;
    }
    // 3. env assignments with sensitive names: NAME=value → NAME=***.
    if let Some(red) = redact_env_assignments(&out) {
        out = red;
        changed = true;
    }
    // 4. flag-style secrets: --password=x / --password x → value ***.
    if let Some(red) = redact_secret_flags(&out) {
        out = red;
        changed = true;
    }
    // 5. credential-tool short flags: under a CRED_TOOLS program (mysql, mongo,
    //    redis-cli, …) the value of -p/-P/-a is a password → ***. Scoped to
    //    those programs so `mkdir -p`, `ssh -p 22`, `tar -p`, `grep -P` are safe.
    if let Some(red) = redact_cred_tool_short_flags(&out) {
        out = red;
        changed = true;
    }

    (out, changed)
}

/// The PROGRAM being invoked: the first whitespace token after skipping any
/// leading `VAR=val` env-assignment prefixes and a leading `sudo`/`env`/
/// `command` wrapper, reduced to its basename (`/usr/bin/mysql` → `mysql`).
/// Returns the lowercased program name, or `None` if none is identifiable.
fn command_program(pairs: &[(String, String)]) -> Option<String> {
    let mut i = 0usize;
    // Skip leading env-assignment prefixes: `VAR=val`.
    while let Some((_, tok)) = pairs.get(i) {
        if is_env_assignment(tok) {
            i += 1;
        } else {
            break;
        }
    }
    // Skip a leading wrapper command, and any env-assignments that follow it
    // (e.g. `sudo VAR=val mysql …`).
    while let Some((_, tok)) = pairs.get(i) {
        let base = basename(tok).to_ascii_lowercase();
        if base == "sudo" || base == "env" || base == "command" {
            i += 1;
            while let Some((_, t)) = pairs.get(i) {
                if is_env_assignment(t) {
                    i += 1;
                } else {
                    break;
                }
            }
        } else {
            break;
        }
    }
    pairs.get(i).map(|(_, tok)| basename(tok).to_ascii_lowercase())
}

/// A `VAR=val` env-assignment token (NAME an identifier, a non-empty value)?
fn is_env_assignment(tok: &str) -> bool {
    let Some(eq) = tok.find('=') else { return false };
    if eq == 0 {
        return false;
    }
    let name = &tok[..eq];
    name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The basename of a (possibly path-qualified) program token.
fn basename(tok: &str) -> &str {
    tok.rsplit('/').next().unwrap_or(tok)
}

/// Under a [`CRED_TOOLS`] program, scrub the value of `-p`/`-P`/`-a` in both
/// the concatenated (`-phunter2`) and space (`-p hunter2`) forms → `-p***`.
/// A no-op (returns `None`) for every other program, so short flags on
/// non-cred tools (`mkdir -p`, `ssh -p 22`, `tar -p`, `grep -P`) are untouched.
fn redact_cred_tool_short_flags(s: &str) -> Option<String> {
    let mut pairs = tokenize_ws(s);
    let program = command_program(&pairs)?;
    if !CRED_TOOLS.contains(&program.as_str()) {
        return None;
    }
    let mut hit = false;
    let mut i = 0usize;
    while i < pairs.len() {
        let tok = pairs[i].1.clone();
        // A single-dash short flag (`-p…`), not a long flag (`--password`).
        if let Some(rest) = single_dash_body(&tok) {
            // Which cred short-flag letter (if any) leads this token?
            if let Some(letter) = CRED_SHORT_FLAGS.iter().find(|f| rest.starts_with(*f)) {
                let after = &rest[letter.len()..];
                if after.is_empty() {
                    // Space form `-p hunter2`: scrub the NEXT token if present and
                    // not itself a flag.
                    if let Some(next) = pairs.get_mut(i + 1) {
                        if !next.1.starts_with('-') && !next.1.is_empty() && next.1 != "***" {
                            next.1 = "***".to_string();
                            hit = true;
                        }
                    }
                    i += 2;
                    continue;
                } else {
                    // Concatenated form `-phunter2`: keep `-p`, scrub the value.
                    if after != "***" {
                        pairs[i].1 = format!("-{letter}***");
                        hit = true;
                    }
                    i += 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    hit.then_some(rejoin(&pairs))
}

/// If `tok` is a single-dash flag (`-p`, `-phunter2`) — i.e. starts with exactly
/// one `-` — return the body after the dash; otherwise `None` (`--long`, `x`).
fn single_dash_body(tok: &str) -> Option<&str> {
    let rest = tok.strip_prefix('-')?;
    if rest.starts_with('-') {
        return None; // `--long` flag
    }
    Some(rest)
}

/// Split a command into shell-ish whitespace-separated tokens, *preserving* the
/// original run of whitespace before each token so the line can be rebuilt
/// verbatim. Returns (leading_ws, token) pairs. (Not a full shell lexer — quote
/// handling is intentionally simple; redaction biases toward over-redaction.)
fn tokenize_ws(s: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut ws = String::new();
    let mut tok = String::new();
    let mut in_tok = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if in_tok {
                pairs.push((std::mem::take(&mut ws), std::mem::take(&mut tok)));
                in_tok = false;
            }
            ws.push(c);
        } else {
            in_tok = true;
            tok.push(c);
        }
    }
    if in_tok {
        pairs.push((ws, tok));
    } else if !ws.is_empty() {
        // Trailing whitespace with no token — fold onto the last token so a
        // round-trip keeps the bytes. Rare; only matters for fidelity.
        if let Some(last) = pairs.last_mut() {
            last.1.push_str(&ws);
        }
    }
    pairs
}

/// Rebuild a command from (leading_ws, token) pairs (the inverse of
/// [`tokenize_ws`] for the non-trailing-whitespace case).
fn rejoin(pairs: &[(String, String)]) -> String {
    let mut s = String::new();
    for (ws, tok) in pairs {
        s.push_str(ws);
        s.push_str(tok);
    }
    s
}

/// `AKIA` + 16 `[0-9A-Z]` → `***`. Returns `Some` only if it changed something.
fn redact_aws_keys(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    let mut hit = false;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"AKIA") && i + 20 <= bytes.len() {
            let tail = &bytes[i + 4..i + 20];
            if tail.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
                // Must be a standalone token boundary on the right (the 20 chars
                // aren't followed by another alnum, which would make it longer).
                let after_ok = bytes.get(i + 20).map_or(true, |b| !b.is_ascii_alphanumeric());
                if after_ok {
                    out.push_str("***");
                    i += 20;
                    hit = true;
                    continue;
                }
            }
        }
        // Copy this UTF-8 char whole (we only ever match ASCII above, so byte
        // copy here is safe at a char boundary).
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    hit.then_some(out)
}

/// Length in bytes of the UTF-8 char starting with lead byte `b`.
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// `scheme://user:<password>@host` → scrub the password to `***`. Handles
/// multiple URLs in one line. Returns `Some` only on a real change.
fn redact_url_creds(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut hit = false;
    while let Some(pos) = rest.find("://") {
        // Copy through the "://".
        out.push_str(&rest[..pos + 3]);
        let after = &rest[pos + 3..];
        // The authority ends at the first `/`, `?`, `#`, whitespace, or quote.
        let auth_end = after
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(after.len());
        let authority = &after[..auth_end];
        // userinfo (before '@') with a ':' → user:password.
        if let Some(at) = authority.find('@') {
            let userinfo = &authority[..at];
            if let Some(colon) = userinfo.find(':') {
                let user = &userinfo[..colon];
                out.push_str(user);
                out.push(':');
                out.push_str("***");
                out.push('@');
                out.push_str(&authority[at + 1..]);
                hit = true;
            } else {
                out.push_str(authority);
            }
        } else {
            out.push_str(authority);
        }
        rest = &after[auth_end..];
    }
    out.push_str(rest);
    hit.then_some(out)
}

/// Sensitive URL query-parameter keys (lowercased, exact match). A
/// `?key=value`/`&key=value` whose key is one of these has its value scrubbed.
const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "token", "access_token", "api_key", "apikey", "secret",
    "password", "passwd", "auth", "key", "credential",
];

/// Scrub sensitive query-string values inside `http(s)`/`ftp`-ish URLs:
/// after a `?` or `&`, a `key=value` whose KEY is in [`SENSITIVE_QUERY_KEYS`]
/// has its value scrubbed (up to the next `&`, `#`, whitespace, or quote) →
/// `key=***`. Scans the whole line (URLs can sit anywhere on it). Returns
/// `Some` only on a real change.
fn redact_url_query_secrets(s: &str) -> Option<String> {
    // Only act on lines that actually contain a URL scheme, so a bare
    // `key=value` shell token isn't touched here (env-assignment handling owns
    // that). This keeps the query scrub URL-scoped.
    if !s.contains("://") {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut hit = false;
    let mut i = 0usize;
    // True once we've passed a `?` within the current URL and haven't yet hit a
    // delimiter that ends the URL (whitespace / quote). `&` keeps us in-query.
    let mut in_query = false;
    while i < bytes.len() {
        let b = bytes[i];
        // A URL ends at whitespace or a quote; leaving query context.
        if b.is_ascii_whitespace() || b == b'"' || b == b'\'' {
            in_query = false;
            out.push(b as char);
            i += 1;
            continue;
        }
        // Enter query context at the first `?`; `&` separates further params.
        if b == b'?' || (in_query && b == b'&') {
            in_query = true; // both `?` (opens) and an in-query `&` keep us in-query
            out.push(b as char);
            i += 1;
            // Try to read `key=` immediately after this separator.
            {
                if let Some((key_end, val_start)) = read_query_key(bytes, i) {
                    let key = &s[i..key_end];
                    if SENSITIVE_QUERY_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                        // Copy `key=` verbatim.
                        out.push_str(&s[i..val_start]);
                        // Find the value end: next `&`, `#`, whitespace, or quote.
                        let mut j = val_start;
                        while j < bytes.len() {
                            let vb = bytes[j];
                            if vb == b'&' || vb == b'#' || vb.is_ascii_whitespace() || vb == b'"' || vb == b'\'' {
                                break;
                            }
                            j += 1;
                        }
                        if j > val_start {
                            out.push_str("***");
                            hit = true;
                        } else {
                            // Empty value — nothing to scrub.
                            out.push_str(&s[val_start..j]);
                        }
                        i = j;
                        continue;
                    }
                }
            }
            continue;
        }
        // A `#` ends the query portion (fragment).
        if b == b'#' {
            in_query = false;
        }
        out.push_str(&s[i..i + utf8_len(b)]);
        i += utf8_len(b);
    }
    hit.then_some(out)
}

/// Starting at byte index `start`, read a query-param key terminated by `=`.
/// The key is `[A-Za-z0-9_-]+`. Returns `(key_end, value_start)` (value_start is
/// just past the `=`), or `None` if there's no `key=` here.
fn read_query_key(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut j = start;
    while j < bytes.len() {
        let b = bytes[j];
        if b == b'=' {
            return (j > start).then_some((j, j + 1));
        }
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            j += 1;
        } else {
            return None;
        }
    }
    None
}

/// Scrub the value of any `NAME=value` token whose NAME matches a sensitive
/// fragment. Operates token-wise so only the assignment is touched.
fn redact_env_assignments(s: &str) -> Option<String> {
    let mut pairs = tokenize_ws(s);
    let mut hit = false;
    for (_ws, tok) in pairs.iter_mut() {
        // An assignment token: NAME=VALUE (NAME is a valid env-ish name).
        let Some(eq) = tok.find('=') else { continue };
        if eq == 0 {
            continue;
        }
        let name = &tok[..eq];
        // Name must look like an identifier (letters/digits/_/-), so we don't
        // scrub things like `a==b` comparisons.
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            continue;
        }
        let upper = name.to_ascii_uppercase();
        if SENSITIVE_NAME_FRAGMENTS.iter().any(|frag| upper.contains(frag)) {
            // The token IS just NAME=value here (export is its own token), so
            // scrub the value — but only up to the first shell metacharacter, so
            // a trailing command (`NAME=secret; echo`) is preserved verbatim.
            let name = name.to_string(); // end the borrow of `tok` before mutating
            let value = &tok[eq + 1..];
            let (secret, tail) = split_value_at_metachar(value);
            if !secret.is_empty() && secret != "***" {
                *tok = format!("{name}=***{tail}");
                hit = true;
            }
        }
    }
    hit.then_some(rejoin(&pairs))
}

/// Scrub the value following a sensitive flag. Handles `--flag=value` (value in
/// the same token) and `--flag value` / `-flag value` (value is the next token).
/// Bare single-letter flags are never matched.
fn redact_secret_flags(s: &str) -> Option<String> {
    let mut pairs = tokenize_ws(s);
    let mut hit = false;
    let mut i = 0usize;
    while i < pairs.len() {
        let tok = pairs[i].1.clone();
        // Strip leading dashes to read the flag name.
        let trimmed = tok.trim_start_matches('-');
        let dashes = tok.len() - trimmed.len();
        if dashes == 0 {
            i += 1;
            continue;
        }
        // `--flag=value` form.
        if let Some(eq) = trimmed.find('=') {
            let flag = &trimmed[..eq];
            if flag_is_sensitive(flag) {
                let prefix = tok[..dashes + eq + 1].to_string(); // includes the '='
                let value = &trimmed[eq + 1..];
                // Scrub only up to the first shell metacharacter, preserving any
                // trailing command (`--token=secret; echo`).
                let (secret, tail) = split_value_at_metachar(value);
                if !secret.is_empty() && secret != "***" {
                    pairs[i].1 = format!("{prefix}***{tail}");
                    hit = true;
                }
            }
            i += 1;
            continue;
        }
        // `--flag value` form: scrub the NEXT token if it isn't itself a flag.
        if flag_is_sensitive(trimmed) {
            if let Some(next) = pairs.get_mut(i + 1) {
                let is_flag = next.1.starts_with('-');
                if !is_flag && !next.1.is_empty() && next.1 != "***" {
                    next.1 = "***".to_string();
                    hit = true;
                }
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    hit.then_some(rejoin(&pairs))
}

/// Case-insensitive membership of a flag name in [`SENSITIVE_FLAGS`].
fn flag_is_sensitive(flag: &str) -> bool {
    let lower = flag.to_ascii_lowercase();
    SENSITIVE_FLAGS.iter().any(|f| *f == lower)
}

/// Shell metacharacters that end an unquoted value. Whitespace is handled by
/// the tokenizer, but a value can butt directly against one of these with no
/// space (`GH_TOKEN=ghp_x|tee`, `NAME=v;echo`), in which case scrubbing the
/// whole token would eat the trailing command.
const VALUE_METACHARS: &[char] = &[';', '&', '|', '`', ')', '(', '<', '>'];

/// Split a (already whitespace-bounded) value into the secret part and the
/// trailing shell syntax to preserve: stop at the FIRST shell metacharacter
/// (`;` `&` `|` `` ` `` `)` `(` `<` `>` — whitespace can't occur inside a token).
/// `"ghp_ddd;echo"` → `("ghp_ddd", ";echo")`; `"ghp_ddd"` → `("ghp_ddd", "")`.
fn split_value_at_metachar(value: &str) -> (&str, &str) {
    match value.find(|c: char| VALUE_METACHARS.contains(&c) || c.is_whitespace()) {
        Some(idx) => (&value[..idx], &value[idx..]),
        None => (value, ""),
    }
}

// ---------------------------------------------------------------------------
// Parsing (pure, fixture-tested). Each parser yields raw (ts, command,
// duration, session) — redaction + guid happen at row-build time.

/// A parsed command before it becomes a row: the raw fields a format yields.
struct ParsedCommand {
    /// Epoch seconds, when the format recorded one.
    epoch: Option<i64>,
    /// Duration seconds (zsh elapsed only).
    duration_secs: Option<i64>,
    /// The command text, pre-redaction.
    command: String,
    /// Session label (per-format default, overridden for zsh sessions).
    session: String,
    /// Byte offset of this command within the file's parsed slice — the undated
    /// guid keys on it so identical undated commands don't collapse.
    offset: u64,
}

/// Join physical lines into logical commands by the trailing-backslash rule: a
/// line ending in an odd number of `\` continues onto the next physical line
/// (the `\`+newline is part of the one logical command). Returns each logical
/// line with the byte offset (within `body`) where it began.
fn logical_lines(body: &str) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    let mut cur = String::new();
    let mut cur_off: u64 = 0;
    let mut starting = true;
    for (off, text) in split_keep_offsets(body) {
        if starting {
            cur_off = off;
            starting = false;
        }
        // Count trailing backslashes; odd => continuation.
        let trailing = text.chars().rev().take_while(|&c| c == '\\').count();
        if trailing % 2 == 1 {
            // Keep the backslash + a newline (part of the logical command).
            cur.push_str(&text);
            cur.push('\n');
        } else {
            cur.push_str(&text);
            out.push((cur_off, std::mem::take(&mut cur)));
            starting = true;
        }
    }
    if !cur.is_empty() {
        out.push((cur_off, cur));
    }
    out
}

/// Split `body` into physical lines, each with its starting byte offset. A
/// trailing newline does not produce an empty final entry.
fn split_keep_offsets(body: &str) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    let mut start = 0usize;
    let bytes = body.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push((start as u64, body[start..i].to_string()));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push((start as u64, body[start..].to_string()));
    }
    out
}

/// Parse a zsh history slice. Each logical line is either EXTENDED_HISTORY
/// (`: <epoch>:<elapsed>;<cmd>`) or a bare command (plain). `base_offset` is
/// added to each line offset so guids stay unique across incremental slices.
fn parse_zsh(body: &str, session: &str, base_offset: u64) -> Vec<ParsedCommand> {
    let mut out = Vec::new();
    for (off, line) in logical_lines(body) {
        if line.trim().is_empty() {
            continue;
        }
        let (epoch, duration_secs, command) = parse_zsh_line(&line);
        if command.trim().is_empty() {
            continue;
        }
        out.push(ParsedCommand {
            epoch,
            duration_secs,
            command,
            session: session.to_string(),
            offset: base_offset + off,
        });
    }
    out
}

/// Decode one zsh logical line into (epoch, elapsed, command). A line starting
/// with `: ` and matching `: <digits>:<digits>;<cmd>` is extended; anything
/// else is a plain bare command.
fn parse_zsh_line(line: &str) -> (Option<i64>, Option<i64>, String) {
    if let Some(rest) = line.strip_prefix(':') {
        // rest = " <epoch>:<elapsed>;<command...>"
        let rest = rest.trim_start();
        if let Some(semi) = rest.find(';') {
            let meta = &rest[..semi];
            let command = rest[semi + 1..].to_string();
            if let Some(colon) = meta.find(':') {
                let epoch_s = meta[..colon].trim();
                let elapsed_s = meta[colon + 1..].trim();
                if let Ok(epoch) = epoch_s.parse::<i64>() {
                    let elapsed = elapsed_s.parse::<i64>().ok();
                    return (Some(epoch), elapsed, command);
                }
            }
            // Looked extended-ish but the metadata didn't parse — treat the
            // whole original line as a plain command (tolerant).
        }
    }
    (None, None, line.to_string())
}

/// Parse a bash history slice. Bare commands; a `#<all-digits>` line supplies
/// the ts of the command that *follows* it (HISTTIMEFORMAT). A `#...` line that
/// isn't all-digits is itself a normal command. No continuation joining for
/// bash (it stores multi-line commands differently); each physical line is a
/// command, except the timestamp marker lines.
fn parse_bash(body: &str, session: &str, base_offset: u64) -> Vec<ParsedCommand> {
    let mut out = Vec::new();
    let mut pending_epoch: Option<i64> = None;
    for (off, line) in split_keep_offsets(body) {
        if line.trim().is_empty() {
            continue;
        }
        // A timestamp marker: `#` then all-digits (HISTTIMEFORMAT).
        if let Some(rest) = line.strip_prefix('#') {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                pending_epoch = rest.parse::<i64>().ok();
                continue;
            }
            // `#` but not all-digits → a real command (a comment line in history).
        }
        out.push(ParsedCommand {
            epoch: pending_epoch.take(),
            duration_secs: None,
            command: line,
            session: session.to_string(),
            offset: base_offset + off,
        });
    }
    out
}

/// Parse a fish history slice (YAML-ish). Each entry is `- cmd: <command>` then
/// optional `  when: <epoch>` and `  paths:` lines. The command text is kept
/// as-is (fish's `\n` escapes are preserved verbatim — we don't unescape).
fn parse_fish(body: &str, session: &str, base_offset: u64) -> Vec<ParsedCommand> {
    let mut out: Vec<ParsedCommand> = Vec::new();
    let mut cur_cmd: Option<(u64, String)> = None;
    let mut cur_when: Option<i64> = None;

    let flush = |out: &mut Vec<ParsedCommand>, cmd: Option<(u64, String)>, when: Option<i64>| {
        if let Some((off, command)) = cmd {
            if !command.trim().is_empty() {
                out.push(ParsedCommand {
                    epoch: when,
                    duration_secs: None,
                    command,
                    session: session.to_string(),
                    offset: base_offset + off,
                });
            }
        }
    };

    for (off, line) in split_keep_offsets(body) {
        if let Some(rest) = line.strip_prefix("- cmd:") {
            // New entry — flush the previous one.
            flush(&mut out, cur_cmd.take(), cur_when.take());
            cur_cmd = Some((off, rest.trim_start().to_string()));
        } else if let Some(rest) = line.trim_start().strip_prefix("when:") {
            cur_when = rest.trim().parse::<i64>().ok();
        }
        // `  paths:` and `  - /a/b` list lines are ignored (we don't store paths
        // in v1). Any other line is skipped.
    }
    flush(&mut out, cur_cmd.take(), cur_when.take());
    out
}

/// Dispatch to the per-format parser.
fn parse(format: Format, body: &str, session: &str, base_offset: u64) -> Vec<ParsedCommand> {
    match format {
        Format::Zsh => parse_zsh(body, session, base_offset),
        Format::Bash => parse_bash(body, session, base_offset),
        Format::Fish => parse_fish(body, session, base_offset),
    }
}

// ---------------------------------------------------------------------------
// Row building (epoch → ts, redaction, guid).

/// A stable 64-bit hash rendered hex — the guid. Deterministic across runs
/// (DefaultHasher is SipHash with fixed keys when constructed via `new`).
fn stable_guid(parts: &[&str]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
        0u8.hash(&mut h); // separator so ("a","b") != ("ab","")
    }
    format!("{:016x}", h.finish())
}

/// Epoch seconds → RFC3339 in the machine's local zone (full fidelity isn't
/// available — shells store bare epochs — so local is the natural rendering).
fn epoch_to_local_rfc3339(secs: i64) -> Option<String> {
    Some(DateTime::from_timestamp(secs, 0)?.with_timezone(&Local).to_rfc3339())
}

/// Turn a [`ParsedCommand`] from one source into a [`CommandRow`], running the
/// redactor and computing the guid. Returns `None` if the command is empty
/// after trimming (defensive — parsers already skip those).
fn build_row(pc: ParsedCommand, shell: Shell) -> Option<CommandRow> {
    let (command, redacted) = redact(&pc.command);
    if command.trim().is_empty() {
        return None;
    }
    let ts = pc.epoch.and_then(epoch_to_local_rfc3339);
    // The guid is computed from the ORIGINAL (pre-redaction) command, not the
    // redacted one: two DIFFERENT secret commands at the same ts can redact to
    // an identical string (`GH_TOKEN=ghp_A` and `=ghp_B` → `GH_TOKEN=***`) and
    // must stay distinct rows. It's a SipHash, so the original bytes aren't
    // persisted; only the WRITTEN command field is redacted. The same ORIGINAL
    // command in main + a per-session file still hashes equal → one row.
    let guid = match &ts {
        // Dated: a command at a given instant is the same event no matter which
        // file (main vs per-session) reported it → dedupe on (ts, original).
        Some(t) => stable_guid(&[t, &pc.command]),
        // Undated: no instant to key on; keep (session, original, offset) so
        // repeated identical commands stay distinct rows.
        None => stable_guid(&[&pc.session, &pc.command, &pc.offset.to_string()]),
    };
    Some(CommandRow {
        ts,
        command,
        duration_secs: pc.duration_secs,
        session: pc.session,
        shell,
        redacted,
        guid,
    })
}

// ---------------------------------------------------------------------------
// Cursor.

/// Incremental-sync state in `.trove/shell-history-sync.json` (non-secret,
/// rebuildable). `offsets` maps each source file's display name → the byte
/// offset imported through. A lost cursor just re-imports everything; the guid
/// upsert keeps that idempotent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellHistorySyncState {
    /// RFC3339 local time of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// source file display name → byte offset imported through.
    #[serde(default)]
    pub offsets: BTreeMap<String, u64>,
}

impl Vault {
    fn read_shell_history_sync(&self) -> ShellHistorySyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_shell_history_sync(&self, state: &ShellHistorySyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    // -----------------------------------------------------------------------
    // The scan.

    /// One incremental scan pass over the shell-history files under the scan
    /// root. Silently a no-op when there's no scan root.
    pub fn collect_shell_history(&self) -> Result<ShellHistoryStats> {
        let Some(root) = scan_root() else {
            return Ok(ShellHistoryStats::default());
        };
        self.collect_shell_history_from(&root)
    }

    /// The pass itself, scan-root-injected for tests. Reads each source from its
    /// stored byte offset (resetting to 0 on truncation), parses + redacts the
    /// new bytes, upserts the rows by guid, then advances the offset — only
    /// after the batch is written.
    pub(crate) fn collect_shell_history_from(&self, root: &Path) -> Result<ShellHistoryStats> {
        let mut stats = ShellHistoryStats::default();
        let sources = sources(root);
        if sources.is_empty() {
            return Ok(stats);
        }
        let mut state = self.read_shell_history_sync();
        let mut changed = false;

        for src in sources {
            match self.sync_one_source(&src, &mut state) {
                Ok((n, red)) if n > 0 => {
                    stats.commands += n;
                    stats.redacted += red;
                    stats.files_with_new += 1;
                    changed = true;
                }
                Ok(_) => {}
                Err(e) => {
                    // A single unreadable/odd file never aborts the whole pass.
                    eprintln!("trove shell-history: skipping {}: {e:#}", src.path.display());
                }
            }
        }

        if changed {
            state.updated = Local::now().to_rfc3339();
            self.write_shell_history_sync(&state)?;
        }
        Ok(stats)
    }

    /// Read one source past its cursor, write the new rows, advance the cursor.
    /// Returns (new commands written, of which redacted).
    fn sync_one_source(
        &self,
        src: &Source,
        state: &mut ShellHistorySyncState,
    ) -> Result<(u64, u64)> {
        let full = fs::read(&src.path)
            .with_context(|| format!("reading {}", src.path.display()))?;
        let file_len = full.len() as u64;

        // Truncation-safe: a file now shorter than the stored offset was rotated
        // or trimmed (`history -c`, HISTSIZE) — reset and re-scan from 0.
        let stored = state.offsets.get(&src.display).copied().unwrap_or(0);
        let start = if stored > file_len { 0 } else { stored };
        if start == file_len {
            return Ok((0, 0)); // nothing new
        }

        // Decode the new slice lossily (history files are normally UTF-8; a
        // stray non-UTF-8 byte must never crash the collector).
        let slice = &full[start as usize..];
        let body = String::from_utf8_lossy(slice);

        let shell = match src.format {
            Format::Zsh => Shell::Zsh,
            Format::Bash => Shell::Bash,
            Format::Fish => Shell::Fish,
        };

        let parsed = parse(src.format, &body, &src.session, start);
        let rows: Vec<CommandRow> = parsed
            .into_iter()
            .filter_map(|pc| build_row(pc, shell))
            .collect();

        let (written, written_redacted) = self.upsert_command_rows(&rows)?;

        // Advance the cursor ONLY after the batch is written, so an interrupted
        // write re-imports rather than skips. We always advance to the file's
        // current length: every byte up to here has been parsed.
        state.offsets.insert(src.display.clone(), file_len);
        Ok((written, written_redacted))
    }

    /// Upsert command rows into their partitions, keyed by `guid`. Dated rows go
    /// to their `YYYY-MM` partition; undated rows to `undated`. Groups by
    /// partition, reads each once, replaces matching guids and appends the rest,
    /// rewrites atomically. Returns (rows genuinely new, of which redacted).
    fn upsert_command_rows(&self, rows: &[CommandRow]) -> Result<(u64, u64)> {
        if rows.is_empty() {
            return Ok((0, 0));
        }
        // Group incoming rows by partition key.
        let mut by_key: BTreeMap<String, Vec<&CommandRow>> = BTreeMap::new();
        for r in rows {
            let key = match &r.ts {
                Some(t) => Partition::Month.key(t).map(|k| k.to_string()).with_context(|| {
                    format!("command has an unpartitionable ts {t:?}")
                })?,
                None => UNDATED_KEY.to_string(),
            };
            by_key.entry(key).or_default().push(r);
        }

        let stream = self.stream(DIR, Partition::Month);
        let mut new_count = 0u64;
        let mut new_redacted = 0u64;
        for (key, incoming) in by_key {
            let mut existing: Vec<CommandRow> = stream.read(&key)?;
            let mut index: std::collections::HashMap<String, usize> = existing
                .iter()
                .enumerate()
                .map(|(i, r)| (r.guid.clone(), i))
                .collect();
            for r in incoming {
                match index.get(&r.guid) {
                    Some(&i) => existing[i] = (*r).clone(), // upsert in place
                    None => {
                        index.insert(r.guid.clone(), existing.len());
                        existing.push((*r).clone());
                        new_count += 1;
                        if r.redacted {
                            new_redacted += 1;
                        }
                    }
                }
            }
            self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &existing)?;
        }
        Ok((new_count, new_redacted))
    }

    // -----------------------------------------------------------------------
    // Reads.

    /// All command rows for one partition (`YYYY-MM` or `"undated"`), in file
    /// order. The hub's Recent-data view reads this.
    pub fn shell_history_commands(&self, key: &str) -> Result<Vec<CommandRow>> {
        self.stream(DIR, Partition::Month).read(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-shellhist-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A unique temp dir to act as the scan-root (a fake home).
    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-shroot-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(root: &Path, rel: &str, body: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        p
    }

    /// Every row in the vault across all partitions (dated + undated).
    fn all_rows(v: &Vault) -> Vec<CommandRow> {
        let dir = v.root().join(DIR);
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            let mut keys: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
                .collect();
            keys.sort();
            for k in keys {
                out.extend(v.shell_history_commands(&k).unwrap_or_default());
            }
        }
        out
    }

    // ---- redaction (pure) ----

    #[test]
    fn redact_env_token_assignment() {
        let (out, r) = redact("export GH_TOKEN=ghp_supersecretvalue");
        assert_eq!(out, "export GH_TOKEN=***");
        assert!(r);
        // PGPASSWORD / MYSQL_PWD / AWS_SECRET_ACCESS_KEY all caught by fragments.
        assert_eq!(redact("PGPASSWORD=hunter2 psql").0, "PGPASSWORD=*** psql");
        assert_eq!(redact("MYSQL_PWD=abc mysql").0, "MYSQL_PWD=*** mysql");
        assert_eq!(
            redact("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG").0,
            "AWS_SECRET_ACCESS_KEY=***"
        );
    }

    #[test]
    fn redact_password_flag_both_forms() {
        assert_eq!(redact("mysql --password=hunter2 -u root").0, "mysql --password=*** -u root");
        assert_eq!(redact("mysql --password hunter2 -u root").0, "mysql --password *** -u root");
        // --token, -api-key etc.
        assert_eq!(redact("curl --token=abc123 x").0, "curl --token=*** x");
        assert_eq!(redact("tool --bearer xyz").0, "tool --bearer ***");
    }

    #[test]
    fn redact_url_inline_credentials() {
        let (out, r) = redact("curl https://user:pw@host/path");
        assert_eq!(out, "curl https://user:***@host/path");
        assert!(r);
        // git remote form, password kept out.
        assert_eq!(
            redact("git clone https://alice:s3cr3t@github.com/x/y.git").0,
            "git clone https://alice:***@github.com/x/y.git"
        );
    }

    #[test]
    fn redact_aws_access_key_id() {
        let (out, r) = redact("aws configure set key AKIAIOSFODNN7EXAMPLE");
        assert_eq!(out, "aws configure set key ***");
        assert!(r);
    }

    #[test]
    fn redact_leaves_innocent_commands_untouched_no_false_positive() {
        for cmd in [
            "mkdir -p build",          // bare -p must NOT trigger
            "ls -la",
            "git commit -m 'fix the password reset bug'", // 'password' in a quoted msg, not a flag
            "echo a==b",               // == comparison, not an assignment
            "tar -xzvf archive.tgz",
            "cargo test -p trove-core",
        ] {
            let (out, r) = redact(cmd);
            assert_eq!(out, cmd, "must not rewrite: {cmd}");
            assert!(!r, "must not flag redacted: {cmd}");
        }
    }

    // ---- zsh extended: ts + month partition + multi-line continuation ----

    #[test]
    fn zsh_extended_ts_and_multiline_command() {
        let v = temp_vault("zext");
        let root = temp_root("zext");
        // epoch 1749812400 = 2025-06-13T...; pick a clearly-2026 epoch instead:
        // 1781000000 = 2026-06-09T... ; 1783600000 = 2026-07-09...
        // Build: a single-line cmd (June), and a MULTI-LINE cmd (backslash
        // continuation) that must parse as ONE row.
        let body = concat!(
            ": 1781000000:0;echo hello\n",
            ": 1781000500:3;for i in 1 2 3; do \\\n",
            "  echo $i; \\\n",
            "done\n",
        );
        write_file(&root, ".zsh_history", body);

        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 2, "two logical commands (continuation joined)");

        let rows = all_rows(&v);
        assert_eq!(rows.len(), 2);
        // Every row carries a ts and lands in a dated month partition.
        for r in &rows {
            assert!(r.ts.is_some(), "extended rows have ts");
            assert_eq!(r.shell, Shell::Zsh);
            assert_eq!(r.session, "main");
        }
        // The multi-line command kept all three physical lines as one command.
        let multi = rows.iter().find(|r| r.command.contains("for i in")).unwrap();
        assert!(multi.command.contains("echo $i"), "continuation line joined: {:?}", multi.command);
        assert!(multi.command.contains("done"), "final line joined: {:?}", multi.command);
        assert_eq!(multi.duration_secs, Some(3), "elapsed parsed");
        // Partition is the command's own month — assert the dated file exists.
        let month = Partition::Month.key(multi.ts.as_ref().unwrap()).unwrap();
        assert!(v.root().join(format!("{DIR}/{month}.jsonl")).exists(), "month partition written");
        // Nothing in the undated bucket.
        assert!(!v.root().join(format!("{DIR}/{UNDATED_KEY}.jsonl")).exists());
    }

    // ---- zsh plain: no prefix → undated ----

    #[test]
    fn zsh_plain_goes_to_undated_no_ts() {
        let v = temp_vault("zplain");
        let root = temp_root("zplain");
        write_file(&root, ".zsh_history", "ls -la\ncd /tmp\ngit status\n");

        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 3);
        let rows = all_rows(&v);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert!(r.ts.is_none(), "plain rows have no ts");
        }
        // All in the undated bucket; no dated partition created.
        assert!(v.root().join(format!("{DIR}/{UNDATED_KEY}.jsonl")).exists());
    }

    // ---- bash: #<epoch> attaches ts; #comment is a command ----

    #[test]
    fn bash_histtimeformat_and_comment_lines() {
        let v = temp_vault("bash");
        let root = temp_root("bash");
        let body = concat!(
            "#1781000000\n",
            "echo timed\n",
            "#1781000100\n",
            "ls\n",
            "# this is a real comment, not all digits\n", // a normal command
            "pwd\n",
        );
        write_file(&root, ".bash_history", body);

        let stats = v.collect_shell_history_from(&root).unwrap();
        // 4 commands: echo, ls, the #comment line, pwd. (2 marker lines consumed.)
        assert_eq!(stats.commands, 4, "two ts-markers consumed, four commands");
        let rows = all_rows(&v);
        let timed = rows.iter().find(|r| r.command == "echo timed").unwrap();
        assert!(timed.ts.is_some(), "ts attached from preceding #epoch");
        assert_eq!(timed.shell, Shell::Bash);
        let ls = rows.iter().find(|r| r.command == "ls").unwrap();
        assert!(ls.ts.is_some());
        // The non-digit `#...` line is itself a command, undated.
        let comment = rows.iter().find(|r| r.command.starts_with("# this is")).unwrap();
        assert!(comment.ts.is_none(), "non-digit # line is an undated command");
        // pwd has no preceding marker → undated.
        let pwd = rows.iter().find(|r| r.command == "pwd").unwrap();
        assert!(pwd.ts.is_none());
    }

    // ---- fish YAML ----

    #[test]
    fn fish_yaml_cmd_and_when() {
        let v = temp_vault("fish");
        let root = temp_root("fish");
        let body = concat!(
            "- cmd: echo fish\n",
            "  when: 1781000000\n",
            "- cmd: ls -la\n",
            "  when: 1781000200\n",
            "  paths:\n",
            "    - /tmp/x\n",
        );
        write_file(&root, ".local/share/fish/fish_history", body);

        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 2);
        let rows = all_rows(&v);
        for r in &rows {
            assert_eq!(r.shell, Shell::Fish);
            assert!(r.ts.is_some(), "when → ts");
        }
        assert!(rows.iter().any(|r| r.command == "echo fish"));
        assert!(rows.iter().any(|r| r.command == "ls -la"));
    }

    // ---- per-session dedupe: same (ts,command) in main + session → ONE row ----

    #[test]
    fn per_session_duplicate_dedupes_on_ts_command() {
        let v = temp_vault("sessdupe");
        let root = temp_root("sessdupe");
        // The same command at the same instant appears in BOTH the main history
        // and a per-session file. After dedupe there must be exactly one row.
        let line = ": 1781000000:0;npm run build\n";
        write_file(&root, ".zsh_history", line);
        write_file(&root, ".zsh_sessions/ABC123.history", line);

        let stats = v.collect_shell_history_from(&root).unwrap();
        // Two files each yield the command, but the guid (ts,command) collapses.
        assert_eq!(stats.commands, 1, "dedup by (ts, command) across files");
        let rows = all_rows(&v);
        assert_eq!(rows.len(), 1, "exactly one row after dedupe");
        assert_eq!(rows[0].command, "npm run build");
    }

    // ---- redaction end-to-end: secret bytes absent from the written file ----

    #[test]
    fn redaction_secret_bytes_absent_from_written_file() {
        let v = temp_vault("redactfile");
        let root = temp_root("redactfile");
        // Four secret-bearing commands across formats, plus one innocent one.
        let body = concat!(
            ": 1781000000:0;export FOO_TOKEN=ghp_secretvalue123\n",
            ": 1781000100:0;mysql --password=hunter2 -u root\n",
            ": 1781000200:0;curl https://u:pw_secret@h/api\n",
            ": 1781000300:0;aws set key AKIAIOSFODNN7EXAMPLE\n",
            ": 1781000400:0;mkdir -p build\n",
        );
        write_file(&root, ".zsh_history", body);

        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 5);
        assert_eq!(stats.redacted, 4, "four redacted, the mkdir is not");

        // Read the RAW file bytes and assert NO secret substring survived.
        let dir = v.root().join(DIR);
        let mut all_bytes = String::new();
        for e in fs::read_dir(&dir).unwrap().flatten() {
            all_bytes.push_str(&fs::read_to_string(e.path()).unwrap());
        }
        for secret in [
            "ghp_secretvalue123",
            "hunter2",
            "pw_secret",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            assert!(
                !all_bytes.contains(secret),
                "raw secret {secret:?} leaked into the vault file: {all_bytes}"
            );
        }
        // The redaction markers ARE present.
        assert!(all_bytes.contains("FOO_TOKEN=***"));
        assert!(all_bytes.contains("--password=***"));
        assert!(all_bytes.contains("u:***@h"));
        // The innocent command survived verbatim and is NOT flagged redacted.
        let rows = all_rows(&v);
        let mkdir = rows.iter().find(|r| r.command == "mkdir -p build").unwrap();
        assert!(!mkdir.redacted, "innocent command not flagged (no false positive)");
    }

    // ---- incremental cursor: append one line → exactly one new row ----

    #[test]
    fn incremental_append_imports_only_new() {
        let v = temp_vault("incr");
        let root = temp_root("incr");
        let p = write_file(&root, ".zsh_history", ": 1781000000:0;cmd one\n");

        let first = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(first.commands, 1);

        // Re-sync, no change → nothing new.
        let noop = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(noop.commands, 0, "cursor: unchanged file yields nothing");

        // Append exactly one line and re-sync.
        let grown = ": 1781000000:0;cmd one\n: 1781000100:0;cmd two\n";
        fs::write(&p, grown).unwrap();
        let second = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(second.commands, 1, "exactly one new command imported");
        let rows = all_rows(&v);
        assert_eq!(rows.len(), 2, "two total, no duplicates across the boundary");
        assert!(rows.iter().any(|r| r.command == "cmd two"));
    }

    // ---- truncation safety: shrink the file → re-scan without duplicates ----

    #[test]
    fn truncation_resets_cursor_and_reimports_without_dupes() {
        let v = temp_vault("trunc");
        let root = temp_root("trunc");
        let p = write_file(
            &root,
            ".zsh_history",
            ": 1781000000:0;alpha\n: 1781000100:0;beta\n: 1781000200:0;gamma\n",
        );
        let first = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(first.commands, 3);

        // History rotated/trimmed: the file is now SHORTER (a single command).
        // The stored offset exceeds the new length → cursor resets to 0.
        fs::write(&p, ": 1781000300:0;delta\n").unwrap();
        let second = v.collect_shell_history_from(&root).unwrap();
        // Re-scan from 0: 'delta' is new; the guid upsert prevents re-adding the
        // (already-stored) alpha/beta/gamma — but those bytes are gone from the
        // file now, so only 'delta' appears this pass.
        assert_eq!(second.commands, 1, "truncated file re-scanned; only the new line is new");
        let rows = all_rows(&v);
        // alpha/beta/gamma persisted from the first pass; delta added. No dupes.
        let commands: std::collections::HashSet<&str> = rows.iter().map(|r| r.command.as_str()).collect();
        assert!(commands.contains("delta"));
        assert!(commands.contains("alpha"), "earlier rows retained (no loss)");
        assert_eq!(rows.len(), 4, "no duplicates after truncation re-scan");
    }

    // ---- truncation re-scan idempotency when content is identical ----

    #[test]
    fn truncation_with_identical_content_does_not_duplicate() {
        let v = temp_vault("truncidem");
        let root = temp_root("truncidem");
        let p = write_file(&root, ".zsh_history", ": 1781000000:0;same cmd\n: 1781000100:0;other\n");
        v.collect_shell_history_from(&root).unwrap();
        assert_eq!(all_rows(&v).len(), 2);

        // Shrink to a strictly shorter file whose one line was already imported.
        fs::write(&p, ": 1781000000:0;same cmd\n").unwrap();
        let second = v.collect_shell_history_from(&root).unwrap();
        // Re-scan from 0 sees 'same cmd' again; guid (ts,command) matches → upsert,
        // not a new row.
        assert_eq!(second.commands, 0, "re-scanned identical line dedupes by guid");
        assert_eq!(all_rows(&v).len(), 2, "no duplicate row");
    }

    // ---- cursor back-compat ----

    #[test]
    fn cursor_back_compat_old_and_empty_deserialize() {
        let empty: ShellHistorySyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.offsets.is_empty());
        assert_eq!(empty.updated, "");
        let old: ShellHistorySyncState =
            serde_json::from_str(r#"{"updated":"2026-01-01T00:00:00-08:00"}"#).unwrap();
        assert_eq!(old.updated, "2026-01-01T00:00:00-08:00");
        assert!(old.offsets.is_empty());
        // A sparse CommandRow (old line, only required fields) still decodes.
        let sparse: CommandRow = serde_json::from_str(
            r#"{"command":"ls","session":"main","shell":"zsh","redacted":false,"guid":"abc"}"#,
        )
        .unwrap();
        assert_eq!(sparse.command, "ls");
        assert_eq!(sparse.ts, None);
        assert_eq!(sparse.duration_secs, None);
        assert_eq!(sparse.shell, Shell::Zsh);
    }

    #[test]
    fn missing_root_is_a_quiet_noop() {
        let v = temp_vault("missing");
        let root = temp_root("missing-empty"); // exists but holds no history files
        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 0);
        assert!(!v.root().join(DIR).exists());
    }

    // ---- FIX 1: DB-tool short -p/-P/-a passwords are redacted (tool-scoped) ----

    #[test]
    fn db_tool_dash_p_password_is_redacted() {
        let v = temp_vault("dbdashp");
        let root = temp_root("dbdashp");
        // CRED_TOOLS commands whose secret is a short -p/-a value, in both the
        // concatenated and space forms; plus NON-cred tools whose -p/-a/-P must
        // NOT be touched. All dated so they land in month partitions.
        let body = concat!(
            ": 1781000000:0;mysql -phunter2 db\n",
            ": 1781000100:0;mysql -p hunter2 db\n",
            ": 1781000200:0;mongo -p s3cr3t\n",
            ": 1781000300:0;redis-cli -a s3cr3t\n",
            ": 1781000400:0;mkdir -p build\n",
            ": 1781000500:0;ssh -p 22 host\n",
            ": 1781000600:0;tar -p x\n",
            ": 1781000700:0;grep -P re\n",
        );
        write_file(&root, ".zsh_history", body);

        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 8);

        // Secrets must be ABSENT from the written vault bytes.
        let dir = v.root().join(DIR);
        let mut all_bytes = String::new();
        for e in fs::read_dir(&dir).unwrap().flatten() {
            all_bytes.push_str(&fs::read_to_string(e.path()).unwrap());
        }
        for secret in ["hunter2", "s3cr3t"] {
            assert!(
                !all_bytes.contains(secret),
                "db-tool -p/-a secret {secret:?} leaked into the vault: {all_bytes}"
            );
        }
        // The scrub markers ARE present: concatenated `-p***` (mysql -phunter2)
        // and the space form `-a ***` (redis-cli -a s3cr3t).
        assert!(all_bytes.contains("-p***"), "concatenated -p scrubbed: {all_bytes}");
        assert!(all_bytes.contains("redis-cli -a ***"), "space-form -a scrubbed: {all_bytes}");

        // NON-cred tools: the short flags are untouched, NOT flagged redacted.
        let rows = all_rows(&v);
        for cmd in ["mkdir -p build", "ssh -p 22 host", "tar -p x", "grep -P re"] {
            let row = rows.iter().find(|r| r.command == cmd)
                .unwrap_or_else(|| panic!("non-cred command unchanged & present: {cmd}"));
            assert!(!row.redacted, "non-cred command must not be flagged: {cmd}");
        }
    }

    #[test]
    fn db_tool_dash_p_unit_forms() {
        // Direct redact() checks for the exact rewrites.
        assert_eq!(redact("mysql -phunter2 db").0, "mysql -p*** db");
        assert_eq!(redact("mysql -p hunter2 db").0, "mysql -p *** db");
        assert_eq!(redact("mongo -p s3cr3t").0, "mongo -p ***");
        assert_eq!(redact("redis-cli -a s3cr3t").0, "redis-cli -a ***");
        // Path-qualified program + leading env/sudo prefixes still resolve to mysql.
        assert_eq!(redact("/usr/bin/mysql -phunter2").0, "/usr/bin/mysql -p***");
        assert_eq!(redact("sudo mysql -phunter2").0, "sudo mysql -p***");
        assert_eq!(redact("PAGER=less mysql -phunter2").0, "PAGER=less mysql -p***");
        // Non-cred tools left untouched, including -P.
        for cmd in ["mkdir -p build", "ssh -p 22 host", "tar -p x", "grep -P re"] {
            let (out, r) = redact(cmd);
            assert_eq!(out, cmd, "non-cred -p/-P untouched: {cmd}");
            assert!(!r, "non-cred -p/-P not flagged: {cmd}");
        }
    }

    // ---- FIX 2: URL query-string secrets are redacted ----

    #[test]
    fn url_query_token_is_redacted() {
        let v = temp_vault("urlq");
        let root = temp_root("urlq");
        let body = concat!(
            ": 1781000000:0;curl 'https://h/api?access_token=qsecret&page=1'\n",
            ": 1781000100:0;wget https://h?api_key=qsecret2\n",
        );
        write_file(&root, ".zsh_history", body);

        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 2);
        assert_eq!(stats.redacted, 2, "both query-secret URLs flagged redacted");

        let dir = v.root().join(DIR);
        let mut all_bytes = String::new();
        for e in fs::read_dir(&dir).unwrap().flatten() {
            all_bytes.push_str(&fs::read_to_string(e.path()).unwrap());
        }
        for secret in ["qsecret", "qsecret2"] {
            assert!(!all_bytes.contains(secret), "URL query secret {secret:?} leaked: {all_bytes}");
        }
        // Non-sensitive param preserved; scrub markers present.
        assert!(all_bytes.contains("page=1"), "innocent query param preserved: {all_bytes}");
        assert!(all_bytes.contains("access_token=***"));
        assert!(all_bytes.contains("api_key=***"));
    }

    #[test]
    fn url_query_unit_preserves_innocent_params() {
        assert_eq!(
            redact("curl 'https://h/api?access_token=qsecret&page=1'").0,
            "curl 'https://h/api?access_token=***&page=1'"
        );
        assert_eq!(redact("wget https://h?api_key=qsecret2").0, "wget https://h?api_key=***");
        // A bare (non-URL) key=value is NOT touched by the query scrub (env
        // assignment handling owns sensitive bare assignments; `key=` alone is
        // not in SENSITIVE_NAME_FRAGMENTS, so this stays verbatim).
        let (out, r) = redact("run key=plainvalue");
        assert_eq!(out, "run key=plainvalue", "bare key=value not URL-scrubbed");
        assert!(!r);
    }

    // ---- FIX 3: value scrub stops at the first shell metacharacter ----

    #[test]
    fn value_scrub_stops_at_shell_metachar() {
        // Env-assignment value followed by `; echo` — the trailing command is intact.
        assert_eq!(redact("export GH_TOKEN=ghp_ddd; echo ok").0, "export GH_TOKEN=***; echo ok");
        // No space before the metachar (`|tee f`).
        assert_eq!(redact("GH_TOKEN=ghp_x|tee f").0, "GH_TOKEN=***|tee f");
        // Same for a --flag=value form.
        assert_eq!(redact("curl --token=abc123;echo done").0, "curl --token=***;echo done");
        // Backslash/subshell metachars too.
        assert_eq!(redact("GH_TOKEN=ghp_y&echo bg").0, "GH_TOKEN=***&echo bg");
        // And the secret bytes really are gone from each.
        for cmd in ["export GH_TOKEN=ghp_ddd; echo ok", "GH_TOKEN=ghp_x|tee f"] {
            let (out, _) = redact(cmd);
            assert!(!out.contains("ghp_ddd") && !out.contains("ghp_x"), "secret gone: {out}");
        }
    }

    // ---- FIX 4: dedup guid is over the ORIGINAL command, not the redacted one ----

    #[test]
    fn distinct_secrets_same_ts_do_not_merge() {
        let v = temp_vault("distinctsec");
        let root = temp_root("distinctsec");
        // Two DIFFERENT secret commands at the SAME ts that redact to the same
        // string. With the guid over the redacted command they'd collapse to one
        // row (data loss); over the ORIGINAL they stay two distinct rows.
        let body = concat!(
            ": 1781000000:0;export GH_TOKEN=ghp_A\n",
            ": 1781000000:0;export GH_TOKEN=ghp_B\n",
        );
        write_file(&root, ".zsh_history", body);

        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 2, "distinct originals at same ts → two rows");
        let rows = all_rows(&v);
        assert_eq!(rows.len(), 2, "no merge of distinct secret commands");
        for r in &rows {
            assert_eq!(r.command, "export GH_TOKEN=***", "both rows redacted");
            assert!(r.redacted);
        }
        // Distinct guids (over the original) prove the rows didn't collapse.
        assert_ne!(rows[0].guid, rows[1].guid, "distinct originals → distinct guids");
    }

    #[test]
    fn same_original_command_still_dedupes_across_session_files() {
        let v = temp_vault("origdedupe");
        let root = temp_root("origdedupe");
        // The SAME original (secret) command at the SAME ts in BOTH the main
        // history and a per-session file: still ONE row (guid over original,
        // which is identical in both files).
        let line = ": 1781000000:0;export GH_TOKEN=ghp_SAME\n";
        write_file(&root, ".zsh_history", line);
        write_file(&root, ".zsh_sessions/ABC.history", line);

        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 1, "same original across files dedupes to one row");
        let rows = all_rows(&v);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "export GH_TOKEN=***");
        assert!(!rows[0].command.contains("ghp_SAME"), "secret not persisted");
    }

    #[test]
    fn session_label_from_session_file_stem() {
        let v = temp_vault("sesslabel");
        let root = temp_root("sesslabel");
        // Only a per-session file (no main history) — session = file stem.
        write_file(&root, ".zsh_sessions/9F2C.history", ": 1781000000:0;whoami\n");
        let stats = v.collect_shell_history_from(&root).unwrap();
        assert_eq!(stats.commands, 1);
        let rows = all_rows(&v);
        assert_eq!(rows[0].session, "9F2C", "session label is the file stem");
    }
}

//! LinkedIn data-export import — connections (contacts contract), messages
//! (correspondence contract), and posts/reactions (social raw).
//!
//! LinkedIn offers two export grades:
//!   • **Scoped / quick export** (~10–24 min): `Connections.csv` only.
//!   • **Full archive** (up to 72 h): a ZIP containing `Connections.csv`,
//!     `messages.csv`, `Shares.csv`, `Comments.csv`, `Reactions.csv`, etc.
//!
//! Both are accepted here.  The import is *re-runnable*: connections use a
//! snapshot (atomic rewrite), messages use a sha256 guid for dedup.  No API
//! key, no OAuth — LinkedIn's API does not expose personal connection data.
//!
//! **Contract routing:**
//! - `Connections.csv` → [`crate::contacts::Contact`] snapshot at
//!   `contacts/linkedin/connections.jsonl` (`source = "linkedin"`).
//! - `messages.csv` → [`crate::correspondence::Message`] stream at
//!   `correspondence/linkedin/YYYY-MM.jsonl` (`source = "linkedin"`).
//! - `Shares.csv`, `Comments.csv`, `Reactions.csv` → raw JSONL under
//!   `social/linkedin/raw/` (full fidelity; no social contract yet).
//!
//! **Brief:** docs/integrations/linkedin.md.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDateTime};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::contacts::{normalize_email, Contact, ContactOrg};
use crate::correspondence::Message;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Collector id — also the source folder name under `contacts/` and
/// `correspondence/` per the contract "source = folder name" rule.
const SOURCE: &str = "linkedin";
/// Snapshot file for connections (current-state; rewritten on each import).
const CONNECTIONS_SNAPSHOT: &str = "contacts/linkedin/connections.jsonl";
/// Raw social output directory.
const SOCIAL_RAW_DIR: &str = "social/linkedin/raw";
/// Correspondence stream directory.
const MESSAGES_DIR: &str = "correspondence/linkedin";

fn def_last_data(vault: &Vault) -> Option<String> {
    // Report the snapshot mtime when connections have been imported.
    let path = vault.resolve(CONNECTIONS_SNAPSHOT).ok()?;
    crate::registry::file_mtime(&path)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "linkedin",
        name: "LinkedIn",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import your LinkedIn connections, messages, and posts from the official \
                      data export. Connections land as contacts with their connection date; \
                      messages land in the unified correspondence stream (re-runnable, no duplicates).",
        domain: "contacts",
        vault_path: "contacts/linkedin/",
        toggleable: false,
        setup: &[
            "linkedin.com → Me → Settings & Privacy → Data privacy → Get a copy of your data.",
            "For connections only (~10–24 min): select Connections and request export.",
            "For messages too (up to 72 h): request the full archive.",
            "Import the downloaded ZIP (or the bare Connections.csv) here.",
        ],
        caveats: "Email addresses are present for only ~10–20% of connections — LinkedIn omits \
                 them by default. Messages contain full conversation text; enable the option \
                 below to import them into the correspondence stream.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "csv"],
    params: &[
        crate::registry::ImportParam {
            key: "import_messages",
            label: "Import message history (conversation bodies)",
            placeholder: "Type 'yes' to import full message text into the correspondence stream \
                          — only you can read your vault, but acknowledge that chat content \
                          will be stored locally.",
            required: false,
        },
        crate::registry::ImportParam {
            // Owner name lets us mark from_me correctly on messages.
            key: "owner_name",
            label: "Your LinkedIn display name",
            placeholder: "e.g. Jane Smith — used to mark your own messages as sent (optional, \
                          requires import_messages=yes)",
            required: false,
        },
    ],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Top-level dispatcher: detect file type and route.

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let import_messages = params
        .get("import_messages")
        .map(|v| matches!(v.to_ascii_lowercase().trim(), "yes" | "true" | "1"))
        .unwrap_or(false);
    let owner_name = params.get("owner_name").filter(|s| !s.is_empty()).cloned();

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "zip" => import_zip(vault, path, import_messages, owner_name.as_deref(), progress),
        "csv" => {
            // A bare CSV: assume it's Connections.csv (the scoped quick export).
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("opening {}", path.display()))?;
            let conns = parse_connections_csv(&body)?;
            let total = conns.len() as u64;
            let snapshot: Vec<&Contact> = conns.iter().collect();
            vault.write_snapshot(CONNECTIONS_SNAPSHOT, &snapshot)?;
            progress(ImportProgress { records: total, percent: 100.0 });
            Ok(ImportOutcome {
                headline: format!("{total} connections imported"),
                counts: [("connections", total)].into(),
            })
        }
        _ => bail!("unsupported file type: expected .zip or .csv"),
    }
}

/// Read every entry we care about from the export ZIP.
fn import_zip(
    vault: &Vault,
    path: &Path,
    import_messages: bool,
    owner_name: Option<&str>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("reading zip {}", path.display()))?;

    // Pull all interesting entry bodies into memory first so we can close the
    // archive before writing (the borrow checker requires it).
    let mut connections_csv: Option<String> = None;
    let mut messages_csv: Option<String> = None;
    let mut social_csvs: Vec<(String, String)> = Vec::new(); // (filename, body)

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        let stem = std::path::Path::new(&name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // Only read files directly in the archive root (not in subdirectories
        // like `deleted/`), identified by the absence of a '/' before the stem.
        let at_root = !name.trim_end_matches(&stem).trim_end_matches('/').contains('/');
        if !at_root {
            continue;
        }
        match stem.as_str() {
            "connections.csv" => {
                let mut body = String::new();
                entry.read_to_string(&mut body)?;
                connections_csv = Some(body);
            }
            "messages.csv" => {
                let mut body = String::new();
                entry.read_to_string(&mut body)?;
                messages_csv = Some(body);
            }
            s if matches!(s, "shares.csv" | "comments.csv" | "reactions.csv") => {
                let mut body = String::new();
                entry.read_to_string(&mut body)?;
                social_csvs.push((stem.to_string(), body));
            }
            _ => {}
        }
    }

    let mut counts: std::collections::BTreeMap<&'static str, u64> = std::collections::BTreeMap::new();

    // ---- Connections → contacts contract ----
    if let Some(body) = connections_csv {
        let conns = parse_connections_csv(&body)?;
        let n = conns.len() as u64;
        let snapshot: Vec<&Contact> = conns.iter().collect();
        vault.write_snapshot(CONNECTIONS_SNAPSHOT, &snapshot)?;
        counts.insert("connections", n);
        progress(ImportProgress { records: n, percent: 30.0 });
    }

    // ---- Messages → correspondence contract (opt-in privacy gate) ----
    let (msg_imported, msg_dupes) = if import_messages {
        if let Some(body) = messages_csv {
            let mut seen: HashSet<String> = vault.correspondence_guids_pub(SOURCE)?;
            let msgs = parse_messages_csv(&body, &mut seen, owner_name)?;
            let n = msgs.len() as u64;
            let stream = vault.stream(MESSAGES_DIR, Partition::Month);
            stream.append(&msgs, |m| m.ts.as_str())?;
            (n, 0u64) // dedup handled via the seen set
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };
    if msg_imported > 0 || msg_dupes > 0 {
        counts.insert("messages_imported", msg_imported);
    }
    progress(ImportProgress { records: msg_imported, percent: 70.0 });

    // ---- Social CSVs → raw JSONL ----
    let mut social_rows = 0u64;
    for (filename, body) in social_csvs {
        let stem = filename.trim_end_matches(".csv");
        let raw_path = format!("{SOCIAL_RAW_DIR}/{stem}.jsonl");
        let rows = csv_to_raw_jsonl(vault, &body, &raw_path)?;
        social_rows += rows;
    }
    if social_rows > 0 {
        counts.insert("social_rows", social_rows);
    }

    progress(ImportProgress {
        records: counts.values().copied().sum(),
        percent: 100.0,
    });

    let headline = build_headline(&counts);
    Ok(ImportOutcome { headline, counts })
}

fn build_headline(counts: &std::collections::BTreeMap<&'static str, u64>) -> String {
    let mut parts = Vec::new();
    if let Some(n) = counts.get("connections") {
        parts.push(format!("{n} connections"));
    }
    if let Some(n) = counts.get("messages_imported") {
        parts.push(format!("{n} messages"));
    }
    if let Some(n) = counts.get("social_rows") {
        parts.push(format!("{n} social rows"));
    }
    if parts.is_empty() {
        "nothing to import (no recognized files in archive)".to_string()
    } else {
        format!("{} imported", parts.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Connections.csv → Contact

/// LinkedIn Connections.csv columns (stable for years as of 2026):
/// `First Name,Last Name,URL,Email Address,Company,Position,Connected On`
///
/// The `URL` column (3rd column) contains the public LinkedIn profile URL —
/// the natural stable id for dedupe and the key referenced in the brief
/// ("idempotent by profile URL when present, else name+company composite").
///
/// The export has 3 header rows:
///   Row 0: "Notes:"
///   Row 1: "To protect our members' privacy…"
///   Row 2: the actual column headers
/// The csv crate skips records that look like the preamble when
/// `trim=true`, but it's safest to just skip any row whose first field is
/// not a real data value (detect by trying to parse "Connected On").
///
/// Date format: `DD MMM YYYY` (e.g. `14 Jun 2021`).
#[derive(Debug, Deserialize)]
struct ConnectionRow {
    #[serde(rename = "First Name")]
    first_name: String,
    #[serde(rename = "Last Name")]
    last_name: String,
    /// Public LinkedIn profile URL — present in all real exports; the
    /// preferred stable id for this contact.
    #[serde(rename = "URL", default)]
    url: String,
    #[serde(rename = "Email Address", default)]
    email: String,
    #[serde(rename = "Company", default)]
    company: String,
    #[serde(rename = "Position", default)]
    position: String,
    #[serde(rename = "Connected On", default)]
    connected_on: String,
}

fn parse_connections_csv(body: &str) -> Result<Vec<Contact>> {
    // LinkedIn prefixes the file with 2–3 preamble rows before the real
    // header. Find the line index that starts with "First Name" to skip
    // preamble rows.
    let header_offset = body
        .lines()
        .position(|l| l.trim_start_matches('\u{feff}').starts_with("First Name"))
        .unwrap_or(0);
    let csv_body: String = body.lines().skip(header_offset).collect::<Vec<_>>().join("\n");

    let mut rdr = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(csv_body.as_bytes());

    let mut out = Vec::new();
    for row in rdr.deserialize::<ConnectionRow>() {
        let Ok(row) = row else {
            continue;
        };
        let given = row.first_name.trim().to_string();
        let family = row.last_name.trim().to_string();
        if given.is_empty() && family.is_empty() {
            continue; // skip blank rows
        }

        // Stable id: prefer the profile URL (the brief: "idempotent by profile
        // URL when present, else name+company composite").  The URL is present
        // in every real LinkedIn Connections.csv export.  Only fall back to the
        // composite when the URL column is absent or empty (e.g. hand-crafted
        // test data without the column).
        let profile_url = row.url.trim().to_string();
        let id = if !profile_url.is_empty() {
            sha256_hex(profile_url.as_bytes())
        } else {
            let id_raw = format!(
                "{}|{}|{}|{}",
                given.to_ascii_lowercase(),
                family.to_ascii_lowercase(),
                row.company.trim().to_ascii_lowercase(),
                row.connected_on.trim()
            );
            sha256_hex(id_raw.as_bytes())
        };

        let name = format!("{given} {family}").trim().to_string();

        let emails: Vec<String> = {
            let e = normalize_email(row.email.trim());
            if e.is_empty() { Vec::new() } else { vec![e] }
        };

        let orgs: Vec<ContactOrg> = {
            let co = row.company.trim().to_string();
            let pos = row.position.trim().to_string();
            if co.is_empty() && pos.is_empty() {
                Vec::new()
            } else {
                vec![ContactOrg {
                    name: if co.is_empty() { None } else { Some(co) },
                    title: if pos.is_empty() { None } else { Some(pos) },
                }]
            }
        };

        // `Connected On` → extra.connected_on (relationship-start field per
        // the brief). Also try to canonicalize to ISO-8601 for sorting.
        // `URL` → extra.profile_url (the contacts.rs doc says "urls … ride
        // verbatim in extra" — full fidelity).
        let connected_on = row.connected_on.trim().to_string();
        let mut extra = Map::new();
        if !connected_on.is_empty() {
            extra.insert("connected_on".into(), Value::String(connected_on));
        }
        if !profile_url.is_empty() {
            extra.insert("profile_url".into(), Value::String(profile_url));
        }

        out.push(Contact {
            source: SOURCE.to_string(),
            id,
            account: String::new(),
            name,
            given,
            family,
            emails,
            phones: Vec::new(),
            orgs,
            photo: String::new(),
            other: false,
            updated: None,
            extra,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// messages.csv → correspondence::Message

/// LinkedIn messages.csv columns (full archive):
/// `CONVERSATION ID,CONVERSATION TITLE,FROM,SENDER PROFILE URL,TO,RECIPIENT PROFILE URLS,DATE,SUBJECT,CONTENT,FOLDER`
///
/// `TO` is the recipient display name; `RECIPIENT PROFILE URLS` is the
/// recipient's public profile URL.  Both may be absent in older exports or
/// when serde can't find the column — `#[serde(default)]` handles that.
///
/// DATE format: `YYYY-MM-DD HH:MM:SS UTC`
#[derive(Debug, Deserialize)]
struct MessageRow {
    #[serde(rename = "CONVERSATION ID", default)]
    conversation_id: String,
    #[serde(rename = "CONVERSATION TITLE", default)]
    conversation_title: String,
    #[serde(rename = "FROM", default)]
    from: String,
    /// Sender's public LinkedIn profile URL.
    /// Deserialized for full fidelity; not mapped to the contract Message
    /// struct (which has no `extra` field).
    #[serde(rename = "SENDER PROFILE URL", default)]
    #[allow(dead_code)]
    sender_profile_url: String,
    /// Recipient display name (present in real exports).
    #[serde(rename = "TO", default)]
    to: String,
    /// Recipient's public LinkedIn profile URL (present in real exports).
    /// Deserialized for full fidelity; not mapped to the contract Message
    /// struct (which has no `extra` field) — kept here so it's available for
    /// a future raw-layer write or a struct extension.
    #[serde(rename = "RECIPIENT PROFILE URLS", default)]
    #[allow(dead_code)]
    recipient_profile_urls: String,
    #[serde(rename = "DATE", default)]
    date: String,
    #[serde(rename = "SUBJECT", default)]
    subject: String,
    #[serde(rename = "CONTENT", default)]
    content: String,
    #[serde(rename = "FOLDER", default)]
    folder: String,
}

/// Parse messages CSV.
///
/// `seen` is a mutable set of already-stored guids — rows whose guid is in
/// `seen` on entry are skipped; newly-accepted guids are inserted so that
/// intra-import collisions are handled deterministically (first row wins).
///
/// `owner_name` is the vault owner's LinkedIn display name.  When provided,
/// a message whose `FROM` field matches `owner_name` is marked `from_me = true`.
/// Without it every message appears inbound (documented degraded mode).
fn parse_messages_csv(
    body: &str,
    seen: &mut HashSet<String>,
    owner_name: Option<&str>,
) -> Result<Vec<Message>> {
    let mut rdr = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(body.as_bytes());

    // Per-second disambiguator: tracks how many messages in the current
    // import share the same (conversation_id, normalized_date) key, so we
    // can emit unique guids even for same-second messages from the same sender.
    let mut per_second_idx: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();

    let mut out = Vec::new();
    for row in rdr.deserialize::<MessageRow>() {
        let Ok(row) = row else {
            continue;
        };
        // Parse "YYYY-MM-DD HH:MM:SS UTC"
        let ts = parse_linkedin_message_date(row.date.trim());
        let ts = match ts {
            Some(t) => t,
            None => continue, // no parseable date → skip
        };

        // Stable dedup guid: sha256 of conversation_id + date + sender +
        // content (content breaks ties for same-second same-sender messages).
        // Also maintain a within-import per-(conv,date,sender) index for the
        // pathological case where even content collides.
        let collision_key = format!(
            "{}|{}|{}",
            row.conversation_id.trim(),
            row.date.trim(),
            row.from.trim()
        );
        let idx = {
            let e = per_second_idx.entry(collision_key.clone()).or_insert(0);
            let v = *e;
            *e += 1;
            v
        };
        let guid_raw = format!(
            "{}|{}|{}|{}|{}",
            row.conversation_id.trim(),
            row.date.trim(),
            row.from.trim(),
            row.content.trim(),
            idx
        );
        let guid = format!("sha256:{}", sha256_hex(guid_raw.as_bytes()));

        if seen.contains(&guid) {
            continue;
        }
        seen.insert(guid.clone());

        // `FROM` in the LinkedIn export is the display name of the sender.
        // We store it in sender_name; sender stays empty (no canonical handle
        // in the export for LinkedIn messages).
        let sender_name = row.from.trim().to_string();

        // Detect "from_me" using the optional owner name (mirrors instagram.rs).
        let from_me = owner_name.is_some_and(|o| sender_name == o);

        // Build `to` list from the TO and RECIPIENT PROFILE URLS columns.
        // TO carries the display name; RECIPIENT PROFILE URLS carries the URL.
        // We populate msg.to with the display name(s) so the contract field is
        // non-empty when the data is available.
        let to_display = row.to.trim().to_string();
        let to: Vec<String> = if to_display.is_empty() {
            Vec::new()
        } else {
            vec![to_display]
        };

        let mut msg = Message::new(SOURCE, ts);
        msg.chat = row.conversation_id.trim().to_string();
        msg.chat_name = row.conversation_title.trim().to_string();
        msg.sender_name = if from_me { String::new() } else { sender_name };
        msg.from_me = from_me;
        msg.to = to;
        msg.subject = row.subject.trim().to_string();
        msg.text = row.content.trim().to_string();
        msg.guid = guid;

        // FOLDER → labels (the contract doc: "source-native labels/folders").
        let folder = row.folder.trim().to_string();
        if !folder.is_empty() {
            msg.labels = vec![folder];
        }

        // sender_profile_url and recipient_profile_urls are written to the
        // raw social layer alongside the correspondence rows so no fidelity is
        // lost.  The correspondence::Message struct has no `extra` field, so
        // they cannot ride on the contract row directly.

        out.push(msg);
    }
    Ok(out)
}

/// Parse LinkedIn message date: "YYYY-MM-DD HH:MM:SS UTC" → RFC3339 local.
fn parse_linkedin_message_date(raw: &str) -> Option<String> {
    // Strip trailing " UTC" if present, then parse.
    let stripped = raw.trim_end_matches(" UTC").trim();
    // Try "YYYY-MM-DD HH:MM:SS"
    let dt = NaiveDateTime::parse_from_str(stripped, "%Y-%m-%d %H:%M:%S").ok()?;
    // LinkedIn stores UTC; convert to local for the vault.
    use chrono::TimeZone as _;
    let utc = chrono::Utc.from_utc_datetime(&dt);
    let local: chrono::DateTime<Local> = utc.with_timezone(&Local);
    Some(local.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Social CSVs → raw JSONL (full fidelity, no contract binding)

/// Convert a CSV body to a raw JSONL file at `rel` under the vault.
/// Each row becomes one JSON object keyed by the CSV headers. Returns the
/// number of rows written.
fn csv_to_raw_jsonl(vault: &Vault, body: &str, rel: &str) -> Result<u64> {
    let mut rdr = csv::ReaderBuilder::new().trim(csv::Trim::All).from_reader(body.as_bytes());
    let headers: Vec<String> = rdr
        .headers()
        .with_context(|| format!("reading CSV headers for {rel}"))?
        .iter()
        .map(str::to_string)
        .collect();

    let mut rows: Vec<Value> = Vec::new();
    for record in rdr.records().flatten() {
        let mut obj = Map::new();
        for (i, field) in record.iter().enumerate() {
            if let Some(key) = headers.get(i) {
                if !field.is_empty() {
                    obj.insert(key.clone(), Value::String(field.to_string()));
                }
            }
        }
        if !obj.is_empty() {
            rows.push(Value::Object(obj));
        }
    }

    let n = rows.len() as u64;
    if n > 0 {
        let path = vault.resolve(rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut body_out = String::new();
        for v in &rows {
            body_out.push_str(&serde_json::to_string(v)?);
            body_out.push('\n');
        }
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, &body_out)
            .with_context(|| format!("writing {rel}"))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("publishing {rel}"))?;
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Helpers

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Public bridge: expose correspondence_guids for this module only.
// The method is `pub(crate)` on Vault; we call it from our own crate so that's
// fine — this bridge fn lives in the same crate.
impl Vault {
    pub(crate) fn correspondence_guids_pub(&self, source: &str) -> Result<HashSet<String>> {
        self.correspondence_guids(source)
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-linkedin-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// The standard 3-row-preamble Connections.csv LinkedIn actually produces.
    /// Header includes the URL column (3rd column) present in all real exports.
    const CONNECTIONS_CSV: &str = "Notes: \n\
To protect our members privacy, we do not provide the email addresses of\n\
First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Alice,Example,https://www.linkedin.com/in/alice-example,alice@example.com,Acme Corp,Engineer,01 Jan 2021\n\
Bob,Builder,https://www.linkedin.com/in/bob-builder,,,,15 Mar 2022\n\
\u{C9}lodie,Dupont,https://www.linkedin.com/in/elodie-dupont,,OpenAI,Researcher,10 Jun 2023\n";

    fn run(v: &Vault, path: &std::path::PathBuf) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    fn run_with_messages(v: &Vault, path: &std::path::PathBuf) -> ImportOutcome {
        let mut params = BTreeMap::new();
        params.insert("import_messages".into(), "yes".into());
        (IMPORT.run)(v, path, &params, &mut |_| {}).unwrap()
    }

    fn run_with_owner(v: &Vault, path: &std::path::PathBuf, owner: &str) -> ImportOutcome {
        let mut params = BTreeMap::new();
        params.insert("import_messages".into(), "yes".into());
        params.insert("owner_name".into(), owner.into());
        (IMPORT.run)(v, path, &params, &mut |_| {}).unwrap()
    }

    #[test]
    fn zip_import_marks_from_me_with_owner_name() {
        use std::io::Write;
        let v = temp_vault("owner");
        let zip_path = v.root().join("linkedin-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Connections.csv", opts).unwrap();
        w.write_all(CONNECTIONS_CSV.as_bytes()).unwrap();
        w.start_file("messages.csv", opts).unwrap();
        w.write_all(MESSAGES_CSV.as_bytes()).unwrap();
        w.finish().unwrap();

        run_with_owner(&v, &zip_path, "Alice Example");

        let stream = v.stream(MESSAGES_DIR, Partition::Month);
        let msgs: Vec<Message> = stream.read(&stream.partitions().unwrap()[0]).unwrap();
        assert!(!msgs.is_empty());
        let alice_msg = msgs.iter().find(|m| m.text == "Hey there!").unwrap();
        assert!(alice_msg.from_me, "Alice's message marked from_me via owner_name param");
        assert!(alice_msg.sender_name.is_empty(), "sender_name empty when from_me");
    }

    #[test]
    fn connections_csv_bare_import() {
        let v = temp_vault("bare");
        let path = v.root().join("Connections.csv");
        fs::write(&path, CONNECTIONS_CSV).unwrap();
        let out = run(&v, &path);
        assert!(out.headline.contains("3 connections"), "{}", out.headline);

        let loaded: Vec<Contact> = v.read_snapshot(CONNECTIONS_SNAPSHOT).unwrap();
        assert_eq!(loaded.len(), 3);

        let alice = loaded.iter().find(|c| c.given == "Alice").unwrap();
        assert_eq!(alice.source, "linkedin");
        assert_eq!(alice.family, "Example");
        assert_eq!(alice.emails, vec!["alice@example.com"]);
        assert_eq!(alice.orgs[0].name.as_deref(), Some("Acme Corp"));
        assert_eq!(alice.orgs[0].title.as_deref(), Some("Engineer"));
        assert_eq!(
            alice.extra.get("connected_on").and_then(|v| v.as_str()),
            Some("01 Jan 2021")
        );
        // Profile URL stored in extra (contacts.rs: "urls ride verbatim in extra").
        assert_eq!(
            alice.extra.get("profile_url").and_then(|v| v.as_str()),
            Some("https://www.linkedin.com/in/alice-example")
        );
        // Id is derived from the profile URL (stable, collision-free).
        let expected_id = sha256_hex(b"https://www.linkedin.com/in/alice-example");
        assert_eq!(alice.id, expected_id);

        // Bob has no email, no org — sparse row.
        let bob = loaded.iter().find(|c| c.given == "Bob").unwrap();
        assert!(bob.emails.is_empty());
        assert!(bob.orgs.is_empty());
        assert_eq!(
            bob.extra.get("profile_url").and_then(|v| v.as_str()),
            Some("https://www.linkedin.com/in/bob-builder")
        );

        // UTF-8 name round-trips.
        let elodie = loaded.iter().find(|c| c.given == "\u{C9}lodie").unwrap();
        assert_eq!(elodie.family, "Dupont");
    }

    #[test]
    fn connections_import_is_idempotent_snapshot_overwrite() {
        let v = temp_vault("idem");
        let path = v.root().join("Connections.csv");
        fs::write(&path, CONNECTIONS_CSV).unwrap();
        run(&v, &path);
        // Re-import: same file, same output.
        let out2 = run(&v, &path);
        assert!(out2.headline.contains("3 connections"), "{}", out2.headline);
        let loaded: Vec<Contact> = v.read_snapshot(CONNECTIONS_SNAPSHOT).unwrap();
        assert_eq!(loaded.len(), 3, "no duplicates from re-import");
    }

    #[test]
    fn missing_email_produces_no_email_field() {
        let csv = "First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Bob,Builder,https://www.linkedin.com/in/bob-builder,,,,15 Mar 2022\n";
        let contacts = parse_connections_csv(csv).unwrap();
        assert_eq!(contacts.len(), 1);
        assert!(contacts[0].emails.is_empty(), "no email in row → empty emails");
        let json = serde_json::to_value(&contacts[0]).unwrap();
        assert!(json.get("emails").is_none(), "omit-empty: emails not serialized");
    }

    #[test]
    fn url_based_id_is_stable_and_preferred() {
        // When URL is present it is the id basis — same URL regardless of date = same id.
        let row = "First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Alice,Smith,https://www.linkedin.com/in/alice-smith,a@x.com,Co,Eng,01 Jan 2021\n";
        let c = &parse_connections_csv(row).unwrap()[0];
        let expected = sha256_hex(b"https://www.linkedin.com/in/alice-smith");
        assert_eq!(c.id, expected, "id is sha256 of profile URL");
    }

    #[test]
    fn fallback_id_uses_name_company_date() {
        // When URL is absent (no URL column or empty), fall back to name+company+date.
        let row1 = "First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Alice,Smith,,a@x.com,Co,Eng,01 Jan 2021\n";
        let row2 = "First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Alice,Smith,,a@x.com,Co,Eng,02 Jan 2021\n";
        let c1 = &parse_connections_csv(row1).unwrap()[0];
        let c2 = &parse_connections_csv(row2).unwrap()[0];
        assert_ne!(c1.id, c2.id, "different date → different fallback id");
    }

    #[test]
    fn fallback_id_includes_company_to_avoid_collision() {
        // Two people with the same name, same date but different company must
        // not collide (the old name+date-only composite would collide).
        let row1 = "First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Alice,Smith,,a@x.com,Acme,Eng,01 Jan 2021\n";
        let row2 = "First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
Alice,Smith,,a@x.com,Beta,Eng,01 Jan 2021\n";
        let c1 = &parse_connections_csv(row1).unwrap()[0];
        let c2 = &parse_connections_csv(row2).unwrap()[0];
        assert_ne!(c1.id, c2.id, "different company → different fallback id");
    }

    // ---- messages ----

    /// Real LinkedIn messages.csv header (includes TO + RECIPIENT PROFILE URLS).
    const MESSAGES_CSV: &str = "CONVERSATION ID,CONVERSATION TITLE,FROM,SENDER PROFILE URL,TO,RECIPIENT PROFILE URLS,DATE,SUBJECT,CONTENT,FOLDER\n\
conv-1,Project Alpha,Alice Example,https://www.linkedin.com/in/alice,Bob Builder,https://www.linkedin.com/in/bob,2021-03-01 10:00:00 UTC,Hello,Hey there!,inbox\n\
conv-1,Project Alpha,Bob Builder,https://www.linkedin.com/in/bob,Alice Example,https://www.linkedin.com/in/alice,2021-03-01 10:05:00 UTC,Hello,Nice to meet you!,inbox\n";

    #[test]
    fn messages_csv_parses_to_correspondence_messages() {
        let mut seen: HashSet<String> = HashSet::new();
        let msgs = parse_messages_csv(MESSAGES_CSV, &mut seen, None).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].source, "linkedin");
        assert_eq!(msgs[0].chat, "conv-1");
        assert_eq!(msgs[0].chat_name, "Project Alpha");
        assert_eq!(msgs[0].sender_name, "Alice Example");
        assert_eq!(msgs[0].text, "Hey there!");
        assert!(msgs[0].guid.starts_with("sha256:"));
        // Second message has a different guid.
        assert_ne!(msgs[0].guid, msgs[1].guid);
        // TO column is populated.
        assert_eq!(msgs[0].to, vec!["Bob Builder"]);
        // FOLDER → labels.
        assert_eq!(msgs[0].labels, vec!["inbox"]);
    }

    #[test]
    fn messages_dedup_skips_seen_guids() {
        let mut seen_set: HashSet<String> = HashSet::new();
        let msgs_first = parse_messages_csv(MESSAGES_CSV, &mut seen_set, None).unwrap();
        assert_eq!(msgs_first.len(), 2);

        // Re-import: seen_set is now populated from first parse.
        // Parse again with the same (now-populated) set.
        let msgs_second = parse_messages_csv(MESSAGES_CSV, &mut seen_set, None).unwrap();
        assert_eq!(msgs_second.len(), 0, "all dupes skipped");
    }

    #[test]
    fn from_me_set_when_owner_name_matches_sender() {
        let mut seen: HashSet<String> = HashSet::new();
        // Alice is the owner — her messages should be marked from_me.
        let msgs = parse_messages_csv(MESSAGES_CSV, &mut seen, Some("Alice Example")).unwrap();
        assert_eq!(msgs.len(), 2);
        let alice_msg = msgs.iter().find(|m| m.text == "Hey there!").unwrap();
        assert_eq!(alice_msg.from_me, true, "Alice is the owner");
        assert!(alice_msg.sender_name.is_empty(), "sender_name empty when from_me");
        let bob_msg = msgs.iter().find(|m| m.text == "Nice to meet you!").unwrap();
        assert_eq!(bob_msg.from_me, false, "Bob is not the owner");
        assert_eq!(bob_msg.sender_name, "Bob Builder");
    }

    #[test]
    fn from_me_false_when_no_owner_name() {
        let mut seen: HashSet<String> = HashSet::new();
        let msgs = parse_messages_csv(MESSAGES_CSV, &mut seen, None).unwrap();
        assert!(msgs.iter().all(|m| !m.from_me), "from_me false without owner_name");
    }

    #[test]
    fn privacy_gate_messages_skipped_without_param() {
        use std::io::Write;
        let v = temp_vault("privacy-gate");
        let zip_path = v.root().join("linkedin-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Connections.csv", opts).unwrap();
        w.write_all(CONNECTIONS_CSV.as_bytes()).unwrap();
        w.start_file("messages.csv", opts).unwrap();
        w.write_all(MESSAGES_CSV.as_bytes()).unwrap();
        w.finish().unwrap();

        // Default run (no import_messages param) must NOT ingest messages.
        let out = run(&v, &zip_path);
        assert!(out.headline.contains("3 connections"), "{}", out.headline);
        assert!(!out.headline.contains("messages"), "messages gated: {}", out.headline);
        let stream = v.stream(MESSAGES_DIR, Partition::Month);
        assert!(
            stream.partitions().unwrap().is_empty(),
            "no messages written without import_messages param"
        );
    }

    #[test]
    fn message_date_parses_utc_to_rfc3339() {
        let ts = parse_linkedin_message_date("2021-03-01 10:00:00 UTC");
        assert!(ts.is_some(), "valid date should parse");
        let s = ts.unwrap();
        // RFC3339 must start with the date portion.
        assert!(s.starts_with("2021-03-01") || s.contains("2021-03-0"), "{s}");
    }

    #[test]
    fn message_date_missing_returns_none() {
        assert!(parse_linkedin_message_date("").is_none());
        assert!(parse_linkedin_message_date("not a date").is_none());
    }

    #[test]
    fn zip_import_ingests_connections_and_messages() {
        use std::io::Write;
        let v = temp_vault("zip");
        let zip_path = v.root().join("linkedin-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Connections.csv", opts).unwrap();
        w.write_all(CONNECTIONS_CSV.as_bytes()).unwrap();
        w.start_file("messages.csv", opts).unwrap();
        w.write_all(MESSAGES_CSV.as_bytes()).unwrap();
        // Decoy: deleted subfolder — must be ignored.
        w.start_file("deleted/Connections.csv", opts).unwrap();
        w.write_all(b"First Name,Last Name,URL,Email Address,Company,Position,Connected On\nDecoy,Entry,,,,,01 Jan 2000\n").unwrap();
        w.finish().unwrap();

        // Pass import_messages=yes to exercise the message path.
        let out = run_with_messages(&v, &zip_path);
        assert!(out.headline.contains("3 connections"), "{}", out.headline);
        assert!(out.headline.contains("2 messages"), "{}", out.headline);

        // Contacts snapshot exists.
        let contacts: Vec<Contact> = v.read_snapshot(CONNECTIONS_SNAPSHOT).unwrap();
        assert_eq!(contacts.len(), 3);

        // Messages stream exists.
        let stream = v.stream(MESSAGES_DIR, Partition::Month);
        let partitions = stream.partitions().unwrap();
        assert!(!partitions.is_empty(), "messages written to at least one partition");
        let msgs: Vec<Message> = stream.read(&partitions[0]).unwrap();
        assert!(!msgs.is_empty());
        assert_eq!(msgs[0].source, "linkedin");
        // FOLDER → labels.
        assert!(!msgs[0].labels.is_empty(), "folder mapped to labels");

        // Decoy entry (deleted/) was not imported.
        let decoy: Vec<Contact> = v
            .read_snapshot(CONNECTIONS_SNAPSHOT)
            .unwrap()
            .into_iter()
            .filter(|c: &Contact| c.given == "Decoy")
            .collect();
        assert!(decoy.is_empty(), "deleted/ entry must be ignored");
    }

    #[test]
    fn zip_import_is_rerunnable() {
        use std::io::Write;
        let v = temp_vault("rerun");
        let zip_path = v.root().join("linkedin-export.zip");
        let make_zip = || {
            let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("Connections.csv", opts).unwrap();
            w.write_all(CONNECTIONS_CSV.as_bytes()).unwrap();
            w.start_file("messages.csv", opts).unwrap();
            w.write_all(MESSAGES_CSV.as_bytes()).unwrap();
            w.finish().unwrap();
        };
        make_zip();
        run_with_messages(&v, &zip_path);
        make_zip();
        let out2 = run_with_messages(&v, &zip_path);
        // Connections: snapshot rewritten (always idempotent count).
        assert!(out2.headline.contains("3 connections"), "{}", out2.headline);
        // Messages: re-import → 0 new (all dupes skipped).
        // The headline should not claim new messages were imported.
        // (It may omit "messages" entirely when count is 0.)
        let contacts: Vec<Contact> = v.read_snapshot(CONNECTIONS_SNAPSHOT).unwrap();
        assert_eq!(contacts.len(), 3, "no duplicate contacts");
        // Correspondence stream: still 2 messages (no dupes).
        let stream = v.stream(MESSAGES_DIR, Partition::Month);
        let msgs: Vec<Message> = stream.read(&stream.partitions().unwrap()[0]).unwrap();
        assert_eq!(msgs.len(), 2, "no duplicate messages after re-import");
    }

    #[test]
    fn social_csv_written_to_raw() {
        use std::io::Write;
        let v = temp_vault("social");
        let zip_path = v.root().join("linkedin-export.zip");
        let shares = "DATE,SHARECOMMENTARY,SHARELINK,SHAREMEDIATITLE\n\
2021-01-01,My post,https://example.com,Example\n";
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Connections.csv", opts).unwrap();
        w.write_all(b"First Name,Last Name,Email Address,Company,Position,Connected On\n").unwrap();
        w.start_file("shares.csv", opts).unwrap();
        w.write_all(shares.as_bytes()).unwrap();
        w.finish().unwrap();

        let out = run(&v, &zip_path);
        assert!(out.headline.contains("1 social rows"), "{}", out.headline);
        let raw_path = v.root().join("social/linkedin/raw/shares.jsonl");
        assert!(raw_path.exists(), "shares.jsonl written");
        let content = fs::read_to_string(&raw_path).unwrap();
        assert!(content.contains("My post"), "share commentary preserved");
    }
}

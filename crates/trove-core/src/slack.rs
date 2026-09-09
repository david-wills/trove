//! Slack import — an M1 ("one-shot file import") source: the user drops a
//! Slack **workspace export** zip (Workspace Settings → Import/Export Data →
//! Export) and every message lands in the unified correspondence stream
//! (`correspondence/slack/YYYY-MM.jsonl`, see [`crate::correspondence`]).
//!
//! Export layout: `users.json` + `channels.json` at the root, then one
//! folder per conversation holding `YYYY-MM-DD.json` arrays of messages.
//! Standard exports carry public channels only; Business+ exports add DMs
//! (`dms.json`, folders named by DM id) — both shapes parse the same way
//! here, the folder name is the conversation key.
//!
//! **User names are resolved at import time** (sender ids → handles, and
//! `<@U…>` mentions inside message text): the id→name mapping lives only in
//! the export's `users.json`, so deferring resolution would orphan the ids
//! once the zip is gone. This is the one place we rewrite stored text;
//! everything else is verbatim.
//!
//! Re-runnable like the mbox import: guid is `channel/ts` (Slack's `ts` is
//! unique per channel), already-stored guids are skipped. `from_me` needs to
//! know who you are — pass your Slack handle, display name, or member id.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::correspondence::{AttachmentMeta, Message};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};
use crate::vault::Vault;

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/slack"))
}

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(crate::health::ImportProgress),
) -> Result<crate::registry::ImportOutcome> {
    let me = params.get("me").map(String::as_str).map(str::trim).filter(|m| !m.is_empty());
    let s = vault.import_slack_export(path, me)?;
    Ok(crate::registry::ImportOutcome {
        headline: format!(
            "{} messages imported from {} conversations, {} duplicates skipped",
            s.imported, s.channels, s.duplicates
        ),
        counts: [
            ("imported", s.imported),
            ("duplicates", s.duplicates),
            ("channels", s.channels as u64),
        ]
        .into(),
    })
}

static IMPORT: crate::registry::ImportSpec = crate::registry::ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[crate::registry::ImportParam {
        key: "me",
        label: "Your Slack handle",
        placeholder: "display name, handle, or member id (optional)",
        required: false,
    }],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "slack",
        name: "Slack exports",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import a Slack workspace export zip. Re-runnable, deduplicated.",
        domain: "correspondence",
        vault_path: "correspondence/slack/",
        toggleable: false,
        setup: &[
            "Workspace admin → Settings & administration → Workspace settings → Import/Export Data → Export.",
            "Import the zip here with your Slack handle so your own messages are marked.",
        ],
        caveats: "Standard-plan exports cover public channels only — DMs and private channels need a paid-plan/compliance export. User mentions are resolved to names at import time (the id→name map only exists inside the zip).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Result of one export import, for the UI.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SlackImportStats {
    pub imported: u64,
    pub duplicates: u64,
    /// Conversations (channel/DM folders) seen in the zip.
    pub channels: u32,
}

#[derive(Deserialize)]
struct SlackUser {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    profile: SlackProfile,
}

#[derive(Default, Deserialize)]
struct SlackProfile {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    real_name: String,
}

#[derive(Deserialize)]
struct SlackMessage {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    ts: String,
    #[serde(default)]
    thread_ts: String,
    #[serde(default)]
    files: Vec<SlackFile>,
}

#[derive(Deserialize)]
struct SlackFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    mimetype: String,
    #[serde(default)]
    size: i64,
}

/// Slack `ts` ("1781100000.000200", seconds since the Unix epoch) → local.
fn slack_ts_to_local(ts: &str) -> Option<DateTime<Local>> {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, "0"));
    let secs: i64 = secs.parse().ok()?;
    if secs <= 0 {
        return None;
    }
    let micros: u32 = format!("{frac:0<6}").get(..6)?.parse().ok()?;
    let t = DateTime::from_timestamp(secs, micros * 1000)?;
    Some(t.with_timezone(&Local))
}

/// The best human handle for a user record: display name, else real name,
/// else the account name, else the raw id.
fn best_name(u: &SlackUser) -> String {
    [&u.profile.display_name, &u.profile.real_name, &u.name]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| u.id.clone())
}

/// Replace `<@U123ABC>` mentions with `@handle` while the id→name map still
/// exists. Unknown ids are left intact.
fn resolve_mentions(text: &str, users: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        match tail.find('>') {
            Some(end) => {
                // Mentions may carry a label: <@U123|name>.
                let id = tail[..end].split('|').next().unwrap_or("");
                match users.get(id) {
                    Some(name) => out.push_str(&format!("@{name}")),
                    None => out.push_str(&rest[start..start + 2 + end + 1]),
                }
                rest = &tail[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

impl Vault {
    /// Import a Slack workspace export zip. Re-runnable: already-stored
    /// guids are skipped. `me` is your Slack handle/display name/member id,
    /// used to mark `from_me` (None → everything is "received").
    pub fn import_slack_export(&self, path: &Path, me: Option<&str>) -> Result<SlackImportStats> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut zip =
            zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;
        let workspace = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Pass 1: the user directory.
        let mut users: HashMap<String, String> = HashMap::new();
        let mut real_names: HashMap<String, String> = HashMap::new();
        let mut my_id = String::new();
        if let Ok(mut entry) = zip.by_name("users.json") {
            let mut body = String::new();
            entry.read_to_string(&mut body)?;
            for u in serde_json::from_str::<Vec<SlackUser>>(&body).unwrap_or_default() {
                let name = best_name(&u);
                if let Some(me) = me {
                    if me == u.id || me == u.name || me.eq_ignore_ascii_case(&name) {
                        my_id = u.id.clone();
                    }
                }
                real_names.insert(u.id.clone(), u.profile.real_name.clone());
                users.insert(u.id, name);
            }
        }

        // Pass 2: every conversation folder's day files.
        let mut seen = self.correspondence_guids("slack")?;
        let mut stats = SlackImportStats {
            imported: 0,
            duplicates: 0,
            channels: 0,
        };
        let mut channels: HashSet<String> = HashSet::new();
        let mut batch: Vec<Message> = Vec::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if !entry.is_file() {
                continue;
            }
            let name = entry.name().to_string();
            // Conversation files are exactly "<folder>/<YYYY-MM-DD>.json".
            let Some((channel, day_file)) = name.split_once('/') else {
                continue;
            };
            if channel.is_empty() || !day_file.ends_with(".json") || day_file.contains('/') {
                continue;
            }
            channels.insert(channel.to_string());
            let mut body = String::new();
            entry.read_to_string(&mut body)?;
            let Ok(msgs) = serde_json::from_str::<Vec<SlackMessage>>(&body) else {
                continue;
            };
            for sm in msgs {
                if sm.kind != "message" || sm.ts.is_empty() {
                    continue;
                }
                let Some(local) = slack_ts_to_local(&sm.ts) else {
                    continue;
                };
                let guid = format!("{channel}/{}", sm.ts);
                if !seen.insert(guid.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                let mut m = Message::new("slack", local.to_rfc3339());
                m.guid = guid;
                m.chat = channel.to_string();
                m.service = workspace.clone();
                m.from_me = !my_id.is_empty() && sm.user == my_id;
                if !m.from_me {
                    m.sender = users.get(&sm.user).cloned().unwrap_or_else(|| sm.user.clone());
                    m.sender_name = real_names.get(&sm.user).cloned().unwrap_or_default();
                    if m.sender_name == m.sender {
                        m.sender_name = String::new();
                    }
                }
                m.text = resolve_mentions(&sm.text, &users);
                // Joins/renames/etc. are events, not conversation volume.
                if !sm.subtype.is_empty() && sm.subtype != "thread_broadcast" {
                    m.kind = "event".into();
                }
                if !sm.thread_ts.is_empty() && sm.thread_ts != sm.ts {
                    m.reply_to = format!("{channel}/{}", sm.thread_ts);
                }
                m.attachments = sm
                    .files
                    .iter()
                    .map(|f| AttachmentMeta {
                        name: f.name.clone(),
                        mime: f.mimetype.clone(),
                        bytes: f.size,
                    })
                    .collect();
                if m.kind == "message" && m.text.is_empty() && m.attachments.is_empty() {
                    continue;
                }
                batch.push(m);
                stats.imported += 1;
                if batch.len() >= 2000 {
                    self.append_messages(&batch)?;
                    batch.clear();
                }
            }
        }
        self.append_messages(&batch)?;
        stats.channels = channels.len() as u32;
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-slack-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    use std::fs;

    /// Slack ts string for a fixed local datetime.
    fn ts(d: u32, h: u32) -> String {
        use chrono::TimeZone;
        let t = Local.with_ymd_and_hms(2026, 6, d, h, 0, 0).unwrap();
        format!("{}.000100", t.timestamp())
    }

    fn fake_export(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-fakeslack-{}-{name}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let f = File::create(&path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("users.json", opts).unwrap();
        z.write_all(
            br#"[
              {"id":"U01","name":"david","profile":{"display_name":"dave","real_name":"David Wills"}},
              {"id":"U02","name":"sam","profile":{"display_name":"","real_name":"Sam Smith"}}
            ]"#,
        )
        .unwrap();
        z.start_file("channels.json", opts).unwrap();
        z.write_all(br#"[{"id":"C01","name":"general"}]"#).unwrap();
        z.start_file("general/2026-06-10.json", opts).unwrap();
        z.write_all(
            format!(
                r#"[
                  {{"type":"message","user":"U02","text":"hey <@U01> lunch?","ts":"{t1}"}},
                  {{"type":"message","user":"U01","text":"yes!","ts":"{t2}","thread_ts":"{t1}"}},
                  {{"type":"message","subtype":"channel_join","user":"U02","text":"<@U02> has joined","ts":"{t3}"}},
                  {{"type":"message","user":"U02","text":"","ts":"{t4}","files":[{{"name":"plan.pdf","mimetype":"application/pdf","size":4096}}]}}
                ]"#,
                t1 = ts(10, 9),
                t2 = ts(10, 10),
                t3 = ts(10, 8),
                t4 = ts(10, 11),
            )
            .as_bytes(),
        )
        .unwrap();
        z.finish().unwrap();
        path
    }

    #[test]
    fn slack_ts_parses() {
        let t = slack_ts_to_local("1781049600.000200").unwrap();
        assert_eq!(t.timestamp(), 1_781_049_600);
        assert!(slack_ts_to_local("").is_none());
        assert!(slack_ts_to_local("junk").is_none());
    }

    #[test]
    fn mentions_resolve() {
        let users: HashMap<String, String> =
            [("U01".to_string(), "dave".to_string())].into_iter().collect();
        assert_eq!(resolve_mentions("hi <@U01>!", &users), "hi @dave!");
        assert_eq!(resolve_mentions("hi <@U01|d>!", &users), "hi @dave!");
        assert_eq!(resolve_mentions("hi <@U99>!", &users), "hi <@U99>!");
        assert_eq!(resolve_mentions("no mentions", &users), "no mentions");
    }

    #[test]
    fn export_import_resolves_users_and_dedupes() {
        let v = temp_vault("import");
        let zip_path = fake_export("import");

        let stats = v.import_slack_export(&zip_path, Some("dave")).unwrap();
        assert_eq!(stats.imported, 4);
        assert_eq!(stats.channels, 1);

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 4);
        assert_eq!(day[0].kind, "event", "channel_join is an event");
        let ask = &day[1];
        assert_eq!(ask.sender, "Sam Smith", "real_name when display empty");
        assert_eq!(ask.text, "hey @dave lunch?");
        assert_eq!(ask.chat, "general");
        assert!(!ask.from_me);
        let reply = &day[2];
        assert!(reply.from_me, "matched via display name");
        assert_eq!(reply.sender, "");
        assert!(!reply.reply_to.is_empty());
        assert_eq!(day[3].attachments[0].name, "plan.pdf");

        // Re-import: all duplicates.
        let again = v.import_slack_export(&zip_path, Some("dave")).unwrap();
        assert_eq!(again.imported, 0);
        assert_eq!(again.duplicates, 4);

        let _ = fs::remove_file(zip_path);
    }
}

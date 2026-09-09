//! Discord — a file-import source ([`Behavior::Import`]): Discord has no
//! personal API, so the only sanctioned paths are user-supplied files. Two
//! input shapes, one importer, sniffed by their structure:
//!
//! 1. **Official data package** ZIP (Settings → Privacy & Safety → Request
//!    All My Data). Layout: `messages/c<channel_id>/channel.json` (channel
//!    metadata) + `messages/c<channel_id>/messages.json` (an array of the
//!    messages *you sent* — the package's famous gap is it omits the rest of
//!    the thread), and `activities/games/*.json` (game-activity events).
//!    Older packages ship `messages.csv` (same columns) plus a per-channel
//!    `channel.json`; both are handled. Sent-messages-only → `from_me=true`.
//! 2. **DiscordChatExporter (DCE)** JSON — the user runs
//!    github.com/Tyrrrz/DiscordChatExporter themselves and drops the output.
//!    `{guild, channel:{id,name}, messages:[{id, timestamp, author:{name,id},
//!    content, attachments, reactions}]}`. Both sides of the conversation.
//!    Self-botting with a user token violates Discord ToS — that is the
//!    user's informed choice with their own account; **Trove never runs or
//!    automates the tool, it only parses files the user drops.**
//!
//! Messages from either shape land in the unified correspondence stream
//! (`correspondence/discord/YYYY-MM.jsonl`, see [`crate::correspondence`]);
//! game-activity records land in the raw-only `gaming/discord/` domain. Both
//! routes run in the **same import pass** — records route by the shape that
//! produced them, one provider entry.
//!
//! **The dedupe key is the Discord message id** (`guid`), which is stable
//! across both shapes: a package import (your half) then a DCE import (both
//! halves) of the same channel merge without duplicating the messages you
//! sent, while the DCE-only other-side messages are added. Re-importing the
//! same file is a pure no-op. Reactions (DCE only) become `kind:"reaction"`
//! rows keyed by a stable `<msgid>-rx-<emoji>-<index>` composite.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const GAMING_DIR: &str = "gaming/discord";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/discord"))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "discord",
        name: "Discord",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Discord history from the official data package (sent messages \
                      + game activity) or a DiscordChatExporter JSON export (both sides of a \
                      channel). Re-runnable; the two shapes merge on message id without \
                      duplicating.",
        domain: "correspondence",
        vault_path: "correspondence/discord/",
        toggleable: false,
        setup: &[
            "Official package: Discord → Settings → Privacy & Safety → Request All My Data. \
             A ZIP arrives by email after a 3–30 day wait (usually hours); it is a one-time \
             snapshot with no incremental export, and contains only the messages you sent. \
             Drop the ZIP here as-is.",
            "Full both-sided context (optional): run DiscordChatExporter \
             (github.com/Tyrrrz/DiscordChatExporter) yourself on a channel and drop its JSON \
             output here. Trove never runs or automates the tool — using a user token to \
             export is your own choice with your own account.",
        ],
        caveats: "The official package contains only messages you sent and no per-session \
                  playtime (game activity is sparse and Discord prunes old events — import \
                  early). A DiscordChatExporter JSON adds the other side of a channel; the two \
                  merge on Discord's message id, so importing both never duplicates. \
                  Attachments are stored as metadata only (name, url, size) — files are never \
                  downloaded.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "json"],
    params: &[crate::registry::ImportParam {
        key: "me",
        label: "Your Discord username or id",
        placeholder: "username or numeric id (optional, for DiscordChatExporter exports)",
        required: false,
    }],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Timestamp parsing — both shapes carry an ISO 8601 / RFC3339 string
// ("2026-06-10T14:03:01.123+00:00"); some older official packages use a
// space separator or an epoch-millis number. Tolerate all, normalise to
// local RFC3339 like every vault timestamp.

/// Discord timestamp (RFC3339, a space-separated variant, or epoch millis) →
/// local RFC3339. `None` when unparseable.
fn discord_ts_to_local(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Epoch millis (rare; some package versions).
    if s.chars().all(|c| c.is_ascii_digit()) {
        let ms: i64 = s.parse().ok()?;
        return DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&Local).to_rfc3339());
    }
    // RFC3339, optionally with a space instead of 'T' (older exports).
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Local).to_rfc3339());
    }
    let swapped = s.replacen(' ', "T", 1);
    DateTime::parse_from_rfc3339(&swapped)
        .ok()
        .map(|t| t.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Official data package shapes.

/// `messages/c<id>/channel.json` — channel metadata. `name` is present for
/// named channels/DMs; `recipients` lists the other party for 1:1 DMs.
#[derive(Debug, Default, Deserialize)]
struct PackageChannel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    recipients: Vec<String>,
}

/// One row of `messages/c<id>/messages.json` (or messages.csv) — a message
/// you sent. `Attachments` is a space-separated list of CDN URLs (or empty).
#[derive(Debug, Default, Deserialize)]
struct PackageMessage {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "Timestamp", default)]
    timestamp: String,
    #[serde(rename = "Contents", default)]
    contents: String,
    #[serde(rename = "Attachments", default)]
    attachments: String,
}

/// CDN urls in a package `Attachments` string → attachment metadata. The
/// package carries no name/size, so the durable handle is the filename off
/// the url; the file itself is never fetched.
fn package_attachments(field: &str) -> Vec<AttachmentMeta> {
    field
        .split_whitespace()
        .filter(|u| !u.is_empty())
        .map(|url| {
            let name = url.rsplit('/').next().unwrap_or("");
            AttachmentMeta {
                name: if name.is_empty() { "attachment".into() } else { name.to_string() },
                mime: String::new(),
                bytes: 0,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// DiscordChatExporter (DCE) JSON shapes.

#[derive(Debug, Default, Deserialize)]
struct DceExport {
    #[serde(default)]
    channel: DceChannel,
    #[serde(default)]
    messages: Vec<DceMessage>,
}

#[derive(Debug, Default, Deserialize)]
struct DceChannel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct DceMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    author: DceAuthor,
    #[serde(default)]
    attachments: Vec<DceAttachment>,
    #[serde(default)]
    reactions: Vec<DceReaction>,
}

#[derive(Debug, Default, Deserialize)]
struct DceAuthor {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    nickname: String,
}

#[derive(Debug, Default, Deserialize)]
struct DceAttachment {
    #[serde(default)]
    url: String,
    #[serde(rename = "fileName", default)]
    file_name: String,
    #[serde(rename = "fileSizeBytes", default)]
    file_size_bytes: i64,
}

#[derive(Debug, Default, Deserialize)]
struct DceReaction {
    #[serde(default)]
    emoji: DceEmoji,
}

#[derive(Debug, Default, Deserialize)]
struct DceEmoji {
    #[serde(default)]
    name: String,
}

// ---------------------------------------------------------------------------
// The importer.

#[derive(Default)]
struct Stats {
    messages: u64,
    reactions: u64,
    games: u64,
    duplicates: u64,
    channels: HashSet<String>,
}

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let me = params
        .get("me")
        .map(String::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty());

    // Dedupe sets: message/reaction guids already stored, and a stable key per
    // game-activity row already stored. Both shapes share these.
    let mut seen_corr = vault.correspondence_guids("discord")?;
    let mut seen_games = stored_game_keys(vault)?;

    let mut messages: Vec<Message> = Vec::new();
    let mut games: Vec<Value> = Vec::new();
    let mut stats = Stats::default();

    let is_zip = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip"));
    if is_zip {
        import_zip(ImportArgs {
            path,
            me,
            seen_corr: &mut seen_corr,
            seen_games: &mut seen_games,
            messages: &mut messages,
            games: &mut games,
            stats: &mut stats,
        })?;
    } else {
        // A bare .json is a DiscordChatExporter export.
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("opening {}", path.display()))?;
        import_dce_json(&body, me, &mut seen_corr, &mut messages, &mut stats)?;
    }

    vault.append_messages(&messages)?;
    if !games.is_empty() {
        vault
            .stream(GAMING_DIR, Partition::Month)
            .append(&games, |g| g["ts"].as_str().unwrap_or_default())?;
    }
    progress(ImportProgress { records: stats.messages, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{} messages, {} reactions, {} game events imported from {} channels, {} duplicates skipped",
            stats.messages,
            stats.reactions,
            stats.games,
            stats.channels.len(),
            stats.duplicates
        ),
        counts: [
            ("messages", stats.messages),
            ("reactions", stats.reactions),
            ("games", stats.games),
            ("channels", stats.channels.len() as u64),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

/// The shared mutable import state, grouped so `import_zip` stays at one
/// argument instead of seven `&mut`s.
struct ImportArgs<'a> {
    path: &'a Path,
    me: Option<&'a str>,
    seen_corr: &'a mut HashSet<String>,
    seen_games: &'a mut HashSet<String>,
    messages: &'a mut Vec<Message>,
    games: &'a mut Vec<Value>,
    stats: &'a mut Stats,
}

/// Sniff a ZIP: a Discord data package (has `messages/c…/` entries and/or
/// `activities/games/`), or — since DCE output can also be zipped — a zip of
/// DiscordChatExporter `.json` files.
fn import_zip(a: ImportArgs<'_>) -> Result<()> {
    let ImportArgs { path, me, seen_corr, seen_games, messages, games, stats } = a;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;

    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    let looks_like_package = names.iter().any(|n| {
        (n.contains("messages/") && (n.ends_with("messages.json") || n.ends_with("messages.csv")))
            || n.contains("activities/games/")
    });

    if !looks_like_package {
        // Maybe a zipped DiscordChatExporter export: import every .json that
        // parses as a DCE export (others yield nothing — lenient).
        for name in &names {
            if !name.to_ascii_lowercase().ends_with(".json") {
                continue;
            }
            let mut body = String::new();
            zip.by_name(name)?.read_to_string(&mut body)?;
            let _ = import_dce_json(&body, me, seen_corr, messages, stats);
        }
        return Ok(());
    }

    // Official package. Group message/channel files per channel folder
    // (`…/messages/c<id>/{channel.json|messages.json|messages.csv}`).
    use std::collections::BTreeMap;
    let mut by_channel: BTreeMap<String, ChannelFiles> = BTreeMap::new();
    for name in &names {
        if let Some(rest) = name.split("messages/").nth(1) {
            let Some((folder, file)) = rest.split_once('/') else { continue };
            if !folder.starts_with('c') || file.contains('/') {
                continue;
            }
            let chan = folder.trim_start_matches('c').to_string();
            let entry = by_channel.entry(chan).or_default();
            match file {
                "channel.json" => entry.channel = Some(name.clone()),
                "messages.json" => entry.messages_json = Some(name.clone()),
                "messages.csv" => entry.messages_csv = Some(name.clone()),
                _ => {}
            }
        }
    }

    for (chan_id, files) in by_channel {
        let mut chat_name = String::new();
        if let Some(cf) = &files.channel {
            let mut body = String::new();
            if read_entry(&mut zip, cf, &mut body).is_ok() {
                if let Ok(c) = serde_json::from_str::<PackageChannel>(&body) {
                    chat_name = if !c.name.is_empty() {
                        c.name
                    } else {
                        // 1:1 DM: name it after the other recipient(s).
                        c.recipients.join(", ")
                    };
                }
            }
        }

        let rows: Vec<PackageMessage> = if let Some(mj) = &files.messages_json {
            let mut body = String::new();
            read_entry(&mut zip, mj, &mut body)?;
            serde_json::from_str(&body).unwrap_or_default()
        } else if let Some(mc) = &files.messages_csv {
            let mut body = String::new();
            read_entry(&mut zip, mc, &mut body)?;
            parse_messages_csv(&body)
        } else {
            Vec::new()
        };

        if !rows.is_empty() {
            stats.channels.insert(chan_id.clone());
        }
        for r in rows {
            if r.id.is_empty() {
                continue;
            }
            let Some(ts) = discord_ts_to_local(&r.timestamp) else { continue };
            if !seen_corr.insert(r.id.clone()) {
                stats.duplicates += 1;
                continue;
            }
            let mut m = Message::new("discord", ts);
            m.guid = r.id.clone();
            m.chat = chan_id.clone();
            m.chat_name = chat_name.clone();
            // The official package is sent-messages-only.
            m.from_me = true;
            m.text = r.contents.clone();
            m.attachments = package_attachments(&r.attachments);
            if m.text.is_empty() && m.attachments.is_empty() {
                seen_corr.remove(&r.id);
                continue;
            }
            messages.push(m);
            stats.messages += 1;
        }
    }

    // Game activity: every JSON under activities/games/.
    for name in &names {
        if !name.contains("activities/games/") || !name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }
        let mut body = String::new();
        if read_entry(&mut zip, name, &mut body).is_err() {
            continue;
        }
        for ev in parse_activity(&body) {
            let Some(row) = game_row(&ev) else { continue };
            let key = row["key"].as_str().unwrap_or_default().to_string();
            if key.is_empty() || !seen_games.insert(key) {
                stats.duplicates += 1;
                continue;
            }
            games.push(row);
            stats.games += 1;
        }
    }

    Ok(())
}

/// Read one zip entry's bytes into `body` as a string.
fn read_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str, body: &mut String) -> Result<()> {
    body.clear();
    zip.by_name(name)
        .with_context(|| format!("entry {name}"))?
        .read_to_string(body)
        .with_context(|| format!("reading {name}"))?;
    Ok(())
}

#[derive(Default)]
struct ChannelFiles {
    channel: Option<String>,
    messages_json: Option<String>,
    messages_csv: Option<String>,
}

/// Parse a DiscordChatExporter JSON export body into correspondence rows
/// (messages + reactions). Lenient: a body that isn't a DCE export yields
/// nothing rather than erroring.
fn import_dce_json(
    body: &str,
    me: Option<&str>,
    seen_corr: &mut HashSet<String>,
    messages: &mut Vec<Message>,
    stats: &mut Stats,
) -> Result<()> {
    let export: DceExport = match serde_json::from_str(body) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    let chan_id = export.channel.id.clone();
    let chat_name = export.channel.name.clone();
    if export.messages.is_empty() {
        return Ok(());
    }
    stats.channels.insert(if chan_id.is_empty() { chat_name.clone() } else { chan_id.clone() });

    for dm in export.messages {
        if dm.id.is_empty() {
            continue;
        }
        let Some(ts) = discord_ts_to_local(&dm.timestamp) else { continue };
        // from_me only when the author matches the importing user, if given.
        // DCE does not otherwise mark "me", so absent `me` everything is
        // received — a deliberate, documented choice: an honest "received"
        // beats a wrongly-attributed "sent".
        let from_me = me.is_some_and(|m| {
            m == dm.author.id
                || m.eq_ignore_ascii_case(&dm.author.name)
                || (!dm.author.nickname.is_empty() && m.eq_ignore_ascii_case(&dm.author.nickname))
        });

        if seen_corr.insert(dm.id.clone()) {
            let mut m = Message::new("discord", ts.clone());
            m.guid = dm.id.clone();
            m.chat = chan_id.clone();
            m.chat_name = chat_name.clone();
            m.from_me = from_me;
            if !from_me {
                m.sender = dm.author.name.clone();
                if !dm.author.nickname.is_empty() && dm.author.nickname != dm.author.name {
                    m.sender_name = dm.author.nickname.clone();
                }
            }
            m.text = dm.content.clone();
            m.attachments = dm
                .attachments
                .iter()
                .map(|att| AttachmentMeta {
                    name: if att.file_name.is_empty() {
                        att.url.rsplit('/').next().unwrap_or("").to_string()
                    } else {
                        att.file_name.clone()
                    },
                    mime: String::new(),
                    bytes: att.file_size_bytes,
                })
                .collect();
            if m.text.is_empty() && m.attachments.is_empty() {
                seen_corr.remove(&dm.id);
            } else {
                messages.push(m);
                stats.messages += 1;
            }
        } else {
            stats.duplicates += 1;
        }

        // Reactions → kind:"reaction" rows, keyed by a stable composite so a
        // re-import dedupes them too.
        for (i, rx) in dm.reactions.iter().enumerate() {
            if rx.emoji.name.is_empty() {
                continue;
            }
            let guid = format!("{}-rx-{}-{}", dm.id, rx.emoji.name, i);
            if !seen_corr.insert(guid.clone()) {
                stats.duplicates += 1;
                continue;
            }
            let mut m = Message::new("discord", ts.clone());
            m.guid = guid;
            m.chat = chan_id.clone();
            m.chat_name = chat_name.clone();
            m.kind = "reaction".into();
            m.reaction = rx.emoji.name.clone();
            m.reply_to = dm.id.clone();
            messages.push(m);
            stats.reactions += 1;
        }
    }
    Ok(())
}

/// Parse `messages.csv` (older packages): columns ID, Timestamp, Contents,
/// Attachments (header present). The csv crate handles quoting/commas.
fn parse_messages_csv(body: &str) -> Vec<PackageMessage> {
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    rdr.deserialize::<PackageMessage>().filter_map(Result::ok).collect()
}

/// Parse an activities JSON body: newline-delimited objects (the usual form)
/// or a single JSON array.
fn parse_activity(body: &str) -> Vec<Value> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<Value>>(body).unwrap_or_default()
    } else {
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }
}

/// A raw game-activity event → a `gaming/discord/` row: full fidelity (the
/// whole event under `raw`) plus the handles a reader needs (`ts`, `game`,
/// `event`) and a stable dedupe `key`. `None` when there's no timestamp or it
/// isn't a game-ish event (the analytics dump mixes in unrelated events).
fn game_row(ev: &Value) -> Option<Value> {
    let obj = ev.as_object()?;
    let raw_ts = obj
        .get("timestamp")
        .or_else(|| obj.get("ts"))
        .or_else(|| obj.get("dt"))
        .and_then(|v| v.as_str())?;
    let ts = discord_ts_to_local(raw_ts)?;
    let game = obj
        .get("game")
        .or_else(|| obj.get("application_name"))
        .or_else(|| obj.get("application"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let event = obj
        .get("event_type")
        .or_else(|| obj.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_game = !game.is_empty() || event.contains("game") || event.contains("application");
    if !is_game {
        return None;
    }
    let key = obj
        .get("event_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("{ts}|{game}|{event}"));
    let mut row = Map::new();
    row.insert("ts".into(), Value::String(ts));
    if !game.is_empty() {
        row.insert("game".into(), Value::String(game));
    }
    if !event.is_empty() {
        row.insert("event".into(), Value::String(event));
    }
    row.insert("source".into(), Value::String("discord".into()));
    row.insert("key".into(), Value::String(key));
    // Full fidelity: the untouched event.
    row.insert("raw".into(), ev.clone());
    Some(Value::Object(row))
}

/// Keys already stored under `gaming/discord/`, the dedupe set for re-runnable
/// game-activity imports.
fn stored_game_keys(vault: &Vault) -> Result<HashSet<String>> {
    let stream = vault.stream(GAMING_DIR, Partition::Month);
    let mut out = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(k) = v.get("key").and_then(|v| v.as_str()) {
                out.insert(k.to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-discord-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path, me: Option<&str>) -> ImportOutcome {
        let mut params = BTreeMap::new();
        if let Some(m) = me {
            params.insert("me".to_string(), m.to_string());
        }
        (IMPORT.run)(v, path, &params, &mut |_| {}).unwrap()
    }

    /// A minimal official data-package zip: one channel with channel.json +
    /// messages.json, plus an activities/games file.
    fn package_zip(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("trove-dpkg-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("messages/c123/channel.json", opts).unwrap();
        z.write_all(br#"{"id":"123","type":1,"name":"general"}"#).unwrap();
        z.start_file("messages/c123/messages.json", opts).unwrap();
        z.write_all(
            br#"[
              {"ID":"1001","Timestamp":"2026-06-10T09:00:00+00:00","Contents":"hi from me","Attachments":""},
              {"ID":"1002","Timestamp":"2026-06-10T09:05:00+00:00","Contents":"with a file","Attachments":"https://cdn.discordapp.com/x/plan.pdf"}
            ]"#,
        )
        .unwrap();
        z.start_file("activities/games/2026.json", opts).unwrap();
        z.write_all(
            br#"{"event_id":"g1","event_type":"launch_game","game":"Hades","timestamp":"2026-06-10T20:00:00+00:00"}
{"event_id":"g2","event_type":"launch_game","game":"Hades","timestamp":"2026-06-11T21:00:00+00:00"}
"#,
        )
        .unwrap();
        z.finish().unwrap();
        path
    }

    /// A DiscordChatExporter JSON of the SAME channel/messages, plus an
    /// other-side message with a reaction.
    fn dce_json(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("trove-dce-{}-{name}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        fs::write(
            &path,
            r#"{
              "guild": {"id":"g","name":"My Server"},
              "channel": {"id":"123","name":"general"},
              "messages": [
                {"id":"1001","type":"Default","timestamp":"2026-06-10T09:00:00+00:00","content":"hi from me",
                 "author":{"id":"me-id","name":"daveuser","nickname":"Dave"},"attachments":[],"reactions":[]},
                {"id":"2001","type":"Default","timestamp":"2026-06-10T09:10:00+00:00","content":"hey dave!",
                 "author":{"id":"sam-id","name":"samuser","nickname":"Sam"},
                 "attachments":[{"id":"a","url":"https://cdn.x/pic.png","fileName":"pic.png","fileSizeBytes":2048}],
                 "reactions":[{"emoji":{"name":"👍"},"count":1}]}
              ]
            }"#
            .as_bytes(),
        )
        .unwrap();
        path
    }

    #[test]
    fn discord_ts_parses_all_shapes() {
        assert!(discord_ts_to_local("2026-06-10T09:00:00+00:00").is_some());
        assert!(discord_ts_to_local("2026-06-10 09:00:00+00:00").is_some(), "space variant");
        let t = discord_ts_to_local("1781773200000").unwrap();
        assert!(t.starts_with("2026-"), "epoch ms parsed: {t}");
        assert!(discord_ts_to_local("").is_none());
        assert!(discord_ts_to_local("not-a-date").is_none());
    }

    #[test]
    fn official_package_imports_sent_rows_and_game_activity() {
        let v = temp_vault("pkg");
        let zip = package_zip("pkg");
        let out = run(&v, &zip, None);
        assert_eq!(out.counts.get("messages"), Some(&2));
        assert_eq!(out.counts.get("games"), Some(&2));
        assert_eq!(out.counts.get("channels"), Some(&1));

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2, "two sent messages (game activity is a different domain)");
        let first = &day[0];
        assert!(first.from_me, "official package is sent-only");
        assert_eq!(first.sender, "", "sender empty when from_me");
        assert_eq!(first.guid, "1001");
        assert_eq!(first.chat, "123");
        assert_eq!(first.chat_name, "general");
        assert_eq!(first.kind, "message");
        assert_eq!(first.text, "hi from me");
        assert_eq!(day[1].attachments[0].name, "plan.pdf", "attachment is metadata-only");
        assert_eq!(day[1].attachments[0].bytes, 0, "package gives no size, never fetched");

        let games = fs::read_to_string(v.root().join("gaming/discord/2026-06.jsonl")).unwrap();
        assert_eq!(games.lines().count(), 2);
        assert!(games.contains("\"game\":\"Hades\""), "raw game row: {games}");
        assert!(games.contains("\"raw\""), "full-fidelity raw event preserved");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn dce_json_imports_received_rows_and_reactions() {
        let v = temp_vault("dce");
        let json = dce_json("dce");
        let out = run(&v, &json, Some("daveuser"));
        assert_eq!(out.counts.get("messages"), Some(&2), "both sides");
        assert_eq!(out.counts.get("reactions"), Some(&1));

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3, "2 messages + 1 reaction row");
        let mine = day.iter().find(|m| m.guid == "1001").unwrap();
        assert!(mine.from_me, "matched via username 'me'");
        let theirs = day.iter().find(|m| m.guid == "2001").unwrap();
        assert!(!theirs.from_me);
        assert_eq!(theirs.sender, "samuser", "sender = author.name");
        assert_eq!(theirs.sender_name, "Sam", "nickname → sender_name");
        assert_eq!(theirs.attachments[0].name, "pic.png");
        assert_eq!(theirs.attachments[0].bytes, 2048);

        let rx = day.iter().find(|m| m.kind == "reaction").unwrap();
        assert_eq!(rx.reaction, "👍");
        assert_eq!(rx.reply_to, "2001", "reaction points at its message");
        assert!(rx.guid.starts_with("2001-rx-"), "stable composite guid: {}", rx.guid);

        let _ = fs::remove_file(json);
    }

    #[test]
    fn cross_shape_guid_merge_does_not_duplicate() {
        let v = temp_vault("merge");
        let zip = package_zip("merge");
        let json = dce_json("merge");

        // Package first: 2 sent messages.
        let p = run(&v, &zip, None);
        assert_eq!(p.counts.get("messages"), Some(&2));

        // Then DCE of the same channel: message 1001 is shared (dedupes),
        // 2001 is the other side (added). The shared one must NOT duplicate.
        let d = run(&v, &json, Some("daveuser"));
        assert_eq!(d.counts.get("messages"), Some(&1), "only the new other-side message");
        assert_eq!(d.counts.get("reactions"), Some(&1));
        assert!(d.counts.get("duplicates").unwrap() >= &1, "the shared 1001 was a duplicate");

        // The stored stream has exactly one row per unique guid.
        let raw = fs::read_to_string(v.root().join("correspondence/discord/2026-06.jsonl")).unwrap();
        let guids: Vec<String> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<Message>(l).ok().map(|m| m.guid))
            .collect();
        let unique: HashSet<&str> = guids.iter().map(String::as_str).collect();
        assert_eq!(guids.len(), unique.len(), "no duplicate guids across shapes");
        assert!(unique.contains("1001"), "mine, from the package");
        assert!(unique.contains("2001"), "theirs, from DCE");

        let _ = fs::remove_file(zip);
        let _ = fs::remove_file(json);
    }

    #[test]
    fn reimport_is_a_noop_for_both_shapes() {
        let v = temp_vault("reimport");
        let zip = package_zip("reimport");
        let json = dce_json("reimport");

        run(&v, &zip, None);
        run(&v, &json, Some("daveuser"));
        let before = fs::read_to_string(v.root().join("correspondence/discord/2026-06.jsonl")).unwrap();
        let games_before = fs::read_to_string(v.root().join("gaming/discord/2026-06.jsonl")).unwrap();

        let p2 = run(&v, &zip, None);
        let d2 = run(&v, &json, Some("daveuser"));
        assert_eq!(p2.counts.get("messages"), Some(&0));
        assert_eq!(p2.counts.get("games"), Some(&0));
        assert_eq!(d2.counts.get("messages"), Some(&0));
        assert_eq!(d2.counts.get("reactions"), Some(&0));

        let after = fs::read_to_string(v.root().join("correspondence/discord/2026-06.jsonl")).unwrap();
        let games_after = fs::read_to_string(v.root().join("gaming/discord/2026-06.jsonl")).unwrap();
        assert_eq!(before, after, "correspondence unchanged on re-import");
        assert_eq!(games_before, games_after, "game activity unchanged on re-import");

        let _ = fs::remove_file(zip);
        let _ = fs::remove_file(json);
    }

    #[test]
    fn messages_csv_variant_is_handled() {
        let v = temp_vault("csv");
        let path = std::env::temp_dir().join(format!("trove-dpkgcsv-{}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("messages/c777/channel.json", opts).unwrap();
        z.write_all(br#"{"id":"777","name":"old-dm"}"#).unwrap();
        z.start_file("messages/c777/messages.csv", opts).unwrap();
        z.write_all(b"ID,Timestamp,Contents,Attachments\n5001,2026-06-12T08:00:00+00:00,\"hello, world\",\n")
            .unwrap();
        z.finish().unwrap();

        let out = run(&v, &path, None);
        assert_eq!(out.counts.get("messages"), Some(&1));
        let day = v.correspondence_timeline("2026-06-12").unwrap();
        assert_eq!(day[0].text, "hello, world", "quoted comma survives csv");
        assert_eq!(day[0].chat_name, "old-dm");
        assert!(day[0].from_me);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn old_message_lines_still_deserialize() {
        // Back-compat: a correspondence line written before this integration
        // (no discord-specific fields) still reads as a Message.
        let line = r#"{"ts":"2026-06-10T09:00:00-07:00","source":"discord","chat":"123","from_me":true,"kind":"message","text":"hi"}"#;
        let m: Message = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "hi");
        assert!(m.from_me);
    }
}

//! TikTok — official JSON data export import.
//!
//! The export is a ZIP downloaded from Profile → Settings and privacy → Account
//! → Download your data. **Select JSON format, not TXT** — the TXT variant is
//! far less complete. The ZIP contains a single `user_data.json` with every
//! category, plus older exports may split categories into individual files.
//!
//! **Download links are valid for only a few days** after the ready
//! notification — the setup copy tells the user to import promptly.
//!
//! ## Export structure (confirmed from community analysis)
//!
//! The top-level keys in `user_data.json`:
//!
//! ```text
//! Activity
//!   Video Browsing History
//!     VideoList            ← watch events: [{"Date": "...", "VideoLink": "..."}]
//!   Like List
//!     ItemFavoriteList     ← liked videos: [{"Date": "...", "Link": "..."}]
//!   Favorite Videos
//!     FavoriteVideoList    ← saved videos: [{"Date": "...", "Link": "..."}]
//!   Comment
//!     Comments
//!       CommentsList       ← comments: [{"Date": "...", "Comment": "...", "URL": "..."}]
//!   Share History
//!     ShareHistoryList     ← shares: [{"Date": "...", "SharedContent": "...", "Link": "...", "Method": "..."}]
//!   Search History
//!     SearchList           ← searches: [{"Date": "...", "SearchTerm": "..."}]
//! Video
//!   Videos
//!     VideoList            ← own posts: [{"Date": "...", "Link": "..."}]
//! Direct Messages
//!   Chat History
//!     ChatHistory          ← DMs: { "Chat History with USERNAME:": [ {messages} ] }
//! Ads and Data
//!   Ad Interests
//!     adInterestCategories ← ad interest labels: ["label", ...]
//!   Off TikTok Activity
//!     OffTikTokActivityDataList ← off-platform events (raw only)
//! Profile
//!   Profile Information
//!     ProfileMap           ← username, bioDescription, etc.
//! App Settings
//!   Settings              ← per-device app settings
//! ```
//!
//! Watch-history items use the key `"VideoLink"` (confirmed: TikTok Unwrapped,
//! toktik, fcthulhu/tiktok-json-extractor). Own-post items use `"Link"`.
//! DM thread keys are `"Chat History with USERNAME:"` (prefix + trailing colon;
//! confirmed from toktik: `dm.removeprefix('Chat History with ').removesuffix(':')`)
//! and are stripped to the bare username for `chat` and `from_me` comparisons.
//!
//! Timestamps are `"YYYY-MM-DD HH:MM:SS"` (UTC, confirmed from community
//! parsers: rothgar's nushell tiktok-download script, gist analyses). The
//! export carries no per-event id, so guids are SHA-256 hashes of
//! (section, date_str, link/content) — stable and injective across re-imports.
//!
//! ## Vault mapping
//!
//! | Data | Path |
//! |------|------|
//! | Raw JSON (full fidelity) | `social/tiktok/raw/<section>.jsonl` |
//! | Watch events (contract) | `media/plays/tiktok/YYYY-MM.jsonl` |
//! | Watch raw | `media/plays/tiktok/raw/YYYY-MM.jsonl` |
//! | DMs (contract, opt-in) | `correspondence/tiktok/YYYY-MM.jsonl` |
//! | Own posts (raw only) | `social/tiktok/raw/posts.jsonl` |
//!
//! Watch events map to the media-plays write contract (`MediaItem`, kind="play",
//! category="video", guid=sha256(date_str+link), link in detail). Own posts,
//! likes, shares, comments, ad interests, and searches stay per-source raw —
//! the social-posts contract is Phase 3 pending and the rest have no bound
//! contract yet.
//!
//! DMs are opt-in with an explicit acknowledgement param (privacy rule for
//! message bodies), matching the snapchat/facebook pattern.
//!
//! ## Dedupe
//!
//! All guids are deterministic sha256 hashes. Re-importing the same or an
//! overlapping archive never duplicates.
//!
//! ## Parser posture
//!
//! The JSON inner field names are confirmed from community analysis (gist
//! searches show `VideoList`, `Date`, `Link`, `CommentsList`, etc.). The
//! importer is tolerant: every array/object access uses `and_then` + fallback
//! so an unexpected layout produces zero records rather than an error. A real
//! export will refine the field names if any differ. Flag: Needs-sample.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use sha2::{Digest, Sha256};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, ImportOutcome, ImportParam, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "tiktok";
const MEDIA_DIR: &str = "media/plays/tiktok";
const MEDIA_RAW_DIR: &str = "media/plays/tiktok/raw";
const SOCIAL_RAW_DIR: &str = "social/tiktok/raw";
const SOCIAL_DIR: &str = "social/tiktok";
const CORR_SOURCE: &str = "tiktok";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(MEDIA_DIR))
        .or_else(|| crate::registry::newest_mtime(&vault.root().join(SOCIAL_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tiktok",
        name: "TikTok",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your TikTok data export — video watch history \
                      (typically 10,000–50,000+ events for active users), liked videos, \
                      comments, shares, searches, and direct messages. \
                      Own posts and ad interests are kept full-fidelity. \
                      Re-runnable; re-importing the same archive never duplicates.",
        domain: "media",
        vault_path: "media/plays/tiktok/",
        toggleable: false,
        setup: &[
            "Profile → three-bar menu → Settings and privacy → Account → Download your data. \
             Select JSON format (not TXT — TXT is far less complete). The export is typically \
             ready within 1–4 days (up to 30 days per TikTok's policy).",
            "The download link in the ready notification is valid for only a few days — \
             save the ZIP and import it promptly.",
            "Drop the ZIP here. Direct messages are sensitive; type 'yes' in the \
             acknowledgement field to also import them.",
        ],
        caveats: "JSON format only (not TXT). The archive is a one-time snapshot; \
                  re-requesting a new export captures new activity. Download links \
                  expire within days of the ready notification. \
                  TikTok does not export video files — only watch history metadata \
                  (timestamp + URL) is available. DM bodies require explicit acknowledgement.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[ImportParam {
        key: "acknowledge_dms",
        label: "Import DMs (optional)",
        placeholder: "Type 'yes' to also import direct message history into correspondence/tiktok/",
        required: false,
    }],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Stats

#[derive(Default)]
struct Stats {
    watch_events: u64,
    dms: u64,
    raw_rows: u64,
    duplicates: u64,
}

// ---------------------------------------------------------------------------
// Main importer

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let import_dms = params
        .get("acknowledge_dms")
        .map(|v| v.trim().eq_ignore_ascii_case("yes"))
        .unwrap_or(false);

    // Existing guids for dedupe.
    let mut watch_seen = {
        let stream = vault.stream(MEDIA_DIR, Partition::Month);
        let mut set = HashSet::new();
        for key in stream.partitions()? {
            for item in stream.read::<MediaItem>(&key)? {
                if !item.guid.is_empty() {
                    set.insert(item.guid);
                }
            }
        }
        set
    };
    let mut dm_seen = if import_dms {
        vault.correspondence_guids(CORR_SOURCE)?
    } else {
        HashSet::new()
    };

    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading {} — is this a TikTok data export ZIP?", path.display()))?;

    // Collect entry names first (ZipArchive cannot be borrowed mutably twice).
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    let mut stats = Stats::default();
    let mut watch_events: Vec<MediaItem> = Vec::new();
    let mut watch_raws: Vec<RawLine> = Vec::new();
    let mut dms: Vec<Message> = Vec::new();
    let mut raw_by_section: BTreeMap<String, Vec<Value>> = BTreeMap::new();

    for name in &names {
        // Skip macOS metadata.
        if name.starts_with("__MACOSX") || name.contains("/.") {
            continue;
        }
        if !name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }

        let mut body = String::new();
        if zip
            .by_name(name)
            .with_context(|| format!("opening entry {name}"))?
            .read_to_string(&mut body)
            .is_err()
        {
            continue;
        }

        let Ok(root) = serde_json::from_str::<Value>(&body) else {
            continue;
        };

        // The primary path: user_data.json contains everything.
        // Older exports may have individual category files — handle both by
        // treating any parsed JSON as a potential user_data root or a section.
        parse_export(
            &root,
            import_dms,
            &mut watch_events,
            &mut watch_raws,
            &mut dms,
            &mut raw_by_section,
            &mut watch_seen,
            &mut dm_seen,
            &mut stats,
        );
    }

    // Flush watch events (contract layer + raw layer).
    vault
        .stream(MEDIA_DIR, Partition::Month)
        .append(&watch_events, |i| &i.ts)?;
    vault
        .stream(MEDIA_RAW_DIR, Partition::Month)
        .append(&watch_raws, |r| &r.ts)?;

    // Flush DMs (correspondence contract layer).
    if import_dms && !dms.is_empty() {
        vault.append_messages(&dms)?;
    }

    // Flush raw sections to social/tiktok/raw/<section>.jsonl.
    for (section, rows) in &raw_by_section {
        if rows.is_empty() {
            continue;
        }
        append_raw_section(vault, section, rows)?;
        stats.raw_rows += rows.len() as u64;
    }

    progress(ImportProgress {
        records: stats.watch_events + stats.dms + stats.raw_rows,
        percent: 100.0,
    });

    let dm_note = if import_dms {
        format!(", {} DMs", stats.dms)
    } else {
        String::new()
    };

    Ok(ImportOutcome {
        headline: format!(
            "{} watch events{}, {} raw rows imported, {} duplicates skipped",
            stats.watch_events,
            dm_note,
            stats.raw_rows,
            stats.duplicates,
        ),
        counts: [
            ("watch_events", stats.watch_events),
            ("dms", stats.dms),
            ("raw_rows", stats.raw_rows),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Top-level export parser

/// Walk the parsed JSON and route each section.
/// Handles both the single-file `user_data.json` shape and older per-category
/// file exports by checking for the documented top-level keys.
#[allow(clippy::too_many_arguments)]
fn parse_export(
    root: &Value,
    import_dms: bool,
    watch_events: &mut Vec<MediaItem>,
    watch_raws: &mut Vec<RawLine>,
    dms: &mut Vec<Message>,
    raw_by_section: &mut BTreeMap<String, Vec<Value>>,
    watch_seen: &mut HashSet<String>,
    dm_seen: &mut HashSet<String>,
    stats: &mut Stats,
) {
    // Activity section: watch history, likes, comments, shares, searches.
    if let Some(activity) = root.get("Activity") {
        // "Video Browsing History" → VideoList (confirmed key from real exports:
        // TikTok Unwrapped, toktik, fcthulhu/tiktok-json-extractor, zollo).
        // Fall back to the older "Video Browse History" spelling defensively.
        let watch_list = activity
            .get("Video Browsing History")
            .or_else(|| activity.get("Video Browse History"))
            .and_then(|v| v.get("VideoList"))
            .and_then(Value::as_array);
        if let Some(list) = watch_list {
            for item in list {
                parse_watch_event(item, watch_events, watch_raws, watch_seen, stats);
            }
        }

        // Like List → ItemFavoriteList (also FavoriteVideoList in some exports)
        let liked = activity
            .get("Like List")
            .and_then(|v| {
                v.get("ItemFavoriteList")
                    .or_else(|| v.get("FavoriteVideoList"))
            })
            .and_then(Value::as_array);
        if let Some(list) = liked {
            for item in list {
                raw_by_section
                    .entry("liked_videos".to_string())
                    .or_default()
                    .push(item.clone());
            }
        }

        // Favorite Videos → FavoriteVideoList
        let favs = activity
            .get("Favorite Videos")
            .and_then(|v| v.get("FavoriteVideoList"))
            .and_then(Value::as_array);
        if let Some(list) = favs {
            for item in list {
                raw_by_section
                    .entry("favorite_videos".to_string())
                    .or_default()
                    .push(item.clone());
            }
        }

        // Comment → Comments → CommentsList (confirmed from toktik: data.Comment.Comments.CommentsList).
        // Fall back to the flat Comment → CommentsList path for schema variants.
        let comments = activity
            .get("Comment")
            .and_then(|v| {
                v.get("Comments")
                    .and_then(|c| c.get("CommentsList"))
                    .or_else(|| v.get("CommentsList"))
            })
            .and_then(Value::as_array);
        if let Some(list) = comments {
            for item in list {
                raw_by_section
                    .entry("comments".to_string())
                    .or_default()
                    .push(item.clone());
            }
        }

        // Share History → ShareHistoryList
        let shares = activity
            .get("Share History")
            .and_then(|v| v.get("ShareHistoryList"))
            .and_then(Value::as_array);
        if let Some(list) = shares {
            for item in list {
                raw_by_section
                    .entry("shares".to_string())
                    .or_default()
                    .push(item.clone());
            }
        }

        // "Search History" → SearchList (confirmed from toktik, fcthulhu).
        // Fall back to older "Searches" spelling defensively.
        let searches = activity
            .get("Search History")
            .or_else(|| activity.get("Searches"))
            .and_then(|v| v.get("SearchList"))
            .and_then(Value::as_array);
        if let Some(list) = searches {
            for item in list {
                raw_by_section
                    .entry("searches".to_string())
                    .or_default()
                    .push(item.clone());
            }
        }

        // Browsing History (profile views, etc. — distinct from watch history)
        let browsing = activity
            .get("Browsing History")
            .and_then(|v| v.get("BrowsingHistoryList"))
            .and_then(Value::as_array);
        if let Some(list) = browsing {
            for item in list {
                raw_by_section
                    .entry("browsing_history".to_string())
                    .or_default()
                    .push(item.clone());
            }
        }

        // Follower / Following lists
        for (key, section) in &[
            ("Follower List", "followers"),
            ("Following List", "following"),
        ] {
            let arr_opt = activity
                .get(*key)
                .and_then(|v| {
                    v.as_array().or_else(|| {
                        v.as_object()
                            .and_then(|o| o.values().find_map(Value::as_array))
                    })
                });
            if let Some(arr) = arr_opt {
                for item in arr {
                    raw_by_section
                        .entry(section.to_string())
                        .or_default()
                        .push(item.clone());
                }
            }
        }
    }

    // Video section: own posts.
    let own_videos = root
        .get("Video")
        .and_then(|v| v.get("Videos"))
        .and_then(|v| v.get("VideoList"))
        .and_then(Value::as_array);
    if let Some(list) = own_videos {
        for item in list {
            raw_by_section
                .entry("posts".to_string())
                .or_default()
                .push(item.clone());
        }
    }

    // Direct Messages → correspondence layer (opt-in).
    // Real export nests: Direct Messages → "Chat History" (with space) → ChatHistory
    // (confirmed from toktik: self.user_data.get('Direct Messages').get('Chat History').get('ChatHistory')).
    // Fall back to the older "ChatHistory" middle key defensively.
    if import_dms {
        let chat_history = root
            .get("Direct Messages")
            .and_then(|v| {
                v.get("Chat History")
                    .or_else(|| v.get("ChatHistory"))
            })
            .and_then(|v| v.get("ChatHistory"));
        if let Some(ch) = chat_history {
            parse_dms(ch, dms, dm_seen, stats);
        }
    }

    // Ads and Data → raw only.
    if let Some(ads) = root.get("Ads and Data") {
        let interests = ads
            .get("Ad Interests")
            .and_then(|v| v.get("adInterestCategories"))
            .and_then(Value::as_array);
        if let Some(list) = interests {
            for item in list {
                raw_by_section
                    .entry("ad_interests".to_string())
                    .or_default()
                    .push(item.clone());
            }
        }
        // Off-TikTok activity.
        let off_tiktok = ads
            .get("Off TikTok Activity")
            .and_then(|v| {
                v.get("OffTikTokActivityDataList")
                    .or_else(|| v.get("BusinessName"))
            })
            .and_then(Value::as_array);
        if let Some(list) = off_tiktok {
            for item in list {
                raw_by_section
                    .entry("off_tiktok_activity".to_string())
                    .or_default()
                    .push(item.clone());
            }
        }
    }

    // Profile → raw.
    let profile = root
        .get("Profile")
        .and_then(|v| v.get("Profile Information"))
        .and_then(|v| v.get("ProfileMap"));
    if let Some(pm) = profile {
        raw_by_section
            .entry("profile".to_string())
            .or_default()
            .push(pm.clone());
    }
}

// ---------------------------------------------------------------------------
// Watch event parser → MediaItem (media-plays contract)

fn parse_watch_event(
    item: &Value,
    watch_events: &mut Vec<MediaItem>,
    watch_raws: &mut Vec<RawLine>,
    seen: &mut HashSet<String>,
    stats: &mut Stats,
) {
    let obj = match item.as_object() {
        Some(o) => o,
        None => return,
    };

    let date_str = obj.get("Date").and_then(Value::as_str).unwrap_or("").trim();
    // Watch-history items use "VideoLink" (confirmed: TikTok Unwrapped example
    // {"Date":...,"VideoLink":"https://www.tiktokv.com/share/video/..."}, toktik, zollo).
    // Own-post VideoList items use "Link" — that path stays unchanged.
    let link = obj
        .get("VideoLink")
        .or_else(|| obj.get("Link"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    if date_str.is_empty() {
        return; // no timestamp → skip
    }

    let ts = match parse_tiktok_date(date_str) {
        Some(t) => t,
        None => return,
    };

    // guid = sha256("watch" | date_str | link) — stable across re-imports.
    // Including the link prevents same-second collision (routine on TikTok).
    let guid = event_guid("watch", date_str, link);
    if !seen.insert(guid.clone()) {
        stats.duplicates += 1;
        return;
    }

    let mut extra = Map::new();
    if !link.is_empty() {
        extra.insert("link".into(), Value::String(link.to_string()));
    }

    watch_events.push(MediaItem {
        ts: ts.clone(),
        source: SOURCE.to_string(),
        category: "video".to_string(),
        device: String::new(),
        kind: "play".to_string(),
        title: String::new(), // TikTok export doesn't include video title
        subtitle: "TikTok".to_string(),
        detail: link.to_string(),
        seconds: 0, // export records events, not durations
        favicon: String::new(),
        guid,
        extra,
    });
    watch_raws.push(RawLine { ts, value: item.clone() });
    stats.watch_events += 1;
}

// ---------------------------------------------------------------------------
// DM parser → Message (correspondence contract)

fn parse_dms(
    chat_history: &Value,
    dms: &mut Vec<Message>,
    seen: &mut HashSet<String>,
    stats: &mut Stats,
) {
    // chat_history is keyed by thread labels of the form "Chat History with USERNAME:"
    // (confirmed from toktik: `dm.removeprefix('Chat History with ').removesuffix(':')`)
    // Strip to the bare username before using it as the contact/chat handle.
    // Also handle older exports that may key by bare contact name directly.
    if let Some(obj) = chat_history.as_object() {
        for (raw_key, msgs_val) in obj {
            let Some(msgs) = msgs_val.as_array() else { continue };
            let contact = strip_chat_history_prefix(raw_key);
            parse_dm_thread(contact, msgs, dms, seen, stats);
        }
    } else if let Some(arr) = chat_history.as_array() {
        for conv in arr {
            if let Some(obj) = conv.as_object() {
                for (raw_key, msgs_val) in obj {
                    let Some(msgs) = msgs_val.as_array() else { continue };
                    let contact = strip_chat_history_prefix(raw_key);
                    parse_dm_thread(contact, msgs, dms, seen, stats);
                }
            }
        }
    }
}

/// Strip the `"Chat History with "` prefix and trailing `":"` from a DM thread key
/// to get the bare username. Returns the original string if the prefix is absent
/// (handles older exports that already use bare contact names).
fn strip_chat_history_prefix(key: &str) -> &str {
    const PREFIX: &str = "Chat History with ";
    if let Some(rest) = key.strip_prefix(PREFIX) {
        rest.trim_end_matches(':').trim()
    } else {
        key
    }
}

fn parse_dm_thread(
    contact: &str,
    msgs: &[Value],
    dms: &mut Vec<Message>,
    seen: &mut HashSet<String>,
    stats: &mut Stats,
) {
    for msg in msgs {
        let obj = match msg.as_object() {
            Some(o) => o,
            None => continue,
        };

        // TikTok DM export keys confirmed from community analysis:
        // "Date", "From", "To", "Content", "MediaType" (or "Media Type").
        let date_str = obj.get("Date").and_then(Value::as_str).unwrap_or("").trim();
        if date_str.is_empty() {
            continue;
        }
        let ts = match parse_tiktok_date(date_str) {
            Some(t) => t,
            None => continue,
        };

        let from = obj
            .get("From")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let to = obj
            .get("To")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let content = obj
            .get("Content")
            .or_else(|| obj.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let media_type = obj
            .get("MediaType")
            .or_else(|| obj.get("Media Type"))
            .and_then(Value::as_str)
            .unwrap_or("text")
            .to_string();

        // from_me heuristic: if From != contact, the vault owner sent it.
        let from_me = !from.is_empty() && from != contact;

        // guid = sha256("dm" | contact | date_str | from | content)
        let guid = dm_guid(contact, date_str, &from, &content);
        if !seen.insert(guid.clone()) {
            stats.duplicates += 1;
            continue;
        }

        let mut m = Message::new(CORR_SOURCE, ts);
        m.guid = guid;
        m.chat = contact.to_string();
        m.from_me = from_me;
        if !from.is_empty() {
            if from_me {
                m.sender_name = from;
            } else {
                m.sender = from.clone();
                m.sender_name = from;
            }
        }
        if !to.is_empty() {
            m.to = vec![to];
        }
        m.text = content;
        m.service = "TikTok".to_string();

        // Non-text messages: record the media type as attachment metadata.
        if !media_type.is_empty() && !media_type.eq_ignore_ascii_case("text") {
            let mime = if media_type.eq_ignore_ascii_case("video") {
                "video"
            } else if media_type.eq_ignore_ascii_case("photo")
                || media_type.eq_ignore_ascii_case("image")
            {
                "image"
            } else {
                "media"
            };
            m.attachments.push(AttachmentMeta {
                name: format!("tiktok-{}", media_type.to_ascii_lowercase()),
                mime: mime.to_string(),
                bytes: 0,
            });
        }

        dms.push(m);
        stats.dms += 1;
    }
}

// ---------------------------------------------------------------------------
// Raw layer helpers

/// A raw watch event that carries `ts` for month-partitioning but serialises
/// the original item verbatim (flattened), mirroring the lastfm.rs pattern.
struct RawLine {
    ts: String,
    value: Value,
}

impl serde::Serialize for RawLine {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.value.serialize(s)
    }
}

/// Append raw rows to `social/tiktok/raw/<section>.jsonl`, one object per line.
/// Not date-partitioned (reference sections), matching the facebook.rs pattern.
fn append_raw_section(vault: &Vault, section: &str, rows: &[Value]) -> Result<()> {
    use std::io::Write;
    let rel = format!("{SOCIAL_RAW_DIR}/{section}.jsonl");
    let path = vault.resolve(&rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {rel}"))?;
    for row in rows {
        writeln!(f, "{}", serde_json::to_string(row)?)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Date parsing

/// TikTok export timestamps are `"YYYY-MM-DD HH:MM:SS"` UTC (confirmed from
/// community parsers: rothgar's nushell tiktok-download, extratone gist).
/// Returns RFC3339 with the local UTC offset.
pub(crate) fn parse_tiktok_date(s: &str) -> Option<String> {
    // Primary format: "YYYY-MM-DD HH:MM:SS" (UTC).
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S") {
        let dt: DateTime<Local> = Local.from_utc_datetime(&ndt);
        return Some(dt.to_rfc3339());
    }
    // With trailing " UTC" suffix (some exports append it).
    let stripped = s.trim().trim_end_matches(" UTC");
    if let Ok(ndt) = NaiveDateTime::parse_from_str(stripped, "%Y-%m-%d %H:%M:%S") {
        let dt: DateTime<Local> = Local.from_utc_datetime(&ndt);
        return Some(dt.to_rfc3339());
    }
    // ISO 8601 with T separator (defensive fallback).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s.trim()) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    None
}

// ---------------------------------------------------------------------------
// Guid helpers (SHA-256, length-prefixed for boundary safety)

/// SHA-256 of (section | date_str | content/link) with length-prefixed fields.
/// For watch events, `content` is the VideoLink URL, making same-second events
/// from different videos produce distinct guids.
fn event_guid(section: &str, date_str: &str, content: &str) -> String {
    let mut h = Sha256::new();
    for part in &[section.as_bytes(), date_str.as_bytes(), content.as_bytes()] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part);
    }
    format!("{:x}", h.finalize())
}

/// SHA-256 for DMs: (contact | date_str | from | content).
fn dm_guid(contact: &str, date_str: &str, from: &str, content: &str) -> String {
    let mut h = Sha256::new();
    for part in &[
        contact.as_bytes(),
        date_str.as_bytes(),
        from.as_bytes(),
        content.as_bytes(),
    ] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part);
    }
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-tiktok-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::vault::Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Date parsing

    #[test]
    fn parses_tiktok_date_formats() {
        // python3: datetime.datetime(2021,10,24,23,43,54,tzinfo=datetime.timezone.utc).timestamp()
        // → 1635119034
        const EPOCH: i64 = 1635119034;

        // Primary format: "YYYY-MM-DD HH:MM:SS" UTC.
        let ts = parse_tiktok_date("2021-10-24 23:43:54").unwrap();
        let dt = DateTime::parse_from_rfc3339(&ts).unwrap();
        assert_eq!(dt.timestamp(), EPOCH, "UTC epoch matches");

        // With trailing " UTC" suffix.
        let ts2 = parse_tiktok_date("2021-10-24 23:43:54 UTC").unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(&ts2).unwrap().timestamp(),
            EPOCH
        );

        // ISO 8601 with T separator (defensive fallback).
        let ts3 = parse_tiktok_date("2021-10-24T23:43:54Z").unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(&ts3).unwrap().timestamp(),
            EPOCH
        );

        // Bogus string → None.
        assert!(parse_tiktok_date("not a date").is_none());
    }

    // ---------------------------------------------------------------------------
    // Guid stability

    #[test]
    fn event_guid_stable_and_injective() {
        let g1 = event_guid("watch", "2021-10-24 23:43:54", "https://v.tiktok.com/abc");
        let g2 = event_guid("watch", "2021-10-24 23:43:54", "https://v.tiktok.com/abc");
        assert_eq!(g1, g2, "same inputs → same guid");
        assert_eq!(g1.len(), 64, "sha256 hex length");

        // Different section → different guid.
        let g3 = event_guid("like", "2021-10-24 23:43:54", "https://v.tiktok.com/abc");
        assert_ne!(g1, g3);

        // Length-prefix makes it injective across boundary shifts.
        let g4 = event_guid("wa", "tch2021-10-24", "");
        assert_ne!(g1, g4, "length-prefix prevents boundary collision");
    }

    #[test]
    fn dm_guid_stable_and_injective() {
        let g1 = dm_guid("alice", "2021-10-24 23:43:54", "alice", "hello");
        let g2 = dm_guid("alice", "2021-10-24 23:43:54", "alice", "hello");
        assert_eq!(g1, g2);
        assert_ne!(g1, dm_guid("alice", "2021-10-24 23:43:54", "alice", "world"));
        // Length-prefix injectivity.
        assert_ne!(g1, dm_guid("alice", "2021-10-2", "4 23:43:54", "hello"));
    }

    // ---------------------------------------------------------------------------
    // Sample user_data.json fixture

    fn sample_user_data() -> Value {
        // Keys confirmed from real TikTok exports:
        // - Watch history: "Video Browsing History" with "VideoLink" field
        // - Searches: "Search History"
        // - Comments: Comment → Comments → CommentsList
        // - DMs: "Chat History" (with space) as middle key, thread keys like
        //        "Chat History with alice:" (stripped to "alice" by parser)
        serde_json::json!({
            "Activity": {
                "Video Browsing History": {
                    "VideoList": [
                        {"Date": "2024-06-10 14:30:00", "VideoLink": "https://www.tiktokv.com/share/video/7380000000000000001/"},
                        {"Date": "2024-06-10 14:45:22", "VideoLink": "https://www.tiktokv.com/share/video/7380000000000000002/"},
                        {"Date": "2024-05-01 08:00:00", "VideoLink": "https://www.tiktokv.com/share/video/7360000000000000003/"}
                    ]
                },
                "Like List": {
                    "ItemFavoriteList": [
                        {"Date": "2024-06-09 20:00:00", "Link": "https://www.tiktokv.com/share/video/7370000000000000001/"},
                        {"Date": "2024-06-09 20:10:00", "Link": "https://www.tiktokv.com/share/video/7370000000000000002/"}
                    ]
                },
                "Comment": {
                    "Comments": {
                        "CommentsList": [
                            {"Date": "2024-06-08 12:00:00", "Comment": "Great video!", "URL": "https://www.tiktok.com/@user/video/12345"}
                        ]
                    }
                },
                "Search History": {
                    "SearchList": [
                        {"Date": "2024-06-07 10:00:00", "SearchTerm": "cat videos"}
                    ]
                }
            },
            "Video": {
                "Videos": {
                    "VideoList": [
                        {"Date": "2024-04-01 16:00:00", "Link": "https://www.tiktokv.com/share/video/7350000000000000001/"}
                    ]
                }
            },
            "Direct Messages": {
                "Chat History": {
                    "ChatHistory": {
                        "Chat History with alice:": [
                            {
                                "Date": "2024-06-05 10:00:00",
                                "From": "alice",
                                "To": "me",
                                "Content": "Hey!",
                                "MediaType": "text"
                            },
                            {
                                "Date": "2024-06-05 10:01:00",
                                "From": "me",
                                "To": "alice",
                                "Content": "Hi back!",
                                "MediaType": "text"
                            }
                        ]
                    }
                }
            },
            "Ads and Data": {
                "Ad Interests": {
                    "adInterestCategories": ["Technology", "Music", "Cooking"]
                }
            },
            "Profile": {
                "Profile Information": {
                    "ProfileMap": {
                        "userName": "testuser",
                        "bioDescription": "Just testing",
                        "followingCount": 42
                    }
                }
            }
        })
    }

    fn make_zip(name: &str, data: &Value) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-tiktok-zip-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("user_data.json", opts).unwrap();
        z.write_all(serde_json::to_string(data).unwrap().as_bytes()).unwrap();
        z.finish().unwrap();
        path
    }

    fn run_helper(v: &Vault, path: &std::path::Path, import_dms: bool) -> ImportOutcome {
        let mut params = BTreeMap::new();
        if import_dms {
            params.insert("acknowledge_dms".to_string(), "yes".to_string());
        }
        (IMPORT.run)(v, path, &params, &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_watch_events_to_media_plays_contract() {
        let v = temp_vault("watch");
        let data = sample_user_data();
        let zip = make_zip("watch", &data);

        let out = run_helper(&v, &zip, false);
        assert_eq!(out.counts.get("watch_events"), Some(&3), "3 watch events: {}", out.headline);

        // Partitioned by month: 2 in 2024-06, 1 in 2024-05.
        let jun =
            fs::read_to_string(v.root().join("media/plays/tiktok/2024-06.jsonl")).unwrap();
        let jun_rows: Vec<MediaItem> =
            jun.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(jun_rows.len(), 2);
        assert_eq!(jun_rows[0].source, "tiktok");
        assert_eq!(jun_rows[0].category, "video");
        assert_eq!(jun_rows[0].kind, "play");
        assert_eq!(jun_rows[0].subtitle, "TikTok");
        assert!(!jun_rows[0].guid.is_empty(), "guid set");
        assert_eq!(jun_rows[0].guid.len(), 64, "sha256 hex guid");
        assert!(
            jun_rows[0].detail.contains("tiktok"),
            "link in detail: {}",
            jun_rows[0].detail
        );
        assert_eq!(jun_rows[0].seconds, 0, "events not durations");

        // Raw layer mirrors the same partition.
        let jun_raw =
            fs::read_to_string(v.root().join("media/plays/tiktok/raw/2024-06.jsonl")).unwrap();
        assert_eq!(jun_raw.lines().count(), 2);
        assert!(jun_raw.contains("7380000000000000001"), "raw carries original link");

        let may =
            fs::read_to_string(v.root().join("media/plays/tiktok/2024-05.jsonl")).unwrap();
        assert_eq!(may.lines().count(), 1, "May event lands in 2024-05");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn dedupes_on_reimport() {
        let v = temp_vault("dedup");
        let data = sample_user_data();
        let zip = make_zip("dedup", &data);

        let first = run_helper(&v, &zip, false);
        let before =
            fs::read_to_string(v.root().join("media/plays/tiktok/2024-06.jsonl")).unwrap();

        let second = run_helper(&v, &zip, false);
        assert_eq!(
            second.counts.get("watch_events"),
            Some(&0),
            "all watch events deduped on re-import"
        );
        let dup_count = *second.counts.get("duplicates").unwrap_or(&0);
        let first_count = *first.counts.get("watch_events").unwrap_or(&0);
        assert_eq!(dup_count, first_count, "all originals counted as duplicates");

        let after =
            fs::read_to_string(v.root().join("media/plays/tiktok/2024-06.jsonl")).unwrap();
        assert_eq!(before, after, "contract file unchanged after re-import");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn imports_dms_with_acknowledgement() {
        let v = temp_vault("dms");
        let data = sample_user_data();
        let zip = make_zip("dms", &data);

        let out = run_helper(&v, &zip, true);
        assert_eq!(out.counts.get("dms"), Some(&2), "2 DM messages: {}", out.headline);

        let month =
            fs::read_to_string(v.root().join("correspondence/tiktok/2024-06.jsonl")).unwrap();
        let msgs: Vec<Message> =
            month.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].source, "tiktok");
        assert_eq!(msgs[0].chat, "alice");
        assert_eq!(msgs[0].text, "Hey!");
        assert!(!msgs[0].guid.is_empty());
        assert_eq!(msgs[0].service, "TikTok");
        // Second message is from_me (From != contact "alice").
        assert!(msgs[1].from_me, "reply is from_me");
        assert_eq!(msgs[1].text, "Hi back!");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn skips_dms_without_acknowledgement() {
        let v = temp_vault("no-dms");
        let data = sample_user_data();
        let zip = make_zip("no-dms", &data);

        let out = run_helper(&v, &zip, false);
        assert_eq!(out.counts.get("dms"), Some(&0));
        assert!(!v.root().join("correspondence/tiktok").exists(), "no DMs folder");

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn raw_sections_written_full_fidelity() {
        let v = temp_vault("raw");
        let data = sample_user_data();
        let zip = make_zip("raw", &data);

        run_helper(&v, &zip, false);

        // Liked videos, comments, searches, posts, ad interests all go to raw.
        let liked =
            fs::read_to_string(v.root().join("social/tiktok/raw/liked_videos.jsonl")).unwrap();
        assert_eq!(liked.lines().count(), 2, "2 liked videos");

        let comments =
            fs::read_to_string(v.root().join("social/tiktok/raw/comments.jsonl")).unwrap();
        assert!(comments.contains("Great video!"), "comment text preserved raw");

        let searches =
            fs::read_to_string(v.root().join("social/tiktok/raw/searches.jsonl")).unwrap();
        assert!(searches.contains("cat videos"));

        let posts = fs::read_to_string(v.root().join("social/tiktok/raw/posts.jsonl")).unwrap();
        assert_eq!(posts.lines().count(), 1, "1 own video post");

        let ad_interests =
            fs::read_to_string(v.root().join("social/tiktok/raw/ad_interests.jsonl")).unwrap();
        assert!(ad_interests.contains("Technology"));

        let profile =
            fs::read_to_string(v.root().join("social/tiktok/raw/profile.jsonl")).unwrap();
        assert!(profile.contains("testuser"));

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn watch_events_join_media_timeline() {
        let v = temp_vault("timeline");
        let data = sample_user_data();
        let zip = make_zip("timeline", &data);
        run_helper(&v, &zip, false);

        // The media timeline picks up the contract layer via contract_media_items.
        let day = v.media_timeline("2024-06-10").unwrap();
        assert_eq!(day.len(), 2, "2 watch events on 2024-06-10");
        assert!(day.iter().all(|i| i.source == "tiktok"));
        assert!(day.iter().all(|i| i.category == "video"));

        let _ = fs::remove_file(zip);
    }

    #[test]
    fn tolerates_missing_sections() {
        // An export with only Activity."Video Browsing History" (everything else absent).
        let v = temp_vault("sparse");
        let sparse = serde_json::json!({
            "Activity": {
                "Video Browsing History": {
                    "VideoList": [
                        {"Date": "2024-06-10 12:00:00", "VideoLink": "https://www.tiktokv.com/v/1/"}
                    ]
                }
            }
        });
        let zip = make_zip("sparse", &sparse);
        let out = run_helper(&v, &zip, true);
        assert_eq!(out.counts.get("watch_events"), Some(&1));
        assert_eq!(out.counts.get("dms"), Some(&0));
        let _ = fs::remove_file(zip);
    }

    #[test]
    fn tolerates_empty_video_list() {
        let v = temp_vault("empty-list");
        let empty = serde_json::json!({
            "Activity": {
                "Video Browsing History": {
                    "VideoList": []
                }
            }
        });
        let zip = make_zip("empty-list", &empty);
        let out = run_helper(&v, &zip, false);
        assert_eq!(out.counts.get("watch_events"), Some(&0));
        let _ = fs::remove_file(zip);
    }

    #[test]
    fn def_is_import_no_connection() {
        assert!(DEF.connection.is_none(), "no login required");
        assert!(DEF.import_spec().is_some(), "has import spec");
        assert_eq!(DEF.import_spec().unwrap().accepts, &["zip"]);
        assert_eq!(DEF.meta.id, "tiktok");
        assert_eq!(DEF.meta.kind, IntegrationKind::Import);
        assert!(!DEF.meta.default_on, "opt-in by default");
    }

    #[test]
    fn strips_chat_history_prefix() {
        // Real DM thread keys: "Chat History with USERNAME:" → "USERNAME"
        assert_eq!(super::strip_chat_history_prefix("Chat History with alice:"), "alice");
        assert_eq!(super::strip_chat_history_prefix("Chat History with bob_99:"), "bob_99");
        // Bare contact name (older exports) passes through unchanged.
        assert_eq!(super::strip_chat_history_prefix("alice"), "alice");
        // Trailing colon only (edge case) → empty string trimmed.
        assert_eq!(super::strip_chat_history_prefix("Chat History with :"), "");
    }

    #[test]
    fn fallback_video_browse_history_key() {
        // Older exports may use "Video Browse History" — parser should still work.
        let v = temp_vault("old-key");
        let old_key = serde_json::json!({
            "Activity": {
                "Video Browse History": {
                    "VideoList": [
                        {"Date": "2024-06-10 09:00:00", "VideoLink": "https://www.tiktokv.com/share/video/999/"}
                    ]
                }
            }
        });
        let zip = make_zip("old-key", &old_key);
        let out = run_helper(&v, &zip, false);
        assert_eq!(out.counts.get("watch_events"), Some(&1), "fallback key works");
        let _ = fs::remove_file(zip);
    }

    #[test]
    fn dm_chat_field_uses_stripped_username() {
        // DM thread key "Chat History with alice:" must produce chat="alice"
        let v = temp_vault("dm-chat-field");
        let data = sample_user_data();
        let zip = make_zip("dm-chat-field", &data);
        let out = run_helper(&v, &zip, true);
        assert_eq!(out.counts.get("dms"), Some(&2));
        let month =
            fs::read_to_string(v.root().join("correspondence/tiktok/2024-06.jsonl")).unwrap();
        let msgs: Vec<Message> =
            month.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        // chat must be bare "alice", not "Chat History with alice:"
        assert_eq!(msgs[0].chat, "alice", "chat is stripped username");
        // from_me: alice → false (alice sent it), me → true (me sent it)
        assert!(!msgs[0].from_me, "alice's message is not from_me");
        assert!(msgs[1].from_me, "our reply is from_me");
        let _ = fs::remove_file(zip);
    }
}

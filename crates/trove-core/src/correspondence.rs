//! Correspondence — the unified message store shared by every conversation
//! source (iMessage, email, Slack, …).
//!
//! All sources write one record shape into one tree, so reads, charts, and
//! future AI analysis never care where a message came from:
//!
//! ```text
//! correspondence/<source>/YYYY-MM.jsonl     one message per line, by month
//! ```
//!
//! ```json
//! {"ts":"2026-06-10T14:03:01.123-07:00","source":"imessage","chat":"+15551234567",
//!  "sender":"+15551234567","from_me":false,"kind":"message","text":"hey!",
//!  "service":"iMessage","guid":"ABCD-…","rowid":88123}
//! ```
//!
//! Monthly files (not daily like `browser/`) per the vault conventions in
//! docs/data-sources.md — message volume is a few thousand per month, and a
//! month of context is the natural unit to hand an LLM.
//!
//! Core fields are universal (`ts`, `source`, `chat`, `sender`, `from_me`,
//! `kind`, `text`); everything else is optional and omitted when empty, so an
//! iMessage line never carries email baggage and vice versa. Source-native
//! identifiers (`guid`, `rowid`) ride along for incremental cursors and
//! dedupe — full fidelity at write time, opinions at read time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::activity::days;
use crate::health::SeriesPoint;
use crate::vault::Vault;

/// One message in any conversation medium.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Message {
    /// RFC3339 local time the message was sent/received.
    pub ts: String,
    /// "imessage" | "email" | "slack" | …
    pub source: String,
    /// Conversation key within the source: the other party's handle or the
    /// group chat identifier (iMessage), the thread subject (email), the
    /// channel name (Slack).
    #[serde(default)]
    pub chat: String,
    /// Human display name for the chat when the source has one (group chat
    /// names, channel topics). Empty for plain 1:1 threads.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat_name: String,
    /// Canonical address of the author: phone/email handle, email address,
    /// Slack username. Empty when `from_me` (the vault owner is implicit).
    #[serde(default)]
    pub sender: String,
    /// Display name for the sender when the source carries one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sender_name: String,
    #[serde(default)]
    pub from_me: bool,
    /// "message" (default), "reaction" (tapbacks etc.), "event" (group
    /// renames, member changes), or "call" (phone/FaceTime).
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    /// Connected seconds for kind "call" (0 = missed/unanswered).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub duration_secs: u64,
    /// Explicit recipients, when the medium has them (email to/cc).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subject: String,
    /// Transport detail: "iMessage"/"SMS"/"RCS", the email account address,
    /// the Slack workspace.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentMeta>,
    /// Reaction name for kind "reaction": "loved", "liked", "disliked",
    /// "laughed", "emphasized", "questioned", "emoji", or "removed-…".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reaction: String,
    /// Source-native id of the message this reacts or replies to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reply_to: String,
    /// Source-native labels/folders the message carries: Gmail system labels
    /// (`INBOX`, `SENT`, `IMPORTANT`, `CATEGORY_PROMOTIONS`, …) and user
    /// labels. A read-time filter — "human correspondence" is `INBOX`/`SENT`
    /// minus the `CATEGORY_*` buckets. Empty for sources without labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Source-unique id (iMessage guid, email Message-ID, Slack channel/ts) —
    /// the dedupe key within a source.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guid: String,
    /// Source-native monotonic id where one exists (iMessage ROWID); drives
    /// incremental cursors and their rebuild from these files.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rowid: i64,
}

fn default_kind() -> String {
    "message".into()
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl Message {
    /// A blank "message"-kind record for `source` at `ts` — fill in the rest.
    pub fn new(source: &str, ts: String) -> Self {
        Self {
            ts,
            source: source.into(),
            chat: String::new(),
            chat_name: String::new(),
            sender: String::new(),
            sender_name: String::new(),
            from_me: false,
            kind: default_kind(),
            text: String::new(),
            duration_secs: 0,
            to: Vec::new(),
            subject: String::new(),
            service: String::new(),
            attachments: Vec::new(),
            reaction: String::new(),
            reply_to: String::new(),
            labels: Vec::new(),
            guid: String::new(),
            rowid: 0,
        }
    }
}

/// Attachment metadata only — the file itself stays wherever the source app
/// keeps it (copying gigabytes of media into the vault is a later, opt-in
/// decision).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AttachmentMeta {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bytes: i64,
}

/// Message volume for one conversation over a range.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ChatUsage {
    pub chat: String,
    /// Display name when any record carried one.
    pub chat_name: String,
    pub source: String,
    pub messages: u64,
    pub sent: u64,
}

/// Aggregate of a date range for the Correspondence view.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CorrespondenceSummary {
    /// kind == "message" only — reactions and group events are stored but not
    /// counted as conversation volume.
    pub messages: u64,
    pub sent: u64,
    pub received: u64,
    /// Messages per source ("imessage" → n), descending.
    pub sources: BTreeMap<String, u64>,
    /// Conversations by message count, descending.
    pub chats: Vec<ChatUsage>,
}

/// One page of the email browser: the requested slice plus the counts the
/// view needs to render filters without a second scan.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct EmailPage {
    /// Messages matching the filters (range/account/query), newest first,
    /// after `offset`/`limit` — full records, body text included.
    pub messages: Vec<Message>,
    /// Total matches before pagination (drives "N emails" + Load more).
    pub total: u64,
    /// Matches per account address (the record's `service`) over the same
    /// filters minus the account filter itself — so the account chips keep
    /// their counts while one account is selected.
    pub accounts: BTreeMap<String, u64>,
}

/// The months (YYYY-MM) an inclusive date range touches.
pub(crate) fn months(from: &str, to: &str) -> Result<Vec<String>> {
    let start = NaiveDate::parse_from_str(from, "%Y-%m-%d").context("bad from date")?;
    let end = NaiveDate::parse_from_str(to, "%Y-%m-%d").context("bad to date")?;
    let mut out = Vec::new();
    let (mut y, mut m) = (start.year(), start.month());
    let (ey, em) = (end.year(), end.month());
    while (y, m) <= (ey, em) {
        out.push(format!("{y:04}-{m:02}"));
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    Ok(out)
}

impl Vault {
    /// Append messages to their month's JSONL log, grouped per source.
    /// Callers are responsible for not re-appending records the vault already
    /// holds (cursors for live collectors, guid dedupe for imports).
    pub fn append_messages(&self, messages: &[Message]) -> Result<()> {
        let mut by_source: BTreeMap<&str, Vec<&Message>> = BTreeMap::new();
        for m in messages {
            by_source.entry(&m.source).or_default().push(m);
        }
        for (source, ms) in by_source {
            self.stream(&format!("correspondence/{source}"), crate::store::Partition::Month)
                .append(&ms, |m| &m.ts)?;
        }
        Ok(())
    }

    /// The source folders that exist under correspondence/.
    fn correspondence_sources(&self) -> Vec<String> {
        let dir = self.root().join("correspondence");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// Every message of one month for one source, in file order.
    pub(crate) fn read_correspondence_month(&self, source: &str, month: &str) -> Result<Vec<Message>> {
        let path = self.resolve(&format!("correspondence/{source}/{month}.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)
            .with_context(|| format!("reading correspondence/{source}/{month}.jsonl"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Message>(l).ok())
            .collect())
    }

    /// All messages of one local day (YYYY-MM-DD) across every source,
    /// sorted by timestamp.
    pub fn correspondence_timeline(&self, date: &str) -> Result<Vec<Message>> {
        let month = &date[..7.min(date.len())];
        let mut out = Vec::new();
        for source in self.correspondence_sources() {
            out.extend(
                self.read_correspondence_month(&source, month)?
                    .into_iter()
                    .filter(|m| m.ts.starts_with(date)),
            );
        }
        out.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(out)
    }

    /// Counts plus per-conversation volume over an inclusive date range.
    pub fn correspondence_summary(&self, from: &str, to: &str) -> Result<CorrespondenceSummary> {
        let mut summary = CorrespondenceSummary {
            messages: 0,
            sent: 0,
            received: 0,
            sources: BTreeMap::new(),
            chats: Vec::new(),
        };
        let mut chats: HashMap<(String, String), ChatUsage> = HashMap::new();
        for source in self.correspondence_sources() {
            for month in months(from, to)? {
                for m in self.read_correspondence_month(&source, &month)? {
                    let day = &m.ts[..10.min(m.ts.len())];
                    if day < from || day > to || m.kind != "message" {
                        continue;
                    }
                    summary.messages += 1;
                    if m.from_me {
                        summary.sent += 1;
                    } else {
                        summary.received += 1;
                    }
                    *summary.sources.entry(m.source.clone()).or_default() += 1;
                    let entry = chats
                        .entry((m.source.clone(), m.chat.clone()))
                        .or_insert_with(|| ChatUsage {
                            chat: m.chat.clone(),
                            chat_name: String::new(),
                            source: m.source.clone(),
                            messages: 0,
                            sent: 0,
                        });
                    entry.messages += 1;
                    if m.from_me {
                        entry.sent += 1;
                    }
                    if entry.chat_name.is_empty() && !m.chat_name.is_empty() {
                        entry.chat_name = m.chat_name.clone();
                    }
                }
            }
        }
        summary.chats = chats.into_values().collect();
        summary
            .chats
            .sort_by(|a, b| b.messages.cmp(&a.messages).then_with(|| a.chat.cmp(&b.chat)));
        Ok(summary)
    }

    /// Messages per day over an inclusive range — a trend series for the
    /// chart. Days with no messages are omitted.
    pub fn correspondence_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut per_day: BTreeMap<String, u64> = BTreeMap::new();
        for source in self.correspondence_sources() {
            for month in months(from, to)? {
                for m in self.read_correspondence_month(&source, &month)? {
                    if m.kind != "message" || m.ts.len() < 10 {
                        continue;
                    }
                    let day = m.ts[..10].to_string();
                    if day.as_str() >= from && day.as_str() <= to {
                        *per_day.entry(day).or_default() += 1;
                    }
                }
            }
        }
        // days() validates the range format; output stays ordered and dense
        // filtering is the chart's job.
        let _ = days(from, to)?;
        Ok(per_day
            .into_iter()
            .map(|(date, n)| SeriesPoint {
                date,
                value: n as f64,
            })
            .collect())
    }

    /// The email browser's read: messages from `correspondence/email/` over
    /// an inclusive date range, newest first, optionally filtered to one
    /// account (the record's `service`) and/or a case-insensitive substring
    /// `query` over sender, subject, and body. `offset`/`limit` paginate;
    /// paged records come back whole (the body is the point of the browser).
    ///
    /// The whole range is scanned (totals and account counts need it — one
    /// linear pass over local monthly files), but months are walked
    /// newest-first so full records are retained only until the requested
    /// page is covered; past that point matches are merely counted, keeping
    /// "all time" over a large archive at page-sized memory.
    pub fn email_list(
        &self,
        from: &str,
        to: &str,
        account: Option<&str>,
        query: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<EmailPage> {
        let needle = query.map(str::to_lowercase).filter(|q| !q.is_empty());
        let mut page = EmailPage {
            messages: Vec::new(),
            total: 0,
            accounts: BTreeMap::new(),
        };
        let wanted = offset as usize + limit as usize;
        let mut kept: Vec<Message> = Vec::new();
        for month in months(from, to)?.into_iter().rev() {
            // File order within a month isn't time order (backfills append
            // newest-first, incremental passes append as mail arrives), so
            // each month is sorted before pagination — across months the
            // partition key already guarantees order.
            let mut in_month: Vec<Message> = Vec::new();
            for m in self.read_correspondence_month("email", &month)? {
                let day = &m.ts[..10.min(m.ts.len())];
                if day < from || day > to || m.kind != "message" {
                    continue;
                }
                if let Some(q) = &needle {
                    let hit = m.sender.to_lowercase().contains(q)
                        || m.sender_name.to_lowercase().contains(q)
                        || m.subject.to_lowercase().contains(q)
                        || m.text.to_lowercase().contains(q);
                    if !hit {
                        continue;
                    }
                }
                // Account counts ignore the account filter so the chips keep
                // showing what switching to them would yield.
                *page.accounts.entry(m.service.clone()).or_default() += 1;
                if account.is_some_and(|a| a != m.service) {
                    continue;
                }
                page.total += 1;
                if kept.len() < wanted {
                    in_month.push(m);
                }
            }
            if !in_month.is_empty() {
                in_month.sort_by(|a, b| b.ts.cmp(&a.ts));
                in_month.truncate(wanted - kept.len());
                kept.extend(in_month);
            }
        }
        page.messages = kept.into_iter().skip(offset as usize).collect();
        Ok(page)
    }

    /// All guids already stored for `source` — the dedupe set for re-runnable
    /// imports (email mbox, Slack zips). Live collectors use cursors instead.
    pub(crate) fn correspondence_guids(&self, source: &str) -> Result<HashSet<String>> {
        let dir = self.root().join("correspondence").join(source);
        let mut out = HashSet::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(out);
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
                    if !m.guid.is_empty() {
                        out.insert(m.guid);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-corr-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn msg(source: &str, ts: &str, chat: &str, from_me: bool, text: &str) -> Message {
        Message {
            chat: chat.into(),
            from_me,
            text: text.into(),
            ..Message::new(source, ts.into())
        }
    }

    #[test]
    fn months_spanning_year_boundary() {
        assert_eq!(
            months("2025-11-03", "2026-02-10").unwrap(),
            vec!["2025-11", "2025-12", "2026-01", "2026-02"]
        );
        assert_eq!(months("2026-06-01", "2026-06-30").unwrap(), vec!["2026-06"]);
        assert!(months("junk", "2026-06-30").is_err());
    }

    #[test]
    fn append_groups_by_source_and_month() {
        let v = temp_vault("append");
        v.append_messages(&[
            msg("imessage", "2026-05-31T23:50:00-07:00", "+1555", false, "may"),
            msg("imessage", "2026-06-01T08:00:00-07:00", "+1555", true, "june"),
            msg("email", "2026-06-01T09:00:00-07:00", "thread", false, "mail"),
        ])
        .unwrap();
        assert!(v.root().join("correspondence/imessage/2026-05.jsonl").exists());
        assert!(v.root().join("correspondence/imessage/2026-06.jsonl").exists());
        assert!(v.root().join("correspondence/email/2026-06.jsonl").exists());

        let day = v.correspondence_timeline("2026-06-01").unwrap();
        assert_eq!(day.len(), 2);
        // Merged across sources, sorted by ts.
        assert_eq!(day[0].text, "june");
        assert_eq!(day[1].source, "email");
    }

    #[test]
    fn summary_counts_messages_not_reactions() {
        let v = temp_vault("summary");
        let mut reaction = msg("imessage", "2026-06-10T10:01:00-07:00", "+1555", false, "Liked “hi”");
        reaction.kind = "reaction".into();
        reaction.reaction = "liked".into();
        v.append_messages(&[
            msg("imessage", "2026-06-10T10:00:00-07:00", "+1555", true, "hi"),
            msg("imessage", "2026-06-10T10:02:00-07:00", "+1555", false, "hey"),
            msg("imessage", "2026-06-11T10:00:00-07:00", "chat-group", false, "yo"),
            reaction,
        ])
        .unwrap();

        let s = v.correspondence_summary("2026-06-10", "2026-06-10").unwrap();
        assert_eq!(s.messages, 2);
        assert_eq!(s.sent, 1);
        assert_eq!(s.received, 1);
        assert_eq!(s.sources.get("imessage"), Some(&2));
        assert_eq!(s.chats.len(), 1);
        assert_eq!(s.chats[0].chat, "+1555");

        let daily = v.correspondence_daily("2026-06-09", "2026-06-12").unwrap();
        assert_eq!(daily.len(), 2);
        assert_eq!(daily[0].value, 2.0);
        assert_eq!(daily[1].value, 1.0);
    }

    #[test]
    fn email_list_filters_sorts_and_paginates() {
        let v = temp_vault("email-list");
        let mut mk = |ts: &str, acct: &str, sender: &str, subject: &str, body: &str| {
            let mut m = msg("email", ts, "thread", false, body);
            m.service = acct.into();
            m.sender = sender.into();
            m.subject = subject.into();
            m
        };
        let mut reaction = mk("2026-06-10T12:00:00-07:00", "a@x.com", "z@y.com", "noise", "n");
        reaction.kind = "event".into();
        v.append_messages(&[
            mk("2026-05-20T10:00:00-07:00", "a@x.com", "alice@y.com", "May invoice", "attached"),
            mk("2026-06-10T09:00:00-07:00", "a@x.com", "bob@y.com", "Lunch", "tacos?"),
            mk("2026-06-10T11:00:00-07:00", "b@x.com", "carol@y.com", "Report", "the invoice is ready"),
            mk("2026-06-11T08:00:00-07:00", "a@x.com", "alice@y.com", "Re: Lunch", "yes"),
            reaction,
        ])
        .unwrap();

        // Newest first across months, non-message kinds excluded.
        let page = v.email_list("2026-05-01", "2026-06-30", None, None, 10, 0).unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(page.messages.len(), 4);
        assert_eq!(page.messages[0].subject, "Re: Lunch");
        assert_eq!(page.messages[3].subject, "May invoice");
        assert_eq!(page.accounts.get("a@x.com"), Some(&3));
        assert_eq!(page.accounts.get("b@x.com"), Some(&1));

        // Pagination: offset past the first page picks up where it left off.
        let p1 = v.email_list("2026-05-01", "2026-06-30", None, None, 2, 0).unwrap();
        let p2 = v.email_list("2026-05-01", "2026-06-30", None, None, 2, 2).unwrap();
        assert_eq!(p1.messages.len(), 2);
        assert_eq!(p2.messages.len(), 2);
        assert_eq!(p1.total, 4);
        assert_eq!(p2.messages[0].subject, "Lunch");
        assert_eq!(p2.messages[1].subject, "May invoice");

        // Account filter narrows messages/total but not the account counts.
        let page = v.email_list("2026-05-01", "2026-06-30", Some("b@x.com"), None, 10, 0).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.messages[0].sender, "carol@y.com");
        assert_eq!(page.accounts.get("a@x.com"), Some(&3));

        // Query matches subject or body, case-insensitively.
        let page = v.email_list("2026-05-01", "2026-06-30", None, Some("INVOICE"), 10, 0).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.messages[0].subject, "Report");
        assert_eq!(page.messages[1].subject, "May invoice");

        // Range bounds apply by local day.
        let page = v.email_list("2026-06-10", "2026-06-10", None, None, 10, 0).unwrap();
        assert_eq!(page.total, 2);
    }

    #[test]
    fn guid_set_reads_back() {
        let v = temp_vault("guids");
        let mut a = msg("email", "2026-06-10T10:00:00-07:00", "t", false, "a");
        a.guid = "<id-1@x>".into();
        let b = msg("email", "2026-06-10T11:00:00-07:00", "t", false, "no guid");
        v.append_messages(&[a, b]).unwrap();
        let guids = v.correspondence_guids("email").unwrap();
        assert_eq!(guids.len(), 1);
        assert!(guids.contains("<id-1@x>"));
        assert!(v.correspondence_guids("slack").unwrap().is_empty());
    }
}

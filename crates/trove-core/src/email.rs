//! Email import — an M1 ("one-shot file import") source: the user drops a
//! `.mbox` file (Google Takeout, Apple Mail export, anything RFC 4155-ish)
//! and every message lands in the unified correspondence stream
//! (`correspondence/email/YYYY-MM.jsonl`, see [`crate::correspondence`]).
//!
//! Imports are **re-runnable**: every stored line carries a guid (the
//! Message-ID, or a content hash when a message has none), and an import
//! first loads the already-stored guid set and skips duplicates — so
//! overlapping Takeout exports, or importing the same file twice, never
//! duplicate a message. An incremental IMAP pull (M5) can land later and
//! write the same stream the same way.
//!
//! `account` is the address whose mailbox the file is — it decides
//! `from_me` and is recorded as `service`, so multiple accounts coexist in
//! one stream. Threading: `chat` is mail-parser's thread name (the subject
//! stripped of Re:/Fwd:/[list] noise), lowercased — cheap, language-tolerant,
//! and good enough to group conversations without walking References graphs.
//!
//! Bodies are stored in full (text part preferred, HTML converted to text
//! otherwise) — full fidelity at write time, trimming/snippeting is a
//! read-time opinion. Attachment *metadata* only, like iMessage.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use mail_parser::{MessageParser, MimeHeaders};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, IntegrationDef};
use crate::vault::Vault;

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/email"))
}

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<crate::registry::ImportOutcome> {
    let account = params.get("account").map(String::as_str).unwrap_or("").trim().to_string();
    if account.is_empty() {
        anyhow::bail!("the mailbox's own address is required (it decides which messages count as sent by you)");
    }
    let s = vault.import_mbox(path, &account, |p| progress(p))?;
    Ok(crate::registry::ImportOutcome {
        headline: format!(
            "{} messages imported, {} duplicates skipped",
            s.imported, s.duplicates
        ),
        counts: [
            ("imported", s.imported),
            ("duplicates", s.duplicates),
            ("failed", s.failed),
        ]
        .into(),
    })
}

static IMPORT: crate::registry::ImportSpec = crate::registry::ImportSpec {
    signatures: &[],
    accepts: &["mbox"],
    params: &[crate::registry::ImportParam {
        key: "account",
        label: "Mailbox address",
        placeholder: "you@example.com",
        required: true,
    }],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "email",
        name: "Email archives",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import Gmail Takeout or Apple Mail .mbox files. Re-runnable — overlapping exports never duplicate.",
        domain: "correspondence",
        vault_path: "correspondence/email/",
        toggleable: false,
        setup: &[
            "Gmail: takeout.google.com → deselect all → Mail → export, then download the .mbox.",
            "Import it here with the mailbox's own address — that decides which messages count as sent by you.",
        ],
        caveats: "One-off archives only for now; an incremental Gmail/IMAP pull on the OAuth foundation is planned. Attachment metadata is kept, attachment files are not (yet, by choice).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Result of one mbox import, for the UI.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct EmailImportStats {
    /// Messages newly written to the vault.
    pub imported: u64,
    /// Messages skipped because their guid was already stored.
    pub duplicates: u64,
    /// Blocks mail-parser could not parse, or that had no usable date.
    pub failed: u64,
}

/// Iterate raw RFC822 message blocks out of an mbox stream. Messages are
/// delimited by "From " envelope lines; ">From"-quoted body lines (mboxrd
/// and friends) are unescaped by stripping one '>'.
fn mbox_blocks<R: BufRead>(
    mut reader: R,
    mut on_block: impl FnMut(Vec<u8>, u64) -> Result<()>,
) -> Result<()> {
    let mut block: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut consumed: u64 = 0;
    let mut in_message = false;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        consumed += n as u64;
        if line.starts_with(b"From ") {
            if in_message && !block.is_empty() {
                on_block(std::mem::take(&mut block), consumed)?;
            }
            in_message = true;
            continue; // the envelope line itself is not part of the message
        }
        if !in_message {
            continue; // preamble before the first envelope
        }
        // Unescape ">From " / ">>From " quoting: strip exactly one '>'.
        let unquoted = {
            let stripped = line.iter().take_while(|&&b| b == b'>').count();
            if stripped > 0 && line[stripped..].starts_with(b"From ") {
                &line[1..]
            } else {
                &line[..]
            }
        };
        block.extend_from_slice(unquoted);
    }
    if in_message && !block.is_empty() {
        on_block(block, consumed)?;
    }
    Ok(())
}

/// Stable fallback guid for messages without a Message-ID, so re-imports
/// still dedupe them: a hash of the full raw message.
fn content_guid(raw: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(raw);
    format!("sha256:{:x}", h.finalize())
}

/// One raw RFC822 message → a correspondence record. None when unparseable
/// or undateable (a message we can't place in a month file). Shared by the
/// mbox importer and the Gmail puller ([`crate::gmail`]), which decodes the
/// API's `format=raw` payload into the same bytes and then layers on Gmail's
/// labels and authoritative `SENT`-based `from_me`.
pub(crate) fn email_to_message(raw: &[u8], account: &str) -> Option<Message> {
    let parsed = MessageParser::default().parse(raw)?;
    let ts = parsed.date().map(|d| d.to_timestamp())?;
    let local: DateTime<Local> = DateTime::from_timestamp(ts, 0)?.with_timezone(&Local);

    let mut m = Message::new("email", local.to_rfc3339());
    m.guid = parsed
        .message_id()
        .map(|id| format!("<{id}>"))
        .unwrap_or_else(|| content_guid(raw));
    m.service = account.to_lowercase();
    if let Some(addr) = parsed.from().and_then(|a| a.first()) {
        let address = addr.address().unwrap_or_default().to_lowercase();
        m.from_me = address == m.service;
        m.sender = address;
        m.sender_name = addr.name().unwrap_or_default().to_string();
    }
    for list in [parsed.to(), parsed.cc()].into_iter().flatten() {
        m.to.extend(
            list.iter()
                .filter_map(|a| a.address())
                .map(|a| a.to_lowercase()),
        );
    }
    m.subject = parsed.subject().unwrap_or_default().to_string();
    m.chat = parsed
        .thread_name()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .unwrap_or_else(|| "(no subject)".into());
    m.text = parsed
        .body_text(0)
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    m.reply_to = parsed
        .in_reply_to()
        .as_text()
        .map(|id| format!("<{id}>"))
        .unwrap_or_default();
    m.attachments = parsed
        .attachments()
        .map(|p| AttachmentMeta {
            name: p.attachment_name().unwrap_or_default().to_string(),
            mime: p
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(sub) => format!("{}/{sub}", ct.ctype()),
                    None => ct.ctype().to_string(),
                })
                .unwrap_or_default(),
            bytes: p.len() as i64,
        })
        .collect();
    Some(m)
}

impl Vault {
    /// Import every message of an mbox file. Re-runnable: already-stored
    /// guids are skipped. `progress` is called with bytes-consumed percent
    /// every few hundred messages (Takeout mboxes run to gigabytes).
    pub fn import_mbox<F>(
        &self,
        path: &Path,
        account: &str,
        mut progress: F,
    ) -> Result<EmailImportStats>
    where
        F: FnMut(ImportProgress),
    {
        let total = std::fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .len()
            .max(1);
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let reader = BufReader::with_capacity(1 << 20, file);

        let mut seen = self.correspondence_guids("email")?;
        let mut stats = EmailImportStats {
            imported: 0,
            duplicates: 0,
            failed: 0,
        };
        let mut batch: Vec<Message> = Vec::new();
        let mut since_progress = 0u32;
        mbox_blocks(reader, |raw, consumed| {
            match email_to_message(&raw, account) {
                Some(m) => {
                    if seen.insert(m.guid.clone()) {
                        batch.push(m);
                        stats.imported += 1;
                    } else {
                        stats.duplicates += 1;
                    }
                }
                None => stats.failed += 1,
            }
            // Flush in chunks so memory stays flat on huge mailboxes.
            if batch.len() >= 2000 {
                self.append_messages(&batch)?;
                batch.clear();
            }
            since_progress += 1;
            if since_progress >= 250 {
                since_progress = 0;
                progress(ImportProgress {
                    records: stats.imported,
                    percent: (consumed as f32 / total as f32) * 100.0,
                });
            }
            Ok(())
        })?;
        self.append_messages(&batch)?;
        progress(ImportProgress {
            records: stats.imported,
            percent: 100.0,
        });
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-email-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    const MBOX: &str = "From 1234567890@xxx Thu Jun 11 10:00:00 2026\n\
Message-ID: <one@example.com>\n\
Date: Wed, 10 Jun 2026 09:00:00 -0700\n\
From: Alice Example <alice@example.com>\n\
To: David Wills <david@wills.dev>\n\
Subject: Lunch plans\n\
\n\
Want to grab lunch?\n\
>From my favorite spot, of course.\n\
\n\
From 1234567891@xxx Thu Jun 11 11:00:00 2026\n\
Message-ID: <two@example.com>\n\
In-Reply-To: <one@example.com>\n\
Date: Wed, 10 Jun 2026 09:30:00 -0700\n\
From: David Wills <david@wills.dev>\n\
To: alice@example.com\n\
Cc: bob@example.com\n\
Subject: Re: Lunch plans\n\
\n\
Absolutely. Noon?\n";

    #[test]
    fn mbox_import_parses_threads_and_dedupes() {
        let v = temp_vault("import");
        let path = std::env::temp_dir().join(format!("trove-test-{}.mbox", std::process::id()));
        fs::write(&path, MBOX).unwrap();

        let stats = v.import_mbox(&path, "david@wills.dev", |_| {}).unwrap();
        assert_eq!(stats.imported, 2);
        assert_eq!(stats.duplicates, 0);
        assert_eq!(stats.failed, 0);

        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);
        let first = &day[0];
        assert_eq!(first.sender, "alice@example.com");
        assert_eq!(first.sender_name, "Alice Example");
        assert!(!first.from_me);
        assert_eq!(first.subject, "Lunch plans");
        assert_eq!(first.chat, "lunch plans");
        assert_eq!(first.to, vec!["david@wills.dev"]);
        // mboxrd ">From" unescaped.
        assert!(first.text.contains("From my favorite spot"));

        let reply = &day[1];
        assert!(reply.from_me);
        assert_eq!(reply.chat, "lunch plans", "Re: strips to the same thread");
        assert_eq!(reply.reply_to, "<one@example.com>");
        assert_eq!(reply.to, vec!["alice@example.com", "bob@example.com"]);

        // Re-import: everything is a duplicate, nothing is appended.
        let again = v.import_mbox(&path, "david@wills.dev", |_| {}).unwrap();
        assert_eq!(again.imported, 0);
        assert_eq!(again.duplicates, 2);
        assert_eq!(v.correspondence_timeline("2026-06-10").unwrap().len(), 2);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn message_without_id_gets_stable_hash_guid() {
        let raw = b"Date: Wed, 10 Jun 2026 09:00:00 -0700\nFrom: x@y.z\nSubject: hi\n\nbody\n";
        let a = email_to_message(raw, "me@my.org").unwrap();
        let b = email_to_message(raw, "me@my.org").unwrap();
        assert!(a.guid.starts_with("sha256:"));
        assert_eq!(a.guid, b.guid, "hash guid is stable for dedupe");
        assert!(!a.from_me);
    }

    #[test]
    fn undateable_blocks_count_as_failed() {
        let v = temp_vault("failed");
        let path = std::env::temp_dir().join(format!("trove-test-{}-bad.mbox", std::process::id()));
        fs::write(&path, "From x@x Thu Jun 11 10:00:00 2026\nSubject: no date header\n\nhello\n")
            .unwrap();
        let stats = v.import_mbox(&path, "me@my.org", |_| {}).unwrap();
        assert_eq!(stats.imported, 0);
        assert_eq!(stats.failed, 1);
        let _ = fs::remove_file(path);
    }
}

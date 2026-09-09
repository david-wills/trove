# Apple Mail

- **id:** `apple-mail`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the local Envelope Index, like iMessage's
  chat.db poll)
- **connection:** none (local files; Full Disk Access TCC grant)
- **evidence:** community-schema — Envelope Index SQLite schema, stable
  across V9/V10, well-documented; confidence high
- **effort / priority:** M / P1
- **needs:** privacy-sensitive (message bodies — opt-in with explicit
  acknowledgement)

## What it is

The Mail.app local store: every account the user has configured (iCloud,
Gmail, Outlook, custom IMAP) keeps a cached copy on disk. Zero-config and
credential-free — for users who won't paste tokens or grant OAuth, this
one FDA grant captures all their mail at once. Complements the cloud
pulls rather than replacing them (covers only what Mail has synced
locally).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Headers + snippets | any Mail.app account | ts, sender, to, subject, ~first-hundreds-of-chars snippet | community schema (Envelope Index) |
| Full bodies | any Mail.app account | full text + attachment metadata via `.emlx` walk | community schema (`.emlx` = RFC822 + plist trailer) |
| Multi-account coverage | n/a | `service` = the account address per message | community schema |

All optional in the contract; snippet-only rows (when an `.emlx` is
missing/unsynced) are valid rows.

## Access & auth

- Envelope Index SQLite at `~/Library/Mail/V10/MailData/Envelope Index`
  (V10 = macOS 13/14/15/26; V9 = Monterey). Key tables: `messages`,
  `subjects`, `addresses`, `recipients`, `attachments`; snippets in
  `summaries`.
- Full bodies: `~/Library/Mail/V10/<UUID>/<Mailbox>.mbox/Messages/<id>.emlx`.
- Permission: Full Disk Access — already on Trove's permission ladder
  (iMessage chat.db needs it), so no new grant for most users.
- Copy-then-read (WAL; Mail.app may hold the DB), per the established
  chat.db pattern. No network, standalone-clean.

## Vault mapping

- **Raw layer:** none separate — parsed messages go straight to contract
  rows (the `.emlx`/RFC822 source is the same shape the email parser
  already consumes).
- **Contract layer:** `correspondence/email/YYYY-MM.jsonl` — the shared
  email sink per the ratified correspondence contract (research entry
  confirms: "Both reads can use the existing correspondence/email/ JSONL
  sink"). Message-ID header as `guid` — critical here, since Apple Mail
  caches the same Gmail/iCloud messages the cloud pulls and mbox imports
  also reach; the shared guid makes all mechanisms mutually dedupe-safe.
- **Cursor:** Envelope Index ROWID watermark in sync state, rebuildable.

## Build plan

1. Module `crates/trove-core/src/apple_mail.rs`: `DEF` (Periodic,
   chat.db-style cadence), permission hook = FDA probe on the Envelope
   Index path.
2. Registration line in `INTEGRATIONS`; no connection.
3. Copy-then-read the SQLite (reuse the imessage.rs copy pattern); join
   messages→addresses→recipients for headers; resolve each message's
   `.emlx` for full body via the shared RFC822 parser, falling back to
   the snippet when absent.
4. Fixtures: a miniature Envelope Index built in-test + sample `.emlx`
   files (plain, multipart, missing-body); V9-vs-V10 path probe test;
   unique temp dirs.
5. Opt-in acknowledgement (message bodies) on enable, per the
   privacy-sensitive rule.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Headers + snippets | ✅ built | 7 unit tests (synthetic Envelope Index) green; `cargo test -p trove-core apple_mail::` |
| from_me detection | ✅ built | Sender in Sent-folder mailbox → `from_me = true`; multi-account safe (set built from Sent folders) |
| Incremental cursor | ✅ built | Cursor survives re-import; second pass with same cursor returns 0 new messages |
| Deleted message exclusion | ✅ built | `deleted = 1` rows filtered in SQL |
| Cross-mechanism dedupe | ✅ by design | `guid = message_id_header` (RFC 5322 Message-ID) — same as gmail.rs / email.rs / imap.rs |
| Full bodies | ❌ v1 scope | Snippets only (`summaries` table); `.emlx` parsing deferred (raw bodies available but requires parsing emlx plist+RFC822 per message) |
| V9 fallback | 🔲 needs test | Path probe covers V9/V10; needs a Monterey machine to validate V9 path |

## Implementation notes (v1 built)

Schema confirmed against a live V10 Envelope Index (macOS 15, 48 174 messages):
- `date_sent` is plain Unix seconds (NOT Apple Core Data epoch — confirmed empirically)
- `message_global_data.message_id_header` contains the RFC 5322 Message-ID string
- `summaries.summary` holds the body snippet / cached preview text (truncated
  for the large majority of messages; full bodies require `.emlx`, deferred to
  v2; absent for cloud-only un-downloaded messages)
- `attachments.name` only (no MIME or size; full attachment metadata in `.emlx`)
- Sent-folder detection: mailbox URL contains `"Sent"` (URL-encoded as `%20`)

Vault path corrected from stub `correspondence/apple-mail/` to `correspondence/email/`
(shared email sink — shared with gmail.rs / email.rs / imap.rs for dedupe).

v1 scope: summary text only. Full `.emlx` body parsing and attachment byte counts
are a follow-up (would need RFC822 + plist trailer parse per message).

`integrations-research.md` → "Email & Messaging Apps" §Apple Mail
(L225-L231). Feasibility 🟢 high; schema stable across V9/V10.
Gotchas: copy before opening (WAL); guard against Mail.app holding the
lock. Sequence after the shared email parser exists (email.rs shipped;
gmail.rs shipped) so this module is mostly SQLite plumbing.

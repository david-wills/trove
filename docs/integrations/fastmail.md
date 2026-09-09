# Fastmail

- **id:** `fastmail`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (JMAP `Email/changes` incremental cursor)
- **connection:** `fastmail` — TokenPaste (API token from Settings →
  Privacy & Security → API tokens; no OAuth dance). The same token grants
  contacts + calendar — future Fastmail defs can share this connection.
- **evidence:** official-docs — JMAP RFC 8620 (core) + RFC 8621 (mail);
  session endpoint `api.fastmail.com/jmap/session`
- **effort / priority:** S / P1
- **needs:** privacy-sensitive (message bodies — opt-in with explicit
  acknowledgement) · Needs-login (validation needs a Fastmail account;
  build proceeds from the RFCs)

## What it is

A privacy-focused paid email provider, popular with exactly the audience
a local-first vault attracts. Its JMAP API is strictly better than IMAP
for sync — `Email/changes` is a standards-defined incremental cursor in
plain JSON — so Fastmail earns a dedicated provider; generic IMAP remains
its fallback.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full message history (backfill) | all plans | Email/query + Email/get → ts, chat, sender, text, to, subject, attachments meta | RFC 8620/8621 |
| Incremental new mail | all plans | Email/changes since stored state token | RFC 8621 |
| Mailbox names as labels | all plans | `labels[]` from mailboxIds → names | RFC 8621 |

All optional in the contract; no tier-specific code paths.

## Access & auth

- Session discovery: `GET https://api.fastmail.com/jmap/session` with
  `Authorization: Bearer <token>`; the session document gives the API URL
  and account id.
- Methods: `Email/query` (backfill paging), `Email/get` (bodies +
  headers), `Email/changes` (incremental). Pure JSON over HTTPS.
- No mature Rust JMAP crate as of the research pass — a thin `reqwest`
  wrapper is the plan; the protocol is plain JSON-RPC-ish batching.
- No TCC; standalone-clean.

## Vault mapping

- **Raw layer:** none separate — JMAP Email objects map directly to
  contract rows (keep unmapped JMAP fields in `extra`).
- **Contract layer:** `correspondence/email/YYYY-MM.jsonl` — the shared
  email sink per the ratified correspondence contract, `service` = the
  Fastmail address. `guid` = RFC Message-ID header (NOT the JMAP server
  id), keeping rows dedupe-safe against the same mailbox reached via
  IMAP, mbox import, or Apple Mail's local cache.
- **Cursor:** JMAP state token in sync state; a `cannotCalculateChanges`
  response triggers re-backfill (guids dedupe).

## Build plan

1. Module `crates/trove-core/src/fastmail.rs`: `DEF` (Periodic) +
   `CONNECTION` (TokenPaste: label/help/placeholder pointing at Settings →
   Privacy & Security → API tokens, per the SimpleFIN affordance rule).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Thin JMAP client over reqwest: session → query/get backfill →
   changes loop; map Email objects to `correspondence::Message`.
4. Fixtures from the RFCs' example responses (session doc, query page,
   get with body parts, changes with cannotCalculateChanges); parser +
   cursor + store tests, unique temp dirs.
5. Opt-in acknowledgement (message bodies) on enable, per the
   privacy-sensitive rule.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Backfill + labels | 🧪 built | Settings → Privacy & Security → New API token (scope: Mail read-only) in Fastmail; paste it in Trove's Fastmail connect card; Sync now; confirm rows in `correspondence/email/` + hub last-data |
| Incremental | 🧪 built | send self a mail, Sync now, exactly one new row + state-token advance |
| Cursor reset | 🧪 built (fixture) | covered by fixture test (cannotCalculateChanges → clean re-backfill, no duplicates) |
| Trash/Spam exclusion | 🧪 built (fixture) | confirm no message bodies from Trash or Spam mailboxes ever land in the vault |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Fastmail (JMAP)
(L233-L239). Feasibility 🟢 high; effort S because the shared email
parsing/store layer already exists (email.rs, gmail.rs) — this module is
mostly the JMAP client. Token auth simpler than OAuth. If the generic
IMAP collector ships first, Fastmail users have a working fallback day
one; this provider is the efficiency upgrade.

## Build notes (Phase-B fan-out, 2026-06-15)

- **Status:** 🧪 built and tested
- **Behavior:** `Behavior::Periodic` (JMAP `Email/changes` incremental cursor, 15 min cadence)
- **Connection:** new `"fastmail"` `ConnectionDef` (TokenPaste, API token); `&crate::fastmail::CONNECTION` added to CONNECTIONS in `integrations.rs`
- **Contract:** `reuse-bound`, domain `correspondence`, sink `correspondence/email/YYYY-MM.jsonl` via `Vault::append_messages` (shared with gmail.rs/imap.rs/email.rs)
- **Implementation decisions:**
  - JMAP `Email/get` with `fetchTextBodyValues=true` returns structured JSON fields — no raw RFC 822 download needed, avoiding an extra blobId fetch per message. `email_to_message` is NOT reused here (JMAP already has parsed headers); instead a `jmap_email_to_message` function builds the `Message` directly from JSON.
  - `guid` = first `messageId` header value (angle-bracket normalised), or `sha256:<jmap-id>` fallback — same dedupe key as all other email collectors.
  - `$sent` JMAP keyword → `from_me = true` (mirrors Gmail's `SENT` label logic).
  - `labels` = resolved mailbox names from `Mailbox/get` (cached per pass).
  - Cursor: JMAP state string in `.trove/fastmail-sync.json`. `cannotCalculateChanges` → clean re-backfill (guid dedupe absorbs overlap).
  - Backfill: `Email/query` sorted `receivedAt` ascending, paged via `position` cursor persisted after each page.
  - The generic IMAP collector (`imap.rs`) already covers Fastmail as a fallback (`fastmail.com`/`fastmail.fm` in WELL_KNOWN table) — this dedicated JMAP provider is the efficiency upgrade (no TLS socket/UIDVALIDITY management, server-native cursor).
- **Tests:** 15 passing (fixture-based, no live network); unique temp dirs
- **No new Cargo deps** — only `ureq`, `serde_json`, `sha2` (all pre-existing)

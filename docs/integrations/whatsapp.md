# WhatsApp

- **id:** `whatsapp`
- **domains:** `correspondence/` (contract: **correspondence — ratified**)
- **status:** 🧪 built (parser-parked: Needs-sample for non-English locale variants)
- **unavailable_reason:** none (the *messages* path works; the calls slice
  is honestly unreachable — stated in-app, see below)
- **behavior:** Import (official per-chat .txt export from iPhone/Mac)
- **connection:** none
- **evidence:** official per-chat export mechanism; the .txt format itself
  is **undocumented** (community-characterized, locale-variant) →
  sample-required; community ChatStorage.sqlite knowledge noted but out of
  scope (iOS-backup domain)
- **effort / priority:** M / P2
- **needs:** privacy-sensitive (message bodies — explicit opt-in
  acknowledgement at import time) · Needs-sample (locale-variant .txt
  timestamps/layouts — parser-last)

## What it is

The world's largest messenger. On macOS it is effectively a web wrapper:
**no complete local message DB exists on the Mac** — the full database
lives on the phone behind E2EE. The only clean path is the official
per-chat export (.txt, optionally .zip with media), which is lossy but
real. No personal API exists (the Business API is businesses-only).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Chat messages (per exported chat) | all users; one export per chat | date, author display name, text | official export, community-characterized format |
| Call events (as text lines in exports) | sometimes present in .txt | "call" lines, no duration structure | research notes (lossy) |
| Call history (proper) | **unreachable** | — | E2EE, on-phone only, no API |

Lossy by design: no sender IDs, no message IDs, no reactions — just
`Date, Author: Text` lines. The brief is honest about that in the UI copy.

## Access & auth

- Export: WhatsApp iPhone/Mac → chat → More → **Export Chat** → .txt (or
  .zip with media). One file per chat; user drops files/folder on Trove.
- Trove side: no auth, no network, no TCC. Standalone-clean.
- **Rejected paths:** Mac app cache (`~/Library/Application
  Support/WhatsApp/` — transient, incomplete); iPhone-backup
  ChatStorage.sqlite (rich, but that's the separate iOS-backup domain —
  revisit if/when Trove grows a backup reader); Business API (not
  personal).
- **Calls:** history lives on-phone behind E2EE with no export — the hub
  card states "WhatsApp call history can't be collected; WhatsApp keeps it
  on your phone with no export." Any call lines that do appear in a .txt
  import are stored as `kind:"call"` best-effort.

## Vault mapping

- **Raw layer:** none beyond contract rows (the .txt is the artifact and
  stays with the user); media from .zip exports is *not* copied — names
  recorded as attachment metadata.
- **Contract layer:** `correspondence/whatsapp/YYYY-MM.jsonl` per the
  ratified correspondence contract — `kind:"message"`, `chat` = exported
  chat name, `sender_name` (display names only — no handles exist in the
  export), `from_me` heuristic from the user-supplied own-name,
  `service:"WhatsApp"`.
- **Dedupe:** no native ids → `guid` = stable hash of
  (chat, ts, author, text). Weakness: identical consecutive texts within
  the same minute collide — acceptable, documented.

## Build plan

1. **Needs-sample / parser-last:** collect real exports in several locales
   (US, EU date orders, 12/24h, RTL) before freezing the line parser —
   the timestamp format follows device locale.
2. Module `crates/trove-core/src/whatsapp.rs`: `DEF` (Import), multi-file
   drop (each .txt = one chat; .zip accepted, media skipped), own-name
   prompt for `from_me`.
3. One registration line in `INTEGRATIONS`. No `CONNECTION`.
4. Fixtures: one per locale variant + a call-line variant + a multiline
   message (continuation lines); parser/dedupe tests, unique temp dirs.
5. Privacy acknowledgement before first import; hub copy carries the
   calls-unreachable statement.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| .txt chat import | ✅ built | export a real chat from iPhone WhatsApp; drop on Trove; confirm rows in `correspondence/whatsapp/`, re-import dedupes |
| iOS bracketed format `[DD/MM/YYYY, HH:MM:SS]` | ✅ unit-tested | fixture covers messages, calls, attachments (LRM-prefixed + Android form), multiline, system events |
| iOS space-separator `[3/6/18 1:55:00 PM]` | ✅ unit-tested | ios_space_sep_import test |
| LRM/RLM-prefixed lines | ✅ unit-tested | lrm_rlm_prefix_import test; zero-import bug fixed |
| Android 12h US `M/D/YY, H:MM AM` | ✅ unit-tested | fixture + AM/PM tests |
| Android EU 24h `DD.MM.YYYY, HH:MM` | ✅ unit-tested | fixture |
| Deduplication (content-hash guid, minute-normalised) | ✅ unit-tested | reimport_deduplicates + cross_format_dedup_minute_normalised |
| iOS _chat.txt zip chat-name | ✅ unit-tested | zip_import_ios_chat_txt (chat key from zip, not "_chat") |
| .zip with meaningful inner filename | ✅ unit-tested | zip_import_finds_txt test |
| Locale variants (non-English) | ⚠️ Needs-sample | non-English timestamp formats / call strings need real exports from non-EN-locale devices |
| Call lines (English) — system events only | ✅ built | exact whole-line match; authored messages with call phrases never misclassified; authored_messages_never_classified_as_call test |
| Attachment metadata (LRM-prefixed, Android form) | ✅ unit-tested | attachment_body_parsed + ios_bracketed_import |

## Build notes (2026-06-21)

- Module `crates/trove-core/src/whatsapp.rs`: `Behavior::Import`, accepts `.txt` and `.zip`.
- Correspondence contract reused (`correspondence/whatsapp/YYYY-MM.jsonl`), `kind:"message"/"call"/"event"`.
- Guid = SHA-256(chat + ts_minute + author + text) first 16 hex chars (no native IDs in export).
  `ts_minute` = timestamp truncated to minute precision (YYYY-MM-DDTHH:MM) so iOS (with seconds)
  and Android (without seconds) exports of the same message produce the same guid.
- Date-order disambiguation: if second date component > 12 → US M/D order; else D/M (EU).
- 12h AM/PM normalization: 12 AM → 0, 12 PM → 12, PM hours +12.
- Space-separator timestamps accepted: "[3/6/18 1:55:00 PM]" (no comma between date and time).
- Leading U+200E (LRM) and U+200F (RLM) stripped from lines and bodies before classification.
- iOS _chat.txt zip: chat key derived from outer .zip filename, not inner "_chat" stem.
- Attachment recognition: LRM-prefixed "<attached: …>", Android "(file attached)" / "<attached>" forms.
- Call classification: system lines only (author==None), exact whole-line match — authored messages
  containing "Video call" etc. are never misclassified.
- 27 unit tests, all green. No new deps (sha2 already present).
- `parser_parked_needs_sample=true` for non-English locale variants (call/media/system strings vary).

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §WhatsApp Mac
desktop (L337–L343) and "Calls, Voice & Meeting Transcripts" §WhatsApp
Calls (L626–L633). Feasibility 🟡 medium (messages), 🟠 low/blocked
(calls — "a hard blocker that no amount of engineering can overcome within
Trove's constraints"). Signal/noise is lower than other messengers —
hence P2. Track Meta API developments; revisit the iPhone-backup path if
an iOS-backup domain ever ships.

# ChatGPT

- **id:** `chatgpt`
- **domains:** `developer/` (raw-only — heterogeneous shapes, no contract;
  AI-session transcripts live here alongside Claude Code, Cursor, Copilot)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (official export ZIP; re-importable, dedupe makes
  re-runs safe)
- **connection:** none (one-shot file import, no login)
- **evidence:** official-docs — official data export, JSON format stable
  since 2023 (`conversations.json` structure documented in the research)
- **effort / priority:** S / P2
- **needs:** privacy (conversation content ≈ message bodies — opt-in with
  explicit acknowledgement; store truncated previews by default)

## What it is

OpenAI's chat assistant — for many users the single largest record of what
they were thinking about, asking, and working on. The official account
export delivers complete conversation history as JSON. Periodic manual
backfill rather than live collection, but the data is complete each time.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Conversation metadata | all consumer plans (not Business/Enterprise) | conversation id, title, create/update ts | official export docs |
| Messages | same | role (user/assistant/tool), content parts, ts, model | official export docs |

All optional in the per-source shape. Business/Enterprise accounts have no
export — the card's help copy should say so honestly.

## Access & auth

- Official export: ChatGPT Settings → Data Controls → Export → emailed ZIP
  containing `conversations.json` (full history) + `chat.html`. Takes **up
  to 7 days** to arrive; download link **expires in 24 hours** — import
  copy must warn about both.
- `conversations.json`: array of `{id, title, create_time, update_time,
  mapping}` where `mapping` is a node dict (`node_id → {message, parent,
  children}`); each message has `author.role`, `content.parts`,
  `create_time`. Walk the mapping tree to linearize messages.
- No API path for consumer conversation history; third-party browser
  extensions (ChatGPT Exporter etc.) export instantly but are not a
  build target — at most a help-copy mention.
- No TCC, no network, no login. Standalone-clean (user drops a file).

## Vault mapping

- **Raw layer:** `developer/chatgpt/YYYY-MM.jsonl`, partitioned by message
  ts — fields per the research: ts, conversation_id, title, role,
  content_preview (truncated at a configurable limit; full text opt-in,
  matching the Claude Code lean-default pattern), model. `developer/` is
  raw-only per the taxonomy (research's `developer/chatgpt/` path happens
  to agree).
- **Dedupe:** `guid` = conversation_id + message_id — re-importing a newer
  export only appends new messages.

## Build plan

1. Module `crates/trove-core/src/chatgpt.rs`: `DEF` with
   `Behavior::Import` (registry-driven import box — `letterboxd.rs` is the
   reference); accept the ZIP or a bare `conversations.json`.
2. One registration line in `INTEGRATIONS`. No connection.
3. Parser: walk each conversation's `mapping` tree parent→children to
   order messages; tolerate missing `create_time` on system nodes.
4. Fixtures: a small synthetic `conversations.json` exercising the mapping
   tree, tool-role messages, and a re-import (dedupe) case; unique temp
   dirs.
5. Privacy gate: opt-in enable with explicit acknowledgement; previews
   truncated by default, full-text toggle per the collection-depth
   convention.
6. Import help copy: 7-day arrival, 24-hour link expiry, no
   Business/Enterprise export.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Full-history import | ✅ unit-tested | request a real export, drop the ZIP on the import box; confirm rows in `developer/chatgpt/` + hub last-data |
| Re-import dedupe | ✅ unit-tested | import the same ZIP twice; row count unchanged; import a newer export; only new messages append |
| ZIP extraction | ✅ unit-tested | accepts ZIP directly; decoy files (chat.html) ignored |
| Content preview truncation | ✅ unit-tested | messages > 280 bytes are truncated at a UTF-8 boundary |
| System node skip | ✅ unit-tested | nodes with `create_time <= 0` skipped; conv-bbb counted as `conversations_skipped` |
| model_slug extraction | ✅ unit-tested | per-message metadata.model_slug promoted; conversation-level fallback fills rows where per-message slug absent (common in real exports); all turns in a conversation share the model slug |
| Tool role message | ✅ unit-tested | `role: "tool"` messages ingested; extra metadata preserved |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §ChatGPT
Conversation Export (L1444–L1450). Feasibility 🟢 high. Cross-cutting note
4 (L1476): AI-session sources share a normalizable metadata shape
(ts/source/project/model/message_count/summary) — a shared ingest helper
is worth extracting once 2–3 of these exist, but `developer/` stays
raw-only per the taxonomy. Natural complement to the Claude Code provider;
sequence nearby.

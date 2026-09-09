# Telegram

- **id:** `telegram`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Telegram Desktop export `result.json`) first; a later
  Periodic MTProto/Takeout pull is a separate spike
- **connection:** none for the export; the MTProto path adds a `telegram`
  connection (TokenPaste-style: user-registered `api_id`/`api_hash` from
  my.telegram.org plus phone + 2FA sign-in — Telegram ToS forbids shipping a
  compiled-in app id, so this is bring-your-own only)
- **evidence:** official export schema — core.telegram.org/import-export;
  official MTProto + Takeout API docs (`grammers` Rust crate exists but is
  unproven — spike before committing)
- **effort / priority:** M / P1 (export); MTProto upgrade is L
- **needs:** privacy-sensitive (message bodies — import is user-initiated;
  any live pull ships opt-in with explicit acknowledgement) · Needs-login
  (MTProto validation only — the export import proceeds from the documented
  schema)

## What it is

One of the largest messengers worldwide. Telegram Desktop ships an official,
full-history export with a stable documented JSON schema — unusually good for
a messenger — so years of personal chat history are recoverable with zero
auth complexity on Trove's side.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full chat history (export) | all accounts | text parts, sender, date, chat, forwarded-from, reactions, media refs | official export schema |
| Media files (export) | all accounts | files exported alongside JSON — metadata only enters the vault | official export schema |
| Live incremental pull (MTProto Takeout) | all accounts, BYO api_id | same, incremental via `offset_id` per dialog | official MTProto docs |

All optional in the contract. No tier gating — Telegram exports are free.

## Access & auth

- **Export (M1):** Telegram Desktop → Settings → Advanced → Export Telegram
  Data → JSON. Output: `result.json` with a top-level `chats` array, each
  chat carrying a `messages` array (id, date, from, text-parts, media info).
  Re-runnable; no Trove-side auth or TCC.
- **MTProto (later spike):** `messages.getHistory` per dialog or the safer
  Takeout API (core.telegram.org/api/takeout) via the `grammers` crate.
  Requires per-user `api_id`/`api_hash` (free registration) + phone/2FA
  sign-in. ToS prohibits mass automation; personal archiving of own messages
  is acceptable, but scraping-shaped traffic risks account bans — Takeout
  exists precisely to make bulk self-export safe.
- Standalone-clean both ways (file parse / plain MTProto over the network).

## Vault mapping

- **Raw layer:** `correspondence/telegram/raw/` only if export objects carry
  meaningfully more than the contract (text-part arrays flatten losslessly;
  likely not needed — decide at build time).
- **Contract layer:** `correspondence/telegram/YYYY-MM.jsonl` per the ratified
  correspondence contract — `guid` = chat id + message id, `chat` = chat id,
  `chat_name` = chat title, `text` = joined text parts, forwarded-from and
  media metadata in `extra`/`attachments`.
- **Dedupe:** message id within chat as `guid`; re-imports of overlapping
  exports skip stored guids. MTProto cursor (`offset_id` per dialog) in
  `.trove/telegram-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/telegram.rs`: `DEF` (Import), parser for
   `result.json` (handle both `result.json` and `results.json` naming, and
   the text-parts array-of-strings/objects union).
2. Registration line in `INTEGRATIONS`; generic import box does the UI.
3. Fixtures from the official schema examples (single chat, group chat,
   reactions, forwarded message); dedupe + re-import tests, unique temp dirs.
4. MTProto pull is a **separate later iteration**: spike `grammers` maturity
   and the Takeout flow first; setup copy must state the BYO `api_id`
   requirement plainly (ToS — never a compiled-in id).

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Export import | — | export a real account from Telegram Desktop; import; confirm rows in `correspondence/telegram/` + hub last-data; re-import dedupes |
| MTProto pull | — | register a personal api_id; sign in; pull a dialog incrementally; confirm no duplicate guids against a prior export import |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Telegram Desktop
export (L265-271, 🟢 high) + §Telegram MTProto (L273-279, 🟡 medium).
Bot API is a dead end (bots can't read user messages). Export first covers
most users; MTProto is the incremental-sync upgrade, gated on a grammers
spike and honest BYO-credentials UI.

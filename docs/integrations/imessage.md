# iMessage / SMS

- **id:** `imessage`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built (shipped pre-pipeline in `imessage.rs`; David promotes
  to ✅ after real-data validation)
- **unavailable_reason:** none
- **behavior:** Periodic (LocalSync — polls `chat.db` every 15 minutes; first
  sync backfills the full retained history)
- **connection:** none (local database read; no account)
- **evidence:** community-documented `chat.db` SQLite schema (stable,
  widely-reverse-engineered); shipped and fixture-tested
- **effort / priority:** S / P0
- **needs:** privacy-sensitive (message bodies — data is local-only; the Full
  Disk Access grant is the explicit user opt-in gate)

## What it is

Apple Messages — iMessage, SMS, and RCS (iOS 18+) — read straight from the
local `~/Library/Messages/chat.db`. For most Mac users this is the densest
personal-correspondence stream on the machine: group threads, reactions, and
tapbacks included, no export step and no cloud call.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Messages (iMessage/SMS/RCS) | none | text, sender handle, ts, chat/group, service, from_me | community schema; shipped |
| Reactions / tapbacks | none | `kind:"reaction"` rows with reaction type + reply_to | community schema; shipped |
| Group threads | none | chat id + display name, participants via handles | community schema; shipped |
| Attachments | none | metadata only (name, mime, size) | community schema; shipped |

All optional in the contract; no tier gating (local data).

## Access & auth

- SQLite at `~/Library/Messages/chat.db`; protected path — **Full Disk
  Access** required for both the app and the `troved` binary (grants apply to
  fresh processes only; restart the daemon after granting).
- Timestamps are Apple-epoch nanoseconds (2001-01-01 offset; legacy
  pre-High-Sierra whole-second rows handled).
- Copy-then-read against WAL; 15-minute poll cadence.
- No network at all. Standalone-clean.

## Vault mapping

- **Raw layer:** none — `chat.db` itself remains on disk as the source;
  contract rows carry full fidelity (`text` untrimmed).
- **Contract layer:** `correspondence/imessage/YYYY-MM.jsonl` per the ratified
  correspondence contract — `guid` = iMessage guid, `chat` = handle/group id,
  `service` = `"iMessage"`/`"SMS"`, `rowid` = source monotonic id. Senders
  are raw handles until a contacts source maps them to people (write
  handles, not guesses).
- **Dedupe:** `guid`; incremental cursor in `.trove/imessage-sync.json`,
  rebuildable from `rowid`.

## Build plan

Already shipped: module + registration + permission hook (FDA detection) +
last-data hook; hub card, toggle, and Recent-data are registry-driven.
Remaining pipeline work is none — this brief exists as the canonical record.
Any future schema drift (new macOS major) lands as a normal fix iteration.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Messages + groups | 🧪 shipped pre-pipeline | grant FDA on a real Mac; wait one poll (or Sync now); confirm rows in `correspondence/imessage/` + hub last-data |
| Reactions/tapbacks | 🧪 shipped pre-pipeline | react to a message; confirm a `kind:"reaction"` row referencing the target guid |
| RCS rows | — | needs a device on iOS 18+ messaging an Android contact; confirm `service` value |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §iMessage / SMS
(L281-287). Feasibility 🟢 high — already built. The FDA permission this
needs is the same grant Apple Mail's Envelope Index path will reuse (the
permission ladder argument for sequencing `apple-mail` nearby).

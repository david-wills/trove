# Skype (archival)

- **id:** `skype`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none (the *service* is dead; the import path is
  alive for users holding an export)
- **behavior:** Import (one-shot historical import of the official .tar
  export)
- **connection:** none
- **evidence:** community-schema — well-characterized `messages.json`
  export format; Skyperious open-source parser as reference (confidence
  high)
- **effort / priority:** S / P2
- **needs:** privacy-sensitive (message bodies — ships opt-in with
  explicit acknowledgement) · **time-sensitive:** export window closes
  June 2026 — after that, only already-downloaded archives can ever be
  imported

## What it is

Skype shut down in May 2025 (accounts migrated to Teams). For two decades
it was many people's primary calling/chat app, so the archives are dense
personal history. This is a purely archival importer for users who pulled
their data export before the June 2026 deadline — no new data will ever
accumulate.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Chat messages | export holder only | ts, sender, body, conversation | messages.json (Skyperious) |
| Call records | export holder only | call type, duration, participants | messages.json (Skyperious) |

Call records sit alongside chat messages in the same `messages.json` —
similar schema territory to the iMessage and Slack importers already
built. All fields optional in the contract.

## Access & auth

- Export was via secure.skype.com/en/data-export (deadline June 2026);
  format: `.tar` archive containing `messages.json`.
- The old local macOS DB (`~/Library/Application Support/Skype/<user>/main.db`)
  was superseded in Skype 8+ when history moved server-side — not a
  target.
- No auth, no TCC, no network: pure offline file import via the generic
  import box. Standalone-clean.

## Vault mapping

- **Raw layer:** `correspondence/skype/raw/` — the `messages.json` as
  shipped, full fidelity.
- **Contract layer:** `correspondence/skype/YYYY-MM.jsonl` per the
  ratified correspondence contract — `kind:"message"` for chats,
  `kind:"call"` with `duration_secs` for call records, `chat` =
  conversation id, `service` = `"Skype"`. Skype-specific fields (call
  type, edit history) in `extra`.
- **Dedupe:** Skype message id as `guid`; re-importing the same archive
  is a no-op.

## Build plan

1. Module `crates/trove-core/src/skype.rs`: `DEF` (Import); accept the
   `.tar` or an extracted folder containing `messages.json`.
2. One registration line in `INTEGRATIONS`.
3. Fixtures: trimmed `messages.json` covering a chat message, a call
   record (answered + missed), and a group conversation — shapes from the
   Skyperious reference parser.
4. Privacy: message bodies — opt-in acknowledgement at import time.
5. Treat like the Apple Health export.zip: a one-shot historical
   migration; hub copy should say plainly that Skype is defunct and this
   imports a saved export.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Chat messages | ✅ built | drop a real Skype export .tar; rows in `correspondence/skype/`; spot-check a known conversation |
| Call records | ✅ built | same import; `kind:"call"` rows with plausible durations; `duration_secs` > 0 for answered calls, 0 for missed |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Skype
Call History (L618-L625; cross-cutting note 9). Feasibility 🟠 low *as a
service* (it's dead) but the import itself is 🟢 simple. The
time-sensitivity is user-facing, not build-facing: anyone who hasn't
exported by June 2026 loses the data forever — worth a line of app copy
even before this ships. Build priority stays P2; nothing here blocks on
external systems.

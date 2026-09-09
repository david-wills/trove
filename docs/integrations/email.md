# Email Import (.mbox / .eml)

- **id:** `email`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built (shipped pre-pipeline; David promotes to ✅ after
  real-data review). One queued extension: accept folders of `.eml` files.
- **unavailable_reason:** none
- **behavior:** Import (drop a file; re-runnable, guid-deduped)
- **connection:** none
- **evidence:** official-docs — RFC 4155 (mbox); shipped implementation
  (`email.rs`, `mail-parser` crate)
- **effort / priority:** S / P0 (the `.eml` extension itself is S)
- **needs:** privacy-sensitive (message bodies — opt-in by nature: the
  user explicitly drops the file)

## What it is

The universal email backstop: any RFC 4155-ish `.mbox` export — Google
Takeout (one `.mbox` per label), Apple Mail File → Export Mailbox,
Thunderbird, ProtonMail's export tool — imports into the unified
correspondence stream. No credentials, no network: the right path for
bulk history, dead accounts, and users who never connect anything live.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| .mbox import | none | full bodies → ts, chat, sender, text, to, subject, attachments meta | RFC 4155; shipped |
| Sent-by-me detection | none | `from_me` via the account address the user supplies at import | shipped |
| .eml folder import (extension) | none | same fields from individual RFC822 files | research rec (L246) — queued |

All optional in the contract. The `.eml` extension is one parser-entry
change (the RFC822-block parser already exists) and is what absorbs
ProtonMail export-tool output.

## Access & auth

- User drops an `.mbox` file into the registry-driven import box; supplies
  the mailbox's own address (decides `from_me`). No auth, no TCC, no
  network — fully standalone.
- mboxrd `>From`-quoting handled; messages parsed with `mail-parser`.
- Extension: accept a folder (or zip) of `.eml` files as a second
  `accepts` variant on the same `ImportSpec`.

## Vault mapping

- **Raw layer:** none separate — the dropped file stays the user's; parsed
  messages go straight to contract rows at full fidelity.
- **Contract layer:** `correspondence/email/YYYY-MM.jsonl` — the shared
  email sink per the ratified correspondence contract, `source` `email`,
  `service` = the supplied account address. Message-ID header as `guid`:
  re-runs, overlapping exports, and the Gmail live pull never duplicate.
- **Dedupe:** skip already-stored guids on import (shipped behavior).

## Build plan

Shipped (`email.rs`, `Behavior::Import`, registered in `INTEGRATIONS`).
Remaining queued work:

1. Extend `ImportSpec.accepts` to take `.eml` files / folders of them;
   route each file through the existing single-message parser path.
2. Fixture set: a small `.eml` folder including a ProtonMail export-tool
   sample (`.eml` + `metadata.json` — metadata file tolerated/ignored in
   v1); re-run-dedupe test; unique temp dirs.
3. Update the def's setup copy to mention `.eml` and ProtonMail's tool.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| .mbox import | 🧪 built (pre-pipeline) | import a real Takeout .mbox; confirm rows + Recent-data view; re-import the same file → zero new rows; David promotes to ✅ |
| .eml folder extension | — | run ProtonMail's export tool, import the output folder, confirm rows land and re-runs dedupe |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Email .mbox import
(L241-L247). Feasibility 🟢 high — already built. The `.eml` extension is
explicitly recommended there and is load-bearing for the `protonmail`
provider (its official export tool emits `.eml`; Bridge was rejected for
requiring a running app). Covers any RFC 4155-compliant source, so new
email providers always have a day-one M1 path even before a live pull
exists.

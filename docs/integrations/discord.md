# Discord

- **id:** `discord`
- **domains:** `correspondence` (contract: ✅ ratified) ·
  `gaming` (raw-only per taxonomy — game-activity JSON from the same package)
- **status:** 🧪 built (fixture-tested; data-package + DCE import; needs a real export to validate)
- **unavailable_reason:** none
- **behavior:** Import — two accepted input shapes, one entry: the official
  data package ZIP, and DiscordChatExporter JSON output (user-run tool)
- **connection:** none (Trove never authenticates to Discord; both paths are
  user-supplied files)
- **evidence:** official data package — `messages/c<channel_id>/messages.json`
  + `channel.json` documented; community — DiscordChatExporter JSON schema
  (widely used, clean); GDPR `activities/games/` JSON (sparse)
- **effort / priority:** M / P1
- **needs:** privacy-sensitive (message bodies — imports are user-initiated)

## What it is

One of the highest-usage chat platforms (communities, gaming, increasingly
group DMs). No personal API exists; the sanctioned path is the official data
package, which has a famous gap — it contains **only messages you sent**, not
the rest of the thread. The community-standard DiscordChatExporter fills the
gap when the user chooses to run it; Trove only parses its output.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Sent messages (official package) | all accounts; 3–30 day wait | id, ts, contents, attachments meta, per-channel metadata | official package docs |
| Full thread context (DCE JSON) | user runs the tool with their own token | both sides: id, ts, author.name, content, attachments, reactions | community schema |
| Game activity (official package) | all accounts | game name, session timestamps (no playtime totals) | official package docs (sparse) |

All optional in the contract — a package-only import simply has `from_me`
rows; a DCE import fills in the other side. No special code paths.

## Access & auth

- **Official package:** Settings → Privacy & Safety → Request All My Data →
  ZIP (3–30 days, usually hours). `messages/` one folder per channel/DM;
  also `activities/`, `servers/`, `account/`. One-time snapshot, no
  incremental export. Attachments are URL references only.
- **DiscordChatExporter:** user runs github.com/Tyrrrz/DiscordChatExporter
  themselves and drops the JSON output folder. Self-botting with a user
  token violates Discord ToS — that is the **user's informed choice with
  their own account; Trove must never run or automate the tool**, only parse
  files. The import UI says exactly this.
- No Trove-side auth, no TCC, no network. Standalone-clean.

## Vault mapping

- **Raw layer:** `gaming/discord/` — game-activity JSON rows as-is (raw-only
  domain; sessions may join media-plays at read time, per taxonomy).
- **Contract layer:** `correspondence/discord/YYYY-MM.jsonl` per the ratified
  correspondence contract — `guid` = message id (stable across both input
  shapes, which is what makes the two-source merge safe), `chat` = channel
  id, `chat_name` from `channel.json`/DCE metadata, reactions as
  `kind:"reaction"`, attachments metadata only.
- **Dedupe:** message id as `guid` — a package import then a DCE import of
  the same channel must interleave without duplicates.

## Build plan

1. Module `crates/trove-core/src/discord.rs`: `DEF` (Import); one importer
   that sniffs the input shape (official package folder layout vs DCE
   `channel.json` + `messages.json`).
2. Registration line in `INTEGRATIONS`; generic import box.
3. Fixtures: official-package channel folder, DCE export of the same
   messages (exercise the guid merge), `activities/games/` sample; dedupe +
   cross-shape merge tests, unique temp dirs.
4. Route game-activity rows to `gaming/discord/` in the same import pass —
   records route by shape, one provider entry.
5. UI copy: state the sent-messages-only limitation of the official package
   and the 3–30 day wait; present DCE as the user-run complement with the
   ToS note.

## Build status — 🧪 2026-06-14

Shipped (`discord.rs`, INDEX #14 — a `Behavior::Import` collector; no auth/API).
correspondence/ bound + gaming/ raw-only → no binding. Two input shapes, one
importer (slack.rs/letterboxd.rs precedent):
- **Official data package ZIP** (Settings → Privacy & Safety → Request All My
  Data): `messages/c<id>/{channel.json, messages.json|messages.csv}` = the user's
  SENT messages + `activities/games/*.json`. Sent-only, one-time snapshot.
- **DiscordChatExporter (DCE) JSON** (the user runs the tool with their own token
  — **Trove never runs/automates it, only parses the dropped files**; the ToS
  note is in the import copy): both sides of a channel.
Sniffs `.json` → DCE; `.zip` with `messages/`//`activities/` → package, else a
zipped DCE.

Messages → `correspondence/discord/YYYY-MM.jsonl` (source="discord", **guid = the
Discord message id** — stable across both shapes, so a package + a DCE import of
the same channel dedupe-merge safely; chat=channel id, chat_name, attachments
**metadata only — never fetched**, partition by local `ts` month). Package →
`from_me=true`/`sender=""`; DCE → `sender=author.name`, `from_me` only when the
author matches the supplied `me` (honest received otherwise). DCE reactions →
`kind:"reaction"` rows. Dedup by guid (`correspondence_guids` + a seen-set)
across both shapes + re-imports. Game activity → `gaming/discord/` raw rows (a
game-ish filter drops Discord's analytics-dump noise; never leaks into
correspondence).

Evidence: the official-package + DCE shapes were verified against the
`purarue/discord_data` parser + the Tyrrrz DiscordChatExporter docs.

Adversarial-verify: **NO DEFECTS FOUND** (format mapping, cross-shape guid merge
in both orders, re-import dedupe, from_me, attachments-no-network, game routing +
noise filter, panic-safety all confirmed). Two documented non-defects: a
lowercase CSV header yields 0 rows (real Discord headers are capitalized); a
DCE-first import without `me` marks the user's own messages received (the `me`
param is the intended attribution mechanism; the official package always carries
`from_me=true`).

Gate: trove-core 564/0 (+7 discord tests), `cargo check` clean, `schedule_doc`
regenerated (discord Import), `bindings.ts` up to date. 🔒 user-initiated import;
**live validation needs a real export** (the package request takes 3–30 days).

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Official package import | 🧪 (needs a real export) | request + import a real package; confirm `from_me` rows in `correspondence/discord/` + hub last-data; re-import dedupes |
| DCE JSON import | 🧪 (needs DCE output) | run DCE on one channel; import; confirm received-side rows merge with package rows, no duplicate guids (pass your handle as `me` to attribute your own messages) |
| Game activity | 🧪 (needs a real export) | confirm rows in `gaming/discord/` from the package's `activities/games/` |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Discord data package
(L289-295) + §DiscordChatExporter import (L417-423); "Media: Books, Reading
& Gaming" §Discord Game Activity (L3615-3621); "Social Media & Web Presence"
§Discord (L4056-4062). Feasibility 🟢 high on both message paths; game
activity 🟡 (sparse, Discord prunes old activity — part of why the catalog
flags this provider time-sensitive, along with the no-incremental-export
snapshot model: import early, re-request periodically). Mojibake caveat from
Meta exports does **not** apply here; Discord JSON is clean UTF-8.

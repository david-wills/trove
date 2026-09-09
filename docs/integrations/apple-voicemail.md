# Visual Voicemail (iPhone backup)

- **id:** `apple-voicemail`
- **domains:** `voice` (contract: **not yet ratified** — Phase 3 decides
  whether Voice Memos / Visual Voicemail / Google Voice voicemails converge
  on one shape or stay raw-only)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (troved slow tick; detect a local iPhone backup and
  extract new voicemail rows)
- **connection:** none (local files; FDA-gated path)
- **evidence:** community-schema, medium confidence — backup layout +
  `voicemail.db` schema community-documented (iMazing / iPhone Backup
  Extractor lineage); **sample-required** for the `.transcript` binary
  plists and exact column set
- **effort / priority:** M / P1
- **needs:** privacy-sensitive (voicemail transcripts are on the mandatory
  opt-in list — explicit acknowledgement required) · Needs-sample
  (unencrypted local backup with voicemails; transcript plist fixtures)

## What it is

iPhone Visual Voicemail, surfaced via the local Finder/iTunes backup on the
Mac. Voicemails are otherwise trapped on the phone; for users who keep
local backups this recovers sender, time, duration, audio reference, and
Apple's machine transcript for every stored voicemail. Only works when the
user makes **unencrypted local backups** — iCloud-backup users get an honest
in-app explanation, not a broken card.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Voicemail metadata | unencrypted local backup exists | sender, date, duration, flags | community-schema (`voicemail.db`) |
| Apple transcript | sparse — where iOS transcribed | text + per-word confidence | community-schema (`.transcript` binary plist) — Needs-sample |
| Audio reference | same | `.amr` file path within the backup | community-schema |

All optional; rows without transcripts are still useful call-record
context. Whisper re-transcription of `.amr` audio is a possible later
opt-in enrichment (needs an AMR decode — ffmpeg-next — so explicitly out of
the first iteration).

## Access & auth

- Backup path: `~/Library/Application Support/MobileSync/Backup/
  <device-UUID>/`; `HomeDomain/Library/Voicemail/voicemail.db` (SQLite,
  hashed-filename lookup via the backup Manifest), `.amr` audio + binary
  plist `.transcript` files keyed by the same row id.
- Permission: Full Disk Access (MobileSync path) — already held by troved;
  reuse the existing FDA gate.
- Hard limits, stated honestly in-app: iCloud backups are unreadable
  (encrypted, remote); encrypted local backups need the user's backup
  password (out of scope first pass — detect and explain). iOS 17 "Live
  Voicemail" real-time transcripts are not persisted readably — never
  promise them.
- Standalone-clean: zero network. No iTunes/Finder automation — read-only
  scan of backups the user already makes.

## Vault mapping

- **Raw layer:** `voice/apple-voicemail/YYYY-MM.jsonl` — one row per
  voicemail: `ts`, `guid` (device-UUID + voicemail rowid), `from`
  (E.164 where parseable), `duration_secs`, `transcript`,
  `transcript_confidence`, `audio` (path within the backup; no audio copies
  into the vault). *(Taxonomy table wins over any path in the research
  entry: `voice/` domain, per-source folder.)*
- **Contract layer:** pending the Phase 3 voice decision (see
  apple-voice-memos.md — same fork). Voicemail rows are shaped like the
  Google Voice voicemail slice, so convergence is plausible.
- **Dedupe:** rowid-scoped `guid` per device; watermark per backup in
  `.trove/apple-voicemail-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/apple_voicemail.rs` (def id
   `apple-voicemail`): `DEF` (Periodic), permission hook = FDA + backup
   detection (distinguish "no backup", "encrypted backup", "backup ok" —
   each gets its own hint copy per the disabled-controls-need-affordance
   rule).
2. Registration line in `INTEGRATIONS`.
3. **Parser-last / Needs-sample:** the `.transcript` binary-plist shape is
   community-described but unfixtured — obtain a real sample (David's or a
   tester's local backup) before writing the transcript parser; ship the
   `voicemail.db` metadata slice first.
4. Fixtures: synthetic `voicemail.db` + Manifest stub + (once sampled) real
   `.transcript` plists; tests for missing/encrypted backup detection;
   unique temp dirs.
5. Opt-in gate: default-off, explicit acknowledgement copy naming voicemail
   transcripts (mandatory privacy flag).
6. AMR transcoding / Whisper pass: deferred, separate opt-in iteration.

## Build notes (fan-out, 2026-06-16)

- **Contract:** reuse-bound `voice::Recording` (`kind:"voicemail"`); no new struct/domain.
- **Behavior:** Periodic (hourly). FDA-gated; no connection needed.
- **Transcript parser parked:** binary plist shape needs a real sample. Metadata
  (sender, date, duration, flags) writes unconditionally; `transcript` stays empty
  until a sample arrives. Set `parser_parked_needs_sample=true`.
- **Schema:** community-documented voicemail.db (iMazing lineage). PRAGMA
  table_info probe before SELECT — columns absent on older iOS are safely skipped.
- **voicemail.db dates** are Unix epoch (NOT Core Data 2001 epoch, unlike calls.db).
- **Encrypted backups** detected from Manifest.plist IsEncrypted byte; graceful skip
  with in-app explanation rather than silent failure.
- **Multi-device:** per-backup-UUID ROWID cursor in `.trove/apple-voicemail-sync.json`.
- **Needs-sample flag** stands for: confirm exact column set + implement transcript
  binary plist parser once a real unencrypted backup with transcribed voicemails
  is available.
- **9/9 tests green**, `cargo check` clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Backup detection states | ✅ unit-tested | Macs with no backup / encrypted backup / unencrypted backup each show the right card state and hint |
| Metadata pull | 🧪 needs device | unencrypted local backup of an iPhone with voicemails; Sync now; confirm rows in `voice/apple-voicemail/` + hub last-data |
| Transcripts | ⏸ parked | same backup where iOS transcribed at least one voicemail; parser parked until a real sample is available |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Visual
Voicemail (L538-545; at-a-glance L475; cross-cutting note 5). Feasibility
🟡 medium — entirely conditional on local-backup habits; many users are
iCloud-only, hence the emphasis on honest empty-state copy. Complements the
built `calls` integration (CallHistoryDB), which has call rows but no
voicemail content. Not time-sensitive while the user keeps making backups,
but each backup refresh can age out old voicemails the phone deleted —
encourage an early first pull.

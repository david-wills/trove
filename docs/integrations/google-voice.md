# Google Voice

- **id:** `google-voice`
- **domains:** `correspondence` (calls + SMS; contract: ✅ ratified) ·
  `voice` (voicemails; contract: **Phase 3 pending** — drafted with Voice
  Memos + Apple Voicemail if shapes converge, else raw-only)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Takeout archive drop; no API exists for Voice)
- **connection:** none (the user logs into takeout.google.com themselves;
  the existing `google` OAuth connection does **not** cover Voice — no
  API surface to use it on)
- **evidence:** community-schema — stable, well-characterized Takeout HTML
  format; community parsers (voice2json, NeighborGeek gist) as reference
  (confidence high)
- **effort / priority:** S / P1
- **needs:** privacy-sensitive (message bodies + voicemail transcripts —
  ships opt-in with explicit acknowledgement)

## What it is

Google's virtual phone number service: calls, SMS, and transcribed
voicemails. For Voice users it is their *primary* call/text record — none
of it lands in Apple's CallHistoryDB or Messages. Takeout is the only
path; it reliably includes voicemail transcripts and the .mp3 audio.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Call log | free | date/time, duration, direction, number | Takeout HTML |
| SMS/MMS threads | free | ts, sender, body, attachments | Takeout HTML |
| Voicemails | free | transcript (embedded in HTML) + .mp3 audio | Takeout HTML + mp3 |

Google Voice does not transcribe or record *calls* — only voicemails; the
brief makes no promise the service doesn't keep. All fields optional in
the contract.

## Access & auth

- takeout.google.com → select "Voice" → download archive. Format: HTML
  file per conversation (calls, voicemails, texts) plus `.mp3` per
  voicemail. No official API for Voice data — Takeout-only.
- No macOS TCC; pure file import via the registry's generic import box.
  Standalone-clean (the parse is fully offline).

## Vault mapping

Taxonomy rule applied: the research entry's
`correspondence/calls/google-voice/` path predates the taxonomy and is
not used. Records route whole, distinct record types split by shape:

- **Raw layer:** `correspondence/google-voice/raw/` — copied source HTML
  per conversation; voicemail audio under `voice/google-voice/audio/`.
- **Contract layer (calls + SMS):**
  `correspondence/google-voice/YYYY-MM.jsonl` per the ratified
  correspondence contract — `kind:"call"` with `duration_secs` (0 =
  missed) for call rows, `kind:"message"` for SMS; `chat` = counterpart
  number, `service` = `"Google Voice"`.
- **Voicemails:** `voice/google-voice/YYYY-MM.jsonl` (transcript text,
  duration, caller, relative path to the .mp3) — Phase 3 voice contract
  pending; written raw-shaped until ratified.
- **Dedupe:** `guid` = stable hash of (counterpart, ts, kind) — the HTML
  carries no native ids; re-importing a newer Takeout must skip
  already-stored rows.

## Build plan

1. ✅ Module `crates/trove-core/src/google_voice.rs`: `DEF` (Import), HTML
   parser for the three conversation page shapes (call, text thread,
   voicemail).
2. ✅ Registered as stub stub already existed; behavior replaced to `Behavior::Import`.
3. ✅ Fixtures: inline HTML fixtures for each shape in the test module; dedupe
   test for re-import of an overlapping archive (11 tests, all green).
4. Privacy: voicemail transcripts + message bodies — import is default_on: false
   (opt-in); full fidelity at write time.
5. Optional later slice: re-transcribe voicemail .mp3s with the bundled
   Whisper model for higher accuracy (offline, fits standalone rule).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Call log | ✅ built | drop a real Takeout Voice archive; rows with `kind:"call"` in `correspondence/google-voice/`; hub last-data updates |
| SMS threads | ✅ built | same import; message rows match the HTML thread; direction from_me detected |
| Voicemails | ✅ built | transcript rows in `voice/google-voice/` + audio_ref pointing at relative .mp3 path |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Google
Voice (Takeout export) (L562-L569; cross-cutting note 8). Feasibility 🟢
high for what exists. A rare case where the manual-export path is both the
only option *and* low friction — worth building promptly. No
re-import-free incremental story: users re-run Takeout occasionally and
the importer dedupes.

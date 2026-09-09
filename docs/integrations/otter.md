# Otter.ai

- **id:** `otter`
- **domains:** `meetings` (contract: **not yet ratified** — Phase 3 drafts it
  from Granola + Fathom + Zoom + Fireflies together; Otter conforms once
  ratified)
- **status:** 🧪 built
- **unavailable_reason:** none (the *API* path is unavailable to individuals;
  the export-import path below is what's queued)
- **behavior:** Import (manual TXT/DOCX/SRT exports from the Otter dashboard
  — no self-serve API exists for individuals)
- **connection:** none (import needs no login; the Enterprise Connect API is
  iceboxed, not specced)
- **evidence:** official-docs — Otter Connect API v2 is Enterprise-only
  (account-manager enablement); export formats TXT (Basic) / DOCX/PDF/SRT
  (paid plans) per official docs. SRT is a standard subtitle format — no
  sample needed for it; TXT/DOCX layout is Otter-specific → sample-required
  for those variants.
- **effort / priority:** M / P2
- **needs:** Privacy-sensitive (opt-in) · Needs-sample (TXT/DOCX layout) ·
  Needs-David (contract: meetings)

## What it is

The most widely used meeting notetaker among individuals and academics —
which makes the manual-export path worth shipping even though Otter offers
no self-serve API: the Connect API v2 is Enterprise-only, enabled by
contacting Otter's sales team (500 req/min once granted). Trove cannot
automate pulls for a typical Otter user; honest copy on the card says
"manual export only — Otter gates its API to Enterprise."

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| SRT export | paid plans | timestamped, speaker-attributed transcript | official export docs (richest format) |
| TXT export | all plans incl. Basic | plain transcript text | official export docs |
| DOCX/PDF export | paid plans | formatted transcript | official export docs (PDF not parsed) |
| Connect API v2 | Enterprise + account manager | full programmatic access | official docs — **iceboxed** |

All optional in the contract; a Basic-tier TXT import simply carries no
timestamps. No tier-specific code paths.

## Access & auth

- **Import (v1):** user exports from the Otter dashboard and drops files in
  the registry-driven import box. Prefer SRT (timestamps + speakers); accept
  TXT/DOCX. No auth, no TCC, no networking — standalone-clean by
  construction.
- **Enterprise API (iceboxed):** no OAuth self-service, sales-gated — out of
  scope for a general-user app. Revisit only if Otter ships a self-serve
  key.

## Vault mapping

- **Raw layer:** `meetings/otter/raw/` — imported export files verbatim.
- **Contract layer:** `meetings/otter/YYYY-MM.jsonl` per the (pending)
  meetings contract — one row per imported meeting; `ts` from SRT timestamps
  or file metadata; `guid` = content hash (exports carry no stable id);
  transcript as sidecar document; speaker/timestamp detail in the sidecar,
  overflow in `extra`.
- **Dedupe:** content-hash `guid` — re-importing the same export is a no-op.

## Build plan

1. Module `crates/trove-core/src/otter.rs`: `DEF` (Import), one registration
   line in `INTEGRATIONS`. No connection.
2. SRT parser first — standard WebSRT-style format, buildable from spec with
   synthetic fixtures.
3. **Parser-last for TXT/DOCX (Needs-sample):** Otter's layout in those
   formats is undocumented — acquire real exports before writing those
   parsers; ship SRT-only if samples lag.
4. Fixtures: synthetic SRT + acquired TXT/DOCX samples; parser + store +
   hash-dedupe tests, unique temp dirs.
5. Privacy: transcripts are message-body-class — default-off, explicit
   opt-in copy.
6. Parked behind Needs-David (meetings contract) like the rest of the
   domain.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| SRT import | ✅ tested (synthetic) | export SRT from a paid Otter account; import; confirm row + timestamped sidecar in `meetings/otter/` |
| TXT import | 🅿 Needs-sample | export TXT from a Basic account; import; confirm row stored + raw verbatim copy |
| Re-import dedupe | ✅ tested | import the same file twice; confirm no duplicate rows |
| Speaker label extraction | ✅ tested | SRT with `Speaker: text` prefix; confirm sidecar row has `speaker` key |

## Build notes (2026-06-17)

- **Behavior:** Import (SRT + TXT; DOCX/PDF parked — Needs-sample)
- **Contract:** `meetings/otter/YYYY-MM.jsonl` via `meetings::Meeting` (reused bound contract; pioneer = `fathom.rs`)
- **Raw layer:** `meetings/otter/raw/<guid>.srt` (verbatim) + `meetings/otter/raw/<guid>-transcript.jsonl` (utterance sidecar)
- **guid:** `sha256("<filename>|<content>")[..16]` — content-hash dedup; re-importing same file is no-op
- **ts:** first SRT timecode anchored to file mtime date (no absolute date in SRT); TXT falls back to import date
- **TXT parser:** raw stored verbatim; `needs_sample: true` in `extra`; parser parked until a real Otter TXT sample is acquired
- **SRT speaker labels:** optional `Speaker: text` prefix detection; blocks without prefix store no `speaker` key
- **Tests:** 14 unit tests; `cargo test -p trove-core otter::` green; `cargo check` clean

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Otter.ai
(L602–L609). Feasibility 🟠 low *for the API*, but the M1 import is
low-friction and serves the largest notetaker user base. Research
recommendation matches this brief: icebox the Enterprise API, build the
export import (SRT richest). Zapier automation exists on Pro+ but only
captures new meetings and adds a cloud dependency — not a Trove path.

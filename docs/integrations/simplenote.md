# Simplenote

- **id:** `simplenote`
- **domains:** `notes/` (contract: **Phase 3 pending** — `notes/` shapes
  drafted across Apple Notes, Bear, Drafts, Day One, Obsidian, Logseq and the
  other note sources; `artifacts/` stays the user-curated layer)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (export ZIP containing `simplenote.json`)
- **connection:** none — the read path is a user-initiated export ZIP. No
  public API exists (Automattic acknowledged a feature request, no timeline).
- **evidence:** community-schema — `simplenote.json` is a clean JSON array of
  note objects (content, tags, creationDate, lastModified, id), well-documented
  in the community; the per-note `.txt` files carry the same content.
- **effort / priority:** S / P2
- **needs:** none

## What it is

Simplenote is Automattic's free, minimalist plain-text note app — fast sync,
tags, no formatting overhead. The data is the user's notes plus tags and
timestamps. Automattic-owned (the WordPress company), so it's likely stable.
A trivial, low-risk import.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Notes | all (export) | content (plain text/markdown), id | community |
| Tags | all (export) | tags[] | community |
| Timestamps | all (export) | creationDate, lastModified | community |
| Deleted notes | all (export) | included in `trashedNotes` array | community |

All optional in the contract (omit-if-empty). The export routes deleted notes
into the top-level `trashedNotes` array (no per-note `deleted` field); they
are imported with `trashed: true` in the contract layer.

## Access & auth

- Export: File → Export Notes (desktop/web app) → ZIP containing
  `simplenote.json` (all notes + metadata) plus one `.txt` file per note (same
  content).
- No public API as of 2025 — the export is the only path.
- No TCC beyond reading the user-chosen ZIP. Standalone-clean.

## Vault mapping

- **Raw layer:** `notes/simplenote/raw/…` — the exported `simplenote.json` (and
  optionally the `.txt` files), full fidelity.
- **Contract layer:** `notes/simplenote/…` per the pending Phase-3 `notes/`
  contract — expected one row per note (`ts` from creationDate, `source`,
  `guid` = note id, `body` = content, `tags[]`, lastModified in `extra`).
  Parked behind the contract until it ratifies.
- **Dedupe:** `guid` = note id; re-import is idempotent (replace on matching
  id). Trashed notes come from the `trashedNotes` array (no per-note `deleted`
  field); they are stored with `trashed: true` and excluded from the active
  notes view by the reader.

## Build plan

1. Module `crates/trove-core/src/simplenote.rs`: `DEF` (Import), import-box
   pull hook accepting the export ZIP.
2. One registration line in `INTEGRATIONS`. No `CONNECTION` (import only).
3. Parse `simplenote.json` (array of note objects); filter/flag `deleted:true`
   rows; map to the notes contract.
4. Fixtures from a small `simplenote.json` (including a deleted-note row);
   parser + store tests, unique temp dirs.
5. Vault writes via `store` helpers once the `notes/` contract is ratified.

## Build notes (Phase 4)

- **Contract:** reuses `notes::Note` (bound by bear.rs). No new struct/DOMAINS/schema added.
- **Behavior:** `Import` — accepts ZIP or bare `simplenote.json`.
- **Wire format confirmed** from `Automattic/simplenote-electron` source (`lib/utils/export/types.ts` + `export-notes.ts`): top-level `{ activeNotes, trashedNotes }`, each note has `id`, `content`, `creationDate`, `lastModified`, optional `tags[]`, `pinned`, `markdown`, `publicURL`, `collaboratorEmails`.
- **Trashed notes** from the `trashedNotes` array are imported with `trashed: Some(true)` (preserved for fidelity, excluded from the active view by the reader).
- **Title:** derived from the first non-empty content line (strip leading `#`), capped at 120 chars — Simplenote has no explicit title field.
- **Raw layer:** full fidelity in `notes/simplenote/raw/YYYY-MM.jsonl`; contract layer in `notes/simplenote/YYYY-MM.jsonl`; both partitioned by `creationDate` month (immutable), upserted by `id`.
- **Tests:** 11 unit tests — active+trashed import, raw fidelity, idempotent re-import, update replacement, missing-date skip, empty export, ZIP extraction, title extraction, serde back-compat, extra fields (publicURL/collaborators), DEF assertions.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Notes import | ✅ built | File → Export Notes from Simplenote; drop the ZIP in the import box; confirm rows in `notes/simplenote/` + hub last-data |
| Deleted-note handling | ✅ built | `trashedNotes` rows land with `trashed:true`; active notes have no trashed flag |
| Re-import idempotent | ✅ built | Import same export twice; confirm row count unchanged (upsert by id) |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Simplenote (L2883–L2889). Feasibility 🟢 high — trivial JSON parser, no API to
maintain. Automattic-owned, likely stable. The one gotcha: the export routes
deleted notes into a top-level `trashedNotes` array (no per-note `deleted`
field) — they are ingested with `trashed: true` and filtered at read time.

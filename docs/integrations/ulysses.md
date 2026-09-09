# Ulysses

- **id:** `ulysses`
- **domains:** `notes/` (contract: **Phase 3 pending** — Apple Notes,
  Bear, Drafts, Day One, Obsidian, Logseq converge here)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Markdown export drop / External-Folders watch)
- **connection:** none (local files; no login)
- **evidence:** community-schema — iCloud library path is readable with Full
  Disk Access but the on-disk `.ulyz`/`.ulgroup` format is proprietary
  XML; community parsers (export-ulysses, ulysses-tools) exist · sample-required
  for the XML path
- **effort / priority:** S / P2
- **needs:** Needs-sample (proprietary `.ulyz` XML — parser parked, must be
  validated against a real Library export before shipping)

## What it is

Ulysses is a subscription Markdown writing app for Mac/iOS popular with
long-form writers, bloggers, and students. Sheets are organized into groups
in a single library. The data matters because it holds the user's drafts and
written notes — first-class personal artifacts that otherwise never leave the
app. Niche relative to Bear/Drafts, so it sequences after them.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Sheet text | all plans (Markdown export) | title, body markdown, group path, modified date | official export menu |
| Library structure | all plans | group hierarchy, sheet order | community parsers |
| Live library read | requires FDA + XML parse | sheet bodies from `.ulyz` files | community-schema (sample-required) |

All optional in the contract (omit-if-empty); tiering needs no special code.

## Access & auth

- **Export path (preferred):** File > Export → Markdown / TextBundle. User
  drops the export into the import box. No auth, no TCC.
- **External Folders mode:** if the user enables it, Ulysses writes plain
  `.md` to a chosen folder — effectively an Obsidian-style watchable tree;
  Trove can watch it directly with no parsing. Advise this for the cleanest
  integration.
- **Live iCloud library:** `~/Library/Mobile Documents/X5AZV975AG~com~soulmen~ulysses3/Documents/Library/`
  (or the container path without iCloud). Requires **Full Disk Access**;
  format is proprietary XML (`.ulyz`, `.ulgroup`). No public API.

## Vault mapping

- **Raw layer:** `notes/ulysses/raw/…` — exported markdown (or watched
  External-Folder `.md`) stored full-fidelity, partitioned by import batch /
  month.
- **Contract layer:** `notes/ulysses/YYYY-MM.jsonl` per the (pending) notes
  contract — expected shape: one row per sheet (`ts` = modified date,
  `source`, `guid`, `title`, `body`, group path), overflow in `extra`.
  Until the notes contract is ratified, parked behind Needs-David.

## Build plan

1. Module `crates/trove-core/src/ulysses.rs`: `DEF` (Import). Two ingest
   paths share the parser: (a) dropped Markdown/TextBundle export, (b)
   watched External-Folders plain `.md`.
2. Registration line in `INTEGRATIONS`. No `CONNECTION` (local).
3. **Parser-last for the XML path:** the `.ulyz`/`.ulgroup` reader is built
   only after a real Library sample is in hand (Needs-sample); ship the
   Markdown/External-Folders path first, which needs no proprietary parsing.
4. Fixtures: a small exported `.md`/TextBundle bundle; parser + store tests,
   unique temp dirs. Add an XML fixture once a sample lands.
5. Vault writes via `store` helpers once the notes contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Markdown export (ZIP) | ✅ built | export sheets to Markdown, zip the folder, drop in the import box; confirm rows in `notes/ulysses/YYYY-MM.jsonl` + hub last-data |
| TextBundle export (ZIP) | ✅ built | export as TextBundle, zip, drop in import box; confirm `format:textbundle` in extra |
| External Folders | — | enable External Folders in Ulysses; zip the folder; re-import the zip; confirm watched `.md` ingest |
| Live XML library | — | requires FDA + a real Library sample to build/validate the `.ulyz` parser (Needs-sample) |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Ulysses (L2923–L2929). Feasibility 🟡 medium. Recommendation: build the
Markdown-export path first; the XML/live-sync path is worthwhile only if a
user demands live sync and is deprioritized vs. Bear/Drafts given Ulysses'
subscription model and niche base. External Folders is the cleanest route —
advise users toward it.

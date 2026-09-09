# Apple Notes

- **id:** `apple-notes`
- **domains:** `notes/` (contract: **Phase 3 pending** — collected notes shape;
  `artifacts/` stays the user-curated layer)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (local SQLite read; treat as low-frequency
  re-scan/one-time migration rather than tight polling)
- **connection:** none — reads a local file under Full Disk Access; no login.
- **evidence:** `NoteStore.sqlite` schema known; gzipped-protobuf bodies;
  `apple_cloud_notes_parser` (Ruby) + `apple-notes-parser` (Python) maintained
  for macOS 15/16
- **effort / priority:** M / P1
- **needs:** Needs-sample (the reverse-engineered `.proto` must be confirmed
  against a real `NoteStore.sqlite` before body decoding is trusted)

## What it is

Apple's built-in Notes app — where most Mac/iPhone users keep freeform notes,
checklists, and clippings. The body content is otherwise never captured
locally in a portable form. High-value for anyone in the Apple ecosystem.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Note metadata | all | title, folder, created/modified dates | `ZICCLOUDSYNCINGOBJECT` table (readable plainly) |
| Note bodies | all | decoded note text/structure | `ZICNOTEDATA.ZDATA` gzip+protobuf (community parsers) |
| Attachments | all | title, dates (not image copies) | attachment table metadata |

All optional in the contract (omit-if-empty). Locked notes simply carry no
body. Metadata alone is useful even when body parsing is skipped.

## Access & auth

- Local file: `~/Library/Group Containers/group.com.apple.notes/NoteStore.sqlite`.
- Body in `ZICNOTEDATA.ZDATA`: gzip-compressed protobuf using Apple's own
  (reverse-engineered, unpublished) schema; attachments under `Media/<UUID>/`.
- **TCC:** Full Disk Access required (already held by Trove for other local
  reads).
- **Standalone-clean:** reads the on-disk SQLite directly; no external app or
  service — the community parsers are references, not runtime deps. Decoding
  is done in-binary via a Rust protobuf lib (`prost`) consuming the same
  `.proto`.
- **Skip locked/encrypted rows:** where `ZISPASSWORDPROTECTED=1` or
  `ZENCRYPTEDVALUEDATA` is non-null — unreadable without the Notes password.

## Vault mapping

- **Raw layer:** `notes/apple-notes/raw/…` — decoded note objects (metadata +
  body text), full fidelity, partitioned by month of modification.
- **Contract layer:** `notes/apple-notes/…` per the **Phase 3 notes contract**
  (collected-notes shape) — expected: one row per note (`ts` = modified,
  `guid` = note id, `title`, `folder`, `body`/excerpt, created date), overflow
  in `extra`. Until that contract ratifies, this provider is **parked behind
  the pending notes contract**.
- **Dedupe:** note id (`Z_PK`/identifier) as `guid`; cursor on max modified
  date in `.trove/apple-notes-sync.json`, rebuildable by scanning output.

## Build plan

1. **Spike: compile the `.proto` first.** `prost` must consume the
   reverse-engineered Apple Notes proto before bodies decode — verify against a
   real `NoteStore.sqlite`. **Parser-last; flag Needs-sample.**
2. Module `crates/trove-core/src/apple_notes.rs`: `DEF` (Periodic, infrequent
   re-scan), gunzip + protobuf decode of `ZDATA`, `store` via notes helpers.
3. Skip rows where `ZISPASSWORDPROTECTED=1` / `ZENCRYPTEDVALUEDATA` non-null.
4. Fixtures from a real (sanitized) `NoteStore.sqlite`; parser + store + cursor
   tests, unique temp dirs. Track the proto schema drifting across macOS
   versions (the Ruby/Python parsers stay updated — mirror their schema).
5. Vault writes once the notes contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Metadata | ✅ built | grant FDA; Sync now; confirm note titles/folders/dates under `notes/apple-notes/` + hub last-data |
| Bodies | ✅ built | confirm a known note's decoded text matches the app; confirm locked notes are skipped (no body, no crash) |

## Build notes (2026-06-16)

Verified against David's real `NoteStore.sqlite` (841 notes, macOS 15/26):

- **Entity numbers**: looked up dynamically from `Z_PRIMARYKEY` (`ICNote` → 12,
  `ICFolder` → 15 on this DB; numbers can shift so we never hardcode them).
- **Date columns**: `ZCREATIONDATE3` (creation) and `ZMODIFICATIONDATE1`
  (modification) — NOT `ZCREATIONDATE` or `ZMODIFICATIONDATE` (those were NULL
  for all note rows; the suffixed variants hold the actual data).
- **Folder names**: `ZTITLE2` column on folder rows (`Z_ENT = folder_ent`),
  joined via `ZFOLDER` FK on the note.
- **Body decoding path** (confirmed in-memory): gzip → outer.field2 → inner.field3
  → body_container.field2 = UTF-8 note text. No prost/protoc build step needed;
  a minimal hand-rolled varint + length-delimited reader is sufficient.
- **Title**: first non-empty line of decoded body (Apple Notes convention);
  falls back to `ZSNIPPET` for locked notes (snippet is the Apple-computed
  preview, not the cryptographic body).
- **flate2** added as a direct dep (was already a transitive dep via ureq/zip;
  miniz_oxide backend, pure Rust, no C/cmake — standalone rule maintained).
- **Needs-sample** flag resolved: the proto path was confirmed against the real
  DB; no ZDATA fabrication was needed.

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Apple Notes (L2819–L2825). Feasibility 🟡 medium — schema known, decoding the
gzipped protobuf is the hard part. Best treated as a one-time migration
importer rather than high-frequency sync (notes don't change often). Sequence
after the plain-file note apps (Obsidian/Bear/Drafts), which are far cheaper.

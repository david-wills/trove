# Evernote

- **id:** `evernote`
- **domains:** `notes/` (contract: **Phase 3 pending** — drafted with Apple
  Notes + Bear + Day One + Obsidian + Logseq together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (parse a user-supplied `.enex` export; re-import is
  idempotent by note GUID)
- **connection:** none — import-only. The API (OAuth 1.0 + Thrift) is
  deliberately skipped.
- **evidence:** community — `.enex` (ENML) is stable XML and widely parsed;
  metadata + base64 resources are well understood
- **effort / priority:** S / P2
- **needs:** none — notes contract was ratified; notes.rs Note struct reused

## What it is

Evernote is one of the original cloud note-taking apps — notebooks, web clips,
scanned documents, and tagged notes accumulated by a large legacy user base
over 15+ years. The user base has been declining, but for anyone who used it,
the archive is a deep store of personal knowledge that is otherwise locked in a
proprietary cloud. Trove ingests the standard `.enex` export so that history
survives independent of the account.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Note bodies | all | ENML content (HTML subset), CDATA | community |
| Metadata | all | title, created, updated, tags, notebook, source-url | community |
| Attachments | all | base64 `<resource>` elements (images, PDFs) | community |

All optional in the contract (omit-if-empty). No tier gating — export captures
everything regardless of plan.

## Access & auth

- Export mechanism: in the Mac app, **File → Export Notes** → `.enex`
  (ENML/XML) or HTML. Up to 100 notes per batch, but whole notebooks export in
  one file; the entire account is exportable notebook-by-notebook.
- Auth: none for the import path. The Evernote API uses **OAuth 1.0** (not 2.0)
  with the Thrift protocol — too painful for a polished integration and on an
  unclear trajectory, so it is explicitly **out of scope**.
- Standalone-clean: a pure file parse, no network, no running app.

## Vault mapping

- **Raw layer:** `notes/evernote/raw/` — parsed note objects at full fidelity
  (ENML content, metadata, resource references), partitioned by month of
  created date.
- **Contract layer:** `notes/evernote/YYYY-MM.jsonl` per the (pending) notes
  contract — expected shape: one row per note (`ts` = created, `source`,
  `guid` = note GUID, `title`, `body` = ENML→markdown, `tags[]`, `notebook`,
  `updated`), attachments as sidecar artifacts (they're documents, not events).
  Parked behind **Needs-David (contract)** until the notes shape is ratified.
- **Dedupe:** note GUID as `guid` — re-importing an overlapping export updates
  rather than duplicates.

## Build plan

1. Module `crates/trove-core/src/evernote.rs`: `DEF` (Import), import box
   wired by the registry (point at one or more `.enex` files).
2. Registration line in `INTEGRATIONS`.
3. Parser: `.enex` is XML; note content lives in `<content>` CDATA as ENML (an
   HTML subset); resources are base64 `<resource>` elements; metadata fields
   are `created`/`updated`/`title`/`tag`/`notebook`/`source-url`.
4. Fixtures: small hand-built `.enex` files (text-only AND resource-bearing
   variants); parser + store + GUID-dedupe tests, unique temp dirs.
5. Vault writes via `store` helpers once the notes contract is ratified.
6. `letterboxd.rs` is the reference Import example to mirror.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Note bodies + metadata | ✅ unit-tested | export a real notebook to `.enex`; import via the box; confirm rows in `notes/evernote/` + hub last-data; spot-check title/tags/created |
| Attachments | ✅ unit-tested (count) | import a note with an image/PDF; confirm resource_count in contract extra |
| Re-import dedupe | ✅ unit-tested | import an overlapping export twice; confirm no duplicate GUIDs |

## Build notes (2026-06-16)

- Behavior: `Import` — accepts `.enex` files, no connection/auth needed.
- Contract: reuses `notes::Note` (notes domain, already ratified). The brief's "Needs-David (contract)" flag was stale — notes.rs was built by the bear pioneer.
- ENEX dates (`YYYYMMDDTHHMMSSz`, UTC) converted to local RFC3339 via chrono; confirmed against the official DTD at xml.evernote.com/pub/evernote-export4.dtd.
- Dedupe id: `created_raw|hash16(content)` — stable across title edits (title
  excluded), collision-resistant for same-second different-body notes, stable
  across re-exports for unchanged content (ENEX DTD v4 has no top-level `<guid>`).
- Parser: quick-xml 0.40 streaming; CDATA handled via `Event::CData`; text via
  `decode()` + `escape::unescape()`; element depth tracked (note_depth counter)
  so `<task>` / `<resource>` subtrees (which carry their own `<title>`,
  `<created>`, `<updated>`) never overwrite the note's own fields.
- Raw layer: full ENML content, resource count, note-attributes verbatim.
- 15 unit tests, all green; `cargo check` clean.

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Evernote (L2875–L2881). Feasibility 🟢 high for the import path; 🟠 low for the
API (OAuth 1.0 + Thrift), which is skipped. ENML is a subset of HTML; content
in `<content>` CDATA, attachments as base64 `<resource>`. The `evernote-backup`
Python lib uses the API but hits the same OAuth 1.0 friction — the desktop
export is the reliable path.

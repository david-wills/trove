# Logseq

- **id:** `logseq`
- **domains:** `notes/` (contract: **Phase 3 pending** — `notes` contract,
  drafted with Apple Notes / Bear / Drafts / Day One / Obsidian / Logseq
  together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user ZIPs their graph folder and drops it here;
  Periodic/folder-watch is gated on a future Tauri folder-picker command)
- **connection:** none (local folder; user picks the graph location)
- **evidence:** official-docs — plain `.md`/`.org` files on disk;
  documented `pages/` + `journals/YYYY_MM_DD.md` structure
- **effort / priority:** S / P1
- **needs:** Needs-David (folder pick — graph location is user-set at app
  first-run; no default path)

## What it is

Logseq is a local-first outliner / knowledge base storing a "graph" as a
folder of plain markdown (`.md`, optionally `.org`) files on disk. Same
story as Obsidian: the user owns the files. The value is the user's notes
and daily journals as durable, queryable text. Bundles with the Obsidian
folder-import code path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Pages | free (local) | page title, body markdown, mtime | official docs |
| Journals | free (local) | day-keyed body (`journals/YYYY_MM_DD.md`) | official docs |
| Block refs / tags | free (local) | inline `[[wikilinks]]`, `#tags` (kept as-is in body) | official docs |

All optional in the contract (omit-if-empty); the journals/pages split is
the only structural distinction.

## Access & auth

- File paths: a user-chosen graph folder containing `pages/` and
  `journals/YYYY_MM_DD.md`; a hidden `.logseq/` folder holds config + `.bak`
  backups (skip the backups, read the live files).
- No API, no auth, no network. Standalone-clean — pure local file reads.
  TCC: needs read access to the chosen folder (same Files-access grant as
  any local-folder import).
- **macOS lazy-flush gotcha:** Logseq had a bug (issue #10510, 2024) where
  edits weren't written to disk promptly. Surface a connect-card hint to
  confirm on-disk mode; treat mtimes as advisory, not authoritative.

## Vault mapping

- **Raw layer:** `notes/logseq/raw/…` — copies of the source `.md` files (or
  their parsed content), full fidelity, partitioned by page vs. journal.
- **Contract layer:** `notes/logseq/…` per the (pending) `notes` contract —
  expected shape: one row per note/page (`ts` from mtime or journal date,
  `source`, `guid` = stable page path/id, `title`, `body`); journals keyed
  by their `YYYY_MM_DD` date — a natural fit for Trove's day-keyed streams.
  Overflow (block structure, tags) in `extra`.
- **Dedupe:** stable page path (relative to graph root) as `guid`; cursor =
  max mtime in `.trove/logseq-sync.json`, rebuildable by scanning files.

## Build plan

1. Implemented as `Behavior::Import` (zip of the graph root) — the brief's
   Periodic/folder-watch future is gated on a Tauri folder-picker command.
2. Module `crates/trove-core/src/logseq.rs`: full implementation (stub
   replaced). The `pub mod logseq` and `INTEGRATIONS &crate::logseq::DEF`
   lines already existed from the Phase-2 stub registration.
3. ZIP prefix auto-detection (heuristic: single shared top-level folder is
   stripped so both `mygraph/pages/…` and `pages/…` ZIPs work).
4. Contract layer: `notes/logseq/YYYY-MM.jsonl` (one `Note` per file,
   partitioned by journal date or import month for pages, deduped by
   graph-relative path as `id`).
5. Raw layer: `notes/logseq/raw/YYYY-MM.jsonl` (full fidelity: kind, stem,
   body, zip_path, graph_path).
6. 13 unit tests passing: classify, detect_zip_prefix, journal date parsing,
   H1 extraction, full import round-trip, re-import dedup, raw fidelity,
   back-compat.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Pages | ✅ built | zip the graph folder; import; confirm page rows in `notes/logseq/YYYY-MM.jsonl` |
| Journals | ✅ built | confirm journal rows are keyed by the `YYYY_MM_DD` date from the filename |
| Re-import | ✅ built | re-import the same ZIP; confirm counts["updated"] increments, counts["imported"]=0 |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Logseq (L2795–L2801). Feasibility 🟢 high. Plain markdown on disk at a
user-chosen location — same code path as Obsidian, build them together.
Journals are daily `YYYY_MM_DD.md` files. Known macOS lazy-flush bug
(#10510) — advise on-disk mode in the UI. The `notes/` folder is the
collected layer; `artifacts/` stays the user-curated layer.

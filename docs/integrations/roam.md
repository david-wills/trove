# Roam Research

- **id:** `roam`
- **domains:** `notes/` (contract: **Phase 3 pending** — `notes/` shapes
  drafted across Apple Notes, Bear, Drafts, Day One, Obsidian, Logseq and the
  other note sources; `artifacts/` stays the user-curated layer)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (manual "Export All" ZIP — JSON or Markdown)
- **connection:** none — the read path is a user-initiated export ZIP. (The
  beta backend API at developer.ro.am does block-level add/append only and has
  **no full-graph export endpoint**; the roam-research-mcp server requires a
  running local service, which violates the standalone constraint — neither is
  a wired path.)
- **evidence:** official-docs (export feature) + community-schema — JSON export
  is a documented graph structure (pages as nodes, blocks as children);
  Markdown export is the human-readable variant.
- **effort / priority:** S / P2
- **needs:** none

## What it is

Roam Research is the original networked-thought outliner — pages and nested
blocks with bidirectional links, daily notes, and a graph database underneath.
Its user base is small and declining (Obsidian has largely displaced it), but
existing users' graphs hold years of linked notes. Trove captures that graph
from the user's export.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Pages / blocks | all (export) | page titles, nested block text | official docs |
| Links / refs | all (export) | block refs, page references | official docs |
| Daily notes | all (export) | date-keyed pages | official docs |

All optional in the contract (omit-if-empty).

## Access & auth

- Export: `…` (Roam logo) → Export All → JSON, Markdown, or EDN → downloads a
  ZIP of all pages. JSON is a graph structure; Markdown is more readable.
- Beta backend API (developer.ro.am): add/append blocks, retrieve individual
  pages/blocks — **no full-graph export**, so not a collector path.
- roam-research-mcp / browser-automation tools require a running local service
  — **violates standalone**; not wired.
- No TCC beyond reading the user-chosen export. Standalone-clean.

## Vault mapping

- **Raw layer:** `notes/roam/raw/…` — the exported graph (JSON preferred for
  fidelity), full structure.
- **Contract layer:** `notes/roam/…` per the pending Phase-3 `notes/`
  contract — expected one row per page (`ts`, `source`, `guid` = page/block
  id, `title`, `body` = flattened block tree, links/refs in `extra`). Parked
  behind the contract until it ratifies.
- **Dedupe:** `guid` = Roam page/block id; re-import is idempotent (replace on
  matching id).

## Build plan

1. Module `crates/trove-core/src/roam.rs`: `DEF` (Import), import-box pull hook
   accepting the export ZIP.
2. One registration line in `INTEGRATIONS`. No `CONNECTION` (import only).
3. Parse the JSON export (graph: pages as nodes, blocks as children) — flatten
   the block tree into a note body, preserve refs in `extra`. Markdown export
   as a simpler fallback path.
4. Fixtures from a small exported graph; parser + store tests, unique temp
   dirs.
5. Vault writes via `store` helpers once the `notes/` contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Graph import | ✅ built | Export All → JSON from Roam; drop the ZIP in the import box; confirm rows in `notes/roam/` + hub last-data |
| Block refs preserved | ✅ built | confirm bidirectional link uids land in `extra.refs` on the imported rows |
| Re-import idempotent | ✅ built | re-import a newer ZIP: rows update in place, no duplicates |

## Implementation notes

- Module: `crates/trove-core/src/roam.rs` — `Behavior::Import`, accepts ZIP
  or bare JSON.
- JSON format (well-established community format): array of page objects
  (`title`, `uid`, `create-time` ms, `edit-time` ms, `children` blocks).
  Block objects: `string`, `uid`, `heading` (0=normal/1-3=ATX), `order`,
  `create-time`, `edit-time`, `children` (recursive), `refs` (back-refs).
- Contract layer: one `Note` per page, body = block tree flattened to
  indented Markdown (blocks sorted by `order`), page/block ref UIDs in
  `extra.refs`. Partition by month of `created`; dedupe by page `uid`.
- Raw layer: verbatim page JSON (full block tree), partitioned by `_created`
  (immutable). Full fidelity — nothing dropped.
- No tags or folder model (everything is a page in Roam). `tags` and `folder`
  are omitted; heading blocks render as `# /## /### ...` in the body.
- Zip: picks the largest `.json` at the ZIP root (the graph dump).
- 12 passing unit tests; `cargo check` and `cargo test roam::` green.

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Roam Research (L2939–L2945). Feasibility 🟡 medium — export is manual (no
programmatic trigger), and the beta API can't dump the graph. The **manual
JSON/Markdown export is the only standalone-compliant path**; the MCP server
needs a running local service and is out. S effort if there's demand; user
base is small and declining, so this sits at P2.

# Obsidian

- **id:** `obsidian`
- **domains:** `notes/` (contract: `notes.Note` — BOUND, reused from `bear.rs`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (hourly folder-scan of user-chosen vault; mtime watermark)
- **connection:** none — plain filesystem (local directory; no auth)
- **evidence:** official-docs confirmed: `.md` files, YAML frontmatter `---`
  delimited, `tags:` lowercase, block-list (`- item`) and inline (`[a,b]`) both
  valid; `created`/`modified` are user-defined date fields (not built-in).
- **effort / priority:** S / P1
- **needs:** none — built and tested

## What it is

Obsidian is a popular local-first Markdown knowledge base. The entire vault is
a plain folder of `.md` files at any path the user chooses (commonly under
`~/Documents`). Because the data is already files-as-format, Trove can ingest
it directly — this is the cleanest possible source and the reference shape for
folder-import notes.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Note bodies | all (free) | title (filename/H1), Markdown body | official — plain files |
| Frontmatter | all (free) | YAML frontmatter (tags, aliases, dates, arbitrary keys) | official — preserved as-is |
| Folder structure | all (free) | relative path / notebook hierarchy | official — directory layout |

All optional in the contract; a note without frontmatter simply carries none.
No tiering, no code paths — Obsidian has no paid gates on local file access
(Sync/Publish are separate cloud services, not needed here).

## Access & auth

- No API, no auth. User selects a vault directory; Trove reads `.md` files
  under it via `Vault::resolve`-style path handling on the *source* side.
- Ignore the hidden `.obsidian/` config subfolder (workspace/plugin JSON —
  not user content).
- No TCC prompt for arbitrary user-chosen folders beyond the standard
  file-access grant; no network. Standalone-clean.
- Same code path as Logseq (folder-of-`.md` watch) — build the two together.

## Vault mapping

- **Raw layer:** `notes/obsidian/raw/…` — verbatim copy/index of source `.md`
  files preserving frontmatter and relative paths, full fidelity.
- **Contract layer:** `notes/obsidian/YYYY-MM.jsonl` per the (pending) `notes/`
  contract — expected shape: one row per note (`ts` = created/modified,
  `source`, `guid`, `title`, `body` markdown, `tags[]`, `path`), frontmatter
  overflow in `extra`.
- **Dedupe:** stable `guid` from vault-relative path (+ frontmatter id/created
  if present); mtime watermark in `.trove/obsidian-sync.json`, rebuildable by
  rescanning files.

## Build plan

1. Module `crates/trove-core/src/obsidian.rs`: `DEF` (Periodic, folder-watch),
   no `CONNECTION` (filesystem source). Source-folder picker setting per the
   import/folder-watch convention.
2. Registration line in `INTEGRATIONS`.
3. Fixtures: a small synthetic vault (notes with/without frontmatter, nested
   folders, a `.obsidian/` dir to confirm it's skipped); parser + store + mtime
   cursor tests, unique temp dirs.
4. Shared folder-of-`.md` reader with Logseq (sequence Logseq right after to
   exercise the same code path against journals/pages layout).
5. Vault writes via `store` helpers once the `notes/` contract is ratified;
   until then this provider is **parked behind Needs-David (contract)**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Note bodies + folders | ✅ tested | 19 unit tests pass; point picker at a real vault; Sync now; confirm rows in `notes/obsidian/` + hub last-data |
| Frontmatter (block list) | ✅ tested | `parses_block_list_tags`, `imports_note_with_frontmatter_tags_and_folder` |
| Frontmatter (inline list) | ✅ tested | `parses_inline_list_tags`, `inline_list_tags_from_vault` |
| Fallback to mtime/heading | ✅ tested | `imports_note_without_frontmatter_falls_back_to_mtime`, `title_falls_back_to_heading` |
| .obsidian/ skip | ✅ tested | `skips_obsidian_hidden_config_dir` |
| Incremental mtime | ✅ tested | `incremental_mtime_watermark_skips_unchanged_files` |
| Upsert on edit | ✅ tested | `upsert_replaces_existing_note_on_edit` |
| Extra frontmatter keys | ✅ tested | `extra_frontmatter_keys_land_in_extra` |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Obsidian (L2787–L2793). Feasibility 🟢 high, effort S — trivially the
easiest source (it *is* the filesystem). User sets vault location freely; no
sync service required; `.obsidian/` config ignored. M1 import or folder-watch.
Logseq (L2795–L2801) shares this exact path — bundle them.

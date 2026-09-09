# Omnivore (historical import)

- **id:** `omnivore`
- **domains:** `reading/` (contract: **Phase 3 pending** — reading contract
  drafted from Readwise + Instapaper + Raindrop + Pinboard + Kindle together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (one-shot file import; hosted service is gone)
- **connection:** none (user supplies their own pre-shutdown export ZIP)
- **evidence:** community-documented format, high confidence — export ZIP of
  one markdown file per article, frontmatter (URL, title, tags) + highlights
  as blockquotes. Source code public at github.com/omnivore-app/omnivore
  (AGPL), so the format is verifiable without the service.
- **effort / priority:** S / P2
- **needs:** none

## What it is

Omnivore was a beloved open-source read-later app, shut down November 2024
after the ElevenLabs acquihire; all hosted data was deleted. Only users who
exported before shutdown have files — a small but passionate base.
Self-hosted instances remain possible (AGPL) and produce the same export,
but they're an edge case, not a live-sync target.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Saved articles | n/a (export file) | URL, title, tags, save metadata from frontmatter | research doc; open-source repo confirms format |
| Highlights | n/a (export file) | highlight text (blockquotes in the article markdown) | research doc |

All capability fields optional in the contract (omit-if-empty); an article
with no highlights simply carries none.

## Access & auth

- No endpoints, no auth. The user drags in their export ZIP.
- Format: ZIP containing one `.md` per saved article — YAML-ish frontmatter
  (URL, title, tags) with highlights embedded as blockquotes in the body.
- No TCC, no network. Standalone-clean by construction.

## Vault mapping

- **Raw layer:** `reading/omnivore/raw/` — per-article parsed records at
  full fidelity (JSONL, partitioned by saved-at `YYYY-MM`); article body
  markdown preserved.
- **Contract layer:** `reading/omnivore/` per the pending Phase 3 reading
  contract — expected shape: one row per save (`ts`, `source`, `guid`,
  `url`, `title`, `tags[]`), highlights either as child rows or an embedded
  list per whatever the contract ratifies; overflow in `extra`.
- **Dedupe:** `guid` from URL (frontmatter); re-importing the same ZIP is a
  no-op.

## Build plan

1. Module `crates/trove-core/src/omnivore.rs`: `DEF` with
   `Behavior::Import` (registry-driven import box; no connection).
2. One registration line in `INTEGRATIONS`.
3. Parser: unzip → per-file frontmatter parse → blockquote-highlight
   extraction. Frontmatter key set comes from the research evidence + the
   public repo; tolerate missing keys (omit-if-empty).
4. Fixtures: hand-built ZIP with highlight-bearing and highlight-free
   articles, odd frontmatter; parser + store + reimport-dedupe tests,
   unique temp dirs.
5. Contract rows wait on the Phase 3 reading contract; raw layer can land
   first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Articles + highlights | — | import a real pre-shutdown export ZIP via the hub import box; confirm rows in `reading/omnivore/` + last-data; re-import and confirm zero new rows. Requires a real user file (hosted data is gone — no fresh export can be generated except from a self-hosted instance) |

## Implementation notes (build)

**Brief format correction:** The brief described "one markdown file per
article with YAML frontmatter." The actual format (confirmed from the
open-source `packages/api/src/jobs/export.ts`) is:

- `metadata_N_to_M.json` — JSON arrays of article metadata objects
  (`id`, `slug`, `title`, `description`, `author`, `url`, `state`,
  `readingProgress`, `thumbnail`, `labels[]`, `savedAt`, `updatedAt`,
  `publishedAt`). Multiple batch files are all parsed.
  State values are **capitalized**: `"Active"` | `"Archived"` | `"Unknown"`
  (via `itemStateMappping()` in `packages/api/src/jobs/export.ts`).
- `content/{slug}.html` — full article HTML (preserved in raw layer).
- `highlights/{slug}.md` — highlights as blockquotes with labels and note on
  **separate** blank-line-separated paragraphs (confirmed from
  `highlightToMarkdown()` in `packages/api/src/utils/parser.ts`):
  ```
  > {quote text}

  #label1 #label2

  {annotation / note}
  ```
  Note-type highlights (`HighlightType.Note`) are emitted as plain paragraphs
  with no blockquote prefix: `${annotation}\n\n`. These are stored as
  highlights with an empty `text` field and the annotation in `note`.

**Contract fit:** `reading.Item` per article + `reading.Highlight` per
highlight. Both layers under `reading/omnivore/`. Raw layer unconditional.
`site` field is derived from the URL hostname. `highlight.ts` uses the
article's `savedAt` as an approximation (no per-highlight timestamp in the
export format).

**Behavior:** `Import` (ZIP drop). No auth, no network. Re-import is a no-op
(dedupe by `guid` = article `id`; highlight guid = `{article_id}::{ordinal}`).

**14 tests pass**, `cargo check` green.

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Omnivore
(Historical Import Only) (L1624–L1631); cross-cutting note 2 (L1708).
Feasibility 🟠 low for the service (dead), high for the format
(confirmed from open-source export handler). No time-sensitivity — the
importable population is fixed since Nov 2024. App copy should be honest:
this only helps users who exported before the November 2024 shutdown.

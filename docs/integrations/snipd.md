# Snipd

- **id:** `snipd`
- **domains:** `reading/` (contract: **Phase 3 pending** — highlights shape
  drafted from Readwise + Instapaper + Kindle + Snipd together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (LocalSync — watch the folder Snipd's Obsidian
  export plugin writes to; the user points Snipd's vault-sync at a folder
  and Trove ingests new `.md` files from it)
- **connection:** none (no API, no login — file-watch only)
- **evidence:** official Obsidian export plugin (one Markdown file per snip,
  frontmatter documented in the research entry: `podcast:`, `episode:`,
  `timestamp:`, `tags:`, `summary:`, plus a transcript block); no public API
- **effort / priority:** M / P2
- **needs:** user-setup (Snipd must be configured to export/sync snips to a
  folder Trove can watch) · reading contract not yet ratified (Needs-David)

## What it is

Snipd is an AI-first podcast player whose signature feature is "snips" —
clipped moments from episodes with an AI summary, transcript excerpt, and
user notes. It is a **highlights/notes source, not a listening-history
source**: only clipped moments exist, there is no playback log. High-signal
for podcast-heavy users; the snips are otherwise locked inside the app
unless exported.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Snips (clipped moments) | all plans | podcast, episode, timestamp-in-episode, tags | export plugin frontmatter |
| AI summary per snip | all plans | summary markdown | export plugin frontmatter |
| Transcript excerpt | excerpt free; full episode transcripts Premium | transcript block | research notes |
| User notes | all plans | note body | export plugin frontmatter |

All optional in the contract; a free user's rows simply carry excerpt-level
transcripts. No tier-specific code paths.

## Access & auth

- No public API. Export paths from the app: Markdown, Obsidian vault sync,
  Readwise, Notion, Logseq. The **Obsidian vault sync** is the automatable
  one — the official plugin writes one `.md` file per snip to a configurable
  folder, continuously.
- Trove's path: the user sets that folder (or we suggest a location), and
  the def watches/polls it. No standard file-drop export to an arbitrary
  folder via the UI, so this setup step is unavoidable.
- No TCC beyond ordinary file access to the chosen folder; no network;
  standalone-clean. Users who run Readwise get the same snips through the
  `readwise` def — dedupe across the two at the contract layer.

## Vault mapping

- **Raw layer:** `reading/snipd/raw/` — the original snip Markdown files
  copied verbatim (full fidelity, including transcript blocks).
- **Contract layer:** `reading/snipd/YYYY-MM.jsonl` per the pending reading
  contract — expected shape: one row per snip (`ts` = snip creation/added
  time, `source`, `guid`, `title` = episode, `author` = podcast,
  `highlight` = transcript excerpt, `note`, `tags[]`), AI summary and
  in-episode timestamp in `extra`.
- **Dedupe:** `guid` from the snip filename / frontmatter id; ingest cursor
  (seen-file set) in `.trove/snipd-sync.json`, rebuildable by rescanning.

## Build plan

1. Module `crates/trove-core/src/snipd.rs`: `DEF` (Periodic, folder poll),
   settings field for the watched-folder path, `pull` hook for Sync-now.
   No `CONNECTION` (no auth).
2. Registration line in `INTEGRATIONS`.
3. Parser for the snip Markdown shape (frontmatter + body sections);
   fixtures synthesized from the research-documented frontmatter fields,
   then verified against a real export early — the format is plugin-defined,
   not formally specced, so treat the parser as tolerant (unknown
   frontmatter keys → `extra`).
4. Setup copy on the def must walk the user through enabling Snipd's
   Obsidian export and choosing the folder (disabled-control affordance
   rule: the toggle explains what's missing until a folder is set).
5. Contract rows written via `store` helpers once the reading contract is
   ratified; until then **parked behind Needs-David (contract)**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Snips + summaries + notes | — | configure Snipd → Profile → Export snips → Obsidian sync to a folder; point the def at it; Sync now; confirm rows in `reading/snipd/` + hub last-data |
| Full episode transcripts | — | requires a Snipd Premium account (any real Premium user's run can validate this slice) |

## Build notes (2026-06-21)

- Behavior: Periodic LocalSync (folder watch, no auth). Hourly scan of the
  user-configured Snipd export folder, mtime-watermarked per file.
- **Two Snipd export formats discovered** (brief was pre-research):
  1. **Obsidian sync plugin** (`snipd-app/snipd-obsidian`, REST API + Bearer
     token): writes one `.md` per *episode* (all snips embedded in template),
     YAML frontmatter includes `snips_count`, `episode_publish_date`, etc.
     The template is configurable; exact field names vary per user.
  2. **Direct Markdown export** (Profile → Export snips → Markdown ZIP): per
     the research notes, one `.md` per *snip* with frontmatter fields
     `podcast`, `episode`, `timestamp`, `tags`, `summary`, and transcript
     body. This is the simpler format the brief describes.
- **Contract layer parked** (`parser_parked_needs_sample=true`): the per-snip
  format varies between formats (episode-level vs snip-level), and no sample
  file is on disk. The raw layer captures everything verbatim. Once a sample
  is confirmed, snip files map to `reading.Highlight` rows (`ts`=date, `guid`
  =path, `title`=episode, `author`=podcast, `text`=transcript, `note`=notes,
  `location`=timestamp, `extra`={summary, url}).
- Brief's "no public API" note is stale: the Obsidian plugin uses
  `https://api.snipd.com/v1/public/api/` with OAuth-like Bearer token, but
  this is the Obsidian plugin's internal path, not a documented public API for
  third parties. Folder-watch is still the right approach for Trove.
- `Needs-sample`: a real export file (either format) is needed to activate
  the contract layer.
- **Cross-month dedup (fixed):** `upsert_snipd_raw` now scans ALL existing
  month partitions before inserting into the target month. Without this, a
  file whose mtime crossed a month boundary (Obsidian plugin rewrites the
  episode `.md` when snips are added, advancing mtime) would leave a stale
  row in the old month alongside the new row — two raw rows, same `id`.
- **Default Obsidian template has no frontmatter:** The `snipd-obsidian`
  plugin's DEFAULT_EPISODE_TEMPLATE emits episode metadata as a markdown
  body list (`- Episode publish date: ...`), not YAML frontmatter. The
  frontmatter date-extraction path (`date`/`created`/`episode_publish_date`)
  is a no-op for default-template exports; `_ts` always falls back to mtime
  for those files. No data loss — the body is captured verbatim in raw —
  but the assumption is noted here so the un-parking pass can parse the body
  section properly (the `## Episode metadata` block and per-snip
  `🎧 HH:MM:SS` blocks).

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Snipd
(L3334–L3340). Feasibility 🟡 medium — the data is good but the path
requires user setup (M2: Snipd syncs to a folder, Trove watches it).
Premium gates full transcripts. Most valuable to users already in the
Obsidian orbit. Readwise export overlaps — sequence after `readwise` and
dedupe against it. No listening history here; podcast plays come from the
podcasts/scrobbler defs.

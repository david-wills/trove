# Pocket (historical import)

- **id:** `pocket`
- **domains:** `reading/` (contract: **Phase 3 pending** — reading contract
  drafted from Readwise + Instapaper + Raindrop + Pinboard + Kindle together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (one-shot file import; no live path exists or ever will)
- **connection:** none (user supplies their own pre-existing export file)
- **evidence:** community-documented format, high confidence — Netscape
  bookmarks HTML (and a CSV variant) with URL, title, tags, timestamp, read
  status. The service itself is dead, so the format is frozen.
- **effort / priority:** S / P2
- **needs:** none

## What it is

Pocket was the dominant read-later service until Mozilla shut it down on
July 8, 2025; the export portal and API closed November 12, 2025. A large
displaced cohort holds export files of years of saved-article history with
nowhere to put them. This is purely historical archival — low effort, high
goodwill for former Pocket users.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Saved articles | n/a (export file) | URL, title, tags, saved-at timestamp, read/unread status | research doc, format community-documented |

All capability fields optional in the contract (omit-if-empty). No tiering —
the export file is self-contained and whatever it carries is what we get.

## Access & auth

- No endpoints, no auth, no live service. The user drags in an export file
  they downloaded before November 12, 2025.
- Format: Netscape bookmarks HTML (`<DL><DT><A HREF=... ADD_DATE=...
  TAGS=...>`); Pocket also issued a CSV variant. Accept both.
- No TCC, no network. Standalone-clean by construction.

## Vault mapping

- **Raw layer:** `reading/pocket/raw/` — the parsed export rows at full
  fidelity (JSONL, partitioned by saved-at `YYYY-MM`).
- **Contract layer:** `reading/pocket/` per the pending Phase 3 reading
  contract — expected shape: one row per save (`ts` = saved-at, `source`,
  `guid`, `url`, `title`, `tags[]`, `read` status), overflow in `extra`.
- **Dedupe:** `guid` from URL + saved-at timestamp (re-importing the same
  file is a no-op; two export files from different dates merge cleanly).

## Build plan

1. Module `crates/trove-core/src/pocket.rs`: `DEF` with `Behavior::Import`
   (registry-driven import box; no connection, no pull hook).
2. One registration line in `INTEGRATIONS`.
3. Parser for Netscape-HTML first (format is standard and well documented);
   the CSV variant second — if no documented column list surfaces from the
   research evidence, build that branch parser-last against a real user
   file (soft Needs-sample on the CSV branch only).
4. Fixtures: hand-built Netscape HTML with tags/read-status edge cases;
   parser + store + dedupe-on-reimport tests, unique temp dirs.
5. Contract rows wait on the Phase 3 reading contract; raw layer can land
   first (full fidelity first, normalization second).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Saved articles (HTML export) | 🧪 built | import a real pre-shutdown export via the hub import box; confirm rows in `reading/pocket/` + last-data; re-import the same file and confirm zero new rows |
| CSV variant | not built | no documented column list surfaced; Pocket only issued the HTML format in practice; CSV branch deferred to Needs-sample |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Pocket
(Historical Import Only) (L1616–L1623); cross-cutting note 2 (L1708): dead
services are a historical-import opportunity. Feasibility 🟠 low for the
*service* (dead, export window closed) but high for the *format*. No
time-sensitivity remains — the export window already closed, so the
population of importable files is fixed. The app copy should be honest that
this only helps users who exported before Nov 12, 2025. Raindrop (separate
brief) is where much of the Pocket cohort migrated; its importer also
accepts Pocket files, but Trove ingesting the original export directly is
cleaner provenance.

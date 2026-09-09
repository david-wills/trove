# Letterboxd

- **id:** `letterboxd`
- **domains:** `media/plays/` (contract: **media-plays, ratified**) ·
  `media/letterboxd/` (curation — ratings/watchlist/lists — per-source raw,
  follow-on)
- **status:** 🧪 built (shipped pre-pipeline as the reference import;
  David promotes to ✅)
- **unavailable_reason:** none
- **behavior:** Import (ZIP/CSV drop; re-runnable, guid-deduped)
- **connection:** none (user-initiated export; no login held by Trove)
- **evidence:** official-docs — letterboxd.com/user/exportdata ZIP (shipped
  parser `letterboxd.rs` proves the format); official API is request-only
  and explicitly rejects personal/data-analysis projects (research doc, 2026)
- **effort / priority:** S / P1
- **needs:** extension — public RSS poll (last 50 diary entries) for fresh
  data between exports

## What it is

The dominant film-diary app: users log every film they watch with a date,
star rating, rewatch flag, and review. The export is excellent quality —
a complete, dated watch history that most film enthusiasts have nowhere
else. Already shipped as Trove's reference Import integration.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Diary (watch log) | all accounts | watched_date, title, year, rating (0.5–5), rewatch, letterboxd_uri | official export (shipped) |
| Ratings / watched / reviews / lists CSVs | all accounts | per-film curation rows | official export ZIP |
| Fresh diary entries | public profiles only | last 50 diary entries via RSS | research doc (RSS documented) |

All optional in the contract; the shipped import consumes `diary.csv` and
ignores the rest today (curation CSVs are the follow-on).

## Access & auth

- Export: letterboxd.com/user/exportdata — instant ZIP (diary.csv,
  watched.csv, ratings.csv, reviews.csv, lists.csv). User downloads and
  drops it on Trove; no credentials stored.
- RSS (extension): `https://letterboxd.com/{username}/rss/` — public
  profiles, keyless, last 50 diary entries; viable Periodic follow-on for
  between-export freshness. Private profiles: export only.
- Official API: do not apply — personal projects are rejected.
- No TCC, standalone-clean.

## Vault mapping

- **Raw layer:** the import parses in place; rows land directly as contract
  lines (CSV is already the full fidelity). Curation follow-on would write
  `media/letterboxd/` (ratings/watchlist/lists stay per-source raw — only
  play events join the contract).
- **Contract layer:** `media/plays/letterboxd/YYYY-MM.jsonl` per the
  ratified media-plays contract — `ts` = watched date, `category:"video"`,
  `kind:"play"`, `title`, `subtitle` (director when known), `detail` =
  letterboxd_uri, `seconds:0` (diary has no duration), rating/rewatch in
  `extra`.
- **Dedupe:** diary-entry guid (as shipped) — re-dropping the same export
  is a no-op.

## Build plan

Shipped (`crates/trove-core/src/letterboxd.rs`, registered, generic import
box). Remaining work, in order:

1. RSS poll extension: optional username field, Periodic poll of the public
   feed, same guid dedupe — closes the export-staleness gap.
2. Curation import: parse ratings/watchlist/lists CSVs from the same ZIP
   into `media/letterboxd/` raw snapshots.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Diary ZIP/CSV import | 🧪 built (pre-pipeline; fixture-tested incl. ZIP path + re-import dedupe) | drop a real export ZIP; confirm rows in `media/plays/letterboxd/` and Media tab; re-drop → no dupes; David promotes to ✅ |
| RSS freshness poll | — | not built; on build: set username, wait a poll, log a film on letterboxd.com, confirm the row appears without an export |
| Curation CSVs | — | not built |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§Letterboxd (L3238–L3244). Feasibility 🟢 high for CSV/RSS, blocked for the
official API. Diary.csv is the richest file. Pairs with Trakt (cross-refs
via TMDB/IMDB ids) and the IMDb ratings import for film-data triangulation.

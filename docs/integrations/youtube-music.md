# YouTube Music

- **id:** `youtube-music`
- **domains:** `media/plays/` (contract: ✅ ratified — listen events) with
  per-source raw at `media/plays/youtube-music/raw/`
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Takeout archive — same archive as YouTube watch
  history; UI nudges periodic re-export)
- **connection:** none (the archive is exported by the user at
  takeout.google.com; no OAuth involved)
- **evidence:** official Takeout export —
  `Takeout/YouTube and YouTube Music/history/watch-history.json`, filtered to
  `header == "YouTube Music"` records (confirmed: YTMtoMaloja, beebls, purarue/google_takeout_parser)
- **effort / priority:** S / P1
- **needs:** none

## What it is

Google's music-streaming service. Listening history is complete back to
account creation but has **no API** — Takeout is the only path. It arrives
in the very same archive as YouTube watch history, so once the Takeout
importer exists this provider is near-zero marginal effort: a distinct
service in the hub (its own card, its own source folder), sharing the
archive-walking machinery.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Listening history | none | title (song), subtitles[0].name (artist), time (ISO 8601) | official Takeout |
| Music library data | none (if selected in the export) | library/playlist metadata | official Takeout |

Fields are sparse by design of the export: **no album, no duration, no
ms_played**. All contract fields are optional — rows simply omit what the
export doesn't carry.

## Access & auth

- takeout.google.com → select "YouTube and YouTube Music" → switch History
  format from HTML to **JSON** → export. File:
  `history/watch-history.json` (contains both YouTube video watches and
  YouTube Music listens, discriminated by the `header` field).
- One-shot manual export; no scopes, no connection. Re-export to refresh.
- No live API for listening history exists. For ongoing capture the brief
  recommends surfacing a hint: scrobble YouTube Music to Last.fm via a
  browser extension (e.g. Web Scrobbler) — covered by the scrobbler
  providers, not built here.
- No TCC; fully offline import. Standalone-clean.

## Vault mapping

- **Raw layer:** `media/plays/youtube-music/raw/` — the native
  music-history JSON rows, full fidelity. Library/playlist data, if
  present, is curation → `media/youtube-music/` per the media-curation
  rule.
- **Contract layer:** `media/plays/youtube-music/YYYY-MM.jsonl` per the
  ratified media-plays contract: `ts`, kind = play, title, artist from
  subtitles[0].name; album/duration omitted (absent from export).
  Records without `subtitles` are dropped (no artist key = unusable for
  grouping; matches reference parser `select(has("subtitles"))`).
- **Dedupe:** no native ids — `guid` = hash(time, title, artist).
  Re-imports of overlapping archives must be idempotent (same rule as
  google-takeout).

## Build plan

1. Sequence **after** the `google-takeout` importer — this def reuses its
   archive-walking code; one archive drop should populate both providers
   (the importer routes `music-history.json` to this def's writer).
2. Module `crates/trove-core/src/youtube_music.rs`: `DEF` with
   `Behavior::Import` (`letterboxd.rs` is the reference import example);
   accept either the full Takeout zip/folder or the bare
   `music-history.json`.
3. Registration line in `INTEGRATIONS`. No connection.
4. Fixtures: music-history rows with and without `subtitles` (artist can
   be absent — optional-chain per the sparse-fixture rule); parser +
   store + idempotent re-import tests, unique temp dirs.
5. UI copy: export steps, JSON-format gotcha, re-export nudge, and the
   Web Scrobbler/Last.fm suggestion for live capture.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Listening history | ✅ unit-tested | run a real Takeout export (JSON format), drop the archive on the import box, confirm rows in `media/plays/youtube-music/` + hub last-data |
| Re-import idempotence | ✅ unit-tested | import the same archive twice; row counts unchanged (tested in `imports_bare_json_writes_both_layers_and_is_rerunnable`) |
| No-subtitles records dropped | ✅ unit-tested | records without `subtitles` are dropped (matches `select(has("subtitles"))`); empty subtitles array also dropped |
| Header filtering | ✅ unit-tested | only `header=="YouTube Music"` records ingested; plain YouTube video watches skipped |
| Zip import | ✅ unit-tested | full Takeout zip accepted; reads `watch-history.json` and filters to YTM records only |
| Error on HTML format | ✅ unit-tested | clear error when user exports HTML instead of JSON |
| "Watched " prefix stripped | ✅ unit-tested | `strips_watched_prefix_from_title` — contract title is bare song name, guid uses bare slug |
| " - Topic" suffix stripped | ✅ unit-tested | `strips_topic_suffix_from_artist` — contract subtitle is bare artist name for top-charts grouping |
| Non-English title locale guard | ✅ unit-tested | `non_english_title_passes_through_unchanged` — no mangling when prefix absent |

## Build notes

- Behavior set to `Import` (replaces `NotWired` stub). Accepts `.zip` or `.json`.
- Contract mode: `reuse-bound` (media-plays / `MediaItem`). No new struct, no DOMAINS change, no schema edit.
- Raw layer written unconditionally to `media/plays/youtube-music/raw/YYYY-MM.jsonl`.
- Contract layer to `media/plays/youtube-music/YYYY-MM.jsonl` (month-partitioned by local ts).
- `guid = "ym-{raw_time}-{title_slug}"` — deterministic, no native id in export; slug uses the bare song name (prefix-stripped) so guids are locale-stable.
- Fields confirmed against multiple external sources: YTMtoMaloja (`select(.header=="YouTube Music")`), beebls README, purarue/google_takeout_parser. The research notes L3314–L3316 claim for a separate `music-history.json` is incorrect — the real file is `watch-history.json`, subset-filtered by `header`.
- **Title prefix stripping:** real Takeout `title` fields carry a localized action-verb prefix ("Watched <song>" in English exports). The parser strips the known English "Watched " prefix when present and passes non-matching titles through unchanged (locale guard — non-English exports keep their localized prefix rather than being mangled). The raw layer preserves the verbatim title.
- **Artist suffix stripping:** auto-generated YouTube Music artist channels carry a " - Topic" suffix (e.g. "Tame Impala - Topic"). The parser strips this suffix before storing `subtitle` (the top-charts grouping key). The raw layer preserves the verbatim channel name.
- **Locale caveat:** the "Watched " prefix recognition is English-only. Users who export Takeout in a non-English account language will see localized verb prefixes in the contract title field. Switching the Google account language to English before exporting avoids this.
- No new deps added; `zip`, `chrono`, `serde_json`, `anyhow` all already in `Cargo.toml`.
- 18 tests (adversarial-verify fixes: correct file, header filtering, subtitles-required), all pass. `cargo check` clean.

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §YouTube
Music (L3310–L3316). Feasibility 🟢 high — official export, straightforward
JSON, zero marginal effort beside the YouTube Takeout work. Gotchas: sparse
fields (no album/duration), HTML-default export format, and no live API
ever — Takeout + optional scrobbling is the complete story.

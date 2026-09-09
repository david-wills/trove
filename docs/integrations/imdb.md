# IMDb

- **id:** `imdb`
- **domains:** `media/imdb/` (curation — ratings/watchlist/lists — raw-only;
  no plays: nothing here is a watch event)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (per-list CSV drop; re-runnable)
- **connection:** none (user-initiated export; no login held by Trove)
- **evidence:** official-docs — instant CSV export via imdb.com/exports
  (ratings, watchlist, each custom list); CSV columns documented in the
  research doc; no public API for user data
- **effort / priority:** S / P1
- **needs:** none

## What it is

The default movie-rating tool for a huge share of casual film watchers —
many users have years of star ratings on IMDb and nowhere else. The export
is official, instant, and stable. The data is **curation, not plays**:
a rating with a "Date Rated" is an opinion event, not a watch event, so it
stays per-source raw rather than joining media-plays.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Ratings CSV | all accounts | Const (IMDb id), Your Rating, Date Rated, Title, Year, Title Type, Genres, Runtime, Directors, URL | official export (columns per research doc) |
| Watchlist CSV | all accounts | Position, Const, Created, Modified, Description, Title, URL, Title Type, IMDb Rating, Runtime (mins), Year, Genres, Num Votes, Release Date, Directors, Your Rating (optional), Date Rated (optional) | official export; unrated entries have empty Your Rating/Date Rated |
| Custom lists | all accounts | same header as watchlist (Position + list-specific columns + rating columns); one CSV per list | official export |

All optional; exports are per-list, so a user may drop any subset.

## Access & auth

- Export: Your Ratings page (or any list) → three-dot menu → Export; the
  generated download appears at imdb.com/exports. Instant, account login
  on IMDb's side only — Trove never sees credentials.
- No API path exists for user data; nothing to poll.
- No TCC, standalone-clean.

## Vault mapping

- **Raw layer:** `media/imdb/ratings.jsonl`, `media/imdb/watchlist.jsonl`,
  `media/imdb/lists/<list>.jsonl` — one row per title, full CSV fidelity,
  snapshot-replace or guid-merge on re-import.
- **Contract layer:** none ratified applies — media curation is per-source
  raw by the taxonomy routing rules. The `Const` IMDb id is the natural
  `guid` and cross-references Trakt/Letterboxd rows at read time.
- **Dedupe:** `Const` per list; re-dropping an export updates in place.

## Build plan

1. Module `crates/trove-core/src/imdb.rs`: `DEF` (Import), CSV parser for
   the documented column set; route file→stream by header sniff. Real
   modern watchlist/list exports contain BOTH list-specific columns
   (`Position`, `Created`, `Modified`, `Description`) AND rating columns
   (`Your Rating`, `Date Rated`). The discriminator is presence of `Position`
   or `Created` (absent from pure ratings exports), NOT the presence of
   `Your Rating`. Within a list export, `Your Rating`/`Date Rated` are
   optional — unrated entries carry empty strings and must not be skipped.
2. Registration line in `INTEGRATIONS`; generic import box does the UI.
3. Fixtures use the real modern export header (Position, Const, Created,
   Modified, Description, Title, URL, Title Type, IMDb Rating, Runtime (mins),
   Year, Genres, Num Votes, Release Date, Directors, Your Rating, Date Rated)
   with unrated rows (empty Your Rating/Date Rated); re-import idempotence
   test; unique temp dirs.
4. No live export sample was on hand during initial build — columns verified
   against community sources (romiojoseph/imdb-watchlist-export-visualizer;
   TMDB forum thread). Validate against a genuine export in Phase 4.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Ratings import | ✅ unit-tested | export Your Ratings on imdb.com; drop the CSV; confirm rows in `media/imdb/ratings/YYYY-MM.jsonl` + hub last-data; re-drop → no dupes |
| Watchlist / custom list | ✅ unit-tested | export a watchlist CSV (WATCHLIST.csv); drop; confirm snapshot at `media/imdb/watchlist.jsonl`; custom list lands at `media/imdb/lists/<stem>.jsonl` |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §IMDb
(L3246–L3252). Feasibility 🟢 high — official, instant, stable. Per-list
exports mean multiple drops. `Const` ids cross-reference Trakt and
Letterboxd data for read-time joins. Quick win; no time sensitivity.

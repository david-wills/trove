# Prime Video

- **id:** `prime-video`
- **domains:** `media/plays/prime-video/` (contract: **media-plays,
  ratified**)
- **status:** 🧪 built (Needs-sample — export shape unverified; see below)
- **unavailable_reason:** none
- **behavior:** Import (user requests the Amazon privacy export, drops the
  CSV/ZIP into the import box)
- **connection:** none (the export is requested at amazon.com with the
  user's own login; Trove never authenticates)
- **evidence:** secondary-research — Amazon privacy-portal research notes
  document columns as `Title, Device, Country, WatchedStartTime,
  WatchedEndTime, SecondsWatched`; no primary Amazon doc publicly confirms
  these exact column names.  **A real DSAR export sample is needed to
  validate the column names, timestamp format, and ZIP folder structure
  before removing the Needs-sample caveat.**
- **effort / priority:** S / P1
- **needs:** real Amazon DSAR export sample (any account; can be anonymised
  with fake titles/dates — we only need the header row and one data row to
  confirm the shape)

## What it is

Amazon's streaming video service — bundled with Prime, so the audience is
enormous and most users have history they've never seen. The privacy-export
CSV is *richer* than Netflix's quick export: it carries actual watch
duration and start/end times, which means honest `seconds` in the contract
rather than zeros.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Watch history | any Prime account | Title, Device, Country, WatchedStartTime, WatchedEndTime, SecondsWatched | official privacy export |

All optional in the contract. No public API for viewing history exists —
the data request is the only path, and it's a real, officially supported
one (not a privacy-portal lottery like Disney+/Hulu/Max).

## Access & auth

- Request flow: amazon.com → Account → "Request your personal
  information" → select `PrimeVideo.WatchHistory` → ZIP with CSVs.
  Turnaround: typically minutes to hours, can be up to 30 days; the
  download link expires — the in-app guide must tell users to import
  promptly.
- No connection, no TCC, no local files. Standalone-clean: the user
  fetches their own export in the browser.
- The primevideo.com watch-history page exists but needs a console script
  to scrape — rejected (fragile, automation-hostile); the formal request
  is the supported path.

## Vault mapping

- **Raw layer:** `media/plays/prime-video/raw/` — the export CSV(s) as
  received (region variants differ slightly; keep full fidelity).
- **Contract layer:** `media/plays/prime-video/YYYY-MM.jsonl` per the
  ratified media-plays contract — `ts` = WatchedStartTime,
  `category:"video"`, `kind:"play"` (or `"partial"` when SecondsWatched is
  a small fraction — mirror the Netflix importer's threshold if one is
  set), `title` from Title, `subtitle` = series name when parseable from
  the title string, `seconds` = SecondsWatched, `device` = Device, `guid`
  = hash of (Title, WatchedStartTime). Country and the raw title string
  ride in `extra`.
- **Dedupe:** `guid` makes re-imports of overlapping exports safe.

## Build plan

1. Module `crates/trove-core/src/prime-video.rs` (`prime_video`): `DEF`
   (Import) — the registry import box, hub card, and Recent-data view are
   free. Setup copy walks the privacy-request flow step by step (the
   research doc flags it as "buried"; the in-app guide is most of the UX
   value).
2. CSV parser tolerant of region variation (EU vs US column/format
   drift is reported) — parse by header name, never position; unknown
   columns to `extra`.
3. Fixtures: synthesize from the documented columns; add a real-export
   fixture when one lands (region-variant fixtures welcome).
4. Sequence right after/alongside the Netflix importer — same shape,
   shared patterns, one review.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Watch history import | 🧪 built (Needs-sample) | request the export from a real Amazon account; drop the ZIP/CSV into the import box; confirm rows with non-zero `seconds` in `media/plays/prime-video/` + Media tab; re-import the same file and confirm no duplicates; **update the fixture and remove Needs-sample once a real export sample confirms the column names** |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Amazon
Prime Video (L3342–L3348). Feasibility 🟡 medium only because the request
is buried in Amazon's privacy portal — the data itself is good. Format can
vary slightly by region; the parser must be header-driven. Cross-cutting
note: shares the generic M1 drop-a-file import pipeline with Netflix, IMDb,
Letterboxd. For ongoing capture, Trakt browser scrobbling covers Prime
Video — mention it on the card.

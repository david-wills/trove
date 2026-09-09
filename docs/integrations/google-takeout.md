# Google Takeout (My Activity)

- **id:** `google-takeout`
- **domains:** `browser-searches` (contract: ✅ ratified by this build — the
  My Activity **Search** query log; **first collector of the domain**) ·
  `media/plays/` (YouTube watch events — **deferred**, not built in this
  pass; see "Deferred" below)
- **status:** 🧪 built (fixture-tested, not validated) — Search slice only
- **unavailable_reason:** none
- **behavior:** Import — user runs a Takeout export at takeout.google.com and
  drops the `.zip` (or the extracted `MyActivity.json`) on the import box;
  re-runnable (newer overlapping exports dedupe on `guid`, never duplicate);
  the hub card shows last-import age as the re-export nudge. Later upgrade
  path: Periodic via the Data Portability API.
- **connection:** none for the archive import. The Data Portability API
  upgrade would ride the existing `google` connection (OAuth; shared today
  by six google-* defs) — BYO-credentials only, since publishing those
  scopes requires Google's app-verification/security assessment.
- **evidence:** community-documented schema (the `google-takeout-parser`
  library documents the JSON shapes); Data Portability API officially
  documented at dataportability.googleapis.com
- **effort / priority:** M / P1
- **needs:** none

## What it is

Google's data-export archive, scoped here to **My Activity**: the
timestamped Google Search query log and the YouTube watch history. Other
Takeout slices (Photos, Location History, Contacts, …) are catalogued under
their own providers; this brief is the My Activity entry.

**As built (this pass):** only the **Search** slice. Each
`header: "Search"` activity whose `title` is `"Searched for <query>"` (or
whose `titleUrl` is a `/search?q=` URL, the locale-agnostic fallback) becomes
one `browser-searches` `Search` row under
`browser/searches/google-takeout/YYYY-MM.jsonl`, with the verbatim activity
object kept under `…/raw/`. This is the **first collector of the
`browser-searches` domain** (ratified by this build; `safari` writes sibling
`browser/` visits and coexists).

**Deferred:** the **YouTube watch history** slice (→ `media/plays/`). It is
the only route to watch history (unavailable via the Data API since 2017) and
remains the highest-value follow-up, but it is a separate `media-plays`
collector and was not built in this pass — `google-takeout` here owns the
**query stream**, not browsing or watching. The import deliberately ignores a
`watch-history.json` in the same archive.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| YouTube watch history | none | title ("Watched …" prefix), titleUrl (videoId), channel (subtitles[0].name), time (ISO 8601) | google-takeout-parser schema |
| Google Search history | none | query, time | google-takeout-parser schema |
| YouTube likes/subscriptions/playlists | none (selectable in the same export) | video/channel/playlist metadata | research notes |
| Programmatic My Activity pulls | BYO Google Cloud credentials | same data, async archive flow, time-filtered | official Data Portability docs |

All optional; users who export only one slice get only that slice's rows.

## Access & auth

- **M1 (build now):** takeout.google.com → select "YouTube and YouTube
  Music" (JSON format for `watch-history.json`) and "My Activity → Search"
  (JSON, timestamped queries; chunks named by date range). One-shot manual
  export; no OAuth scope involved.
- **Upgrade (spike later):** `POST
  dataportability.googleapis.com/v1/portabilityArchive:initiate` with
  resources `myactivity.search` / `myactivity.youtube` (scopes
  `auth/dataportability.myactivity.*`). Programmatic with user consent, but
  public distribution requires Google's verification + security
  assessment — so compiled-in only as a BYO-credentials option.
- No TCC, no local protected paths. Standalone-clean (archive import is
  fully offline).

## Vault mapping

Research-entry paths predate the taxonomy; the taxonomy table wins. Data
routes by shape:

- **Contract layer (BUILT):** Search queries normalize into the
  `browser-searches` `Search` contract under
  `browser/searches/google-takeout/YYYY-MM.jsonl` (`ts` = `time` → local
  RFC3339; `source` = `google-takeout`; `query` = decoded title/`q=`;
  `engine` = `google`; `url` = `titleUrl` HTTPS-upgraded; `guid` =
  `gt-search-<time>-<query>`; `header`/`products`/`locationInfos` in
  `extra`). Partitioned by the month of the local `ts`.
- **Raw layer (BUILT):** the verbatim search-activity object under
  `browser/searches/google-takeout/raw/YYYY-MM.jsonl` (full fidelity —
  every search record, including fields the contract drops).
- **Dedupe:** no native ids — `guid` = `(time, query)` hash. Re-imports of
  overlapping archives are idempotent: a guid already on disk is skipped
  before append (the letterboxd/readwise pattern).
- **Deferred layer:** `media/plays/google-takeout/` (watch events) and
  `media/google-takeout/` (likes/subscriptions/playlists curation) — not
  written by this pass.

## Build plan

**Done (this pass), Search slice:**

1. ✅ Module `crates/trove-core/src/google_takeout.rs`: `DEF` with
   `Behavior::Import(&IMPORT)` (accepts `zip`/`json`). Reads the Search
   `MyActivity.json` from a bare `.json` or by locating any
   `…/Search/MyActivity.json` inside the export `.zip`; ignores
   `watch-history.json` and non-Search records.
2. ✅ Struct `crates/trove-core/src/browser_searches.rs` (`Search`) +
   `DOMAINS` entry in `contracts.rs` + `pub mod`/`pub use` in `lib.rs` +
   `spec_validation` round-trip — **ratifies `browser-searches`**.
3. ✅ Registration: `&crate::google_takeout::DEF` already in `INTEGRATIONS`
   (Phase-2 stub); no new connection.
4. ✅ Fixtures + tests from Google's My Activity / `google-takeout-parser`
   shapes (canonical Search record, Visited, YouTube, localized-title
   fallback); zip + bare-json import, dedupe/idempotence, unique temp dirs.
5. Import UI is registry-driven (the generic import box); setup copy +
   one-shot/re-export caveat live on `DEF.meta`.

**Deferred (follow-up passes):**

6. YouTube watch history → `media-plays` collector (separate slice).
7. Data Portability API as a Periodic def on the shared `google`
   connection, BYO credentials, time-filtered incremental pulls.

## Validation matrix

Build axis: 🧪 fixture-tested (`google_takeout.rs` tests: record→`Search`
mapping incl. prefix-strip + `q=` fallback + HTTPS upgrade + stable guid;
Visited/YouTube/no-time/empty-query skips; bare-JSON and nested-zip imports;
re-run idempotence; zip-without-Search clear error — plus the
`spec_validation` round-trip binding the `browser-searches` contract).
Validate axis below is **real-data, David-only** (no synthetic substitute —
needs a real Takeout archive; no token/credential to paste, this is an
import).

| Capability | Build | Validate | How David validates (exact steps) |
|---|---|---|---|
| Search history → `browser-searches` | 🧪 | — | **No credential to paste — this is a file import.** (1) takeout.google.com → "Deselect all" → tick **My Activity** → under it click "All activity data included" and keep only **Search**, format **JSON** (not HTML). (2) Create export, download the `.zip`. (3) In Trove, open google-takeout's import box and drop the `.zip` as-is (or the `MyActivity.json` from inside `Takeout/My Activity/Search/`). (4) Confirm the result toast reads "N searches imported, 0 duplicates skipped"; confirm rows in `~/Documents/Trove/browser/searches/google-takeout/YYYY-MM.jsonl` (decoded `query`, `engine":"google"`) and the verbatim objects under `…/raw/`; confirm the hub card shows a recent last-data date. |
| Re-import idempotence | 🧪 | — | Drop the **same** `.zip` again → toast reads "0 searches imported, N duplicates skipped"; the `YYYY-MM.jsonl` files are byte-unchanged. |
| YouTube watch history → `media/plays` | — (deferred) | — | Not built this pass. When built: export "YouTube and YouTube Music" history as **JSON**, import, confirm `media/plays/google-takeout/` rows. |
| Data Portability API upgrade | — (deferred) | — | BYO Google Cloud project; `portabilityArchive:initiate` for `myactivity.search`; compare programmatic output against the archive import. |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Google
Takeout — YouTube Watch + Search History (L1528–L1535, 🟢 high) and §Google
Data Portability API — My Activity (L1536–L1543, 🟡 medium — verification
requirement is the blocker; BYOC sidesteps it). Watch history JSON must be
exported in **JSON format** (Takeout defaults HTML for some slices).
Overlap note: the shipped `google-youtube` def covers liked
videos/subscriptions via the Data API — watch *history* is Takeout-only;
enrichment of watch rows via the Data API is a read-time idea, not a write
path.

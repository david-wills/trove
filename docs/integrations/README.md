# Integration Briefs

One file per **provider** — the build spec an agent works from and the
living status record afterward. Generated in the pipeline's Phase 2 catalog
pass by distilling `docs/integrations-research.md` (which stays as the raw
research reference; briefs are the working layer). Doctrine and phase plan:
`docs/integration-pipeline.md`. Loop contract: `docs/collector-loop.md`.

## Files here

- `INDEX.md` — the queue: one row per provider (order, domains, status,
  needs-flags). **The build loop reads this top-down**; reordering the
  queue is editing this file. Generated in Phase 2.
- `_template.md` — the brief skeleton (copy it; keep every section, write
  "none" rather than deleting).
- `<provider>.md` — one brief per provider. `granola.md` is the worked
  example.

## Rules of the catalog

- **Combine by provider.** All of a provider's mechanisms (export + API +
  local DB) are one brief, one in-app entry. Many integration defs may
  share one connection (Google: six defs, one login).
- **Everything is catalogued**, including hard-blocked sources — those get
  `status: 🚫 unavailable` and an honest `unavailable_reason` that the app
  shows on the greyed card.
- **Statuses:** 🚫 unavailable · 📋 queued · 🚧 building · 🧪 built
  (fixture-tested, not validated) · ✅ validated (real-data confirmed —
  only David promotes). Validation is tracked per capability slice in the
  brief's matrix.
- **Evidence hierarchy** (record which level the brief rests on): official
  API docs with examples → community-documented schemas → real sample
  files. Folklore formats without a sample are built parser-last and
  flagged Needs-sample.

## Domain taxonomy (Phase 1, 2026-06-12)

The research doc's 17 domain catalogs mapped onto the vault's domain
folders. **Contract** means a shared multi-source record shape under
`docs/vault-spec/domains/` + JSON Schema; **raw-only** means each source
writes its own native shape in its own folder (the vault-wide conventions
still apply — guids, timestamps, partitions, handles). The four foundational
contracts (`correspondence`, `tasks`, `media-plays`, `calendar`) were ratified
first; the **✅ ratified** rows below are the Phase 3 batch, ratified 2026-06-14
and recorded in `docs/vault-spec/PHASE3-REVIEW.md`. "document" = the shape
already exists in code and the spec page documents it without redesigning it.

| Vault folder | Research catalog(s) | Converging sources | Contract |
|---|---|---|---|
| `correspondence/` | Email & Messaging; Calls (call rows) | email, every messenger, team chat, calls | ✅ ratified |
| `meetings/` | Calls, Voice & Meetings | Granola, Fathom, Zoom, Fireflies, Otter, Meet, Teams, … | ✅ ratified |
| `voice/` | Calls, Voice & Meetings | Voice Memos, Visual Voicemail, Google Voice **voicemails** (its calls + SMS route to `correspondence/`) | ✅ ratified (thin audio-item shape; converged) |
| `contacts/` | People & Relationship Graph | macOS Contacts, Google Contacts (already writes here), LinkedIn, vCard, personal CRMs | ✅ ratified |
| `health/` | Health: Wearables & Biometrics | Apple Health export, Oura, WHOOP, Withings, Fitbit, Dexcom, …; workout records (Strava, Garmin, Apple Health) route here **whole**, embedded GPS routes included | document (per-metric CSV + per-source raw, as built) |
| `health/nutrition/` | Health: Nutrition | Cronometer, MyFitnessPal, MacroFactor | ✅ ratified |
| `health/medical/` | Health: Medical Records & Labs | SMART-on-FHIR providers (Epic, Quest, Labcorp, …) | ✅ ratified (FHIR-shaped; one client, many providers) |
| `health/genetics/` | Health: Genetics | 23andMe, AncestryDNA (same TSV) | raw-only (shared parser) |
| `activity/` | Computer Activity | root day-files: the live watcher (single writer, by design); `activity/<source>/`: imported observed app/window-span histories (RescueTime, Timing, Qbserve, ActivityWatch imports) | none (owned stream); subfolders raw-only |
| `screen-time/` | Computer Activity | Biome streams | raw-only |
| `developer/` | Developer Activity | shell history, local git, GitHub, Claude Code transcripts | raw-only (heterogeneous shapes) |
| `browser/` | Web Activity | visits (with duration): Chrome, Safari, the extension; sibling stream `browser/ads/` (ad impressions, as built) | document (visit shape); siblings raw-only |
| `browser/searches/` | Web Activity | search queries: Takeout My Activity, Safari search history | ✅ ratified (query shape) |
| `reading/` | Web Activity; Books | Readwise, Instapaper, Raindrop, Pinboard, Kindle clippings | ✅ ratified |
| `home/` | Home, IoT & Smart Devices | HomeKit, Hue, Tempest, Enphase, Green Button, … | ✅ ratified |
| `environment/` | Environment & Ambient Context | AQI, quakes, alerts, sun/moon, aurora, wildfire feeds | ✅ ratified (existing `weather/` stays where it is, catalogued under this domain) |
| `location/` | Geolocation & Travel | standalone GPX/FIT imports, Arc/Timeline imports (workout-embedded routes live in `health/`, joined at read time) | ✅ ratified (trails shape now; visits waits for a visits-shaped source) |
| `travel/` | Geolocation & Travel | Flighty, TripIt, airline-email parses | ✅ ratified (trip-segment shape: flights, hotels, cars, trains) |
| `calendar/` | Calendar & Productivity | EventKit, Google Calendar, CalDAV | ✅ ratified |
| `tasks/` | Tasks & Productivity | Reminders, TickTick, Google Tasks, Todoist, Linear, … | ✅ ratified |
| `habits/` | Habits & Productivity | Habitica, Streaks, TickTick habits | ✅ ratified |
| `time-entries/` | Productivity (time tracking) | Toggl, Clockify — **user-asserted entries only**; observed trackers (RescueTime, Timing) land in `activity/<source>/` | ✅ ratified |
| `notes/` | Artifacts: Notes & Drafts | Apple Notes, Bear, Drafts, Day One, Obsidian, Logseq | ✅ ratified (`artifacts/` stays the user-curated layer; collected notes land here) |
| `files/` | Artifacts: Documents & Files | cloud-drive watchers, Spotlight recents | raw-only |
| `photos/` | Photos & Visual Media | Apple Photos, Takeout, EXIF import — metadata only, never image copies | ✅ ratified (photos-metadata) |
| `media/plays/` | Media: Music, Podcasts, Video, TV; Books (reads) | scrobblers, Trakt, Letterboxd, YouTube, … | ✅ ratified (library snapshots — `music/library/`, `podcasts/`, `books/`, `youtube/` — stay raw-only; existing paths grandfathered) |
| `gaming/` | Media: Gaming | Steam, Chess.com, Lichess, BGG | raw-only (heterogeneous; sessions may join media-plays at read time) |
| `finance/` | Financial, Spending & Purchases | SimpleFIN, statement CSVs, brokerage/crypto **trades**, chain reads | document (canonical ledger as built — the recorded write-time-dedupe exception; new substreams below follow the normal per-source rule) |
| `finance/purchases/` | Financial, Spending & Purchases | itemized orders/receipts: Amazon order history, email-receipt parses, loyalty-program exports | ✅ ratified (purchase line-item shape; per-source subfolders) |
| `finance/holdings/` | Financial, Spending & Purchases | brokerage/crypto **position snapshots** (Schwab, IBKR, Coinbase) — trades stay in the ledger | ✅ ratified (holdings-snapshot shape) |
| `social/` | Social Media & Web Presence | Bluesky, Mastodon, X/Reddit/Meta archives (posts) | ✅ ratified (social-posts; archive **DMs** route to `correspondence/`; likes/saves/follows/profile snapshots stay per-source raw under `social/<source>/` — saved posts never route to `reading/`) |

Routing rules the table implies:

- **This table wins.** Vault paths embedded in `docs/integrations-research.md`
  entries (`geo/…`, `social/dating/`, …) predate the taxonomy and are not
  authoritative. Phase 2 briefs take their paths from this table, never
  from the research doc.
- **Data routes by shape, not by provider.** One Takeout lands YouTube
  history in `media/plays/`, location in `location/`, contacts in
  `contacts/`. A provider's brief lists every folder it touches.
- **Records route whole.** Never split one record across folders: a
  workout with an embedded GPS route is one `health/` record — the
  location view joins it at read time. Distinct record *types* from one
  provider still split by shape (Google Voice: calls + SMS →
  `correspondence/`, voicemails → `voice/`).
- **Assigned-work shape → `tasks/`; platform activity stream →
  `developer/`.** GitHub issues assigned to you are tasks; the
  commit/PR/event firehose is developer activity.
- **`home/` vs `environment/`: owned device vs public feed.** A sensor
  the user owns (Tempest, Enphase) writes `home/`; public feeds (weather,
  AQI, quakes) write `environment/`. Same-shaped readings merge at read
  time.
- **Media curation is per-source raw.** Ratings, watchlists, playlists,
  and library lists from new sources land in `media/<source>/` alongside
  any `media/plays/<source>/` rows; only play events join the contract.
- **`activity/` root day-files stay single-writer** (the live watcher's
  owned stream). Imported observed-span histories write
  `activity/<source>/` subfolders, raw-only, like any other domain.
- **Privacy-sensitive is a first-class needs-flag** carried in the Phase 2
  INDEX: dating apps, genetics, clipboard history, mic dB sampling,
  voicemail transcripts, message bodies, financial detail, location
  trails ship opt-in with explicit acknowledgement; password-manager
  imports hard-strip secrets at parse time.
- **Grandfathered paths — the closed set** (pre-taxonomy folders; the
  path is the schema identifier and additive evolution forbids renames):
  `weather/`, `youtube/`, `books/` (Apple Books), `books/google/`,
  `music/library/`, `podcasts/`. The taxonomy governs *new* streams; a
  one-time consolidated tidy-up migration of exactly this set is
  scheduled **post-wave** (see the pipeline doc's After-the-wave
  section); until then the set must not grow — this list is the audit
  baseline.
- Per-source raw folders under a contract domain
  (`<domain>/<source>/`) are always allowed alongside the contract rows —
  full fidelity first, normalization second.
- **Out of scope for now:** own-website analytics (GA4, Plausible) and
  web-presence assets — catalogued as out-of-scope, no folder assigned.
- **Known catalog gap:** education/learning sources (Anki, Duolingo,
  course platforms) appear in none of the 17 research catalogs. Catalogue
  them in a later pass; route by shape when they arrive (likely
  activity-adjacent).

## Hub UX at catalog scale (decided Phase 1)

~100 providers can't live in one flat "Apps" list. When the Phase 2 stubs
land: the master list groups by **taxonomy domain** (collapsible sections,
Collectors staying first), a search/filter box sits above the list, and
unavailable entries render dim, sort last within their domain, and are
**visible by default** — the catalog's whole point is answering "why isn't
X available?" in-app — with a "hide unavailable" filter for users who want
the working set. `NotWired` stubs get a "planned" badge so built vs.
catalogued is legible at a glance. Phase 1 ships the unavailable-card
rendering (greyed + reason, from `Behavior::Unavailable`); Phase 2 ships
grouping + search alongside the stubs.

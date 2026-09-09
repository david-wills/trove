# BoardGameGeek

- **id:** `boardgamegeek`
- **domains:** `gaming/` (raw-only per the taxonomy — heterogeneous shapes;
  sessions may join `media/plays/` at read time, no write-time contract)
- **status:** 🧪 built (fixture-tested; XML API2 plays + collection; keyless; needs a public BGG username to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (poll plays + collection; date cursor)
- **connection:** none — keyless public XML API; the user supplies their
  public BGG username (a def setting, not a credential). Private-profile
  cookie auth deliberately skipped.
- **evidence:** official-docs — stable BGG XML API2 at
  boardgamegeek.com/xmlapi2, extensively community-documented
- **effort / priority:** S / P1
- **needs:** none

## What it is

The canonical board-game database and community. Its logged *plays* are a
unique stream — date, game, duration, players, location, comments — that no
other service captures: tabletop sessions are otherwise invisible to any
collector. Collection data (owned/wishlist/ratings) adds a library snapshot.
High value per unit effort for anyone who logs plays.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Logged plays | public profiles (free) | date, game id/name, quantity, duration, players, location, comments | official docs |
| Collection | public profiles (free) | owned/wishlist/played status, user rating, num plays | official docs |
| Private data | private profiles only | same shapes | skipped — needs session cookie |

All optional in the raw shape; a user who never logs plays still gets the
collection snapshot. Private profiles are out of scope (cookie auth not
worth the complexity; the connect card should say "profile must be public").

## Access & auth

- XML API2, keyless for public data: `/xmlapi2/plays?username=X&page=N`
  (play sessions) and `/xmlapi2/collection?username=X` (collection; note the
  API's 202-then-retry queueing behavior on collection requests).
- Throttle ~2 req/sec to be safe; paginate plays, use `mindate` for
  incremental pulls.
- Parse XML with the `quick-xml` crate. No TCC, no local files.
  Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `gaming/boardgamegeek/plays/YYYY-MM.jsonl` — one row per
  logged play (`ts` = play date, `guid` = BGG play id, game id/name,
  quantity, duration_mins, players[], location, comment), partitioned by
  play month; `gaming/boardgamegeek/collection.jsonl` — latest collection
  snapshot (ratings, statuses).
- **Contract layer:** none at write time — `gaming/` is raw-only. Plays are
  session-shaped and may join a read-time plays view later.
- **Dedupe:** BGG play id as `guid`; `mindate` watermark in
  `.trove/boardgamegeek-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/boardgamegeek.rs`: `DEF` (Periodic,
   daily-ish), username setting, `pull` hook (plays pagination + collection
   snapshot, honoring the 202-retry on collection).
2. Registration line in `INTEGRATIONS` (no `CONNECTIONS` entry — keyless).
3. Add `quick-xml` parsing; fixtures from documented XML response shapes
   (plays page, collection, 202 queue response); parser + store + cursor
   tests, unique temp dirs.
4. Connect card copy: public username input + "profile must be public"
   hint (disabled-affordance rule).

## Build status — 🧪 2026-06-15

Shipped (`boardgamegeek.rs`, INDEX #19). `Behavior::Periodic` (daily-ish), keyless
public **XML API2** (`quick-xml` parse + the `ureq` HTTP pattern, ~2 req/s
politeness throttle). `gaming/` is **raw-only — no contract binding** (like
discord's gaming/ + garmin's health/). **Default-on** (public play data, not
sensitive). **Connection:** a **keyless `ConnectMethod::TokenPaste`** holding the
public BGG username (the listenbrainz precedent — *not* a "def setting"; there is
no generic non-connection input; `configured: true` always, verified on connect
with a 1-page plays probe → a bad username surfaces a clear error). Connect copy:
"profile must be public."

- **Plays** `GET /xmlapi2/plays?username=X&page=N&mindate=…` → one row per logged
  play (guid = BGG **play id**, full fidelity: date/quantity/`length` [**minutes**]/
  location/incomplete/`nowinstats` + `<item>` game + `players[]` + comment) →
  `gaming/boardgamegeek/plays/YYYY-MM.jsonl` (partition by play-date month),
  upsert-by-guid. **Pagination pages until an empty OR short page — never trusts
  `<plays total=>`, which BGG reports as `0` for plays** (long-documented quirk);
  `mindate` (the latest play date, inclusive) is held constant across pages and
  the watermark advances only after the full drain (re-fetch boundary day → guid
  dedup). Runaway cap 1000 pages.
- **Collection** `GET /xmlapi2/collection?username=X` → `gaming/boardgamegeek/
  collection.jsonl` (whole-snapshot rewrite each pull; one row per item, full
  fidelity incl. originalname/wishlistcomment/want-&has-partslist + privateinfo if
  present). Handles the **HTTP 202 "queued" → retry** (back off 3s, ≤5 tries; if
  still queued, keep the plays + skip collection this pull, no error, retry next
  tick — never blanks a good `collection.jsonl`).
- Watermark (`mindate` + last-collection-sync time) in
  `.trove/boardgamegeek-sync.json` (non-secret); the username is stored 0600 via
  the connection store, never in the cursor/logs.

Evidence: XML shapes confirmed against the `tnaskali/bgg-api` captured responses
+ XSD schemas, `lcosmin/boardgamegeek`, and the wiki (official wiki + direct API
were Cloudflare-blocked from the build egress). Two priors were corrected against
real evidence: the play attr is **`nowinstats`** (not nowinrecord) and the
collection root counts via **`totalitems`** (not total).

Adversarial-verify: **1 BLOCKING + 1 minor, both fixed** — (B) pagination
terminated on `<plays total=>`, which BGG reports as `0`, so a >100-play backfill
stranded everything past page 1 + advanced the watermark past it (permanent loss);
fixed to page-until-empty/short (+ regression tests for `total=0`/short/empty-page
traces). (m) the collection parse dropped documented child fields → now a generic
capture of every child element (full fidelity). guid stability, the 202 give-up,
entity handling (incl. unknown entities preserved), network isolation, and the
watermark mechanics were all independently confirmed correct.

Gate (my run, serial): trove-core 605/0 (+ BGG tests), `cargo check` clean,
`schedule_doc` regenerated (boardgamegeek Periodic), `bindings.ts` up to date.

**Deferred:** private-profile cookie auth (out of scope per the brief — public
profiles only); collection `version=1`/subtype filters (lean default).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Logged plays | 🧪 (needs a public username) | enter a real public BGG username in the connect card; Sync now; confirm play rows in `gaming/boardgamegeek/plays/` match the profile's plays page (incl. a >100-play account paginating fully) |
| Collection | 🧪 (needs a public username) | same run; confirm `gaming/boardgamegeek/collection.jsonl` count matches the BGG collection page |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming"
§BoardGameGeek (L3567–L3573). Feasibility 🟢 high — stable, keyless,
official. Research recommends "build now": unique data (no other service
logs board-game sessions), well-defined scope. The only gotcha is the XML
API's queued-collection 202 behavior and politeness throttling. Private
collections deliberately skipped — most loggers have public profiles.

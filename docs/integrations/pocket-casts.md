# Pocket Casts

- **id:** `pocket-casts`
- **domains:** `media/plays/` (contract: **media-plays, ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the unofficial history endpoint; the 100-item
  window makes polling cadence matter — start early, poll regularly)
- **connection:** `pocket-casts` — TokenPaste-shaped login (account
  email/password POSTed to the unofficial API; no public auth docs, no
  OAuth). Not shared with other defs.
- **evidence:** community-schema, medium confidence — unofficial
  `api.pocketcasts.com/user/history` works per 2025 community posts (it's
  what the web app uses); no official API or export; export feature request
  open since 2020 (#654)
- **effort / priority:** M / P2
- **needs:** Needs-login (no public auth docs — request/response shapes
  must be verified against a real account before the parser is trusted)

## What it is

A major cross-platform podcast app (owned by Automattic). Listening data is
locked behind an unofficial API that returns only the ~100 most recent
history items with played status but **no timestamp of when played** — the
weakest of the podcast sources, but the only path for its users. Worth
building because those users have no alternative; worth building *later*
because Overcast and Apple Podcasts return more for less risk.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Recent history (≤100 items) | all accounts | episode title, podcast title, played status | community posts (2025) |
| Played timestamps | — | not available from the endpoint | research doc |
| Full archive / export | — | none exists (Android-only DB backup; iOS nothing) | research doc |

All optional in the contract; absent timestamps are handled honestly (see
mapping), never invented.

## Access & auth

- Unofficial: `POST https://api.pocketcasts.com/user/history` with
  email/password JSON body → recent episodes (≤100). Reverse-engineered
  from the web app; may change without notice.
- Credentials via the connect card; treat any auth/shape failure as a
  visible hub error with honest copy (unofficial API, may break).
- No TCC, plain HTTPS, standalone-clean.

## Vault mapping

- **Raw layer:** `media/plays/pocket-casts/raw/` — the API history items as
  fetched, full fidelity.
- **Contract layer:** `media/plays/pocket-casts/YYYY-MM.jsonl` — the
  endpoint gives no play time, so `ts` = first-observed time (the poll that
  first saw the item), `category:"podcast"`, `kind` = `"play"` if played
  else `"partial"`, `title` = episode, `subtitle` = podcast, `seconds:0`
  (unknown — honest zeros per the contract), provenance note in `extra`
  (`{"ts_basis":"first_seen"}`).
- **Dedupe:** episode identifier from the response as `guid`; seen-set
  state in `.trove/`, rebuildable by scanning output files.

## Build plan

1. **Pre-build recheck (mandatory, per catalog note):** check whether
   Automattic shipped an official export (issue #654) — an official path
   would change the design.
2. Spike against a real account (Needs-login): capture exact request/
   response shapes; only then freeze the parser.
3. Module `crates/trove-core/src/pocket_casts.rs`: `DEF` (Periodic),
   `CONNECTION` (email/password), history poll + first-seen watermark.
4. Registration lines in `INTEGRATIONS` + `CONNECTIONS`; fixtures from the
   spike's captured responses; dedupe + ts-basis tests; unique temp dirs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Login + history poll | — | connect a real Pocket Casts account; Sync now; confirm raw items + hub last-data |
| Play rows + dedupe | — | play an episode in the app; next poll emits one row; subsequent polls emit nothing new |
| Breakage handling | — | wrong password / simulated 4xx → clear hub error stating the API is unofficial |

## Build notes (2026-06-16)

- Endpoint confirmed: `POST https://api.pocketcasts.com/user/history` with `Authorization: Bearer <token>`.
- Login endpoint: `POST https://api.pocketcasts.com/user/login` with `{"email":"…","password":"…","scope":"webplayer"}` returns `{"token":"…"}`.
- Auth design: user pastes `email:password` composite; run fn exchanges for bearer token, stores only the token (never the raw credentials).
- Response shape: `{"episodes":[...]}`. Episode fields on the new
  `api.pocketcasts.com` host (camelCase, per B-Lach Swift model and live
  DevTools captures): `uuid`, `title`, `podcastTitle` (flat string, show name),
  `podcastUuid`, `playingStatus` (0=unplayed, 2=in_progress, 3=played),
  `playedUpTo`, `duration`, `url`, `published` (date string, format unverified),
  `starred`. Old `play.pocketcasts.com` host used snake_case equivalents
  (`playing_status`, `played_up_to`, `published_at`); both shapes handled
  defensively so either host works.
- `playingStatus=3` → `kind="play"`, others → `kind="partial"` (honest — no
  played timestamp available from API).
- `ts` = first-seen RFC3339 (poll time); `extra.ts_basis="first_seen"` documents
  this honestly.
- Seen-set cursor at `.trove/pocket-casts-seen.json` (non-secret, rebuildable by
  scanning raw layer). Guid inserted into seen-set only after a successful parse
  so a parse failure leaves the item retryable.
- 10 tests all pass. No new Cargo deps needed. No shared contract files touched.
- Needs-login flag retained: field names inferred from open-source clients and
  Android/Swift app sources; no real account available to confirm exact shape.
  Parser reads both new-host camelCase and old-host snake_case defensively.

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§Pocket Casts (L3262–L3268) + the cross-cutting podcast-depth note
(Overcast > Apple Podcasts > Pocket Casts). Feasibility 🟡 medium —
fragile, shallow (100 items, no timestamps). **Time-sensitive:** history
beyond the 100-item window is unrecoverable, so users gain by connecting
early even though the source is low-priority to build.

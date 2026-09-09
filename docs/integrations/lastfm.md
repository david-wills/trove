# Last.fm

- **id:** `lastfm`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified**)
- **status:** 🧪 built 2026-06-14 (fixture-tested; live validation pending an
  API key — see needs)
- **unavailable_reason:** none
- **behavior:** Periodic (hourly; watermark poll on `from=`; one-time paginated
  backfill on first sync)
- **connection:** `lastfm` — TokenPaste. **As built:** the pasted field is the
  **public username**; the `api_key` is read from `TROVE_LASTFM_API_KEY` (env)
  with an empty compiled-in default (baked+BYO precedent — `ConnectMethod::
  TokenPaste` is single-field, and every API call requires a key, so the key is
  app-level/env and the user supplies only their username). No user OAuth for
  read-only public-profile history. Private profiles (session key) are out of
  scope for v1. Not shared with other defs.
- **evidence:** official-docs — ws.audioscrobbler.com REST API,
  `user.getRecentTracks` (`format=json&extended=1`); stable and unchanged for
  years. Fixture from a real extended response (now-playing row included to
  prove it's skipped).
- **effort / priority:** S / P0
- **needs:** **Needs-login** for validation. The brief's original "none" was
  optimistic — there is **no keyless path**; every ws.audioscrobbler.com call
  requires an `api_key`. Build is complete and fixture-green; live
  backfill/poll waits on a free key (register at last.fm/api/account/create,
  `export TROVE_LASTFM_API_KEY=…` — see the journal Needs-David queue).
  Time-sensitive: it's a live aggregation hub, so sooner-polling beats relying
  on backfill alone.

## What it is

The original music scrobbling service: any player that scrobbles (Spotify
plugin, the Last.fm apps, Navidrome, Tidal's built-in scrobbler, Trove's own
future writes) lands plays in one unified per-user stream going back to
account creation. The highest-value music source in the media domain — one
integration captures listening from every service the user has ever pointed
at it. Universal-aggregator pattern; Trakt is its TV/film twin.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full scrobble history | free; public profile (or session key for private) | artist, track, album, unix timestamp per play | official docs |
| Aggregates (top tracks/artists, weekly charts) | free | `user.getTopTracks`, `user.getWeeklyTrackChart` | official docs |

All optional in the contract. Aggregates are derivable from the raw stream —
Trove computes its own charts at read time, so they are not pulled.

## Access & auth

- REST: `GET https://ws.audioscrobbler.com/2.0/?method=user.getRecentTracks
  &user=USERNAME&api_key=KEY&limit=200&page=N&from=UNIX_TS`.
- API key is free and self-registered; no user-level OAuth for reads of
  public profiles. Private-profile reads need an `sk` session token —
  out of scope for v1 (document the limitation on the connect card).
- Rate limits: 5 req/s sustained over a 5-min window; 200 tracks/page. A
  >100k-scrobble history is many pages but fine with watermark polling.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `media/plays/lastfm/raw/YYYY-MM.jsonl` — API track objects,
  full fidelity, partitioned by play month.
- **Contract layer:** `media/plays/lastfm/YYYY-MM.jsonl` per media-plays:
  `ts` from the scrobble's unix timestamp, `category:"music"`,
  `kind:"play"`, `title` = track, `subtitle` = artist (the chart grouping
  key), `detail` = album, `seconds: 0` (Last.fm records events, not
  durations — honest unknowns beat invented numbers), MBIDs and loved-flag
  in `extra`.
- **Dedupe:** `guid` = `lastfm-<uts>-<artist>-<track>` slug (the API has no
  per-scrobble id; timestamp+track is unique in practice). Watermark cursor
  in `.trove/lastfm-sync.json`, rebuildable from output files.
- **Overlap note:** users who scrobble Apple Music to Last.fm AND run
  Trove's built-in scrobbler get the same play from two sources — sources
  stay separate per the contract; read-time merge handles it.

## Build plan

1. Module `crates/trove-core/src/lastfm.rs`: `DEF` (Periodic, ~15-min
   poll), `CONNECTION` (TokenPaste: API key + username fields, setup copy
   pointing at last.fm/api/account/create), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Backfill: page from `from=0` to now on first sync (respect 5 req/s);
   then incremental `from=<watermark>`.
4. Fixtures from documented example responses (incl. the `nowplaying`
   attribute row, which must be skipped — it has no `uts`); parser +
   store + cursor tests, unique temp dirs.
5. ListenBrainz shares this exact poll pattern — build it immediately
   after to amortize the design.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Backfill | 🧪 | (1) `export TROVE_LASTFM_API_KEY=<free key from last.fm/api/account/create>`; (2) in the app, Connect → Last.fm → paste a public **username** (the key comes from env, you do NOT paste it); (3) toggle on + Sync now; (4) confirm `~/Documents/Trove/media/plays/lastfm/YYYY-MM.jsonl` (+ `raw/`) month-partitioned back to account creation, row count ≈ the profile's lifetime scrobble total |
| Incremental poll | 🧪 | scrobble one track from any client; wait a poll cycle (hourly) or Sync now; confirm exactly one new row; re-run Sync and confirm no duplicate (guid dedupe) |

## Build notes (as-built, 2026-06-14)

- **Contract type is `MediaItem`** (not the `Play` struct the build plan named —
  `Play` is Apple-Music-only in `music.rs`). Rows written directly via
  `vault.stream("media/plays/lastfm", Partition::Month).append(&items, |i| &i.ts)`,
  the `letterboxd.rs` idiom. media-plays was already Rust-bound, so no
  struct/`DOMAINS`/`spec_validation` change.
- **Dedupe is the collector's job** (the store `append` does not dedupe): existing
  on-disk `guid`s are read into a set and already-stored guids skipped before
  append, so re-runs are byte-identical.
- **`ts`** = `date.uts` (UTC seconds) → RFC3339 with the machine's local offset
  (`DateTime::from_timestamp(uts,0).with_timezone(&Local)`), the standard vault
  convention; the human `date.#text` is never parsed. `seconds` is always `0`
  (Last.fm records events, not durations).
- **Username** is stored in the `access_token` slot of the 0600 `.trove/sync/lastfm.json`
  TokenSet (Oura idiom; no dedicated username field). Watermark cursor is the
  separate rebuildable `.trove/lastfm-sync.json` (max `uts` written, forward-only).
- **Adversarial verify:** 9/9 attack vectors PASS (mbid source-key mapping,
  artist name/`#text` fallback, uts-not-`#text` timestamp, now-playing skip for
  both layers, guid stability, play-month partition, no silent defaults,
  single-object-vs-array, string pagination parsing) — no defects.

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Last.fm
(L3198–L3204). Feasibility 🟢 high; flagged "build now — highest-value
music source in this domain". Pairs naturally with the shipped
`music-scrobbler` def. Keyless-for-public-profiles makes this one of the
cheapest P0s in the catalog.

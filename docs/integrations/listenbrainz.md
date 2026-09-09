# ListenBrainz

- **id:** `listenbrainz`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified**)
- **status:** 🧪 built 2026-06-14 (fixture-tested; keyless — validation just
  needs a public username)
- **unavailable_reason:** none
- **behavior:** Periodic (hourly; one unified loop pages `max_ts` downward —
  full backfill on first run, draining the whole gap down to the watermark on
  incremental runs)
- **connection:** `listenbrainz` — TokenPaste. **As built (brief OVERRIDDEN):**
  the brief said "connection: none / store the username via a setup field," but
  `setup` is static instructional copy and there is **no generic non-connection
  input mechanism** — the only registry-driven way to capture a typed value is a
  `ConnectionDef`, and a bespoke config screen (weather-style) is forbidden by
  the "no per-provider UI" rule. So the username is captured via a single
  `TokenPaste` field (the lastfm pattern) — but **keyless**: ListenBrainz public
  reads need no token/api_key at all. `configured` is always true. Connect
  verifies the username with a one-listen probe (404 → reject). Not shared.
- **evidence:** official-docs — listenbrainz.readthedocs.io core API,
  `GET /1/user/{username}/listens`; pagination semantics (count max 1000, `min_ts`
  strictly-after / `max_ts` strictly-before, UTC unix seconds, newest-first)
  confirmed from the docs. Read shape (`payload.listens[]` with nested
  `track_metadata.mbid_mapping` on matched listens) per the brief.
- **effort / priority:** S / P0
- **needs:** none. **Keyless** — no API key, no account registration, no sample.
  Live validation only needs someone to type a public ListenBrainz username and
  Sync. Time-sensitive: aggregation hub, same argument as Last.fm.

## What it is

The open-source scrobbling service from MetaBrainz (the MusicBrainz
people): the community-governed alternative to Last.fm, with MusicBrainz
recording/artist/release GUIDs attached to matched listens — the cleanest
entity identifiers in the music slice. Navidrome, Beets, and most modern
scrobblers submit to it natively. No commercial restrictions, no shutdown
risk profile of a VC service. For self-hosters it is often the *only*
aggregator in use.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full listen history | free, public profile | listened_at, track, artist, release + MusicBrainz GUIDs when matched | official OpenAPI spec |

All optional in the contract; unmatched listens simply carry no MBIDs.

## Access & auth

- REST: `GET https://api.listenbrainz.org/1/user/{username}/listens
  ?min_ts=UNIX&max_ts=UNIX&count=100`. No auth for public-profile reads.
- Rate limiting is advertised via response headers (no published hard
  caps) — honor the headers in the poll loop.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `media/plays/listenbrainz/raw/YYYY-MM.jsonl` — the listen
  objects with full `track_metadata`, partitioned by listen month.
- **Contract layer:** `media/plays/listenbrainz/YYYY-MM.jsonl` per
  media-plays: `ts` from `listened_at`, `category:"music"`, `kind:"play"`,
  `title` = track, `subtitle` = artist, `detail` = release, `seconds: 0`
  (listens are events, not spans), MusicBrainz recording/artist/release
  GUIDs and the submitting client in `extra`.
- **Dedupe:** `guid` = `lb-<listened_at>-<recording_msid>` (every listen
  carries a MessyBrainz/recording MSID). Watermark cursor in
  `.trove/listenbrainz-sync.json`, rebuildable.
- **Overlap note:** users who mirror scrobbles to both Last.fm and
  ListenBrainz will hold the same plays under two sources — by design;
  sources stay separate and the read-time merge handles it.

## Build plan

1. Module `crates/trove-core/src/listenbrainz.rs`: `DEF` (Periodic,
   ~15-min poll), username setup field, `pull` hook. No `CONNECTION`.
2. Registration line in `INTEGRATIONS`.
3. Backfill by paging `max_ts` downward from now, then incremental
   `min_ts=<watermark>` — deliberately the same loop shape as `lastfm.rs`;
   build immediately after Last.fm and share any extracted poll helper.
4. Fixtures from the OpenAPI spec examples (MBID-matched AND unmatched
   listens — per the sparse-fixture rule); parser + store + cursor tests,
   unique temp dirs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Backfill | 🧪 | Connect → ListenBrainz → enter any public username (no key needed); toggle on + Sync now; confirm `~/Documents/Trove/media/plays/listenbrainz/YYYY-MM.jsonl` (+ `raw/`) month-partitioned, row count ≈ the profile's listen count |
| Incremental poll | 🧪 | submit a listen from any client; wait a poll cycle (hourly) or Sync now; exactly one new row; re-run Sync → no duplicate (guid dedupe). For a large gap: let >100 listens accumulate, Sync once, confirm ALL land (the gap-drain fix) |

## Build notes (as-built, 2026-06-14)

- **Cloned `lastfm.rs`** (same media-plays mapping, guid/raw/cursor idioms,
  injectable HTTP client, `MediaItem` via `vault.stream(…).append`). media-plays
  already Rust-bound → no struct/`DOMAINS`/`spec_validation` change.
- **Field mapping:** `ts`←`listened_at` (UTC seconds → RFC3339 local, same helper
  as lastfm); `title`←`track_name`, `subtitle`←`artist_name`, `detail`←`release_name`;
  `guid`=`lb-<listened_at>-<recording_msid>` (recording_msid always present).
  `extra` (omit-empty): `recording_mbid`/`artist_mbids`/`release_mbid` from the
  **nested** `track_metadata.mbid_mapping` (matched listens only), `recording_msid`,
  `submission_client`. `artist_mbids` array is comma-joined for the flat `extra`
  string; the verbatim array is kept in the raw layer. `seconds`=0.
- **One unified pull loop** pages `max_ts` downward: first run drains to 0 (full
  backfill); incremental runs drain the whole gap down to the watermark before
  advancing it — fixing a found data-loss defect where a single newest-first page
  per tick stranded listens in a >100-listen gap. Watermark (`max listened_at`,
  forward-only) in `.trove/listenbrainz-sync.json`; username in `.trove/sync/listenbrainz.json`.
- **Adversarial verify:** 1 blocking (the gap data-loss, now fixed + regression-tested)
  + 1 minor (stale comment, fixed); all other vectors (mbid nesting/source keys,
  uts unit, guid stability, unmatched-listen omission, partition month, field
  swaps, backfill termination, array handling, empty payload) PASS.

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§ListenBrainz (L3222–L3228). Feasibility 🟢 high; "build now (alongside
Last.fm)". Small incremental effort given the shared pattern. The MBIDs it
carries are the best hook for future entity resolution across music
sources. Private profiles exist but are rare; v1 documents the
public-profile requirement on the card.

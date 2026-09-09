# Overcast

- **id:** `overcast`
- **domains:** `media/plays/` (contract: **media-plays, ratified**;
  raw OPML snapshots in the per-source raw layer alongside)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (daily-ish OPML fetch; ~10 req/day server limit)
- **connection:** `overcast` — TokenPaste-shaped login (overcast.fm
  email/password → session cookie; no OAuth exists). Not shared with other
  defs.
- **evidence:** community-schema, high confidence — the extended OPML
  endpoint (`overcast.fm/account/export_opml/extended`) is unofficial but
  years-stable and well documented (overcast-to-sqlite parses it); rate
  limit ~10/day stated by Marco Arment
- **effort / priority:** M / P2
- **needs:** none

## What it is

The leading iOS podcast app for power users — and per the research doc's
podcast hierarchy, the **best podcast listening history available from any
app**: per-episode played status with timestamps and progress, which Apple
Podcasts (last-played + count only) and Pocket Casts (100 items, no
timestamps) can't match. Worth an in-app suggestion: non-Overcast users
can adopt its free tier just to get exportable history.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Per-episode play state | all accounts (free incl.) | `overcast:played` (0/1), `overcast:progress` (secs), `overcast:userUpdatedDate`-style timestamps, `overcast:addedDate` | community-schema (overcast-to-sqlite) |
| Episode/feed metadata | all accounts | episode title, GUID, enclosure URL, feed title/URL | same OPML |
| Subscriptions + playlists | all accounts | feed list; playlist membership ("all data" OPML) | research doc |

No total-listen-time or playback-speed fields exist. All optional in the
contract; `seconds` comes from progress, honest `0` when absent.

## Access & auth

- `POST overcast.fm/login` with email/password → session cookie; then
  `GET overcast.fm/account/export_opml/extended` (XML, custom `overcast:`
  namespace attributes). Standard OPML (subscriptions only) is a separate
  keyless URL but carries no play data.
- Rate limit ~10 requests/day — poll once or twice daily, back off hard on
  429/5xx; never retry-loop.
- Credentials via the connect card; store the session cookie, re-login on
  expiry. No TCC, plain HTTPS, standalone-clean.

## Vault mapping

- **Raw layer:** `media/plays/overcast/raw/` — dated OPML snapshots (or a
  parsed full-fidelity JSONL of the latest fetch), full attribute set.
- **Contract layer:** `media/plays/overcast/YYYY-MM.jsonl` — one row per
  newly-played episode: `ts` = the played/updated timestamp,
  `category:"podcast"`, `kind` = `"play"` when played=1 else `"partial"`,
  `title` = episode, `subtitle` = feed title, `seconds` = progress,
  `detail` = enclosure/episode URL, `extra` = addedDate etc.
- **Dedupe:** episode GUID (falling back to enclosure URL) as `guid`;
  snapshot diffing against the previous fetch emits only new/changed play
  states — cursor state in `.trove/`, rebuildable from output.

## Build plan

1. Module `crates/trove-core/src/overcast.rs`: `DEF` (Periodic, daily),
   `CONNECTION` (email/password fields with the disabled-affordance rule),
   login + fetch, OPML parser (namespace attrs), snapshot-diff emitter.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures: synthetic extended-OPML built from the community-documented
   attribute set (overcast-to-sqlite as the shape reference); tests for
   parse, diff-emit, dedupe, and rate-limit backoff; unique temp dirs.
4. Unofficial endpoint: degrade gracefully — on auth/shape breakage show a
   clear hub error, never silent failure.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Login + extended OPML fetch | built — Needs-login | connect with a real overcast.fm account; Sync now; confirm raw snapshot + hub last-data |
| Play rows | built — Needs-login | finish an episode in Overcast; next poll emits a `kind:"play"` row with progress seconds in `media/plays/overcast/` |
| Rate-limit behavior | built (handled) | 429 surfaced as a clear error, no retry loop; daily cadence stays under the ~10/day limit |
| Silent baseline on first sync | built + tested | first pull emits 0 contract rows, subsequent diffs emit only changed play states |

## Build notes

- Module: `crates/trove-core/src/overcast.rs`; 13 unit tests, all green.
- Snapshot-diff: cursor at `.trove/overcast-sync.json` records per-episode `played`+`progress` state. First fetch is a silent baseline (raw written, zero contract rows). Subsequent fetches diff and emit only changed episodes with play data.
- Auth: email:password TokenPaste → POST `overcast.fm/login` → `o` session cookie extracted from `Set-Cookie` response header. Cookie stored in `.trove/sync/overcast.json` (0600). Never persists credentials.
- OPML parser: quick-xml, handles both `overcast:` namespaced attributes and bare attribute names. Strips namespace prefix from attribute keys.
- Contract fields: `ts` = `userUpdatedDate` (→ `pubDate` → fetch time); `category="podcast"`, `kind="play"` (played=1) / `"partial"` (progress>0); `seconds` = progress; `subtitle` = feed title; `detail` = enclosure URL.
- New `CONNECTION` added to CONNECTIONS registry in `integrations.rs`.

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Overcast
(L3254–L3260) + the cross-cutting "podcast depth ordering" note
(Overcast > Apple Podcasts > Pocket Casts > Castro). Feasibility 🟢 high
despite being unofficial — stable for years. Sequence after the shipped
Apple Podcasts collector; it strictly upgrades history depth for users who
have it.

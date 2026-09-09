# Jellyfin

- **id:** `jellyfin`
- **domains:** `media/plays/` (contract: ✅ ratified — watch events) with
  per-source raw at `media/plays/jellyfin/raw/`
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the local server's REST API)
- **connection:** `jellyfin` — TokenPaste (local API key generated in the
  Jellyfin admin dashboard; plus the server URL, default
  `http://localhost:8096`). Not shared with other defs.
- **evidence:** official docs — Swagger at
  `localhost:8096/api-docs/swagger/index.html`; `IsPlayed` filter and
  Playback Reporting plugin endpoints documented
- **effort / priority:** M / P2
- **needs:** none

## What it is

Free, open-source self-hosted media server — the community alternative to
Plex, growing on macOS. For users who run one, it holds a first-party
record of their movie/TV/music watching. Self-hosters are technical and
exactly the local-first audience Trove courts. Built alongside Plex (same
audience, same extraction pattern).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Watched items + resume state | base install | item metadata, played flag, resume position, DateCreated | official Swagger |
| Per-session play history | Playback Reporting **plugin** (optional install) | timestamped sessions via `/user_usage_stats/` | official plugin docs |
| Server activity log | base install | server events incl. plays via `/System/ActivityLog/Entries` | official Swagger |

All optional in the contract; without the plugin the rows carry
watched-state timestamps only, never a hard failure. No plugin-specific
code paths beyond probing the endpoint and degrading with a UI hint.

## Access & auth

- REST: `GET http://{server}/Users/{userId}/Items?Filters=IsPlayed&SortBy=DateCreated`
  with an API key from the admin dashboard. Default port 8096.
- Prefer the API over SQLite: the DB path varies by install method
  (`~/.config/jellyfin/`, `/opt/homebrew/var/jellyfin/`, Docker volumes…),
  so a direct DB read is unreliable — the opposite call from Plex, where
  the path is fixed.
- No TCC. Standalone rule: Trove talks to the user's own already-running
  server — it never installs or requires Jellyfin; the card stays a normal
  connect-gated entry (greyed Connect with affordance hint until a server
  URL + key are pasted).
- Localhost HTTP to the user's own server is consistent with
  local-and-private (no third-party cloud).

## Vault mapping

- **Raw layer:** `media/plays/jellyfin/raw/` — native API item/session
  JSON, full fidelity.
- **Contract layer:** `media/plays/jellyfin/YYYY-MM.jsonl` per the ratified
  media-plays contract: `ts` (session timestamp when the plugin exists,
  else the watched-state date), kind = movie/episode/track from item type,
  title + series/season detail, resume position in `extra`.
- **Dedupe:** `guid` = hash(server id, item id, ts); plugin session rows
  and base watched rows for the same item dedupe by item id + timestamp.
  Watermark cursor in `.trove/jellyfin-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/jellyfin.rs`: `DEF` (Periodic) +
   `CONNECTION` (TokenPaste: server URL + API key fields, setup copy on
   the def per the SimpleFIN affordance rule), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Probe for the Playback Reporting plugin at connect time; store which
   capability tier the server offers; degrade gracefully.
4. Fixtures from the Swagger response shapes (items with/without plugin
   session data); parser + store + cursor tests, unique temp dirs.
5. Build adjacent to Plex — shared media-type mapping helpers.

## Build notes (2026-06-17)

- `DEF` wired as `Periodic` (hourly). `CONNECTION` is TokenPaste composite
  `URL|API_KEY` (Philips Hue pattern). Registered in `CONNECTIONS`.
- Raw layer: `media/plays/jellyfin/raw/YYYY-MM.jsonl` — verbatim `BaseItemDto`.
- Contract layer: `media/plays/jellyfin/YYYY-MM.jsonl` — `MediaItem` rows,
  deduped by `guid = jellyfin-{ItemId}-{day}`, watermarked by `LastPlayedDate`.
- Pagination: `GET /Items?Filters=IsPlayed&SortBy=DatePlayed&SortOrder=Descending`
  with `StartIndex`; stops when page is short or all items are below watermark.
- `Type` → category (Movie/Episode → video, Audio → music). Episode detail is
  `S{season:02}E{ep:02}`. `RunTimeTicks` stored as `extra["runtime_secs"]` —
  NOT in `seconds` (base /Items cannot report watched-seconds; per contract
  `seconds = 0` when unknown; only the Playback Reporting plugin can supply it).
- `UserData.PlaybackPositionTicks` → `extra["resume_secs"]` when > 0 (brief
  vault-mapping: "resume position in extra"). `kind` = "partial" when
  `PlayedPercentage < 85` and `PlaybackPositionTicks > 0`, else "play".
- Watermark uses strict `<` (not `<=`) so same-second new plays at the boundary
  are not silently dropped; guid dedupe suppresses re-writes.
- Playback Reporting plugin endpoint defined in trait but not used in main
  path — degrade-gracefully approach: base `Items` API always works, plugin
  data is a future enhancement when a real server sample is available.
- 16 tests: parsing (incl. resume + partial kind), writing, dedupe, watermark,
  credential parsing, connection.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Watched items | built | against a real local Jellyfin: paste URL + API key, watch something, Sync now, confirm row in `media/plays/jellyfin/` + hub last-data |
| Plugin session history | not wired | install Playback Reporting on the test server; confirm per-session timestamped rows (future enhancement) |

Needs a real Jellyfin install to validate — any self-hosting user's run
can promote slices.

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§Jellyfin (L3294–L3300). Feasibility 🟡 medium — niche audience, solid
mechanism. Key gotcha: **no timestamped play history without the Playback
Reporting plugin** — base install is watched/unwatched + resume only.
Local API preferred over SQLite (install-dependent DB path). No
time-sensitivity.

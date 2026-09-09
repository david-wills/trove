# Steam

- **id:** `steam`
- **domains:** `gaming/` (raw-only per taxonomy — heterogeneous shapes;
  play sessions may join `media/plays/` at read time, no write-time
  contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll owned-games playtime; cumulative counters,
  so frequent polls reconstruct sessions)
- **connection:** `steam` — TokenPaste (free Web API key from
  steamcommunity.com/dev + the user's SteamID/vanity name). Not shared
  with other defs.
- **evidence:** official-docs — Steam Web API
  (api.steampowered.com `IPlayerService/GetOwnedGames`,
  `ISteamUserStats/GetPlayerAchievements`, `GetUserStatsForGame`); free
  self-registered key, no OAuth
- **effort / priority:** S / P1
- **needs:** none

## What it is

The dominant PC game store/launcher. The Web API is the gold standard of
the gaming domain — official, free, stable, no scraping — and yields the
user's full library with per-game playtime plus achievement unlock history.
Highest-value gaming source; the template the other gaming pulls
(RetroAchievements, PSN, Xbox) copy.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Owned games + playtime | none (own key sees own private profile) | appid, name, `playtime_forever` (min), `playtime_2weeks` | official docs |
| Achievements | per-game; only games with schemas | unlock flags + timestamps per appid | official docs |
| Game stats | per-game | numeric stats per appid | official docs |

All optional in the contract sense (omit-if-empty). Caveat: playtime is
**cumulative** — there is no historical event log, so anything before the
first sync is a single lifetime total; deltas between polls approximate
sessions thereafter.

## Access & auth

- REST, key as query param: `GetOwnedGames` (library + playtime),
  `GetPlayerAchievements` per appid (batch schema via `GetSchemaForGame`
  for large libraries), `GetUserStatsForGame`.
- Key: free at steamcommunity.com/dev (Steam account + any domain string).
  Profile privacy: the owner's key reads their own private profile;
  otherwise the profile must be public — surface this in connect-card help.
- No published rate limit; practical cap ~100k req/day — a personal
  periodic pull is nowhere close. reqwest + serde; no SDK.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `gaming/steam/library/YYYY-MM.jsonl` (owned-games
  snapshots with playtime counters), `gaming/steam/achievements/…`
  (unlocks, `ts` = unlock time, `guid` = `appid:apiname`). Raw-only domain:
  Steam's native shapes, vault-wide conventions (guids, timestamps,
  partitions) still apply.
- **Derived sessions:** playtime deltas between polls can be emitted as
  approximate play-span rows (clearly marked derived) — candidates for a
  read-time join into the media-plays view, **not** written to
  `media/plays/`.
- **Dedupe:** achievements by `appid:apiname`; snapshots by poll timestamp;
  watermark cursor in `.trove/steam-sync.json`, rebuildable from output.

## Build plan

1. Module `crates/trove-core/src/steam.rs`: `DEF` (Periodic — a few times
   daily; finer polls sharpen derived sessions), `CONNECTION` (TokenPaste:
   key + SteamID/vanity-name field, help text covering the
   public-profile-or-own-key rule per the SimpleFIN affordance lesson),
   `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Vanity-name → SteamID64 resolution via `ResolveVanityURL` at connect
   time.
4. Fixtures from documented response shapes (library with/without
   `playtime_2weeks`; achievement list; private-profile error); parser +
   store + cursor tests, unique temp dirs.
5. Achievement iteration is per-appid — fetch lazily (recently-played
   first), not the whole library every poll.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Library + playtime | ✅ built | paste a real key + SteamID; Sync now; confirm snapshot rows in `gaming/steam/library/YYYY-MM.jsonl` + hub last-data |
| Achievements | ✅ built | unlock any achievement; next poll shows the unlock row with timestamp in `gaming/steam/achievements/YYYY-MM.jsonl` |
| Derived sessions | deferred | play a game ~30 min across two polls; delta computation is read-time, not write-time |

## Build notes (2026-06-16)

- Module: `crates/trove-core/src/steam.rs` — Periodic (8h), raw-only (`gaming/`).
- CONNECTION: `steam` TokenPaste (composite `KEY STEAMID64`, space-separated). Vanity URLs resolved to SteamID64 at connect time via `ResolveVanityURL`.
- Library: full-fidelity month-partitioned snapshots; upsert within a month (no double-append).
- Achievements: guid = `appid:apiname`; deduped; recently-played appids fetched first; max 50 appids per pull.
- Field names confirmed against wiki.teamfortress.com/wiki/WebAPI/GetOwnedGames and wiki.teamfortress.com/wiki/WebAPI/GetPlayerAchievements.
- `unlocktime` is a well-known returned field (u64 Unix seconds); `achieved` is 0/1 int.
- 24/24 tests pass; cargo check green. `&crate::steam::CONNECTION` added to CONNECTIONS in integrations.rs.
- **Defect fixes (2026-06-16 adversarial review):** (1 — MAJOR) `GetPlayerAchievements` URL now appends `&l=english`; without this Steam omits `name`/`description` entirely (HTTP 200, fields absent — not None). Added `achievements_tf2_no_lang()` fixture + `parses_achievements_no_lang_name_is_none` test to pin the real no-language shape. (2 — MINOR) `connect_with` probe now parses the `GetRecentlyPlayedGames` response body and logs a warning when `games` is absent/empty (Steam returns HTTP 200 + `{}` for a private profile, not 401). (3 — MINOR) Removed contradictory multi-line comment in `parse_library`.

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Steam
(L3463–L3469). Feasibility 🟢 high — "build now," cleanest API in the
domain. Cross-cutting notes 1–2: shared simple-HTTP + per-service
credential pattern with RetroAchievements/BGG/Chess.com/Lichess; Steam has
no built-in cursor (cumulative playtime), so the watermark is the poll
time. RetroAchievements is "cheap once Steam ships" — sequence it right
after. GOG Galaxy's local DB also absorbs Steam data for Galaxy users
(separate brief).

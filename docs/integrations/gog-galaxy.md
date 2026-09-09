# GOG Galaxy

- **id:** `gog-galaxy`
- **domains:** `gaming/` (raw-only per the taxonomy — heterogeneous shapes;
  sessions may join `media/plays/` at read time, no write-time contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the local `galaxy-2.0.db`; copy-then-read, daily)
- **connection:** none — local SQLite, no login. Requires Full Disk Access
  (already granted for other Trove collectors) and GOG Galaxy installed +
  run at least once. Galaxy need not be running at collection time.
- **evidence:** AB1908/GOG-Galaxy-Export-Script (cross-platform Python script,
  explicitly macOS-supported) confirms: macOS path =
  `/Users/Shared/GOG.com/Galaxy/Storage/galaxy-2.0.db`; tables = GamePieces
  (releaseKey, gamePieceTypeId, value), GamePieceTypes (id, type), ProductPurchaseDates
  (gameReleaseKey), GAMETIMES (releaseKey, minutesInGame), LASTPLAYEDDATES
  (gameReleaseKey, lastPlayedDate). GOG Galaxy Integrations Python API confirms
  GameTime fields: time_played (minutes), last_played_time (unix timestamp).
  Type IDs fetched dynamically. Schema confidence: medium-high (Windows-confirmed
  + macOS path confirmed from the cross-platform script; real macOS DB unverified).
- **effort / priority:** M / P2
- **needs:** Needs-sample (real macOS `galaxy-2.0.db` to validate path +
  schema on macOS before promotion to stable)

## What it is

GOG's launcher for DRM-free PC games — and, more importantly, a
*multi-platform aggregator*: its integration plugins pull Steam, Epic, and
other launchers' libraries and playtime into one local SQLite database.
Reading that DB gets cross-launcher PC gaming coverage in one collector;
notably it is the only feasible path to Epic Games Store data (Epic itself
is catalogued unavailable and points here).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Game library | free (Galaxy installed) | titles, platform of origin, metadata | community schema (`GamePieces`) |
| Playtime per game | free | minutes played per title | community schema (`GameTimeStatistics`) |
| Aggregated platforms | per enabled Galaxy plugin | Steam/Epic/etc. titles + playtime in the same tables | community schema |

All optional; a user with no plugins enabled just yields the GOG-native
library. Playtime is cumulative (no per-session event log).

## Access & auth

- Local SQLite, expected at
  `~/Library/Application Support/GOG.com/Galaxy/storage/galaxy-2.0.db`
  (inferred from the well-known Windows path `C:\ProgramData\GOG.com\
  Galaxy\storage\galaxy-2.0.db` — **must be verified on a real macOS
  install before trusting**). Tables per the export script: `GamePieces`
  (library), `PlayTasks`, `GameTimeStatistics` (playtime, joinable to
  `GamePieces`).
- TCC: Full Disk Access (the established Trove FDA grant covers it).
  Copy-then-read pattern (shared utility with iMessage/Apple Books) to
  avoid locking a live DB.
- Fully local — standalone-clean by construction; nothing leaves the
  machine.

## Vault mapping

- **Raw layer — library snapshot:** `gaming/gog-galaxy/library.jsonl` —
  rewritten whole on every pull (one row per owned game: releaseKey, title,
  platform, playtime_mins [None when never launched], last_played_ts,
  snapshot_ts). This is a current-state snapshot, not an append log.
- **Raw layer — change-log:** `gaming/gog-galaxy/delta/YYYY-MM.jsonl` —
  one row appended only when a game's playtime advances or a new game
  appears in the library for the first time (after the silent first-run
  baseline). The delta stream is reconstructable from library.jsonl if
  deleted; the two streams together give full historical fidelity.
- **Contract layer:** none at write time — `gaming/` is raw-only.
  Cumulative playtime is snapshot-shaped, not event-shaped; any read-time
  join derives sessions from snapshot deltas if ever wanted.
- **Cursor:** `.trove/gog-galaxy-sync.json` — stores every owned releaseKey
  → Option<playtime_mins> (None for never-played games). Rebuildable from
  library.jsonl. Tracking all keys (not just played ones) prevents spurious
  delta rows for backlog / bundle / giveaway games that are owned but
  never launched.

## Build plan

1. **Spike first (Needs-sample):** obtain a real macOS `galaxy-2.0.db`
   (any Galaxy-on-Mac user), confirm the path glob and that
   `GamePieces`/`GameTimeStatistics` match the community schema. The
   parser is written **last**, against the verified sample — evidence is
   community-documented for Windows only.
2. Module `crates/trove-core/src/gog_galaxy.rs` (def id `gog-galaxy`):
   `DEF` (Periodic, daily), permission hook (FDA + file-exists check so
   the hub card explains "GOG Galaxy not installed" honestly), `pull` via
   copy-then-read rusqlite.
3. Registration line in `INTEGRATIONS` (no connection).
4. Fixtures: a trimmed sample DB checked into test fixtures; parser +
   snapshot-delta + store tests, unique temp dirs.
5. In-app copy: note that enabling Galaxy's Steam/Epic plugins enriches
   the data (this is the documented Epic path).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| macOS path + schema | 🟡 needs sample | locate `galaxy-2.0.db` on a real Mac Galaxy install; confirm path = `/Users/Shared/GOG.com/Galaxy/Storage/galaxy-2.0.db`; confirm tables exist |
| Library + playtime | 🟡 needs sample | enable the def; Sync now; confirm `gaming/gog-galaxy/library.jsonl` rows match the Galaxy UI's library and "time played" |
| Plugin aggregation | 🟡 needs sample | on an install with the Steam or Epic plugin enabled, confirm those titles appear with correct platform tags |
| First-run silent baseline | ✅ unit tested | `never_played_games_no_spurious_deltas` calls `pull_from()` (real code path) and asserts 0 deltas on run 1 |
| No spurious deltas for unplayed games | ✅ unit tested | `never_played_games_no_spurious_deltas` asserts 0 deltas on run 2 with unchanged DB including a never-played game (Disco Elysium) |
| Delta detection on playtime advance | ✅ unit tested | `never_played_games_no_spurious_deltas` advances Witcher 3 playtime and asserts exactly 1 delta on run 3 |
| Platform prefix parsing | ✅ unit tested | `platform_from_key_known_prefixes` confirms gog/steam/epic/origin/uplay/battlenet |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §GOG Galaxy
(L3591–L3597); see also §Epic Games Store (L3607–L3613), whose unavailable
card points users here. Feasibility 🟡 medium solely on the unverified
macOS path/schema — Windows-side evidence is solid. Research recommendation:
"Spike first — verify exact macOS path and schema; straightforward once
confirmed." Cross-cutting note 3 (L3629): share the FDA copy-then-read
SQLite utility with Apple Books/iMessage.

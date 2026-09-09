# RetroAchievements

- **id:** `retroachievements`
- **domains:** `gaming/` (raw-only per the taxonomy — heterogeneous shapes;
  sessions may join `media/plays/` at read time, no write-time contract)
- **status:** 🧪 built (fixture-tested, not validated — Needs-login)
- **unavailable_reason:** none
- **behavior:** Periodic (poll for new unlocks; time-range cursor)
- **connection:** `retroachievements` — TokenPaste (per-user API key from
  retroachievements.org/settings → API Key; no app registration). Not shared
  with other defs. Registered in `CONNECTIONS` (integrator added the line).
- **evidence:** official-docs — api.retroachievements.org REST API;
  maintained `@retroachievements/api` JS library confirms endpoint shapes
- **effort / priority:** S / P2
- **needs:** Needs-login (real-data validation needs an RA account + API key)

## What it is

The retro-gaming achievement service: emulator users earn community-authored
achievements across classic consoles (NES through PS2-era). For its users it
is the *only* record of retro play — no platform holder tracks it. Yields
games played, unlock timestamps, hardcore/softcore mode, and mastery awards;
a genuine play-history stream for an otherwise invisible hobby.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Completion progress | free (all accounts) | games played, achievements earned/total, mastery awards | official docs |
| Unlock events | free | achievement id/title, unlock timestamp, hardcore vs softcore flag | official docs |
| Rank & score | free | per-game rank, points | official docs |

All optional in the raw shape (omit-if-empty); no plan tiers exist.

## Access & auth

- REST at `api.retroachievements.org`; key endpoints
  `getUserCompletionProgress` (all played games + awards),
  `getAchievementsEarnedBetween` (time-range query — the incremental pull),
  `getUserGameRankAndScore`. Auth: per-user API key as a query param.
- Rate limits: fair-use; the official JS library implements back-off — do
  the same (polite hourly/daily poll is far below any threshold).
- No TCC, no local files. Standalone-clean (plain HTTPS + serde). Identical
  pattern to Steam — cheap once Steam ships; sequence after it.

## Vault mapping

- **Raw layer:** `gaming/retroachievements/YYYY-MM.jsonl` — unlock events
  (one row per achievement earned: `ts`, `guid` = achievement id + unlock
  ts, game id/title, hardcore flag, points) partitioned by unlock month;
  `gaming/retroachievements/progress.jsonl` (or latest-snapshot file) for
  per-game completion/mastery state.
- **Contract layer:** none at write time — `gaming/` is raw-only. Play
  sessions aren't directly observable (only unlock moments); any
  media-plays join happens at read time later.
- **Dedupe:** achievement id + unlock timestamp as `guid`; watermark cursor
  in `.trove/retroachievements-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/retroachievements.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: key from RA settings page, help copy on the
   def), `pull` hook using `getAchievementsEarnedBetween` from the cursor.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the documented API response shapes (completion-progress
   and earned-between variants); parser + store + cursor tests, unique
   temp dirs.
4. Reuse the Steam HTTP/key pattern (shared reqwest client, per-service
   credential store) — build after Steam to inherit it.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unlock events | 🧪 | paste a real RA API key; Sync now; confirm rows in `gaming/retroachievements/` + hub last-data |
| Completion progress | 🧪 | same run; confirm progress snapshot matches the RA profile page |

## Implementation notes (2026-06-17)

- **Module:** `crates/trove-core/src/retroachievements.rs` — fully replaces
  the NotWired stub.
- **Behavior:** Periodic (hourly). `API_GetAchievementsEarnedBetween` with a
  Unix-timestamp watermark cursor; first sync passes `from=0` to backfill all
  history. Progress snapshot via `API_GetUserCompletionProgress` (paginated,
  drain until short page).
- **Raw-only:** `gaming/` domain — no write-time contract. Two streams:
  - `gaming/retroachievements/YYYY-MM.jsonl` — one row per unlock, month-
    partitioned, `guid = "{AchievementID}_{epoch}"`, full-fidelity PascalCase
    fields preserved verbatim.
  - `gaming/retroachievements/progress.jsonl` — current-state completion
    snapshot, rewritten whole each pull.
- **Auth:** TokenPaste — personal Web API Key from
  retroachievements.org/settings → "Keys". Stored in `.trove/sync/`.
  A new `CONNECTION` def is declared; needs one `&crate::retroachievements::CONNECTION`
  line added to CONNECTIONS in `integrations.rs` by the integrator.
- **API field names** confirmed from `api-docs.retroachievements.org`:
  PascalCase — `Date`, `HardcoreMode`, `AchievementID`, `Title`,
  `Description`, `BadgeName`, `Points`, `TrueRatio`, `Type`, `Author`,
  `AuthorULID`, `GameTitle`, `GameIcon`, `GameID`, `ConsoleName`,
  `CumulScore`, `BadgeURL`, `GameURL`.
- **Tests:** 11 tests, all green. `cargo check` clean.

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming"
§RetroAchievements (L3559–L3565). Feasibility 🟢 high — official, actively
maintained. Per-user key means no app registration or compiled-in secret.
Niche audience but the cleanest API in the console/retro group; the research
doc recommends building it as a near-free follow-on to Steam (same simple
HTTP-key pattern, cross-cutting note 1 at L3625).

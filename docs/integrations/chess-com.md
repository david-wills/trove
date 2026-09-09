# Chess.com

- **id:** `chess-com`
- **domains:** `gaming/` (raw-only per the taxonomy — heterogeneous shapes;
  sessions may join `media/plays/` at read time, no write-time contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll monthly archives; month cursor)
- **connection:** none — keyless official public API; the user supplies
  their Chess.com username (a def setting, not a credential).
- **evidence:** official-docs — Chess.com Published-Data API (PubAPI),
  api.chess.com/pub/player/{user}/games/{Y}/{M}
- **effort / priority:** S / P1
- **needs:** none

## What it is

The largest online chess platform. Every game a user plays is published
with full PGN (the complete move record), time control, result, ratings,
opening, and Chess.com's accuracy score — a rich, replayable activity
stream for anyone who plays online chess. Pairs with Lichess (same shape,
same effort); together they cover nearly all online chess players.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Game archives | all accounts (public data) | PGN, time control, result, ratings, ECO opening, start/end timestamps | official docs |
| Accuracy score | when computed (Chess.com proprietary metric) | per-game accuracy | official docs |
| Player stats | all accounts | current ratings, win/loss records by mode | official docs |

All optional in the raw shape; accuracy is simply absent on games where
Chess.com didn't compute it — no special code path.

## Access & auth

- PubAPI, keyless: `GET https://api.chess.com/pub/player/{username}/games/{year}/{month}`
  returns all games for a month (JSON with embedded PGN);
  `/pub/player/{username}/stats` for ratings/win-loss. There's also an
  archives-list endpoint to enumerate available months for backfill.
- No published rate limit — be polite (sequential month fetches, small
  delay). No auth at all; public data only.
- No TCC, no local files. Standalone-clean (plain HTTPS + serde).

## Vault mapping

- **Raw layer:** `gaming/chess-com/YYYY-MM.jsonl` — one row per game
  (`ts` = end time, `guid` = game URL/id, PGN stored as-is for replay,
  time control, result, ratings, opening, accuracy), partitioned by the
  archive month (the API's native partition — a perfect fit);
  `gaming/chess-com/stats.jsonl` latest ratings snapshot.
- **Contract layer:** none at write time — `gaming/` is raw-only. Games
  are session-shaped and may join a read-time plays view later.
- **Dedupe:** game URL/id as `guid`; month cursor in
  `.trove/chess-com-sync.json` (re-fetch the current month each run),
  rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/chess_com.rs` (def id `chess-com`):
   `DEF` (Periodic, daily-ish), username setting, `pull` hook — enumerate
   archives once for backfill, then re-pull current month + advance cursor.
2. Registration line in `INTEGRATIONS` (no `CONNECTIONS` entry — keyless).
3. Fixtures from the documented PubAPI response shapes (monthly archive
   with PGN, stats); parser + store + cursor tests, unique temp dirs.
4. Store PGN verbatim inside the row — full fidelity first; never parse
   moves at write time.
5. Build in the same iteration as Lichess — same pattern, shared review.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Game archives | ✅ built | enter a real Chess.com username; Sync now; confirm game rows in `gaming/chess-com/` match the account's archive page for that month |
| Stats snapshot | ✅ built | same run; confirm `gaming/chess-com/stats.jsonl` ratings match the Chess.com profile page |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Chess.com
(L3575–L3581). Feasibility 🟢 high — keyless, official, stable. Research
recommends "build now, pair with Lichess". Monthly-archive endpoint makes
pagination trivial and maps 1:1 onto the vault's month partitions. Accuracy
score is proprietary — record it, don't try to recompute.

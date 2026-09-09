# Lichess

- **id:** `lichess`
- **domains:** `gaming/` (raw-only per the taxonomy — heterogeneous shapes;
  sessions may join `media/plays/` at read time, no write-time contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (NDJSON stream pull; `since=` cursor)
- **connection:** none for v1 — keyless public API; the user supplies their
  Lichess username (a def setting). A later optional `lichess` TokenPaste
  connection (personal API token) would unlock private games + higher rate
  limits — not required to ship.
- **evidence:** official-docs — lichess.org/api (open-source project),
  `GET /api/games/user/{username}` NDJSON stream
- **effort / priority:** S / P1
- **needs:** none

## What it is

The open-source, nonprofit online chess platform — second only to Chess.com
in reach, and the ethos match for Trove (open API, no ads, no paywalls on
data). Every game is retrievable with full moves, clocks, opening, ratings,
and result. Pairs with Chess.com: same effort, same shape; together they
cover nearly all online chess players.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Public games | keyless | moves, clocks, opening, players, ratings, result, timestamps | official docs |
| Private games | personal OAuth/API token (free) | same shape | official docs |
| Higher rate limits | with token | n/a (throughput only) | official docs |

Same row shape with or without a token — no tier code paths; tokenless
users simply miss games they've marked private.

## Access & auth

- `GET https://lichess.org/api/games/user/{username}?since=<ms>&until=<ms>&max=300`
  — NDJSON stream, one complete game object per line (ask for JSON moves
  via the documented params; PGN also available). Streaming avoids
  pagination entirely, even for huge archives.
- Keyless for public games; optional `Authorization: Bearer <token>`
  (personal token from the Lichess account page) for private games and
  higher limits — fits ConnectSpec TokenPaste if/when added.
- Respect the API's politeness rules (it's a donation-funded nonprofit):
  stream sequentially, back off on 429.
- No TCC, no local files. Standalone-clean (plain HTTPS + serde, line-wise
  NDJSON parse).

## Vault mapping

- **Raw layer:** `gaming/lichess/YYYY-MM.jsonl` — one row per game
  (`ts` = game end time, `guid` = Lichess game id, moves/clocks stored
  as-is, opening, players, ratings, result, speed/variant), partitioned by
  game month.
- **Contract layer:** none at write time — `gaming/` is raw-only. Games are
  session-shaped and may join a read-time plays view later.
- **Dedupe:** Lichess game id as `guid`; `since` millisecond watermark in
  `.trove/lichess-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/lichess.rs`: `DEF` (Periodic, daily-ish),
   username setting, `pull` hook streaming NDJSON from the `since` cursor.
2. Registration line in `INTEGRATIONS`. No `CONNECTIONS` entry for v1;
   leave a note for the optional TokenPaste follow-up (private games).
3. Fixtures from the documented NDJSON game shape (several line variants:
   rated/casual, with/without clocks); parser + store + cursor tests,
   unique temp dirs.
4. Store the game object verbatim — full fidelity first; no move parsing
   at write time.
5. Build in the same iteration as Chess.com — same pattern, shared review.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Public games | 🧪 | enter a real Lichess username; Sync now; confirm rows in `gaming/lichess/` match the account's game list; re-sync confirms `since` cursor pulls only new games |
| Private games (token) | — | future: paste a personal API token; confirm private games appear |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Lichess
(L3583–L3589). Feasibility 🟢 high — official, open-source, "excellent
API". Research recommends "build now alongside Chess.com — same effort,
open-source ethos matches Trove's". The NDJSON stream with `since=` is the
cleanest incremental story in the gaming domain; no key management at all
for the v1 public-games path.

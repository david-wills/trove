# PlayStation Network

- **id:** `playstation`
- **domains:** `gaming/` (raw-only per taxonomy — heterogeneous shapes;
  sessions may join `media/plays/` at read time)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll titles + trophies; self-rate-limited)
- **connection:** `playstation` — TokenPaste (64-char NPSSO cookie the user
  copies from ca.account.sony.com/api/v1/ssocookie while logged into
  PlayStation.com; exchanged for access/refresh tokens, refresh valid
  ~2 months). Not shared with other defs.
- **evidence:** community-schema, medium-high confidence — unofficial API
  reverse-documented by two mature libraries (psn-api JS/TS, psnawp
  Python); stable as of May 2026 per psnleaderboard.com status page. Risk:
  Sony could require app-based auth.
- **effort / priority:** M / P2
- **needs:** Needs-login (a real PSN account to spike the NPSSO flow and
  validate — build proceeds from the libraries' documented shapes)

## What it is

Sony's network for PS4/PS5. Yields the played-titles list with playtime
and the full trophy history with unlock timestamps — console gaming that
no local-Mac collector can see. Unofficial API, but widely used and mature
enough that the research judged the risk acceptable.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Played titles + playtime | PS5 titles native; **PS4 titles may read 0 hours** | title list, playtime where visible | psn-api/psnawp (community) |
| Trophies | none | per-title earned trophies, unlock timestamps, rarity, trophy-title list | psn-api/psnawp (community) |

All optional (omit-if-empty); a PS4-heavy account simply carries sparse
playtime — never special-case it, and never present 0 as "0 hours played".

## Access & auth

- NPSSO flow: user logs into PlayStation.com, visits
  `https://ca.account.sony.com/api/v1/ssocookie`, pastes the 64-char token.
  Trove exchanges it for access/refresh tokens (flow documented in psn-api
  source) — reimplement the HTTP auth directly in Rust, no shim.
- Endpoints: `getUserTitlesPlayedList` (games + playtime),
  `getUserTrophiesForSpecificTitle` (trophy data).
- **Self-rate-limit: 300 req/15min** to avoid bans. Refresh token lasts
  ~2 months — on expiry the card must show a clear "re-paste NPSSO" state,
  not fail silently (disabled-controls-need-affordance rule).
- No TCC, no local files. Standalone-clean (plain HTTPS, no proxy
  service).

## Vault mapping

- **Raw layer:** `gaming/playstation/titles/YYYY-MM.jsonl` (played-titles
  snapshots), `gaming/playstation/trophies/YYYY-MM.jsonl` (`ts` = unlock
  time, `guid` = title id + trophy id). Raw-only domain; vault conventions
  apply.
- **Contract layer:** none at write time (gaming is raw-only); play
  sessions are a read-time join candidate for the media-plays view.
- **Dedupe:** trophies by `(np_comm_id, trophy_id)`; cursor in
  `.trove/playstation-sync.json`, rebuildable from output.

## Build plan

1. **Spike first:** confirm the NPSSO → access-token exchange still works
   (research recommendation) before building the module proper.
2. Module `crates/trove-core/src/playstation.rs`: `DEF` (Periodic, gentle
   cadence within the self-limit), `CONNECTION` (TokenPaste with
   step-by-step NPSSO help copy on the def), `pull` hook.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Auth in Rust from the psn-api-documented flow; store refresh token via
   the standard credential path; expiry → clear reconnect affordance.
5. Fixtures from the libraries' documented response shapes (PS5-with-
   playtime AND PS4-zero-playtime variants); graceful-disable error path
   for upstream breakage (unofficial-API rule: status line, not hard
   failure).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| NPSSO auth + refresh | Needs-login | paste a real NPSSO; confirm token exchange, then a successful pull after the access token expires |
| Titles + playtime | Needs-login | account with PS5 titles: confirm playtime rows; PS4-only title: confirm absent/zero playtime renders honestly |
| Trophies | Needs-login | earn any trophy; next poll shows the unlock row with timestamp |

## Build notes (fan-out)

- **Contract mode:** raw-only (`gaming/` domain — no write-time contract).
- **Auth:** TokenPaste (NPSSO 64-char cookie) → two-step Sony exchange for access+refresh tokens.
  Auth endpoints confirmed from `psnawp` Python library `authenticator.py`.
- **Title fields confirmed** from `psnawp` `title_stats.py`: `titleId`, `name`, `imageUrl`,
  `category`, `playCount`, `firstPlayedDateTime`, `lastPlayedDateTime`, `playDuration`.
- **Trophy fields confirmed** from `psn-api` JS library examples: `trophyId`, `trophyType`,
  `trophyName`, `trophyRare`, `trophyEarnedRate`, `earned`, `earnedDateTime`, `trophyGroupId`;
  title trophy fields: `npCommunicationId`, `trophyTitleName`, `trophyTitlePlatform`.
- **Rate-limit:** 300 ms between requests; 30-min periodic cadence stays well within 300 req/15 min.
- **Dedupe:** trophies by `{npCommunicationId}/{trophyId}` guid set persisted in
  `.trove/playstation-sync.json` (rebuildable from raw on delete).
- **PS4 playtime:** titles without `playDuration` are stored as-is (field absent); never
  special-cased, never presented as "0 hours".
- **Expiry affordance:** `ConnectStatus.needs_reconnect = true` when `expired() && no refresh_token`;
  label says "re-paste NPSSO" per disabled-controls-need-affordance rule.
- **Needs-login flag:** NPSSO flow not spikeable without a real PSN account.
- 7 unit tests, all green. cargo check clean.

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §PlayStation
Network (PSN) (L3543–L3549). Feasibility 🟡 medium — unofficial but stable
May 2026; "build later," spike the NPSSO flow first. Cross-cutting note 4:
unofficial APIs (PSN, Xbox, Audible) need the graceful-disable error path;
PSN and Xbox were judged worth the risk given user base. Time-sensitivity:
none flagged, though trophy history is server-side and survives a late
start (playtime totals are whatever Sony exposes at first sync).

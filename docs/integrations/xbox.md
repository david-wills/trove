# Xbox

- **id:** `xbox`
- **domains:** `gaming/` (raw-only per taxonomy — heterogeneous shapes;
  sessions may join `media/plays/` at read time)
- **status:** 🧪 built (parser parked — Needs-sample to verify exact xbl.io field names)
- **unavailable_reason:** none
- **behavior:** Periodic (poll title history + achievements)
- **connection:** `xbox` — TokenPaste in the xbl.io variant (OpenXBL API
  key from xbl.io); the preferred direct-XSTS variant is a Microsoft
  login flow — **spike decides** (see build plan). Not shared with other
  defs.
- **evidence:** community-schema — unofficial proxy xbl.io is documented
  and stable; direct Microsoft REST documented at
  github.com/MicrosoftDocs/xbox-live-docs (XSTS auth flow, harder but
  dependency-free)
- **effort / priority:** M / P2
- **needs:** Needs-login (a real Xbox/Microsoft account to spike auth and
  validate)

## What it is

Microsoft's gaming network spanning console and PC Game Pass. Yields game
library with per-title playtime, achievement unlock dates, and gamerscore
— `GetTitleHistory` covers both console and PC Game Pass play, which no
local collector captures. Two roads in: the easy third-party proxy
(xbl.io) or the harder direct Xbox Live REST.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Title history + playtime | none; covers console + PC Game Pass | titles, playtime per title | xbl.io docs / MS REST docs |
| Achievements | none | unlock dates, gamerscore, recent unlocks | xbl.io docs / MS REST docs |

All optional (omit-if-empty). On the xbl.io path the free tier
(150 req/hour) is sufficient for a personal periodic sync.

## Access & auth

- **Path A — OpenXBL proxy (xbl.io):** sign in at xbl.io with a Microsoft
  account, get an API key, send as `X-Authorization`. Endpoints:
  `/v2/player/titles` (library + playtime), `/v2/achievements/title/{id}`,
  `/v2/achievements/player`. Free tier 150 req/hour; paid from $5/mo.
  Downside: a third-party service sits between the user and their data —
  it could shut down or change pricing, and gameplay metadata transits a
  middleman.
- **Path B — direct Xbox Live REST:** XSTS token via the Xbox Live auth
  flow (documented at MicrosoftDocs/xbox-live-docs). More complex auth,
  but no service dependency — **the better fit for Trove's standalone
  posture**; the research recommends spiking it to avoid xbl.io.
- No TCC, no local files. Either path is plain HTTPS.

## Vault mapping

- **Raw layer:** `gaming/xbox/titles/YYYY-MM.jsonl` (title-history
  snapshots with playtime), `gaming/xbox/achievements/YYYY-MM.jsonl`
  (`ts` = unlock date, `guid` = title id + achievement id). Raw-only
  domain; vault conventions apply.
- **Contract layer:** none at write time; play sessions are a read-time
  join candidate for the media-plays view.
- **Dedupe:** achievements by `(title_id, achievement_id)`; cursor in
  `.trove/xbox-sync.json`, rebuildable from output.

## Build plan

1. **Spike first — auth path decision:** attempt direct XSTS (Path B) per
   the research recommendation; fall back to xbl.io TokenPaste (Path A)
   only if XSTS proves impractical, and then label the proxy dependency
   honestly in the connect-card copy (user choice to route data through
   xbl.io).
2. Module `crates/trove-core/src/xbox.rs`: `DEF` (Periodic, cadence well
   inside 150 req/hour if Path A), `CONNECTION` per the spike outcome,
   `pull` hook.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from the documented response shapes (titles with playtime;
   achievement lists); graceful-disable error path for upstream breakage
   (unofficial-API rule: status line, not hard failure).
5. Reuse the Steam module's snapshot/achievement store patterns — same
   shape of data.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Auth (chosen path) | Needs-login | Connect an xbl.io API key; Sync now succeeds; hub last-data populates |
| Title history + playtime | Needs-sample | Account with console + PC Game Pass play: confirm both appear in `gaming/xbox/titles/`; verify `titleId`, `name`, and date field names match xbl.io response |
| Achievements | Needs-sample | Unlock any achievement; next poll shows row with unlock date; verify `progressState`, `progression.timeUnlocked`, `titleId` field names match xbl.io response |

## Build notes (2026-06-21)

- Auth path chosen: **Path A — OpenXBL proxy (xbl.io)** via TokenPaste API key in `X-Authorization` header.
- Connection def: `pub static CONNECTION: ConnectionDef` added (id="xbox"); needs one `&crate::xbox::CONNECTION,` line in `CONNECTIONS` (integrations.rs).
- Raw-only domain (`gaming/`): no contract bind. Two streams: `gaming/xbox/titles/YYYY-MM.jsonl` (title history, upserted by pull) and `gaming/xbox/achievements/YYYY-MM.jsonl` (unlocked achievements, deduped by guid=`{titleId}:{achievementId}`).
- **parser_parked_needs_sample=true** — The xbl.io OpenAPI spec (openapi.yaml) provides NO response body schemas for `/api/v2/achievements` or `/api/v2/player/titleHistory` (both list only "200: Success"). The `{content, code}` envelope wrapper mentioned in community examples is NOT in the openapi.yaml; both parsers fall back to the root object when absent.
- Achievements parser uses the canonical modern Xbox Live REST shape (`AchievementResponse` from OpenXbox/xbox-webapi-python): flat `{achievements:[...]}`, `progressState=="Achieved"`, `progression.timeUnlocked` (nested), `titleAssociations[0].id`. Defensive fallbacks for `isUnlocked`(bool) and grouped `{titles:[{titleId,achievements:[...]}]}` variant are also handled. Real shape must be confirmed against a live response.
- Sync warns (stderr) when achievements body is non-empty but parse yields 0 rows — unofficial-API breakage signal per the brief.
- 19 unit tests pass (all offline, stub API, unique temp dirs).
- Cadence: every 6 hours (well within the free-tier 150 req/hour limit).

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Xbox / Xbox
Live (OpenXBL) (L3551–L3557). Feasibility 🟡 medium — "build later"; the
xbl.io service dependency is the stated concern and direct XSTS the stated
alternative. Cross-cutting note 4: unofficial-API breakage risk shared
with PSN/Audible; worth it for the user base. Sequence near PlayStation —
the two console briefs share the snapshot+achievement vault shape and the
graceful-disable pattern.

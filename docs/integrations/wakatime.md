# WakaTime

- **id:** `wakatime`
- **domains:** `activity/wakatime/` (observed coding-time spans — imported
  observed histories live in `activity/<source>/` subfolders, raw-only per
  the taxonomy; **not** `time-entries/`, which is user-asserted entries
  only, and not `developer/` despite the research doc's embedded path)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (daily summaries pull; date-watermark cursor)
- **connection:** `wakatime` — TokenPaste (personal API key from
  wakatime.com account settings; no OAuth dance). Not shared with other
  defs.
- **evidence:** official-docs — REST API at api.wakatime.com/api/v1
  (heartbeats, durations, summaries, stats endpoints all documented)
- **effort / priority:** S / P2
- **needs:** Needs-login (validation only — build proceeds from documented
  shapes). Requires a pre-existing WakaTime account + editor plugin —
  Trove imports, it does not install the plugin.

## What it is

The standard cloud coding-time tracker: editor plugins send heartbeats
(file, project, language, editor, OS) to WakaTime, which aggregates them
into durations and daily summaries. For users who already run it, this is
years of fine-grained coding history the Trove activity watcher can't
backfill — and ongoing document-level context the watcher doesn't capture.
Supplementary to the watcher, never a replacement.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Daily summaries | free tier limits history depth (~2 weeks); paid unlocks full range | per-day totals by project/language/editor/OS | official API docs |
| Durations | same | 15-min aggregated spans per project | official API docs |
| Heartbeats | same | individual events: file, project, language, ts | official API docs |
| All-time stats | all plans | aggregate ranges | official API docs |

All optional in the per-source shape; free-tier users simply get a shorter
backfill. Summaries are the v1 pull (highest value per request);
durations/heartbeats are a depth opt-in per the collection-depth
convention.

## Access & auth

- REST API `api.wakatime.com/api/v1/` — API key as base64 Basic auth or
  Bearer. Key endpoints: `/users/current/summaries` (daily breakdowns),
  `/users/current/durations`, `/users/current/heartbeats`,
  `/users/current/stats/{range}`, `/users/current/projects`.
- The local offline cache (`~/.wakatime/offline_heartbeats.bdb`) is Go
  BoltDB — not readable from Rust; **skip it**, the API covers everything.
- **Wakapi** (self-hosted, WakaTime-compatible API, local SQLite) is the
  privacy-forward alternative: support a configurable base URL on the
  connection so Wakapi users point at their own instance — same code path.
- No TCC. HTTPS to the user's chosen endpoint only. Standalone-clean.

## Vault mapping

- **Raw layer:** `activity/wakatime/YYYY-MM.jsonl` — one row per
  (day, project) from summaries: ts (day), project, language/editor/OS
  breakdown, total_secs. Depth opt-in adds duration-span rows. Raw-only;
  the `activity/` root day-files stay single-writer (the live watcher) —
  this never writes them. No contract applies.
- **Dedupe:** `guid` = hash(day, project) for summary rows; date watermark
  in `.trove/wakatime-sync.json`, rebuildable from output files;
  re-pulling a recent window upserts (summaries for today change).

## Build plan

1. Module `crates/trove-core/src/wakatime.rs`: `DEF` (Periodic, daily),
   `CONNECTION` (TokenPaste: API-key label/help/placeholder per the
   SimpleFIN affordance rule, plus optional base-URL field for Wakapi),
   `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Backfill: on first sync walk summaries backward until the API returns
   empty/403 (free-tier history wall) — degrade gracefully with a UI hint,
   never fail.
4. Fixtures from the official docs' example responses (summaries with and
   without entity breakdowns); parser + store + cursor tests, unique temp
   dirs.
5. Read-time note: coding spans may join the watcher's activity views
   later; write-time stays per-source.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Summaries pull | ✅ built (Needs-login) | paste a real API key in the connect card; Sync now; confirm rows in `activity/wakatime/` + hub last-data |
| Backfill wall | ✅ built (Needs-login) | on a free account, confirm the backfill stops at the history limit with a visible hint, not an error |
| Wakapi endpoint | ✅ built (Needs-login) | point the connection at a Wakapi instance (key\nURL paste format); same pull succeeds |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §WakaTime
Coding Stats (L1340–L1346). Feasibility 🟢 high. Cross-cutting note 7
(L1482): time trackers are backfill + supplementary context for the built
activity watcher, not replacements. The research's `developer/wakatime/`
path predates the taxonomy — `activity/wakatime/` governs. Routing rule:
observed trackers → `activity/<source>/`; only user-asserted entries
(Toggl, Clockify) go to `time-entries/`.

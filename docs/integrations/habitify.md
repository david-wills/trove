# Habitify

- **id:** `habitify`
- **domains:** `habits/` (contract: **Phase 3 pending** — habits contract
  drafted from Habitica + Streaks + TickTick habits together)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll habits + journal; watermark by date)
- **connection:** `habitify` — TokenPaste (API key from Habitify Settings →
  API Access). **Pro-subscription-gated** — the API key only exists on a paid
  plan. Not shared with other defs.
- **evidence:** official-docs — docs.habitify.me (clean, documented REST API:
  `/habits`, `/journal`). **Confidence: medium** — documented but smaller
  user base; Pro gate limits who can reach it.
- **effort / priority:** S / P2
- **needs:** Needs-login (API is Pro-gated — surface that in the UI so a
  free-plan user understands why the key field is unusable until they upgrade)

## What it is

Cross-platform habit tracker with a clean, minimalist log. Used by people who
want straightforward habit streaks without Habitica's RPG layer. Smaller
reach than Habitica, but the API is well-documented and the data slots into
the same `habits` contract. iCloud-synced — there's no local DB on macOS to
read, so the API is the only path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Habits | Pro (API gate) | habit id, name, streak data, cadence | official docs |
| Journal / completion log | Pro (API gate) | per-habit completion entries, dates | official docs |

All optional in the contract (omit-if-empty). The whole API sits behind the
Pro gate — there is no free-tier slice.

## Access & auth

- REST: `GET https://api.habitify.me/habits` (all habits + streak data),
  `GET /journal?habit_id={id}` (completion log; date-filtered queries
  supported).
- Auth: API key header, from Habitify Settings → API Access — **only present
  on a Pro subscription**.
- iCloud-based sync → **no local DB on macOS**, so no file-read fallback; the
  API is the sole path. Standalone-clean (HTTPS).
- No TCC.

## Vault mapping

- **Raw layer:** `habits/habitify/raw/YYYY-MM.jsonl` — the `/habits` +
  `/journal` API objects, full fidelity, partitioned by month.
- **Contract layer:** `habits/habitify/…` per the (pending Phase 3) habits
  contract — one record per habit (name, cadence, streak) with completion
  entries from `/journal`; anything not in the shared shape to `extra`.
- **Dedupe:** habit id + completion date as `guid`; date watermark cursor in
  `.trove/habitify-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/habitify.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: API key; label/help/placeholder per the
   SimpleFIN affordance rule, with copy that **names the Pro requirement** so
   the field doesn't look broken to free-plan users).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from docs.habitify.me example `/habits` + `/journal` responses;
   parser + store + cursor tests, unique temp dirs.
4. Two-call pull: list habits, then fan out `/journal?habit_id=` per habit
   for completion logs.
5. Vault writes via `store` helpers once the habits contract is ratified;
   until then parked behind Needs-David (contract).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Habits | — | paste a Pro-account API key in the connect card; Sync now; confirm habit rows in `habits/habitify/` + hub last-data |
| Journal / completions | — | with logged completions, Sync now; confirm dated completion entries appear under each habit |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Habitify (L2682–L2688). Feasibility 🟡 medium — "build later": the API is
clean and well-documented, but the **Pro subscription gate** limits reach and
the user base is smaller than Habitica's, so it sequences after Habitica in
the habits contract work. Carried forward: `/habits` has streaks, `/journal`
has the completion log, date-filtered queries are supported, and iCloud sync
means **no local DB** — the API is the only way in. Shares the habits
contract with Habitica / Streaks / TickTick habits.

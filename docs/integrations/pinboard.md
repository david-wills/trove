# Pinboard

- **id:** `pinboard`
- **domains:** `reading/` — **reuses the `reading/` contract bound by
  `readwise` (INDEX #24)**; reads `crate::reading::Item` (no struct / `DOMAINS`
  / `spec_validation` change — not first-in-domain).
- **status:** 🧪 built (fixture-tested, not validated) — **Needs-login**
- **unavailable_reason:** none
- **behavior:** `Behavior::Periodic` — daily (`PINBOARD_SYNC_SECS = 86_400`),
  every-on-run cadence (the timer only advances when it actually runs, so
  re-enabling fires immediately). `/posts/all` is one full-archive call,
  short-circuited by a cheap `/posts/update` check when nothing changed;
  the 1-req/3s rate limit is respected by a sleep between the two calls.
- **connection:** `pinboard` — TokenPaste (`user:TOKEN` from
  **pinboard.in/settings/password**; no OAuth, token rides as the
  `auth_token` query param), verified at connect with a real `GET /posts/update`,
  stored 0600 at `.trove/sync/pinboard`. Not shared with other defs.
- **default:** off (`default_on: false`) — a Needs-login cloud sync, off until
  the user pastes a token; the def is toggleable once connected.
- **evidence:** official-docs — pinboard.in/api/ (simple documented v1 API,
  alive); service in maintenance mode, 288M bookmarks stored.
- **effort / priority:** S / P2
- **needs:** none — the reading contract is already ratified; live validation
  needs a real Pinboard API token (a Needs-login item, no app registration).
  **Time-sensitive:** service reliability is declining — archives are safest
  pulled sooner rather than later.

## What it is

Minimalist paid bookmarking service beloved by a small, long-tenured
audience — dedicated users hold 15+ years of bookmark history with tags
and descriptions. The service is in maintenance mode (slow responses,
limited development as of 2024–2026), which makes getting the archive into
the vault more urgent, not less.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full bookmark archive | all accounts (paid service) | url, title, extended description, tags, save time, shared/toread flags | official docs, research L1568–L1575 |
| Export files (XML/JSON/Netscape HTML) | all accounts, pinboard.in/export/ | same fields | research L1573 |

All optional in the contract; no tiering beyond the service itself being
paid.

## Access & auth

- REST: `GET https://api.pinboard.in/v1/posts/all?auth_token=user:TOKEN&format=json`
  — returns the full archive in one call (paginated for very large
  accounts). Use `/posts/update` to skip pulls when nothing changed.
- **Rate limit: 1 request / 3 seconds** — slow but fine since `/posts/all`
  is a single call; the runner must respect it and never hot-retry.
- **Health check:** the collector should treat timeouts/5xx as
  "service degraded" with honest hub copy, not as a Trove error — the
  research doc flags declining reliability.
- Export fallback: pinboard.in/export/ JSON via the generic import box —
  same parser, no-login path.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `reading/pinboard/raw/YYYY-MM.jsonl` — API post objects
  full-fidelity, partitioned by save time.
- **Contract layer:** `reading/pinboard/YYYY-MM.jsonl` per the pending
  reading contract — `ts` = save time, `guid` = Pinboard hash (or URL
  hash), `url`, `title`, `tags[]`, extended description as note;
  shared/toread flags in `extra`.
- **Dedupe:** guid as above — full-archive re-pulls and export imports
  write one stream idempotently; last-update timestamp cached in
  `.trove/pinboard-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/pinboard.rs`: `DEF` (Periodic — daily is
   plenty for bookmarks), `CONNECTION` (TokenPaste: help copy pointing at
   pinboard.in/settings/password, per the SimpleFIN affordance rule),
   `pull` hook for Sync-now with the 1-req/3s guard and update-check
   short-circuit.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the documented JSON post shape (tagged/untagged, with
   extended description, toread); parser + store + idempotent-repull tests,
   unique temp dirs.
4. Same-module Import path for the JSON export file (identical shape).
5. Contract rows once the reading contract is ratified; raw can ship first.

## Validation matrix

Promotion to ✅ needs David's real token (Needs-login).

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connect + auth | 🧪 fixture | `connection_def_is_token_paste_and_references_pinboard`, `stored_token_shows_username_in_status`, `pull_without_token_returns_clear_error`. **David:** open **pinboard.in/settings/password** (signed in) → copy the **API token** (it looks like `username:hex-token`) → paste the whole string into the **Pinboard** connect card → it verifies via `GET /posts/update` and stores 0600 at `.trove/sync/pinboard`; the connected-account row should show your **username** (the part before the `:`). |
| Archive pull | 🧪 fixture | `full_pull_writes_raw_and_contract_and_advances_cursor`, `maps_tagged_post_to_contract_item`, `maps_toread_post_to_state_saved`, `maps_post_with_no_tags_and_no_extended`, `partitions_by_local_month_of_ts`. **David:** enable the **Pinboard** toggle (default-off) → **Sync now** → confirm `Item` rows in `reading/pinboard/YYYY-MM.jsonl` (url + title + your tags, `state` = `saved`, `toread`/`shared` in `extra`) + the lossless `reading/pinboard/raw/` mirror + the hub "last data" date. |
| Idempotent re-pull / update-check short-circuit | 🧪 fixture | `second_pull_same_update_time_is_noop`, `idempotent_repull_with_new_update_time_dedupes_existing_guids`, `cursor_back_compat_empty_and_partial`. **David:** **Sync now** a second time with no new bookmarks → row count unchanged and the headline reads "up to date" (the run skips `/posts/all` entirely after the `/posts/update` check). |
| Degraded-service copy | 🧪 fixture | `degraded_service_on_update_check_is_not_an_error`. **David (only if Pinboard is flaky):** if the service times out or 5xxs, the hub shows a "service degraded — skipped" note (not an error stack) and the cursor does **not** advance, so the next Sync retries cleanly. |
| Secret hygiene | 🧪 fixture | `full_pull_writes_raw_and_contract_and_advances_cursor` asserts the `user:` token never lands in `.trove/pinboard-sync.json`. **David:** none — automated. |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Pinboard
(L1568–L1575), 🟡 medium — API trivially simple and alive, but the service
is in maintenance mode; an API v2 draft exists and never shipped. The
research doc still says "build now": low implementation cost, and the
15-year archives are exactly what a vault is for. Time-sensitivity is the
real argument — if Pinboard goes dark, the export-file import path is the
only fallback, so ship both together.

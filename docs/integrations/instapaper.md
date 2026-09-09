# Instapaper

- **id:** `instapaper`
- **domains:** `reading/` (contract: **Phase 3 pending** — reading contract
  drafted from Readwise + Instapaper + Raindrop + Pinboard + Kindle
  clippings together)
- **status:** 🧪 built (Import/CSV, v1)
- **unavailable_reason:** none
- **behavior:** Import (CSV/HTML export — v1 path, zero friction) with a
  later Periodic upgrade via the full API (same def family, second
  iteration; highlights are API-only)
- **connection:** none for the v1 import. The API upgrade adds an
  `instapaper` connection: xAuth (OAuth 1.0a, HMAC-SHA1) — Trove registers
  one consumer key (baked credential per ConnectSpec); the connect card
  collects username + password, exchanges them for an access token
  immediately, and **stores only the token, never the password**.
- **evidence:** official-docs — instapaper.com/api/full (active API,
  documented endpoints); multiple Rust/Python/Ruby client libraries as
  reference implementations.
- **effort / priority:** M / P2
- **needs:** Needs-sample — the LANDED path is the CSV save-export Import
  (no login). Fixtures were authored from documented column headers, not a
  real export on disk, so the header/shape must be confirmed against a real
  Instapaper CSV (see Needs-David step). The API path (highlights, incremental
  pull) is **deferred**: it additionally needs login + a one-time Trove-side
  consumer-key registration.

## What it is

The original read-later service, still active with a large long-tenured
installed base — many users have a decade-plus of saved articles. Yields
the saved-article archive plus (via API) highlights, a record of what was
actually read and marked.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Saved articles (CSV export) | all accounts, **most recent 2000 saves** | URL, title, selection, folder (4 columns) | research L1552–L1559 |
| Bookmarks (API) | all accounts | /api/1/bookmarks/list — full archive | official docs |
| Highlights (API only) | free: 5 highlights/month stored; paid: unlimited | /api/1/highlights/list | official docs |

All optional in the contract; CSV-only users simply carry no highlight rows
and no special code paths exist for the tiering.

## Access & auth

- **Export (v1):** Settings → Export → Download CSV (≤2000 articles) or
  HTML. No auth inside Trove, no TCC, no network. Standalone-clean.
- **API (upgrade):** instapaper.com/api/full with xAuth — OAuth 1.0a signed
  requests (HMAC-SHA1); access token obtained by sending username/password
  once. Consumer key/secret requested at
  instapaper.com/main/request_oauth_consumer_token (one-time registration).
  Awkward UX by modern standards — the connect card copy must be explicit
  that the password is exchanged and discarded.
- Per the disabled-controls rule: if the API card ships before the consumer
  key is registered, the gated state carries an inline hint.

## Vault mapping

- **Raw layer:** `reading/instapaper/raw/YYYY-MM.jsonl` — CSV rows / API
  bookmark + highlight objects full-fidelity. CSV rows lack timestamps
  beyond folder ordering — confirm on a real export; undated rows partition
  to import date with the original position preserved in `extra`.
- **Contract layer:** `reading/instapaper/YYYY-MM.jsonl` per the pending
  reading contract — `ts` = save time (API) or best-available, `guid` =
  bookmark id (API) / URL hash (CSV), `url`, `title`, folder + selection in
  `extra`.
- **Dedupe:** guid as above — CSV import then API upgrade write one stream;
  URL-hash vs bookmark-id reconciliation handled at the API iteration.

## Build plan

1. Module `crates/trove-core/src/instapaper.rs`: `DEF` (Behavior::Import);
   generic import box hosts the CSV/HTML parser. Fixtures: 4-column CSV
   with/without selection and folder; parser + store tests, unique temp
   dirs.
2. Later iteration (separate loop item): register a consumer key with
   Instapaper, add `CONNECTION` (xAuth flow as above), Periodic pull of
   bookmarks + highlights with a cursor; same guid stream.
3. Contract rows land once the reading contract is ratified; raw import can
   ship first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import | ✅ built + unit-tested (14 tests) | download a real CSV export, drop into the import box, confirm rows in `reading/instapaper/` + hub last-data |
| Re-import dedupe | ✅ tested | import the same CSV twice; row count unchanged |
| API bookmarks + highlights | ⏳ deferred | needs the registered consumer key + a real login through the connect card; confirm highlight rows (paid account for >5/month) |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption"
§Instapaper (L1552–L1559), 🟢 high, "build now". The research doc itself
recommends the CSV path as the zero-friction fallback if OAuth proves
burdensome — we invert that: CSV first (M effort is mostly the xAuth leg),
API as the follow-on for highlights and >2000-save archives. Highlights
never appear in the CSV, so the API iteration is what makes this more than
a bookmark list.

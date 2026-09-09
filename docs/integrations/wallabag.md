# Wallabag

- **id:** `wallabag`
- **domains:** `reading/` (contract: **Phase 3 pending** — reading contract
  drafted from Readwise + Instapaper + Raindrop + Pinboard + Kindle together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the instance for new/updated entries)
- **connection:** `wallabag` — TokenPaste composite of five pipe-separated
  fields (instance URL | client_id | client_secret | username | password).
  The paste is immediately exchanged for an OAuth2 **password-grant** bearer
  token + refresh token (not client_credentials — Wallabag requires username
  and password in the grant). Credentials are never stored in plain text;
  only the bearer/refresh tokens persist (0600). Not shared with other defs.
- **evidence:** official-docs — well-documented REST API,
  `{instance}/api/entries.json`, OAuth2; v2.6.14 current per the research doc
- **effort / priority:** M / P2
- **needs:** none

## What it is

Wallabag is the open-source, self-hostable read-later service (also hosted
at wallabag.it for ~11 EUR/yr) — it stores the **full article text** of
everything saved, plus tags, reading status, and highlights. Niche next to
Readwise/Instapaper, but self-hosting users are exactly Trove's
privacy-first audience, and the data is unusually rich (whole articles, not
just URLs).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Saved entries | all (self-hosted or wallabag.it) | URL, title, tags, reading status, timestamps | official API docs |
| Full article text | all | extracted article content per entry | official API docs |
| Highlights/annotations | all | highlight text per entry | research doc (exposed via API) |

All capability fields optional in the contract (omit-if-empty); no plan
tiers — Wallabag is open source, every instance has the full API.

## Access & auth

- REST: `GET {instance}/api/entries.json` (paginated; supports `since` for
  incremental pulls). Auth: OAuth2 with client id/secret created in the
  user's instance settings — token fetch is a plain HTTPS POST, so it fits
  a paste-credentials connect card rather than a browser OAuth dance.
- Instance URL is user-supplied (self-hosted or wallabag.it) — never assume
  a fixed host; validate and store the base URL with the connection.
- No documented rate limits in the research evidence; a personal instance
  is the only consumer. Plain HTTPS to a server the user controls —
  standalone-clean and unusually aligned with local-first ethos.
- No TCC, no local files.

## Vault mapping

- **Raw layer:** `reading/wallabag/raw/YYYY-MM.jsonl` — API entry objects at
  full fidelity (article text included; it's the user's own instance data).
- **Contract layer:** `reading/wallabag/` per the pending Phase 3 reading
  contract — expected shape: one row per save (`ts` = created-at, `source`,
  `guid` = entry id, `url`, `title`, `tags[]`, read status), article text as
  sidecar/raw rather than contract payload, overflow in `extra`.
- **Dedupe:** entry id as `guid`; incremental cursor (`since` watermark) in
  `.trove/wallabag-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/sync/wallabag.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste method with instance-URL + client id + secret
   fields; setup copy on the def per the registry rule, with the
   SimpleFIN-lesson affordance hints), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Token-refresh handling: client_credentials tokens expire — refresh on
   401, surface auth failures on the connect card, never fail silently.
4. Fixtures from the documented `entries.json` response shape (entries with
   and without highlights/article text); parser + store + cursor tests,
   unique temp dirs.
5. Contract rows wait on the Phase 3 reading contract; raw layer can land
   first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Entries + tags + status | ✅ unit tested | `cargo test -p trove-core wallabag::` — 18 tests green |
| Full text + highlights | ✅ unit tested | full_pull_writes_contract_raw_and_highlights test confirms raw layer + highlights stream |
| Incremental cursor | ✅ unit tested | second_pull_dedupes_existing_entries + cursor advancement test |
| Live instance | Needs-David | point the connect card at a test instance (wallabag is trivially self-hostable in Docker); Sync now; confirm rows in `reading/wallabag/` + hub last-data |

## Build notes (2026-06-21)

- Behavior: `Periodic` (hourly). Contract: `reuse-bound` → `reading.Item` + `reading.Highlight`.
- Auth: OAuth2 **password grant** (not client_credentials — the brief assumed client_credentials
  but the actual Wallabag API requires username + password in the token exchange).
  Five-field composite paste `{url}|{client_id}|{client_secret}|{username}|{password}`.
  Bearer + refresh tokens stored (0600); plaintext credentials never persisted.
- Annotations (highlights) mapped to `reading.Highlight` under `reading/wallabag/highlights/`.
- Article content stored raw-only (full HTML in `reading/wallabag/raw/`; not in contract rows).
- Cursor: Unix timestamp `since` passed to `GET /api/entries.json?since=…`; advanced only
  after full drain (max `updated_at` across the page sweep).
- 18 unit tests; 0 new cargo deps; touches integrations.rs (CONNECTION line added).

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Wallabag
(Self-Hosted Read-Later) (L1672–L1679); cross-cutting note 1 (L1706): token
auth dominates this domain — share the HTTP/token plumbing with the other
reading-domain collectors. Feasibility 🟡 medium only because the audience
is niche; the API itself is clean. Note the wallabag.it hosted option means
not every user is self-hosting — the connect card must treat the instance
URL as a first-class field, not an "advanced" option. Wallabag can itself
import from Pocket/Instapaper, so some users' Wallabag instances will
already contain their Pocket history — dedupe across providers is a
read-time concern, not write-time (per-source folders keep provenance).

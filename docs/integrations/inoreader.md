# Inoreader

- **id:** `inoreader`
- **domains:** `reading/` (contract: **reading.Item**, bound; articles and
  starred items map to Item; feed-subscription lists stay per-source raw)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll stream contents; `ot` watermark cursor; 30 min)
- **connection:** `inoreader` — **OAuth 2.0** (`read` scope; redirect port
  38805). NOTE: the brief said TokenPaste but the official developer docs
  confirm no personal token path exists — only OAuth 2.0 and the deprecated
  ClientLogin. Brief updated to match reality. Not shared with other defs.
- **evidence:** official-docs — inoreader.com/developers (Google-Reader-style
  API: subscription list, stream contents, starred, read state, tags)
- **effort / priority:** M / P2
- **needs:** Needs-login (Pro paywall — no free-tier API path; validation
  needs a Pro account). Needs-David (app registration at
  www.inoreader.com/developers; bake TROVE_INOREADER_CLIENT_ID +
  TROVE_INOREADER_CLIENT_SECRET into build for zero-setup login)

## What it is

Inoreader is a power-user cloud RSS service, second to Feedly in user base.
Its API follows the familiar Google Reader pattern and covers feed
subscriptions, article streams, read state, tags, and starred items — the
user's follow-and-read history. Like Feedly, the API sits behind a Pro
paywall; free users get only the OPML feed-list export.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Article streams | **Pro required** | article URL, title, feed, published ts, read state | official docs |
| Starred items + tags | Pro required | starred/tagged article rows | official docs |
| Feed subscriptions | Pro via API; **free via OPML export** (Settings → Import/Export) | feed URL, title, folder | official docs |

All capability fields are optional in the contract (omit-if-empty); tiering
never needs special code paths — a free user has no token to paste, and the
connect card says why (disabled-affordance rule).

## Access & auth

- REST (Google-Reader-style):
  `GET https://www.inoreader.com/reader/api/0/subscription/list` for feeds;
  `/reader/api/0/stream/contents/…` for articles; read state, tags, and
  starred items all reachable through the same API family.
- Auth: OAuth 2.0 or personal token — TokenPaste of the personal token,
  Pro plan required. Help copy on the connect card states the Pro
  requirement plainly.
- OPML export is free and covers the feed list only — accept it through
  the generic import box as a feeds-snapshot fallback.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `reading/inoreader/raw/YYYY-MM.jsonl` — API article
  objects, partitioned by month; feed-subscription snapshots in
  `reading/inoreader/feeds.jsonl` (per-source raw; OPML imports land here
  too).
- **Contract layer:** `reading/inoreader/YYYY-MM.jsonl` per the (pending)
  reading contract — one row per read/starred article (`ts`, `source`,
  `guid` = item id, `url`, `title`, `feed`, `read`/`starred` flags), tag
  names and overflow in `extra`.
- **Dedupe:** stream item id as `guid`; continuation watermark in
  `.trove/inoreader-sync.json`, rebuildable from output files.

## Build plan

1. Sequence **after Feedly** (same pattern, smaller base) — and after the
   local readers (NetNewsWire, Reeder), per the research recommendation.
   Most of the Feedly module's shape (paginated streams, tags, OPML
   fallback) transfers directly.
2. Module `crates/trove-core/src/inoreader.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: help copy with the Pro requirement + token
   location).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from inoreader.com/developers documented response shapes
   (subscription list, stream page with continuation, starred stream);
   parser + store + cursor tests, unique temp dirs.
5. OPML feed-list import via the generic import box (free-tier fallback).
6. Vault writes via `store` helpers once the reading contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Article streams + read state | — | paste a real Pro token; Sync now; confirm rows in `reading/inoreader/` + hub last-data (requires a Pro account — any Pro user's run validates) |
| Starred items | — | star an article in Inoreader; Sync now; confirm the starred row |
| OPML feed list (free) | — | export OPML from Settings → Import/Export; drop in the import box; confirm `feeds.jsonl` |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Inoreader
(L1608–L1615). Feasibility 🟡 medium — functional, well-covered API; the
Pro paywall is the only blocker. Lower priority than Feedly (smaller user
base, same caveat). The Google-Reader-style API is well-trodden ground —
expect few format surprises.

# Feedly

- **id:** `feedly`
- **domains:** `reading/` (contract: **Phase 3 pending** — articles/saved
  boards are reading-shaped; feed-subscription lists stay per-source raw)
- **status:** 🧪 wired
- **unavailable_reason:** none
- **behavior:** Periodic (poll stream contents; watermark cursor)
- **connection:** `feedly` — TokenPaste (personal developer token from
  feedly.com/i/team/api; **requires Feedly Pro**, ~$72/yr). Not shared with
  other defs.
- **evidence:** official-docs — developers.feedly.com (streams, tags/boards,
  read state; up to 100 articles/request)
- **effort / priority:** M / P2
- **needs:** Needs-login (Pro paywall — no free-tier API path; validation
  needs a Pro account) · reading contract not yet ratified (Needs-David at
  the contract-write step)

## What it is

Feedly is the largest cloud RSS service post-Google-Reader. Its API yields
feed subscriptions, article streams, saved/starred boards, and read state —
the user's full follow-and-read graph. The catch: API access requires a
Feedly Pro developer token, so the addressable audience is Pro subscribers
only; free users get nothing beyond an OPML feed list.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Article streams | **Pro required** (dev token) | article URL, title, feed, published ts, read state | official docs |
| Saved/starred boards | Pro required | tagged/saved articles via `/v3/tags` | official docs |
| Feed subscriptions | Pro via API; **free via OPML export** | feed URL, title, category | official docs |

All capability fields are optional in the contract (omit-if-empty); tiering
never needs special code paths — a free user simply has no token to paste,
and the connect card says why (disabled-affordance rule).

## Access & auth

- REST: `GET https://cloud.feedly.com/v3/streams/contents?streamId=…` for
  articles; `/v3/tags` for saved/starred boards; up to 100 articles per
  request, paginated with continuation cursors.
- Auth: personal developer token pasted from feedly.com/i/team/api (Pro
  plan). Full OAuth exists for registered apps but is aimed at non-personal
  use — TokenPaste is the right personal-scale shape; revisit OAuth only if
  Feedly ever opens it to free tiers.
- OPML export (free) covers the feed list only — worth accepting through
  the generic import box as a feeds-snapshot fallback.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `reading/feedly/raw/YYYY-MM.jsonl` — API article objects,
  partitioned by month; feed-subscription snapshots in
  `reading/feedly/feeds.jsonl` (per-source raw; OPML imports land here too).
- **Contract layer:** `reading/feedly/YYYY-MM.jsonl` per the (pending)
  reading contract — one row per read/saved article (`ts`, `source`,
  `guid` = Feedly entry id, `url`, `title`, `feed`, `read`/`saved` flags),
  board/tag names and overflow in `extra`.
- **Dedupe:** Feedly entry id as `guid`; continuation watermark in
  `.trove/feedly-sync.json`, rebuildable from output files.

## Build plan

1. Sequence **after the local readers** (NetNewsWire, Reeder) per the
   research recommendation — and note Reeder syncing with Feedly already
   mirrors much of this history locally for free.
2. Module `crates/trove-core/src/feedly.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: help copy states the Pro requirement plainly and links the
   token page — the disabled-affordance rule applies to the gating).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from developers.feedly.com documented response shapes (stream
   page, tags response, continuation pagination); parser + store + cursor
   tests, unique temp dirs.
5. OPML feed-list import via the generic import box (free-tier fallback).
6. Vault writes via `store` helpers once the reading contract is ratified.

## Implementation notes (2026-06-17)

- `Behavior::Periodic` (30 min cadence), `reading` domain, `reuse-bound` reading.Item contract.
- TokenPaste `ConnectionDef` (id=`"feedly"`); connection requires Feedly Pro or Enterprise (API
  access is no longer available on free plans; docs updated June 2026).
- Watermark = max `crawled` epoch-ms across the drain; stored in `.trove/feedly-sync.json`
  (non-secret). User id cached in same cursor to avoid a profile round-trip on every sync.
- State mapping: board/tag membership → `"favorite"`, `unread=false` → `"read"`, else `"saved"`.
- OPML feed-list import (free-tier fallback) deferred to a generic import box step; path would
  land at `reading/feedly/feeds.jsonl` (raw-only, no contract row).
- 15 unit tests, all green. cargo check clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Article streams + read state | 🧪 built | paste a real Pro/Enterprise dev token; Sync now; confirm rows in `reading/feedly/` + hub last-data |
| Saved boards | 🧪 built | save an article to a board in Feedly; Sync now; confirm the row has `state:"favorite"` and the board label in `tags` |
| OPML feed list (free) | — deferred | export OPML from a free account; drop in the import box; confirm `reading/feedly/feeds.jsonl` |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Feedly
(L1600–L1607). Feasibility 🟡 medium — the API itself is good; the Pro
paywall is the only blocker, and it's the user's choice, not ours. Free
users cannot use the API at all. Inoreader is the same pattern with a
smaller base; build Feedly first of the two.

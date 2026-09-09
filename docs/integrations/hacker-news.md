# Hacker News

- **id:** `hacker-news`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  favorites stay per-source raw under `social/hacker-news/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the public API by username; watermark on
  newest seen item id)
- **connection:** none — keyless; the user supplies their HN username in
  the def's settings
- **evidence:** official Firebase API (hacker-news.firebaseio.com),
  confirmed by YC to lack a favorites endpoint; community reference
  scrapers github.com/reactual/hacker-news-favorites-api,
  github.com/kisabaka/hackernews-stories
- **effort / priority:** M / P2
- **needs:** none

## What it is

The tech-community forum. For HN regulars, submitted stories and comments
are a public intellectual trail; favorites are a curated reading record.
Niche audience, but the submitted slice is among the cheapest live pulls
in the catalog: keyless, public, official.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Submitted stories | none | id, ts, title, url, score | official Firebase API |
| Comments | none | id, ts, text, parent | official Firebase API |
| Favorites | profile must be public | item ids → resolved items | HTML scrape only (no API endpoint) |
| Karma/about | none | karma, created | `/v0/user/{name}.json` |

All optional in the contract. Favorites is a degraded-gracefully slice:
private profile → skip with a UI hint, never fail the pull.

## Access & auth

- Official: `GET https://hacker-news.firebaseio.com/v0/user/{username}.json`
  returns submitted item ids; resolve each via `/v0/item/{id}.json`. No
  auth, no key, no rate-limit drama for a personal poll.
- Favorites: **no API endpoint exists** (confirmed). Only path is scraping
  `news.ycombinator.com/favorites?id={username}` — paginated HTML, works
  only when the profile is public. Same for upvotes (private — not even
  scrapeable; out of scope).
- No TCC, plain HTTPS, standalone-clean. The scrape hits the public site
  the user could open in a browser — no credential automation.

## Vault mapping

- **Raw layer:** `social/hacker-news/raw/YYYY-MM.jsonl` — resolved item
  objects; `social/hacker-news/favorites.jsonl` — favorite refs (per-source
  raw per the taxonomy: saves never route to `reading/`).
- **Contract layer:** submissions + comments → `social/hacker-news/` rows
  per the pending social-posts contract (`ts`, `source`, `guid` = HN item
  id, `body`/`title`, `url`, `score` in `extra`).
- **Dedupe:** HN item id as `guid`; cursor (max seen id) in
  `.trove/hacker-news-sync.json`, rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/hacker_news.rs`: `DEF` (Periodic,
   daily-ish; username as required setting — the disabled-control
   affordance rule applies until it's set).
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. API client + item resolver; fixtures from real public API responses
   (the API is unauthenticated — fixture capture is a one-time curl, not
   fresh research).
4. Favorites scraper second, behind the same def: parse the paginated
   HTML, tolerate markup drift (scrape fragility is the known risk),
   degrade to "favorites unavailable (private profile?)" on failure.
5. Store via `store` helpers; contract rows wait on social-posts
   ratification — raw layer can ship first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Submitted + comments | ✅ built | enter a real username; Sync now; confirm rows in `social/hacker-news/` + hub last-data |
| Favorites | ✅ built | use an account with a public profile and ≥1 favorites page; confirm `social/hacker-news/favorites.jsonl` populated; test private-profile path logs a note but doesn't fail |

## Build notes (2026-06-17)

- **Connection:** `pub static CONNECTION: ConnectionDef` (TokenPaste — username
  only; API is keyless). Registered as a new CONNECTIONS entry.
- **Contract:** `reuse-bound` → `social` domain; `Post` struct via
  `crates/trove-core/src/social.rs`. Story → `kind:"post"`, comment →
  `kind:"comment"`. `score`/`descendants`/`parent`/`deleted`/`dead` → `extra`.
- **Raw layer:** `social/hacker-news/raw/YYYY-MM.jsonl` — the resolved Firebase
  item object verbatim (partitioned by item `time` month). Unconditional.
- **Favorites:** `social/hacker-news/favorites.jsonl` — per-source flat file,
  not the contract stream (favorites are not authored content). Scraped from
  public `news.ycombinator.com/favorites?id={username}` HTML; handles both the
  current `span.titleline > a` and the older `a.storylink` shapes; degrades
  gracefully on private profiles.
- **Cursor:** `.trove/hacker-news-sync.json` → `max_id` (max submitted item id
  written). Incremental: next run fetches only ids > watermark.
- **No new Cargo deps** — ureq + serde_json + chrono already in tree; no HTML
  parser dependency added (string-scanning parser keeps the standalone rule).
- **18 tests** — all pass; `cargo check` clean.

## Research notes

`integrations-research.md` → Web Activity §Hacker News (L1640–L1647).
Feasibility 🟡 medium (the scrape half). The favorites scrape is the only
M6-style piece — keep it isolated so HTML drift can't break the official
API slice. Upvoted items are private and unreachable; say so in the UI
rather than implying completeness.

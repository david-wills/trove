# Reddit

- **id:** `reddit`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts), plus
  `correspondence/` (✅ ratified) for chat/DM history
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (GDPR export ZIP; user re-imports periodically)
- **connection:** none (export is requested logged-in on reddit.com; no
  credential ever touches Trove)
- **evidence:** official export flow — reddit.com/settings/data-request
  (GDPR option); community reference implementation rexport
  (github.com/karlicoss/rexport); API path documented but gated by
  pre-approval since the Nov 2025 crackdown
- **effort / priority:** M / P1
- **needs:** privacy (chat message bodies → opt-in with explicit
  acknowledgement for the correspondence slice)

## What it is

The dominant forum/communities platform. A heavy user's comment and
submission history is a longitudinal record of interests, opinions, and
communities over years — often the richest "what was I into in 2017"
signal anywhere. Chats are private correspondence.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Comments | all accounts | id, permalink, date, subreddit, parent, body, score | official export (research L4028–L4030) |
| Submissions (posts) | all accounts | id, permalink, date, subreddit, title/body, score | official export |
| Votes (up/down) | all accounts | item id, direction | export entry L1636 |
| Chats / DMs | all accounts | from, to, subject, body, date, id, permalink | messages_archive.csv (current); legacy chat_history.csv also supported |
| Saved posts | **disputed** | item refs | extractors disagree — see Research notes |

All optional in the contract (omit-if-empty). No paid-tier gating — the
GDPR export is identical for every account and, unlike the API, has no
1000-item cap.

## Access & auth

- User flow: reddit.com/settings/data-request → GDPR option → ZIP arrives
  by link (up to 30 days, usually much faster). Import box accepts the ZIP.
- **Format:** CSV. posts.csv body column is `body` (confirmed via
  guilamu/reddit-gdpr-export-viewer — NOT `text` or `selftext`). DM file
  is `messages_archive.csv` (columns: from, to, subject, body, date, id,
  permalink) in current exports; legacy `chat_history.csv` also matched by
  substring. The parser accepts both shapes. L1636's JSON path describes an
  older format — build against CSV, keep the parser tolerant.
- API path (OAuth personal-script app, 100 QPM, 1000-item caps) requires
  Reddit pre-approval for new apps as of Nov 2025 — **skip**; spike later
  only if incremental sync proves valuable.
- No TCC, no network at import time. Standalone-clean.

## Vault mapping

- **Raw layer:** `social/reddit/raw/` — the export files as parsed, full
  fidelity, partitioned by export date; votes/saved stay per-source raw
  here (saved posts never route to `reading/`).
- **Contract layer:** comments + submissions → `social/reddit/` rows per
  the pending social-posts contract (`ts`, `source`, `guid` = reddit
  fullname/id, `body`, `subreddit` and `score` in `extra`). Chats →
  `correspondence/reddit/` per the ratified correspondence contract
  (`guid` = message id; thread id as the conversation handle).
- **Dedupe:** reddit item ids as `guid` — re-imports of overlapping
  exports merge cleanly.

## Build plan

1. Module `crates/trove-core/src/reddit.rs`: `DEF` (Import; setup copy
   walks the data-request flow and warns about the up-to-30-day wait).
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. ZIP walker + tolerant CSV/JSON dual-shape parser; fixtures synthesized
   from the field lists at research L4030 and rexport's documented shapes.
4. Privacy gate: the correspondence slice (chat bodies) is opt-in with
   explicit acknowledgement; posts/comments slice imports without it.
5. Saved-posts handling lands parser-last within the module — verify
   presence on a real export before claiming the capability in UI.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Comments + posts | 🧪 built | request a real GDPR export; import ZIP; confirm rows in `social/reddit/` + hub last-data |
| Chats | 🧪 built | same export; confirm opt-in gate, then rows in `correspondence/reddit/` |
| Saved/votes | raw-only | inspect the real ZIP — saved posts confirmed NOT in standard export (L4030); votes raw if present |

## Research notes

`integrations-research.md` → Web Activity §Reddit (L1632–L1639) and Social
Media §Reddit (L4024–L4030). Feasibility 🟢 high for import. Known
disagreements to settle on first real export: file format (JSON vs CSV)
and whether saved posts are included (L4030 says no; L1636 says yes).
comments.csv includes an `ip` column — drop it at parse time. Pushshift
exists but is bulk-historical, not a personal-data path.

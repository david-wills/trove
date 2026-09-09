# Threads

- **id:** `threads`
- **domains:** `social/` (posts/replies → social-posts contract, **Phase 3
  pending**; likes/followers stay per-source raw under `social/threads/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import — parsed from the **same Meta Accounts Center ZIP as
  Instagram**. Distinct service, distinct hub card and def; shared
  mechanism and importer code.
- **connection:** none. No public Threads API exists as of June 2026;
  ActivityPub federation is in progress but not a stable data-access path.
- **evidence:** official-docs — Meta Accounts Center export (same flow and
  ZIP as Instagram); Threads content in `threads_and_replies.json`
- **effort / priority:** S / P1
- **needs:** none (the bundle's Instagram/DM portions carry the privacy
  flag on the `instagram` brief; Threads' own slice — public posts,
  replies, likes, follows — is not privacy-flagged)

## What it is

Meta's microblogging platform (the X competitor), attached to Instagram
accounts. For active users it holds their public writing — posts and reply
threads. Because Meta bundles Threads data inside the Instagram export,
supporting it is nearly free once the Instagram importer exists: same ZIP,
one extra subfolder to parse.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Posts + replies | none | text, timestamps (`threads_and_replies.json`) | official export |
| Likes | none | liked-content references | official export |
| Followers / following | none | account lists | official export |

All optional in the contract; a user who never posted simply yields likes
and graph data.

## Access & auth

- Export: Accounts Center > Your information and permissions > Export your
  information — **the same flow and the same ZIP as Instagram**. Choose
  JSON (not HTML); date-range filter available; ~48h turnaround and a
  4-day download link (per the Instagram entry — import promptly).
- No API. No TCC, no local files. Standalone-clean (user drags a ZIP in).

## Vault mapping

- **Raw layer:** `social/threads/raw/` — the Threads subfolder of the Meta
  ZIP (`threads_and_replies.json` + likes/graph files), partitioned by
  month.
- **Contract layer:** `social/threads/` rows per the pending social-posts
  contract (one row per post/reply: `ts`, `source`, `guid`, text,
  reply-to linkage in `extra`). Likes/followers stay per-source raw.
- **Dedupe:** post id (or timestamp+text hash if the export lacks stable
  ids — confirm against a real sample) as `guid`.

## Build plan

1. **Build inside the Instagram importer** — the catalog records this as a
   distinct service with a shared mechanism. One ZIP parser detects and
   routes Instagram posts, Instagram DMs, *and* Threads content; the
   `threads` def is its own one-line registration so the hub shows a
   Threads card with its own last-data and toggle.
2. Module: either `crates/trove-core/src/threads.rs` with a thin `DEF`
   delegating to the shared Meta-ZIP parser, or a second `DEF` exported
   from the instagram module — whichever keeps the one-module-one-line
   convention cleanest.
3. Fixtures: a `threads_and_replies.json` sample (and a ZIP *without* a
   Threads folder — Instagram-only users must import cleanly with the
   Threads def reporting no data, not erroring).
4. Contract rows wait on the Phase 3 social-posts contract; raw import can
   land first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Posts + replies | ✅ | request a Meta Accounts Center export (JSON) from an account with Threads activity; drag the ZIP in; confirm rows under `social/threads/` + hub last-data |
| No-Threads account | ✅ | import a ZIP from an Instagram-only account; Threads card shows no data, no error — headline explains absent file |

## Build notes (2026-06-16)

- `Behavior::Import` — accepts the Meta Accounts Center ZIP; extracts only
  `threads_and_replies.json` (all other files ignored so as not to conflict
  with the Instagram importer).
- Contract layer: `social/threads/YYYY-MM.jsonl` via the `social::Post`
  contract (source=`"threads"`, kind=`"post"`). Guid = sha256(unix_ts |
  caption | primary_uri) — same scheme as instagram.rs so cross-card
  re-imports are idempotent.
- Raw layer: `social/threads/raw/threads_and_replies.jsonl` (reference
  section, not date-partitioned).
- Meta mojibake repaired via `meta_encoding::fix_value`.
- No Threads file → graceful empty result with an explanatory headline (not
  an error) — handles Instagram-only exports cleanly.
- The Instagram importer ALSO writes Threads posts via the same ZIP in its
  own pass. The two importers share the same guid scheme so neither
  duplicates the other's work.
- 7 unit tests, all green. No new dependencies; no shared contract files
  touched.

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Threads (Meta)
(L4088–L4094); cross-cutting note 5 ("Threads shares an export with
Instagram — a free win, one import handles two platforms"). Feasibility
🟢 high. No public Threads API as of June 2026 — the bundled export is the
only path, so there is no incremental-sync upgrade to record yet; revisit
if ActivityPub federation stabilizes. P1 because it rides the Instagram
(P1) importer at marginal cost.

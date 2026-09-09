# Tumblr

- **id:** `tumblr`
- **domains:** `social/` (posts → social-posts contract, pending format confirmation;
  raw preservation active now)
- **status:** 🧪 built (parser parked — raw-only)
- **unavailable_reason:** none
- **behavior:** Import (archive ZIP; API v2 incremental sync is a possible
  later upgrade)
- **connection:** none for the import path. A future `tumblr` connection
  (OAuth, API v2, free app registration) would enable incremental sync —
  not part of this brief's scope.
- **evidence:** official-docs — official export at
  `tumblr.com/settings/blog/<name>/export` + Tumblr API v2 (OAuth 2, live since 2021)
- **effort / priority:** S / P2
- **needs:** Needs-sample — real export ZIP required to confirm format before
  the post parser can be completed (see Implementation notes below)

## What it is

Long-form/multimedia blogging platform with a dedicated (if smaller) user
base. For its users the blog *is* their creative archive — years of posts,
reblogs, tags, and media. The export is reportedly fast to generate (~38s),
but the exact ZIP structure is unconfirmed (see below).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Posts (own blog) | none | full content, tags, timestamps, notes | official export |
| Media | none | photos/videos included in the ZIP | official export |
| Likes/reblogs of others' content | none | URL references only (not content) | official export notes |
| Follows / likes via API | none (free OAuth app) | `GET /v2/user/likes`, `/v2/user/following` | official API v2 docs |

All optional in the contract; URL-only like references simply carry no body.

## Access & auth

- Export: Settings > Account > Export Data, or directly
  `tumblr.com/settings/blog/<YOURBLOGNAME>/export`. ZIP contains posts data,
  media files, and possibly an HTML viewer — exact structure unconfirmed.
- Multi-blog accounts need one export per blog — the import box should
  accept several ZIPs and key by blog name.
- API v2 (later): OAuth 2, free app registration, no approval gate;
  undocumented but generous rate limits. Not needed for the M1 import.
- No TCC, no local files. Standalone-clean (user drags a ZIP in).

## Vault mapping

- **Raw layer:** `social/tumblr/raw/files.jsonl` — manifest of every file in
  the ZIP (JSON files stored inline as parsed values; HTML/binary files stored
  in `social/tumblr/raw/html/<filename>`). Content-hash deduped; re-imports
  add nothing.
- **Contract layer:** `social/tumblr/` (month-partitioned JSONL) — NOT YET
  WRITTEN. Parked pending format confirmation.
- **Dedupe:** content hash of each raw file — re-importing the same ZIP adds
  nothing.

## Build plan

1. [x] Module `crates/trove-core/src/tumblr.rs`: `DEF` with
   `Behavior::Import` (letterboxd.rs is the reference import shape).
2. [x] One registration line in `INTEGRATIONS`. No connection.
3. [ ] **BLOCKED — Needs-sample**: obtain a real Tumblr export ZIP, confirm
   the ZIP structure (posts.json vs per-post HTML vs other), then rewrite the
   post parser accordingly.
4. [ ] Slot into the shared social-archive-importer detection layer (drag ZIP →
   auto-detect platform, show summary, confirm) once that exists.
5. [ ] Contract rows wait on Phase 3 social-posts contract and format
   confirmation.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Raw preservation | ✅ unit-tested | ZIP accepted, all files stored in raw layer, content-hash dedupe confirmed |
| Re-import dedupe | ✅ unit-tested | re-importing the same ZIP adds 0 files, counts unchanged |
| Posts import (contract rows) | ⏸ parked | export format unconfirmed — parser gutted until real ZIP inspected |
| Multi-blog | ⏸ parked | blocked on format confirmation |
| Likes/follows raw | ⏸ parked | blocked on format confirmation |

## Implementation notes (built 2026-06-17; fixed 2026-06-17)

Behavior: `Import` (ZIP). Raw preservation layer active now; post parser
**parked** (see below).

**Why the parser was parked:** The original build shipped a full JSON parser
targeting a `posts.json` (API-v2-shaped array) believed to be in the export
ZIP, based on the brief's claim of "ActivityPub-compatible JSON". Post-build
adversarial review found that independent sources describe the Tumblr export as
"a Posts folder with an HTML file for each post" — no `posts.json`. No real
export ZIP is available on disk to resolve the question. Against a real export
the prior parser's `find_posts_json()` would return an error and collect zero
posts. The synthetic fixture it was tested against proved self-consistency, not
real-format fidelity. Per the defect fix policy, the active parser was gutted
and the `Needs-sample` flag set as a true block.

**Additionally fixed (in the gutted code — carry-forward for the rewrite):**

1. Contract field mismatch for `kind:"quote"`: the original code wrote the
   upstream id to `repost_of` unconditionally, even when `kind="quote"`.
   The social contract requires `quote_of` for quotes and `repost_of` for pure
   reposts. The rewrite must gate on `kind`.

2. NPF (Neue Post Format) silent body loss: `type:"blocks"` posts store content
   in a `content[]` array (NPF), not in legacy string keys (body/caption/text).
   The original `_ =>` fallback silently produced empty `text` for NPF posts.
   The rewrite must handle `content[].{type:"text"}.text` concatenation.

3. Timestamp comment was wrong: `1718445600 = 2024-06-15T10:00:00Z` (not
   `14:00:00Z` as originally stated). Corrected in the parked fixture comment.

4. Photo URL preference: `original_size.url` was preferred but is increasingly
   absent from modern Tumblr API responses; `alt_sizes[0].url` (the largest)
   is the right primary in modern data.

`integrations-research.md` → "Social Media & Web Presence" §Tumblr
(L4072–L4078). Feasibility 🟢 high — one of the fastest, cleanest exports
in the catalog. Export covers posts on *your* blog; liked/reblogged content
from others is tracked by URL reference only. API v2 (free OAuth registration)
could add incremental sync as a later M5 follow-up — record it as an upgrade
path, don't build it now.

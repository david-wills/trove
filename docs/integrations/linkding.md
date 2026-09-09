# Linkding / Shiori

- **id:** `linkding`
- **domains:** `reading/` (contract: **bound** — `crate::reading::Item`
  ratified in Phase 3, with Linkding named explicitly as a source in
  `src/reading.rs`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the instance's bookmarks endpoint)
- **connection:** `linkding` — TokenPaste (instance URL + API token generated
  in the instance's settings). Not shared with other defs. Shiori support is
  a second def in the same module reusing the same connect-card pattern with
  its own connection (different instance, different token).
- **evidence:** official-docs — clean Linkding REST API
  (`GET {instance}/api/bookmarks/`, `Token` auth header); Shiori has a
  similar REST API; both tools' SQLite paths are known
- **effort / priority:** M / P2
- **needs:** none

## What it is

Linkding and Shiori are the two popular self-hosted bookmark managers —
lightweight, Docker-friendly, open source. Their users self-host precisely
because they want their data under their own control, which makes them
exactly Trove's audience. The data is the user's curated bookmark/save
stream: URLs, titles, tags, archive status (Linkding also keeps local HTML
snapshots of bookmarked pages).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Bookmarks (Linkding) | all (open source, no tiers) | URL, title, description, tags, timestamps, archived/unread flags | official API docs |
| Bookmarks (Shiori) | all | URL, title, tags, timestamps | research doc (similar REST API) |
| Page snapshots (Linkding) | all | local HTML snapshot per bookmark | research doc — not pulled; noted as available |

All capability fields optional in the contract (omit-if-empty); no plan
tiers on either tool.

## Access & auth

- Linkding: `GET {instance}/api/bookmarks/` with `Authorization: Token …`
  header (token generated in Linkding settings); paginated. SQLite lives at
  `/etc/linkding/data/db.sqlite3` inside the Docker volume — a same-Mac M3
  read is possible later, but REST is the cleaner, deployment-agnostic path.
- Shiori: similar REST API; SQLite at `/srv/shiori/data/shiori.db`.
- Instance URL is user-supplied — never assume a host; many instances are
  LAN-only/HTTP, so don't hard-require HTTPS for self-hosted targets.
- No documented rate limits (personal instance). Standalone-clean: plain
  HTTP(S) to a server the user runs. No TCC.

## Vault mapping

- **Raw layer:** `reading/linkding/raw/YYYY-MM.jsonl` (and
  `reading/shiori/raw/…` for the Shiori def) — API bookmark objects at full
  fidelity. Page snapshots are not copied into the vault (they stay on the
  instance; vault stores metadata only).
- **Contract layer:** `reading/linkding/` (resp. `reading/shiori/`) per the
  pending Phase 3 reading contract — expected shape: one row per save
  (`ts` = added-at, `source`, `guid` = bookmark id, `url`, `title`,
  `tags[]`, status flags), overflow in `extra`.
- **Dedupe:** bookmark id as `guid`; incremental cursor (modified-since
  watermark where the API supports it, else full-page diff) in
  `.trove/linkding-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/sync/linkding.rs`: Linkding `DEF`
   (Periodic) + `CONNECTION` (TokenPaste: instance URL + token fields,
   setup copy on the def, disabled-state affordance per the SimpleFIN
   lesson), `pull` hook for Sync-now.
2. Shiori `DEF` + `CONNECTION` in the same module, registered separately —
   one brief/provider entry, two defs, mirroring the shared parsing core.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from the documented Linkding response shape (tagged/untagged,
   archived, unread variants); Shiori fixtures from its documented API.
   Parser + store + cursor tests, unique temp dirs.
5. Build Linkding first (cleaner docs); Shiori follows once the shared core
   is proven. Contract rows wait on the Phase 3 reading contract; raw layer
   can land first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Linkding active bookmarks | ✅ built | run Linkding in Docker locally; paste `https://instance|token` in the connect card; Sync now; confirm rows in `reading/linkding/` + hub last-data |
| Linkding archived bookmarks | ✅ built | archive a bookmark in Linkding; Sync now; confirm a row with `"state":"archived"` appears in `reading/linkding/` |
| Incremental cursor (`date_added__gt`) | ✅ built | add a bookmark on the instance, re-sync, confirm only the new row lands |
| Shiori bookmarks | deferred | Shiori has no registered stub in INDEX; out of scope for this build |

## Build notes (2026-06-17)

- Replaced NotWired stub with full Periodic collector draining both `GET /api/bookmarks/` (active) and `GET /api/bookmarks/archived/` (separate endpoint) with offset pagination.
- TokenPaste connection: composite `url|token` string (pipe is RFC 3986 safe for URLs).
- Raw layer: `reading/linkding/raw/YYYY-MM.jsonl` — full-fidelity API objects unconditional.
- Contract layer: `reading/linkding/YYYY-MM.jsonl` — `reading::Item` rows, deduped by `id` (integer → string guid).
- Cursor: `.trove/linkding-sync.json` with `last_added` (max `date_added` seen across successfully-mapped items from both endpoints); passed as `date_added__gt` on next run. Writer is append-only; `date_added` is the correct watermark for new-bookmarks incrementality.
- Field mapping: `id`→guid, `url`→url, `title`→title, `description`→excerpt, `tag_names`→tags, `date_added`→ts, `is_archived`→state("archived"/"saved"); overflow in `extra` (notes, shared, unread, date_modified, web_archive_snapshot_url, favicon_url, preview_image_url).
- 25 unit tests; all green. cargo check clean.
- Shiori not built: no stub registered in the INDEX; brief's suggestion deferred.

## Fix notes (2026-06-17 — post-adversarial-review)

- **Archived endpoint**: archived bookmarks live at a separate `GET /api/bookmarks/archived/` endpoint (`is_archived` is always `false` on the active endpoint). Added `drain_endpoint(archived=true)` call so archived bookmarks are actually collected.
- **Cursor corrected**: switched from `modified_since`/`date_modified` to `added_since`/`date_added` (`date_added__gt` query param). The writer is append-only (existing guids skipped), so only new additions matter; `date_added` is the right watermark. Old `last_modified` cursor fields in existing sync files are silently ignored (serde unknown-field default), triggering a safe one-time full re-drain.
- **Cursor-advance safety**: cursor advances only for items that were successfully mapped by `bookmark_to_item`, never from raw items that fail parsing.

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Linkding
/ Shiori (Self-Hosted Bookmark Managers) (L1696–L1703); cross-cutting note 1
(L1706) — token auth dominates this domain, share the HTTP/token plumbing
across the reading collectors. Feasibility 🟡 medium purely on audience
size; the APIs are clean and the self-hosting users align perfectly with
Trove's privacy-first ethos. The same-Mac SQLite read (M3) is a real later
option for Linkding (`/etc/linkding/data/db.sqlite3` in the Docker volume)
but adds Docker-volume pathing complexity for no fidelity gain over REST.

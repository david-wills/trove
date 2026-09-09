# Readwise + Readwise Reader

- **id:** `readwise`
- **domains:** `reading/` — **first-in-domain collector; this build binds the
  `reading/` contract** (`Item` + `Highlight` Rust types + the `reading` DOMAINS
  entry in `contracts.rs`; the `reading.item`/`reading.highlight` fixtures are
  promoted to the ratified triad in `spec_validation`). kindle / instapaper /
  raindrop / pinboard / pocket / feedly / inoreader / etc. follow this shape.
- **status:** 🧪 built (fixture-tested, not validated) — **Needs-login**
- **unavailable_reason:** none
- **behavior:** `Behavior::Periodic` — hourly (`READWISE_SYNC_SECS = 3600`),
  every-on-run cadence (the timer only advances when it actually runs, so
  re-enabling fires immediately). Both endpoints poll with an `updatedAfter`
  ISO cursor; the first sync backfills everything, later syncs are incremental.
- **connection:** `readwise` — TokenPaste (a Readwise access token from
  **readwise.io/access_token**; no OAuth dance), stored 0600 at
  `.trove/sync/readwise`, verified at connect with a real `GET /api/v2/auth/`.
  **One def, two pulls** (not two defs): the single pasted token serves both the
  Readwise (v2) highlights pull and the Reader (v3) documents pull.
- **default:** off (`default_on: false`) — a Needs-login cloud sync, off until
  the user connects a token.
- **evidence:** official-docs — readwise.io/api/v2 (`/export/`, `/auth/`) and
  /api/v3/list (Reader documents), documented rate limits and `updatedAfter`
  params; widely used, many reference clients. **Opus verify re-confirmed the v2
  `/export/` + v3 `/list/` shapes from primary docs (model policy) before parse.**
- **effort / priority:** S / P1
- **needs:** none — the reading contract is now **ratified by this build**; live
  validation needs a real Readwise token (a Needs-login item, no app registration).

## What it is

The reading hub: Readwise aggregates highlights from Kindle, Apple Books,
web articles, and PDFs into one account; Reader is its companion read-later
app (articles, PDFs, emails, RSS). One token-based integration covers
sources that are individually hard or impossible (Kindle has no native
Amazon API) — the highest-leverage single pull in the reading domain.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Highlights + books (v2 export) | all plans | Book + Highlight records: title, author, category, source, tags, notes, timestamps; `parent_id` links highlight → source doc | official docs, research L3471–L3477 |
| Reader documents (v3 list) | all plans | saved articles/PDFs/emails/RSS with highlights + location data | official docs, research L1520–L1527 |
| Feed OPML | all plans | Reader feed subscriptions (account-page export) | research L1525 |

All optional in the contract; a Readwise-only (no Reader) account simply
yields no document rows.

## Access & auth

- **Readwise:** `GET https://readwise.io/api/v2/export/` — full highlights +
  books, `updatedAfter` ISO-8601 cursor. Token header auth.
- **Reader:** `GET https://readwise.io/api/v3/list/?updatedAfter=` — same
  auth scheme.
- **Token-count ambiguity — RESOLVED at build:** the research notes disagreed
  on whether Reader (v3) needs a *separate* token from Readwise (v2). Both
  endpoints document the same `Authorization: Token XXX` scheme against the same
  account, so the build tries **the one pasted token for both**. If v3 rejects it
  (401), the Reader leg falls back to a second token — `TROVE_READWISE_READER_TOKEN`
  (env → baked, empty default) — for the Reader pull only; if that's absent, the
  **highlights leg still succeeds and the Reader leg is a clean skip** (no error).
  The single pasted token is the only connect field.
- Rate limits: export/LIST endpoints 20 req/min — fine for an incremental
  personal pull (a very large library backfills over a few minutes). Both
  endpoints paginate with `nextPageCursor` (request param `pageCursor`); the
  pull follows the cursor until null.
- No TCC, no local files. Standalone-clean (plain HTTPS, `reqwest`).

## Vault mapping

- **Raw layer (unconditional, full fidelity):** `reading/readwise/raw/YYYY-MM.jsonl`
  — the API Book/Highlight/Document objects verbatim, partitioned by
  highlight/save time.
- **Contract layer — two streams:**
  - **Highlights** (v2 `/export/`) → `crate::reading::Highlight` under
    `reading/readwise/highlights/YYYY-MM.jsonl`. `guid` = the Readwise highlight
    id; `ts` = `highlighted_at` (ISO → **local**); the parent book `title`/`author`
    carried inline; `category` / `highlighted_at` / source ids ride in `extra`.
  - **Reader documents** (v3 `/list/`) → `crate::reading::Item` under
    `reading/readwise/YYYY-MM.jsonl`. `guid` = the document id; `ts` = `saved_at`
    (ISO → local); `reading_progress` (0..1 fraction) → an **integer percent
    0–100** (the PHASE3-REVIEW `reading.progress` carry-forward, clamped);
    `location` → a coarse `state`. **Reader highlight/note *child* docs are
    excluded from the item stream** (they are not saves — `4fae883`).
- **Dedupe:** Readwise ids as `guid` (skip guids already held). A **per-endpoint**
  `updatedAfter` watermark lives in `.trove/readwise-sync.json` (non-secret,
  rebuildable by scanning output files); it advances **only after a full drain**,
  so a crash re-drains rather than skips, and a partial export failure leaves the
  watermark untouched (nothing strands). The token never touches the cursor.
  Cross-source note: Kindle-clippings and Apple Books data may arrive twice
  (native import + via Readwise) — streams stay per-source; read-time views
  handle the overlap, raw data is never merged or dropped.

## Build plan — DONE (2026-06-15, INDEX #24)

1. ✅ Module `crates/trove-core/src/readwise.rs`: `DEF` (`Behavior::Periodic`,
   hourly, default-off), `CONNECTION` (TokenPaste: label/help per the SimpleFIN
   affordance rule), `def_pull` for Sync-now + `def_collect` for the loop (quiet
   no-op on a missing token / network blip). **One def, two pulls** (the Google
   model permits it; the single token serves both endpoints).
2. ✅ Registration: one line in `INTEGRATIONS`, one in `CONNECTIONS`; `pub mod`
   in `lib.rs`. (Registry projection: `readwise` Not-wired → Periodic in
   `docs/integration-schedule.md`.)
3. ✅ **First-in-domain contract bind:** `crate::reading::{Item, Highlight}` +
   the `reading` DOMAINS entry in `contracts.rs` + the `reading.item` /
   `reading.highlight` fixtures promoted to the ratified triad in
   `spec_validation` (`spec_validation` 5/5 green).
4. ✅ Fixtures from documented v2 export + v3 list shapes (highlight with/without
   note, location range, Reader doc, progress-fraction → percent, child-doc
   exclusion); parser + store + two-watermark drain + 401-fallback + 0600-token
   + cursor back-compat tests (15 unit tests), unique temp dirs.

## Validation matrix

Built + **fixture-green** (15 unit tests in `readwise.rs` + `spec_validation`
5/5 + workspace `cargo check` + regenerated `schedule_doc` + bindings clean).
Promotion to ✅ needs David's real token (Needs-login).

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connect + auth | 🧪 fixture | `connection_stores_token_0600_and_absent_from_cursor`, `empty_token_rejected_and_pull_needs_connection`, `connection_exposes_token_paste_method`. **David:** open **readwise.io/access_token** (signed in) → copy the token → paste it into the **Readwise + Readwise Reader** connect card → it verifies via `GET /api/v2/auth/` and stores 0600 at `.trove/sync/readwise`. |
| Highlights (v2 `/export/`) | 🧪 fixture | `maps_highlight_with_note_tags_location_and_inline_parent`, `maps_noteless_highlight_with_location_range`, `tag_names_handles_object_array_and_strings`. **David:** enable the toggle (it's default-off) → **Sync now** → confirm `Highlight` rows in `reading/readwise/highlights/YYYY-MM.jsonl` (text + book title/author; notes where present) + the lossless `reading/readwise/raw/` + the hub "last data" date. |
| Reader docs (v3 `/list/`) | 🧪 fixture | `maps_reader_doc_progress_fraction_to_integer_percent_and_state`, `progress_clamps_and_states_map`, `reader_child_docs_are_not_saved_items`. **David:** same token, same Sync (works on the **free plan**) → confirm `Item` rows in `reading/readwise/YYYY-MM.jsonl` with `progress` as an **integer 0–100** and a coarse `state`; confirm highlight/note *child* docs did **not** create item rows. |
| Reader-token fallback / clean skip | 🧪 fixture | `reader_401_without_fallback_skips_reader_but_keeps_highlights`. **David (only if needed):** if the card connects but **no** Reader documents appear and the highlights still sync, your token lacks Reader scope — set a second token via `export TROVE_READWISE_READER_TOKEN=…` in `~/.zshrc` and re-Sync. (Most accounts need only the one pasted token.) |
| Incremental cursor (two watermarks) | 🧪 fixture | `full_pull_writes_both_layers_dedupes_and_advances_two_watermarks`, `partial_export_drain_failure_does_not_advance_watermark`, `drain_follows_next_page_cursor_to_completion`, `parse_page_reads_results_and_next_cursor_and_bare_array`, `cursor_back_compat_empty_and_partial_deserialize`. **David:** **Sync now** a second time with no new highlights/saves → row counts in both streams stay stable (pulls only what changed). |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Readwise
+ Readwise Reader (L1520–L1527) and "Media: Books, Reading & Gaming"
§Readwise (L3471–L3477) — both 🟢 high, "build now". Richest highlights API
available; sequence it first in the reading domain so the Phase 3 reading
contract is drafted against the richest source, with Instapaper / Raindrop /
Pinboard exercising it after.

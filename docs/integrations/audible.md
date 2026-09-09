# Audible

- **id:** `audible`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified** — listening
  spans derived from position deltas) · `media/audible/` (library snapshots:
  titles, purchase dates, positions — per the media-curation routing rule)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the library endpoint; diff listening positions)
- **connection:** `audible` — unofficial Amazon device-registration flow (the
  user signs in with Audible credentials; an RSA key exchange registers Trove as
  a "device" and yields refreshable tokens). Neither plain OAuth nor TokenPaste —
  closest `ConnectMethod` fit is decided at build (likely an OAuth-style webview
  step). Not shared with other defs.
- **evidence:** community-schema — mkb79/Audible Python library
  (audible.readthedocs.io), well-maintained (updated Jan 2026), endpoints and
  auth flow documented in library source; Libation and OpenAudible use the same
  paths. High community confidence; zero official support.
- **effort / priority:** M / P2
- **needs:** Needs-login (spike the auth flow with a real account before build) ·
  breakage risk: Amazon could close the flow at any time

## What it is

Amazon's audiobook platform — for audiobook listeners it holds the entire library,
purchase dates, finish status, and last listening position per title. There is no
official API and no data export with listening data; the unofficial internal API
(reverse-engineered and kept current by the mkb79/Audible community, used by
Libation and OpenAudible) is the only programmatic path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Library | any Audible account | ASIN, title, authors, purchase_date | mkb79 docs: `GET /1.0/library?response_groups=product_details,product_attrs` |
| Listening position | any | `last_position_heard` (offset ms + total length ms) | mkb79 docs: `response_groups=last_position_heard` |
| Finish status / progress | any | derivable from position vs total length | mkb79 docs |

No per-play history exists — positions are snapshots; plays are *derived* by
diffing successive polls.

## Access & auth

- Unofficial internal REST API; auth is Amazon's device-registration flow
  (username/password or OAuth2-style login, then RSA key exchange) — documented in
  the mkb79/Audible library source. The Python library itself can't be compiled
  into the Rust binary: **reimplement the HTTP auth in Rust** (preferred,
  standalone-clean) or, failing that, the M6 bundled-script shape — never a
  runtime Python dependency.
- Marketplace-specific endpoints (audible.com / .co.uk / .de …) — derive from the
  login, don't hardcode.
- No TCC, no local files. Breakage risk is the defining property: on auth or
  schema failure, disable gracefully with an honest card hint.

## Vault mapping

- **Raw layer:** `media/audible/raw/YYYY-MM.jsonl` — library API responses
  (snapshots with positions), full fidelity.
- **Contract layer:** `media/plays/audible/YYYY-MM.jsonl` per media-plays —
  position-delta spans: when a title's `last_position_heard` advances between
  polls, write `ts` = poll time of the advance (start-of-span best estimate),
  `category:"audiobook"`, `kind:"play"` (or `"partial"` for small skips),
  `title` = book title, `subtitle` = author, `seconds` = position delta in
  seconds, ASIN + offsets in `extra`. Honest unknowns: backfill before the first
  poll is impossible — a finished book imports as one `kind:"play"` row at its
  finish status with `seconds: 0` only if a finish date exists.
- **Dedupe:** `guid` = `audible-<asin>-<from_offset_ms>` for spans; ASIN for
  library rows. Poll watermark + last-known positions in
  `.trove/audible-sync.json`, rebuildable from raw snapshots.

## Build plan

1. **Spike first** (research doc's call): reimplement the device-registration auth
   in Rust against a real account; capture library responses as fixtures.
   Needs-login gates this.
2. Module `crates/trove-core/src/audible.rs`: `pub static DEF` (Periodic),
   `pub static CONNECTION` (method per spike outcome; honest "unofficial API"
   card copy), `pull` hook.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Position-delta engine + tests: advance, regress (re-listen), multi-device
   jumps, new title, removed title — unique temp dirs.
5. Graceful-failure path (status line + disabled card) for the day Amazon breaks
   the flow.

## Build notes (2026-06-17)

- **Auth**: Amazon device-registration flow (OAuth + PKCE + RSA signing) is complex and requires a real account to spike. The `CONNECTION` uses a `TokenPaste` accepting a pre-serialized JSON credential blob (shape: `AudibleCreds` with `access_token`, `refresh_token`, `adp_token`, `device_private_key`, `api_url`). The HTTP pull scaffold uses Bearer-only auth pending the full RSA-signing spike. **Needs-David: complete the auth spike with a real account before end-user use.**
- **Position-delta engine**: PARKED. `last_position_heard` is NOT a valid `response_group` for `GET /1.0/library` (confirmed against mkb79/Audible external_api.rst — it is only valid on `/1.0/content/{asin}/licenserequest`). The valid progress groups for `/1.0/library` are `listening_status` and `percent_complete`, but their field shapes are undocumented in the primary source. Shipping a parser with invented field names would silently produce zero contract rows. The engine will be activated once a real-account spike captures a sample confirming the field paths.
- **Raw layer**: `media/audible/raw/YYYY-MM.jsonl` — full library API responses, partitioned by poll month. Each row embeds `_polled_at` (RFC3339) so snapshots are timestamped and the watermark is rebuildable from raw. Progress fields (`listening_status`, `percent_complete`) land in raw as-is via `#[serde(flatten)]` for full fidelity with no shape assumptions.
- **Contract layer**: PARKED — `media/plays/audible/YYYY-MM.jsonl` will be populated once the progress field shape is confirmed.
- **response_groups in use**: `product_desc,product_attrs,contributors,listening_status,is_finished,percent_complete` (all valid for `/1.0/library` per primary docs).
- **parser_parked_needs_sample**: `true`.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Auth spike | ❌ Needs-David | Complete device registration with a real Audible account; tokens refresh across restarts |
| Library snapshot | ❌ Needs-login | After auth spike: Sync now; `media/audible/raw/` rows match the in-app library count; each line has `_polled_at` field |
| Raw timestamp | ✅ unit-tested | `cargo test -p trove-core audible::tests::pull_with_embeds_poll_timestamp_in_every_raw_row` |
| Progress field shape | ❌ Needs-sample | Capture a real library response; confirm `listening_status` / `percent_complete` field names; update parser |
| Derived plays | ❌ Needs-sample+login | After parser is activated: listen ~10 min; wait a poll; one span row in `media/plays/audible/` |
| Raw layer + pagination | ✅ unit-tested | `cargo test -p trove-core audible::` — 9 tests green |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Audible
(L3519–L3525) + cross-cutting note 4 (unofficial-API risk handling). Feasibility
🟡 medium; "build later — implement after Readwise/Kindle." mkb79/Audible is
AGPL-3.0 — reimplementing the documented HTTP flow in Rust (not linking the
library) keeps licensing clean and the binary standalone. Kindle/Readwise cover
the *reading* side of Amazon books; this is the only listening-side source.

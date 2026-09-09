# Bandcamp

- **id:** `bandcamp`
- **domains:** `finance/purchases/` (contract: **Phase 3 pending** —
  purchase line-item shape; per-source subfolders)
- **status:** 🧪 built (raw Import; contract parser parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (CSV produced by a community browser extension; no
  official buyer export)
- **connection:** none (the user logs into bandcamp.com themselves to run
  the extension; Trove holds no credential)
- **evidence:** community-schema — github.com/rxdazn/bandcamp-purchase-history
  (Chrome extension that DOM-scrapes bandcamp.com/purchases to CSV);
  medium confidence, no official format — **sample-required**
- **effort / priority:** M / P2
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement) · Needs-sample (extension CSV format is community
  folklore — parser built last, against a real export)

## What it is

Bandcamp is where people *buy* music — DRM-free albums downloaded and
played in local apps. There is no listening history to collect (plays of
Bandcamp-bought files are already captured by the music scrobbler); the
value here is the ownership record: artist, album, date purchased, price,
format. "I own this album" vs "I streamed this album" is meaningful
context next to a music library, and it's also genuine purchase history.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Purchase history CSV | any buyer account (via community extension) | artist, album, date purchased, price, format (MP3/FLAC/…) | community extension README |
| Listening history | — does not exist as a concept on Bandcamp | none | research doc |
| Official buyer API | — none; bandcamp.com/developer is seller/label-only (sales reports) | none | official docs |

All optional in the contract (omit-if-empty); no tier gating.

## Access & auth

- No official export for buyers and no buyer API. The documented path:
  user installs the community Chrome extension, logs into bandcamp.com,
  visits `/purchases`, exports CSV, and drops the file into Trove's
  import box. The brief's setup copy must walk the user through this
  (extension link + steps) — Trove itself never scrapes and never holds
  the login.
- No TCC, no network from Trove's side. Standalone-clean (the extension
  is the user's tool, not a runtime dependency — same posture as any
  "go export your data from the service" import).

## Vault mapping

- **Raw layer:** `finance/purchases/bandcamp/` — the CSV as received plus
  parsed rows, partitioned `YYYY-MM.jsonl` by purchase date.
- **Contract layer:** purchase line-item rows per the (pending) Phase 3
  purchases contract: `ts` = purchase date, `source`, merchant =
  "Bandcamp" / artist as line-item detail, amount/currency, `guid` =
  hash of date+artist+album (the scrape has no stable ids); format and
  album metadata in `extra`. Ownership-vs-streamed views join against
  `media/` at read time — records route whole, nothing is split.
- **Dedupe:** content-hash guid; re-importing a fresh, longer export is
  idempotent over the overlap.

## Build plan

1. Module `crates/trove-core/src/bandcamp.rs`: `DEF` (Import, with setup
   copy explaining the extension route); one line in `INTEGRATIONS`.
2. **Parser-last (Needs-sample):** the CSV columns above come from the
   extension's README, not a spec — acquire a real export before writing
   the parser; build the def/UI shell first.
3. Header-sniff defensively (extension versions may drift); unknown
   columns pass through to `extra`.
4. Privacy gate: opt-in enable with explicit acknowledgement (financial
   detail).
5. Contract rows wait on the Phase 3 purchases contract; raw import can
   land first.

## Build notes (2026-06-17)

- **status:** 🧪 built (raw layer + Import behavior wired; contract layer parked)
- **behavior:** `Import` — CSV only (no ZIP); single-click import box
- **contract_mode:** `reuse-bound` (finance-purchases / LineItem) — parser
  parked pending a real sample; raw layer unconditional
- **CSV format confirmed from extension source (`popup.js`):** 14 semicolon-
  delimited columns: `payment_date`, `bandcamp_id`, `artist_name`, `item_title`,
  `quantity`, `unit_price`, `tax`, `tax_type`, `currency`, `card_brand`,
  `card_num`, `payer_email`, `item_url`, `download_url`. The extension builds a
  data URI with URL-encoded content (`%3B` delimiter, `%22value%22` fields) —
  the parser detects and handles both the encoded and browser-decoded variants.
- **Value formats NOT confirmed** (no real export sample exists): `payment_date`
  pattern, `unit_price` decimal convention. Parser skeleton in
  `try_map_to_line_item` with `PARSER_ACTIVE = false`.
- **Re-wiring:** set `PARSER_ACTIVE = true` + implement the stub once a sample
  lands; the raw layer preserves full fidelity so no re-import is required.
- **Tests:** 8/8 green — plain CSV, URL-encoded CSV, data-URI prefix stripping,
  idempotent re-import, empty CSV no-op, non-CSV rejection, hub card, decode fn.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Purchase CSV import (raw) | ✅ built | drop the CSV from the extension; rows land in `finance/purchases/bandcamp/raw/YYYY-MM.jsonl`; hub last-data updates; re-import dedupes |
| Purchase CSV import (contract LineItem) | 🅿️ parked | needs a real export sample to confirm value formats; flip `PARSER_ACTIVE = true` in `bandcamp.rs` |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§Bandcamp (L3398–L3404). Feasibility 🟡 medium for purchases, low/N-A for
listening (doesn't exist — local plays are the scrobbler's job; routed to
`finance/purchases/`, not `media/plays/`, per the taxonomy). Filed under
finance because the record's shape is a purchase, even though the user
thinks of Bandcamp as music — the brief and hub copy should say both.
Part of the shared M1 importer pattern noted in the catalog cross-cutting
notes (one drag-drop pipeline, per-format parsers). Risk: extension is a
single community project scraping live DOM — format may break; the raw
CSV preserved in the vault protects past imports.

# Tinder

- **id:** `tinder`
- **domains:** `social/` (swipe stats, profile, purchases — per-source raw
  under `social/tinder/`; social-posts contract **Phase 3 pending** doesn't
  apply to dating data) · `correspondence/` (per-match message threads —
  contract: **✅ ratified**)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (official data export, JSON)
- **connection:** none (export is requested on Tinder's site, then dragged
  in)
- **evidence:** official-docs — export at `account.gotinder.com/data`;
  format community-confirmed stable and parseable (SwipeStats.io analyzes
  exactly this file)
- **effort / priority:** S / P2
- **needs:** privacy (dating app — **ships opt-in with explicit
  acknowledgement**; contents include match history, full message text,
  and swipe behavior) · Needs-David (dating-app opt-in UX + vault
  isolation sign-off)

## What it is

The largest dating app. Its export is a candid behavioral record — daily
swipe counts, every match, and full message threads — that exists nowhere
else on the user's devices once the app is deleted. Technically one of the
easiest imports in the catalog; the work here is privacy treatment, not
parsing.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Swipe statistics | none | daily right/left/super counts (`Usage`) | official export |
| Matches + messages | none | per-match thread with timestamps, full message text (`Messages`) | official export |
| Profile + preferences | none | account info, settings | official export |
| Photos | none | CDN URLs (not files) | official export |
| Purchases / ad data | none | purchased products, ad interactions | official export |

Matches are **ID-only — no names** (Tinder's own privacy design); threads
import keyed by match id.

## Access & auth

- Export: `account.gotinder.com/data` (or Settings > Get My Data in-app).
  Returns `data.json` (sometimes zipped). 1–3 days to generate; **download
  link expires 48 hours after generation** — UI copy must say "import
  promptly".
- No API, no TCC, no local files. Standalone-clean.

## Vault mapping

- **Raw layer:** `social/tinder/raw/` — the export JSON in full fidelity,
  vault-isolated in its own per-source folder per the dating-app rule.
- **Contract layer:** message threads map to the ratified
  `correspondence/` contract (`correspondence/tinder/`): one row per
  message, `guid` = match id + message timestamp/index, thread = match id,
  counterpart = match id (no display name available). Swipe stats,
  profile, and purchases stay raw under `social/tinder/` — no contract
  pending for them.
- **Dedupe:** stable match-id-derived guids; re-import upserts.

## Build plan

1. **Privacy gate first:** opt-in category with explicit acknowledgement
   on enable (per the privacy needs-flag rule); copy states what the
   import contains (matches, full message text, swipe behavior).
   Needs-David sign-off on the opt-in UX and vault-isolation shape before
   build.
2. Module `crates/trove-core/src/tinder.rs`: `DEF` with
   `Behavior::Import`; one line in `INTEGRATIONS`; no connection.
3. **Build alongside Hinge as one dating-app import category** with shared
   parsing/import logic — the research doc recommends one pattern for
   Tinder/Hinge (Bumble/OkCupid join later, parser-last).
4. Parser for `data.json` (Usage / Messages / Photos / Purchases
   sections); fixtures from a sanitized real export; correspondence rows
   via the `store` helpers.
5. UI: surface the 48-hour link expiry in the import-box copy.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Opt-in gate | — | enable the integration; confirm explicit acknowledgement is required before any import is possible |
| Messages → correspondence | — | request a real export, import `data.json`, confirm thread rows in `correspondence/tinder/` keyed by match id |
| Swipe stats raw | — | same import; confirm Usage daily counts under `social/tinder/` + hub last-data |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Tinder
(L4104–L4110); cross-cutting note 4 (dating apps: opt-in category,
per-source folders `social/<source>/`, privacy-sensitive needs-flag,
shared JSON import pattern across Tinder/Hinge). Feasibility 🟢 high
technically. SwipeStats.io's existence confirms the format is stable and
third-party-parseable. Match names are not in the export by design; Hinge
(the sibling brief) does include names. The research doc's
`social/dating/` path suggestion predates the taxonomy — the taxonomy's
`social/tinder/` + `correspondence/` routing wins.

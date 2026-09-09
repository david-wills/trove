# Hinge

- **id:** `hinge`
- **domains:** `social/` (like/skip/block events, profile — per-source raw
  under `social/hinge/`; social-posts contract **Phase 3 pending** doesn't
  apply to dating data) · `correspondence/` (match message threads —
  contract: **✅ ratified**)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (official in-app data export, ZIP of JSON)
- **connection:** none (export is requested in-app, then dragged in)
- **evidence:** official-docs — in-app export (Settings > Download My
  Data); `matches.json` / `events.json` structure community-documented
- **effort / priority:** S / P2
- **needs:** privacy (dating app — **ships opt-in with explicit
  acknowledgement**; contents include match names, full message threads,
  and like/skip activity) · Needs-David (dating-app opt-in UX + vault
  isolation sign-off)

## What it is

Major dating app ("designed to be deleted") owned by Match Group. Its
export captures the full relationship-formation record — matches *with
names* (unlike Tinder), complete message threads, and like/skip/block
event history. Same technically-easy / privacy-heavy profile as Tinder;
the two ship together as one opt-in dating-app import category.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Matches + messages | none | match history incl. names/identifiers, full message threads with timestamps (`matches.json`) | community-documented |
| Activity events | none | like/skip/block events with timestamps (`events.json`) | community-documented |
| Profile | none | profile data (`user.json`) | community-documented |
| Media | none | photos (`media/`) | community-documented |

All optional in the contract mapping; an inactive account simply yields a
profile and little else.

## Access & auth

- Export: Settings > Download My Data (in-app). ZIP of JSON files; 1–3
  days to generate.
- No API, no TCC, no local files. Standalone-clean.

## Vault mapping

- **Raw layer:** `social/hinge/raw/` — the export's JSON files in full
  fidelity, vault-isolated in its own per-source folder per the
  dating-app rule.
- **Contract layer:** `matches.json` threads map to the ratified
  `correspondence/` contract (`correspondence/hinge/`): one row per
  message, thread = match, counterpart = match name/identifier (Hinge
  includes names — richer than Tinder here). `events.json`, profile, and
  media metadata stay raw under `social/hinge/` — no contract pending for
  them.
- **Dedupe:** match identifier + message timestamp/index as `guid`;
  re-import upserts.

## Build plan

1. **Privacy gate first:** same opt-in category and explicit
   acknowledgement as Tinder; copy states contents (match names, full
   message text, like/skip behavior). Needs-David sign-off on the opt-in
   UX and vault isolation before build.
2. **Build alongside Tinder** as one dating-app import category with
   shared import/ZIP plumbing; Hinge gets its own module
   `crates/trove-core/src/hinge.rs` + one `INTEGRATIONS` line (distinct
   service, shared pattern). No connection.
3. Parser for `matches.json` / `events.json` / `user.json`; fixtures from
   a sanitized real export; correspondence rows via `store` helpers.
4. Privacy framing in UI may note the broader Match Group context (2026
   FTC action over OkCupid data sharing) — honest grounding for why this
   category is opt-in.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Opt-in gate | — | enable the integration; confirm explicit acknowledgement is required before any import is possible |
| Messages → correspondence | — | request a real export in-app, import the ZIP, confirm thread rows in `correspondence/hinge/` with match names attached |
| Events raw | — | same import; confirm like/skip/block events under `social/hinge/` + hub last-data |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Hinge
(L4112–L4118); cross-cutting note 4 (dating apps: opt-in category,
per-source folders `social/<source>/`, shared JSON import pattern).
Feasibility 🟢 high technically; privacy-sensitive by nature. Key
difference vs Tinder: Hinge includes match names; Tinder is match-ID-only.
Bumble (30-day wait, Needs-sample) and OkCupid (support-ticket only,
Needs-sample) extend the same category later, parser-last. The research
doc's `social/dating/` path suggestion predates the taxonomy —
`social/hinge/` + `correspondence/` routing wins.

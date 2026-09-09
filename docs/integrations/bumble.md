# Bumble

- **id:** `bumble`
- **domains:** `social/bumble/` (raw + per-source; social-posts contract is
  **Phase 3 pending**, though dating data is mostly per-source raw),
  `correspondence/` (match message threads — contract: **✅ ratified**)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (user drops the export ZIP; no API exists)
- **connection:** none (export is requested inside the Bumble app; nothing to
  OAuth or token-paste)
- **evidence:** official request flow exists (Settings > Contact & FAQ >
  Request My Data) but the JSON schema is poorly documented publicly —
  **sample-required**; research-doc level: 🟡 Medium
- **effort / priority:** S / P2
- **needs:** privacy (dating app — opt-in with explicit acknowledgement,
  vault-isolated) · Needs-sample (schema undocumented; parser-last) ·
  Needs-David (dating-app opt-in UX + vault isolation sign-off)

## What it is

Dating app (the "women message first" one). The export covers account data,
match history, and full message threads — relationship-formation history
that exists nowhere else on the user's machine. Third of the dating-app
trio behind Tinder and Hinge: same import pattern, slower export and
murkier format.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Account data | all accounts | profile, registration, settings | official request flow; contents not itemized publicly |
| Match history | all accounts | matches with timestamps | expected per research doc; schema unverified |
| Messages | all accounts | per-match threads, full text + timestamps | expected per research doc; schema unverified |

All optional in the contract; exact field availability is confirmed only
when a real sample lands.

## Access & auth

- In-app data request: Settings > Contact & FAQ > Request My Data. **Up to
  30 days processing** (vs 1–3 days for Tinder/Hinge) — a real UX friction
  point; the import box copy must set that expectation.
- Returns a ZIP with JSON. No API, no self-serve web flow documented.
- No TCC, no network calls from Trove — pure local file import.
  Standalone-clean.

## Vault mapping

- **Raw layer:** `social/bumble/raw/` — the export files verbatim
  (taxonomy: dating-app data lives under `social/<source>/`, NOT the
  research doc's old `social/dating/` path).
- **Contract layer:** message threads normalize to `correspondence/` rows
  (ratified contract: `ts`, `source: "bumble"`, `guid`, handle = match id,
  direction, body); match/profile/usage records stay per-source raw under
  `social/bumble/` — they are not posts, so the pending social-posts
  contract likely doesn't apply.
- **Dedupe:** `guid` from match id + message timestamp/index (confirm
  stable ids against the sample); re-imports must be idempotent.

## Build plan

1. **Parser-last** (Needs-sample): sequence after Tinder + Hinge prove the
   shared dating-app import pattern; reuse their opt-in category, ZIP
   detection, and correspondence mapping.
2. Module `crates/trove-core/src/bumble.rs`: `DEF` (Import), import hook;
   one registration line in `INTEGRATIONS`. No `CONNECTION`.
3. Privacy gate: ships opt-in with explicit acknowledgement (match history
   + message content); same UX David signs off for the whole dating
   category.
4. Fixtures only once a real export exists (anonymized); until then the
   card can ship as a NotWired/planned stub with the 30-day-wait note.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| ZIP import end-to-end | — | request export in-app (allow up to 30 days); drop ZIP on the import box; confirm `social/bumble/` raw files + `correspondence/` rows + hub last-data |
| Message thread fidelity | — | spot-check a known conversation's text/timestamps against the app |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Bumble
(L4120–L4126); cross-cutting note 4 (dating apps: opt-in category,
`social/<source>/` folders, shared parsing). Feasibility 🟡 Medium purely
on documentation/wait-time grounds — the export itself is official and
GDPR-mandated. Research doc recommended icebox until the Tinder/Hinge
pattern is proven; this brief keeps that sequencing.

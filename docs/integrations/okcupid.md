# OkCupid

- **id:** `okcupid`
- **domains:** `social/okcupid/` (per-source raw; social-posts contract is
  **Phase 3 pending**, though dating data is mostly per-source raw),
  `correspondence/` (message threads — contract: **✅ ratified**)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (user obtains a GDPR/CCPA export via support ticket
  and drops it in; no API)
- **connection:** none
- **evidence:** support-ticket GDPR request only (no self-serve menu as of
  2026); format undocumented — **sample-required**; research-doc level:
  🟠 Low
- **effort / priority:** M / P2
- **needs:** privacy (dating app — opt-in with explicit acknowledgement,
  vault-isolated) · Needs-sample (format undocumented; parser-last) ·
  Needs-David (dating-app opt-in UX + vault isolation sign-off)

## What it is

Dating app (Match Group, like Tinder and Hinge) built around long-form
profiles and match questions. Export would carry profile answers, match
history, and messages. The weakest of the dating-app quartet: no self-serve
export, undocumented format, and a March 2026 FTC enforcement action
(OkCupid shared ~3M user photos with Clarifai for AI training without
consent) — context that belongs in the privacy framing the app shows.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Account/profile data | all accounts | profile, question answers (expected) | support-request export; contents unverified |
| Match history | all accounts | matches with timestamps (expected) | unverified — no public schema |
| Messages | all accounts | threads with timestamps (expected) | unverified — no public schema |

Everything above is expectation, not documentation — the brief firms up
when a real export sample lands.

## Access & auth

- No self-serve export. User contacts OkCupid support and requests their
  personal data under GDPR/CCPA; returns JSON or CSV on an unspecified
  timeline. The import-box copy must explain this manual flow honestly.
- No API path worth pursuing.
- No TCC, no network calls from Trove — pure local file import.
  Standalone-clean.

## Vault mapping

- **Raw layer:** `social/okcupid/raw/` — export files verbatim (taxonomy:
  `social/<source>/`, not the research doc's pre-taxonomy dating path).
- **Contract layer:** message threads → `correspondence/` (ratified: `ts`,
  `source: "okcupid"`, `guid`, handle, direction, body); profile/match/
  question-answer records stay per-source raw — not posts, so the pending
  social-posts contract likely doesn't apply.
- **Dedupe:** `guid` from whatever stable ids the export carries — decided
  against the sample; re-imports idempotent.

## Build plan

1. **Parser-last** (Needs-sample): last in the dating-app sequence
   (Tinder → Hinge → Bumble → OkCupid); reuse the shared opt-in category,
   ZIP/file detection, and correspondence mapping wholesale.
2. Module `crates/trove-core/src/okcupid.rs`: `DEF` (Import), import hook;
   one line in `INTEGRATIONS`. No `CONNECTION`.
3. Privacy gate: opt-in with explicit acknowledgement; surface the 2026
   FTC-action context in the sensitivity copy so consent is informed.
4. Until a sample exists, ship as a NotWired/planned stub whose card
   explains the support-ticket request flow.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Export import end-to-end | — | obtain a support-ticket export from a real account; drop on import box; confirm `social/okcupid/` raw + `correspondence/` rows + hub last-data |
| Format coverage | — | diff parsed fields against the raw files; log-and-keep anything unrecognized (raw layer stays complete) |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §OkCupid
(L4168–L4174); cross-cutting note 4 (dating-app handling). Feasibility
🟠 Low: no self-serve export, undocumented format, privacy track record.
Research doc verdict: icebox — low incremental value vs Tinder/Hinge.
Catalogued queued (an export path does exist) but deliberately last among
dating apps; effort M reflects the undocumented format, not data volume.

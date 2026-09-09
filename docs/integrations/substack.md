# Substack

- **id:** `substack`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  stats/subscriber data stay per-source raw under `social/substack/`)
- **status:** 📋 queued
- **unavailable_reason:** none (the **writer** side is buildable; the
  reader side has no path — the card says so honestly, see below)
- **behavior:** Import (writer export ZIP + stats CSV)
- **connection:** none
- **evidence:** official writer export (Dashboard → Settings →
  Import/Export — HTML-in-ZIP posts; subscriber CSV; stats CSV added
  March 2026). Reader side: officially confirmed **no export and no API**
  (research L1656–L1663). Unofficial `/api/v1/posts` endpoints exist but
  are ToS-gray and fragile — not a path.
- **effort / priority:** S / P2
- **needs:** Needs-David — subscriber CSV contains reader emails and
  revenue; confirm PII handling before shipping that slice

## What it is

Newsletter publishing. For users who **write** on Substack, the export is
their complete published archive plus audience/revenue stats. For users
who only **read** newsletters there is nothing to integrate — and that
absence is part of this brief: the in-app entry must explain that reading
data arrives via the email integration (newsletters are emails), not via
Substack.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Published posts | writer account | one HTML file per post (title, body, date) | official export |
| Aggregate stats | writer account | stats CSV (no per-post engagement breakdown) | official, added Mar 2026 |
| Subscriber list | writer account | name, email, status, date, revenue — **sensitive PII** | official CSV export |
| Reader subscriptions / reading history | — | **nothing** — no export, no API | confirmed gap (L1660) |

All optional in the contract; a stats-only import is valid.

## Access & auth

- Writer flow: Publication Dashboard → Settings (bottom-left) →
  Import/Export → Export (posts ZIP); Subscriber Dashboard → Export CSV;
  Dashboard → Analytics → Export as CSV. Import box accepts all three.
- Multi-publication writers export each publication separately.
- The unofficial `https://<pub>.substack.com/api/v1/posts` JSON endpoint
  is explicitly out of scope (unofficial, subject to change, ToS-gray).
- No TCC, no network at import time. Standalone-clean.

## Vault mapping

- **Raw layer:** `social/substack/raw/` — post HTML (converted/kept at
  full fidelity), stats CSV rows, subscriber rows (post-PII-decision),
  partitioned per publication.
- **Contract layer:** published posts → `social/substack/` rows per the
  pending social-posts contract (`ts` = publish date, `source`, `guid` =
  post slug/filename, `title`, body text, publication in `extra`). Stats
  and subscribers never join a contract — per-source raw only.
- **Dedupe:** post slug as `guid`; re-export + re-import merges cleanly.

## Build plan

1. Module `crates/trove-core/src/substack.rs`: `DEF` (Import; setup copy
   walks the three export buttons and states plainly: "reader-side data
   has no export — your subscribed newsletters are captured by the Email
   integration").
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. Parsers: HTML-post (title/date/body extraction) and stats CSV. The
   stats CSV columns are not field-level documented — that parser lands
   **parser-last, Needs-sample**; posts ZIP first.
4. Subscriber CSV slice is **gated on Needs-David**: emails + revenue are
   third-party PII in the user's vault. Options to present: import whole
   (it is the user's business data), aggregate-only (counts/revenue,
   drop emails), or skip. Do not ship the slice before the decision.
5. Store via `store` helpers; contract rows wait on social-posts
   ratification — raw layer can ship first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Posts | — | export a real publication; import ZIP; confirm rows in `social/substack/` + hub last-data |
| Stats CSV | — | Needs-sample — capture a real stats CSV, fix the parser to it, then re-validate |
| Subscriber list | — | blocked on the Needs-David PII decision; validate per the chosen mode |
| Reader-side copy | — | confirm the card/setup copy explains the no-reader-data gap and points to Email |

## Research notes

`integrations-research.md` → Web Activity §Substack Reader Subscriptions
(L1656–L1663, 🟠 low: icebox — no stable access path, monitor for an
official API) and Social Media §Substack as writer (L4048–L4054, 🟢 high
for the export). Combined per the one-provider rule: one entry, writer
slice buildable now, reader slice honestly absent. Subscriber CSV
sensitivity is flagged in the research itself ("treat as sensitive PII in
the vault").

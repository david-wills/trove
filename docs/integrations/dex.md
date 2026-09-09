# Dex (Personal CRM)

- **id:** `dex`
- **domains:** `contacts/` (contract: **Phase 3 pending** — contacts)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll only — Dex has no webhooks), with a CSV
  Import path in the same entry
- **connection:** `dex` — TokenPaste (Bearer token; **Professional plan,
  $20/mo, required for the API**). Not shared with other defs.
- **evidence:** api.getdex.com REST documented but paywalled; CSV export
  format documented (header matching on
  first_name/last_name/email/company/tags). Medium confidence — VC-funded
  startup, long-term API stability not guaranteed.
- **effort / priority:** S / P2
- **needs:** Needs-login (a paid Professional account is required to
  validate either path — API and CSV export are both plan-gated)

## What it is

A personal CRM oriented around LinkedIn: it auto-logs interactions from
LinkedIn, Gmail, Calendar, iMessage, and Twitter, and layers on tags,
notes, reminders, and last-contact dates. Niche, but for its users it
holds relationship data that exists nowhere else. Both access paths cost
money, which shapes the whole brief.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Contacts via REST | Professional plan ($20/mo) | name, email, company, tags, notes, last-contact dates | official docs (paywalled) |
| CSV export | paid tier only — free tier cannot export | same field set via Settings → Export Data | documented format |

All optional in the contract. On a 401/403 plan error, surface the
plan requirement on the card (disabled-controls-need-affordance), never
fail silently.

## Access & auth

- REST: `https://api.getdex.com/api/rest/contacts`, Bearer token. Polling
  only — no webhook or push.
- Import fallback: Settings → Export Data → CSV, dragged into the generic
  import box. Note the friction honestly in the card copy: the free tier
  can neither export nor use the API.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `contacts/dex/contacts.jsonl` — API objects or parsed
  CSV rows, full fidelity.
- **Contract layer:** Phase 3 contacts contract pending — one row per
  person, `source: dex`; Dex-specific enrichment (tags, notes,
  last-contact, LinkedIn enrichment) rides `extra` unless adopted.
- **Dedupe:** Dex contact id as `guid` on the API path; the CSV export
  carries no id — name+email composite hash for imported rows, and API
  rows supersede CSV rows for the same composite.

## Build plan

1. Module `crates/trove-core/src/dex.rs`: `DEF` (Periodic, slow tick),
   `CONNECTION` (TokenPaste; setup copy states the Professional-plan
   requirement up front), `pull` hook, plus the CSV import path in the
   same module (one provider, one entry).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures: documented CSV header set; API fixtures from the documented
   REST shapes — confidence is medium (paywalled docs, VC-stability risk),
   so keep the parser tolerant and the raw layer complete.
4. Plan-gate handling: 401/403 → card hint about the Professional plan
   (mirrors the Granola/Otter plan-gating pattern).
5. Sequence after the contacts infrastructure exists (cross-cutting note:
   all personal CRMs are secondary to Apple Contacts + interaction graph);
   Monica first — it exercises the same shape with free validation.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Contacts via API | — | requires a Professional-plan account (David doesn't hold one — any real Dex user's run can validate); paste token, Sync now, confirm `contacts/dex/` rows + hub last-data |
| CSV import | — | paid-tier export dragged into the import box; re-import idempotent |
| Plan-gate affordance | — | invalid/free-tier token → card shows the plan requirement, no silent failure |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§Dex Personal CRM (L712–L718). Feasibility 🟡 medium — the API exists but
is paywalled, the free tier cannot export, and VC-funding makes long-term
API stability uncertain (poll defensively, keep raw fidelity). Cross-
cutting note 7's CRM priority order places Dex after Monica.

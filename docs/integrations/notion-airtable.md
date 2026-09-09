# Notion / Airtable Contacts

- **id:** `notion-airtable`
- **domains:** `contacts` (contract: **not yet ratified** — Phase 3 drafts
  the contacts shape; this source lands raw-first behind a user
  field-mapping step)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll the user-designated contact database/table)
- **connection:** `notion-airtable` — TokenPaste (Notion integration
  secret from notion.so/my-integrations, shared with the specific
  database; or Airtable PAT with `data.records:read`). Not shared with
  other defs.
- **evidence:** official-docs — api.notion.com v1 (database query) and
  api.airtable.com v0 (records list); both stable and well-documented.
  The schemas are *user-defined*, so no fixed record shape exists.
- **effort / priority:** M / P2
- **needs:** Needs-David (confirm demand before building — user schemas
  vary wildly, needs a field-mapping step; plus contract: contacts)

## What it is

Many people keep their personal CRM as a hand-rolled Notion database or
Airtable base. Both APIs are stable; the hard part is that every user
designed their own columns, so Trove can't ship a fixed-schema connector
— it needs a one-time mapping step ("which property is name? email?
phone?") at connect time. One provider entry covers both backends: same
shape of problem, same mapping UI, two thin API clients.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Contact records (Notion) | free API; per-database share grant | whatever the user maps: title/rich-text/select/date/relation properties | official docs |
| Contact records (Airtable) | free PAT; legacy API keys dead since Feb 2024 | whatever the user maps from table fields | official docs |
| CSV export fallback | both UIs export CSV | same columns, one-shot | official docs |

All optional in the eventual contract — unmapped columns ride along in
`extra`, nothing is dropped.

## Access & auth

- Notion: `POST https://api.notion.com/v1/databases/{database_id}/query`,
  Bearer integration secret; the user must share the database with the
  integration.
- Airtable: `GET https://api.airtable.com/v0/{base_id}/{table_name}`,
  PAT with `data.records:read`.
- TokenPaste both ways (no OAuth dance); plain HTTPS, no TCC.
  Standalone-clean. The connect flow carries the extra mapping step —
  this is the one piece the registry doesn't give for free.

## Vault mapping

- **Raw layer:** `contacts/notion-airtable/contacts.jsonl` — one record
  per line carrying the raw property bag plus the resolved mapped fields;
  a `backend` field (`notion`|`airtable`) and the database/base id
  distinguish sources. Mapping config in `.trove/` (rebuildable
  preference, not data). *(Taxonomy wins: per-source folder under
  `contacts/`.)*
- **Contract layer:** `contacts/` per the pending Phase 3 contacts
  contract; `guid` = Notion page id / Airtable record id (both stable).
  Unmapped properties in `extra`.

## Build plan

1. **Gate: Needs-David — confirm demand before building.** The simpler
   first path is the CSV export through a generic import (or the user
   reshapes to vCard); a live connector only pays off if users ask.
2. If built: module `crates/trove-core/src/notion_airtable.rs` (def id
   `notion-airtable`), `DEF` (Periodic) + `CONNECTION` (TokenPaste, two
   labeled credential variants), registration lines in `INTEGRATIONS` +
   `CONNECTIONS`.
3. Field-mapping step at connect time: fetch the database/table schema,
   ask the user to bind name/email/phone/birthday columns; persist the
   mapping; everything unbound goes to `extra`.
4. Fixtures: a Notion query response and an Airtable records page with
   divergent user schemas; mapping-resolution tests; unique temp dirs.
5. Watch for Airtable bases that are mirrors of the macOS address book
   (via Airtable's Contact Import Extension) — `apple-contacts` is
   strictly better there; say so in the card copy.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Notion pull | — | create an integration secret; share a contacts DB; map fields; Sync now; confirm `contacts/notion-airtable/` rows + hub last-data |
| Airtable pull | — | PAT with `data.records:read`; same flow against a base |
| Mapping persistence | — | restart app; Sync now; mapping survives, no re-prompt |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§Notion / Airtable as Personal Contact DB (L752-758); at-a-glance L659;
cross-cutting note 7 (last in the CRM priority order). Feasibility 🟡
medium — APIs fine, schema flexibility is the cost. Research verdict:
"build later, prioritize if user demand surfaces" — hence the
Needs-David gate. Not time-sensitive; not privacy-flagged.

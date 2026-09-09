# Monica (Personal CRM)

- **id:** `monica`
- **domains:** `contacts/` (contract: **Phase 3 pending** — contacts)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll the REST API; paginated)
- **connection:** `monica` — TokenPaste (Bearer token from Settings → API;
  no OAuth) plus a base-URL field so self-hosted instances work — cloud
  and self-hosted share one API contract. Not shared with other defs.
- **evidence:** official-docs — monicahq.com/api (stable REST, actively
  maintained, open-source Laravel with an active GitHub); JSON export
  format official
- **effort / priority:** S / P2
- **needs:** verify Monica v3 ("Chandler") endpoint/schema compatibility
  vs v2 before building fixtures

## What it is

Open-source personal CRM with a niche but dedicated user base, cloud or
self-hosted. Its unique value over a plain address book is relationship
*context*: notes timelines, last-contacted dates (`last_called`,
`last_talked_to`), reminders, relationship labels, gifts, conversations —
the human layer no contact store carries.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Contacts | cloud free tier capped at 10 contacts; self-hosted unlimited | names, emails, phones, relationship labels, last_called/last_talked_to | official docs |
| Activities / notes / reminders | same | notes timeline, activities, reminders per contact | official docs |
| Full JSON export | all | contacts + activities + notes + reminders + relationships + gifts + conversations | official docs |

All optional in the contract; the cloud free-tier cap needs no special
code path — fewer rows, same shape.

## Access & auth

- REST: `GET /api/contacts` (paginated), `/contacts/{id}`, `/activities`,
  `/notes`, `/reminders`. Bearer token; base URL `app.monicahq.com` or
  the user's self-hosted instance.
- One-time migration alternative: Settings → Export Data → JSON (the full
  dump) — accept it through the same module as an Import path so a user
  leaving Monica keeps everything.
- No TCC, no local files. Standalone-clean (plain HTTPS). A community
  Monica MCP server exists but requires a running client — not a path for
  the compiled-in collector.

## Vault mapping

- **Raw layer:** `contacts/monica/contacts.jsonl` (API objects, full
  fidelity) plus `contacts/monica/raw/` for a dropped JSON export.
- **Contract layer:** Phase 3 contacts contract pending — one row per
  person, `source: monica`; relationship-context fields (notes, last
  contacted, labels) ride `extra` unless the contract adopts them.
- **Dedupe:** Monica contact `id` (namespaced by instance base URL, so a
  cloud account and a self-hosted one never collide) as `guid`.

## Build plan

1. Verify v3 (Chandler) vs v2 endpoint compatibility from the official
   docs carried in the research entry; build fixtures against whichever
   the live docs confirm in Phase 4's verify step.
2. Module `crates/trove-core/src/monica.rs`: `DEF` (Periodic, slow tick),
   `CONNECTION` (TokenPaste with base-URL field; setup copy explains
   Settings → API), `pull` hook for Sync-now.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. JSON-export import path through the same parser (one provider, both
   mechanisms, one entry).
5. Fixtures from the documented API response shapes; pagination + store
   tests, unique temp dirs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Contacts via API | — | create a free cloud account (10-contact cap suffices), paste token, Sync now; confirm `contacts/monica/` rows + hub last-data |
| Self-hosted base URL | — | point at a docker-compose Monica; same token flow |
| JSON export import | — | Settings → Export Data → JSON; drop in the import box; rows match the API pull |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§Monica HQ (L704–L710). Feasibility 🟢 high; cross-cutting note 7 ranks
the personal-CRM order Monica > Dex > Clay/Mesh > Notion/Airtable, all
secondary to the Apple Contacts pull and the derived interaction graph.
JSON export best for one-time migration, API for ongoing sync.

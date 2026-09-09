# Clay (Mesh)

- **id:** `clay`
- **domains:** `contacts` (contract: **not yet ratified** — Phase 3 drafts
  the contacts shape; this import lands raw-first regardless)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user-initiated CSV export drop from clay.earth)
- **connection:** none — no public API exists for personal-tier accounts
- **evidence:** documented — Clay's export is stated to be "backward compatible
  with Google Contacts" (library.me.sh, article 6821989456027); the Mesh CSV
  docs name the email header verbatim as "E-mail 1 - Value". clay.earth is the
  personal CRM (clay.com is an unrelated B2B enrichment product).
- **effort / priority:** S / P2
- **needs:** Needs-David (contract: contacts)

## What it is

Clay (partially rebranded "Mesh" at clay.earth) is a personal CRM that
enriches the user's address book by pulling from email, calendar,
LinkedIn, Twitter, and iMessage — job changes, company updates, news
mentions layered onto contacts. For Trove the differentiator is exactly
that enrichment layer: data beyond the raw address book. But it's
Clay-computed and not independently verifiable — supplementary metadata,
never authoritative over the user's own contact records.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Contact records | free tier ≤1,000 contacts; Pro ($10/mo annual) unlimited | names, emails, phones, organizations — Google Contacts CSV format | documented |
| Enrichment metadata | per Clay's own computation | job titles, company updates, social profiles (in `extra`) | community |

All optional in the eventual contract; enrichment fields land in `extra`
as supplementary, source-attributed metadata.

## Access & auth

- CSV export from the Clay dashboard — the **only** Trove-accessible
  artifact. No public REST API for personal accounts; do not invest in an
  API connector unless one ships.
- No auth, no TCC, no network: user exports the file and drops it on the
  registry-driven import box. Standalone-clean.

## Vault mapping

- **Raw layer:** `contacts/clay/contacts.jsonl` — one record per line,
  full export fidelity, snapshot-style (contacts are state, not events).
  *(Taxonomy wins over any research-doc path: per-source folder under
  `contacts/`.)*
- **Contract layer:** `contacts/` per the pending Phase 3 contacts
  contract; `guid` = Clay row id if the export carries one, else
  name+email composite. Enrichment overflow in `extra`.
- Feeds entity resolution like every other contacts source.

## Build plan

1. Module `crates/trove-core/src/clay.rs` (def id `clay`): `DEF` with
   `Behavior::Import` (`letterboxd.rs` is the reference import shape).
   No connection.
2. Registration line in `INTEGRATIONS`.
3. **Parser last** — the export format is undocumented; acquire a real
   export (any Clay user) before writing the column mapping. Until then
   the def can ship as a `NotWired` stub so the catalog card exists.
4. Consider folding the parser into a shared personal-CRM CSV layer with
   Dex (research recommends one generic "Personal CRM CSV import" flow);
   keep the def/card per-provider either way.
5. Fixtures from the acquired sample; tests with unique temp dirs.

## Build notes (fan-out phase)

- `Behavior::Import` wired; `contacts::Contact` contract reused via `write_snapshot`
  at `contacts/clay/contacts.jsonl`.
- Parser is header-driven and case-insensitive. Clay's export is documented as
  "backward compatible with Google Contacts" (library.me.sh, article 6821989456027).
  The parser maps the real Google Contacts CSV columns:
  `E-mail N - Value`, `Phone N - Value`, `Organization N - Name`,
  `Organization N - Title`, `Given Name`, `Family Name`, `Name`. All indexed
  variants (1, 2, 3…) are captured via pattern predicates; comma-separated
  multi-values within a cell are split. Common alternative/legacy column names
  are retained as fallbacks. Unrecognised columns land in `extra` for full fidelity.
- Guid: explicit id column preferred; fallback hashes `name|first_email|first_website`
  so two distinct people with the same display name do not collide.
- 13 unit tests pass; `cargo check` clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import | ✅ unit-tested (Google Contacts format) | export from a real clay.earth account; drop on the import box; confirm `contacts/clay/contacts.jsonl` rows + hub last-data; verify column mapping matches actual headers |
| Re-import idempotence | ✅ unit-tested | drop the same file twice; row count unchanged |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§Clay / Mesh (L720-726); at-a-glance L655; cross-cutting note 7 (CRM
priority order: Monica > Dex > Clay > Notion/Airtable). Feasibility 🟡
medium. Research verdict was "icebox": few users, no API, ongoing
rebrand/pivot uncertainty (VC-funded) — revisit if an API appears. P2,
build after Monica/Dex. Not time-sensitive; not privacy-flagged (contact
records, not message bodies).

# vCard Import (.vcf)

- **id:** `vcard`
- **domains:** `contacts` (contract: **not yet ratified** — Phase 3 drafts
  the contacts shape from Apple Contacts + Google Contacts + LinkedIn +
  vCard together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user drops any `.vcf` file)
- **connection:** none
- **evidence:** official-spec — RFC 6350 (vCard 4.0) / vCard 3.0;
  maintained Rust parsers: `vcard_parser` (RFC 6350) and `calcard`
  (Stalwart Labs, also JSContact)
- **effort / priority:** S / P1
- **needs:** Needs-David (contract: contacts) — otherwise none; not
  privacy-flagged (contact records, not message bodies)

## What it is

The universal contact interchange format — every contact service exports
to `.vcf`: iCloud.com, Google Contacts / Takeout (`contacts.vcf`),
Outlook, any CardDAV server. One importer is the zero-auth fallback for
every contacts source that doesn't have a direct connector yet, and the
free path for sources that never will. Notably it makes the Google
Contacts Takeout fallback automatic with zero additional code.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Contact records | none | N, FN, ORG, TEL, EMAIL, ADR, URL, NOTE | RFC 6350 |
| Life events & relations (vCard 4.0) | depends on exporter | BDAY, ANNIVERSARY, GENDER, KIND, RELATED, IMPP | RFC 6350 |
| Profile photos | depends on exporter | base64 PHOTO blob — stripped (or extracted separately), never inlined into JSONL | RFC 6350 |

All optional in the eventual contract; vCard 3.0 exports (Google) simply
carry fewer fields.

## Access & auth

- Any `.vcf` file drop on the registry-driven import box. No auth, no
  TCC, no network. Standalone-clean.
- Known producers to support from day one: iCloud.com → Export vCard;
  Google Contacts / Takeout → vCard (prefer it over `google.csv`, whose
  non-standard columns aren't worth parsing); Outlook; CardDAV downloads.
- Large exports (10k+ contacts) must be **streamed**, not loaded whole.

## Vault mapping

- **Raw layer:** `contacts/vcard/contacts.jsonl` — one contact per line,
  full fidelity minus base64 photo blobs. *(Research doc said a flat
  `contacts/contacts.jsonl` with `source: vcard_import`; the taxonomy +
  identity convention win: per-source folder under `contacts/`.)*
- **Contract layer:** `contacts/` per the pending Phase 3 contacts
  contract. **Dedupe `guid`:** vCard UID (a UUID in 4.0 exports) when
  present, else name+email composite key — re-imports are idempotent.
  Overflow properties in `extra`.
- Feeds entity resolution and the interaction graph like every other
  contacts source.

## Build plan

1. Module `crates/trove-core/src/vcard.rs` (def id `vcard`): `DEF` with
   `Behavior::Import` (`letterboxd.rs` is the reference import shape).
   No connection.
2. Registration line in `INTEGRATIONS`.
3. Parse with `calcard` (preferred: maintained, 4.0 + JSContact) or
   `vcard_parser`; streamed reader; strip PHOTO blobs at parse time.
4. Fixtures: vCard 3.0 (Google Takeout style) AND 4.0 samples; year-less
   BDAY; multi-TYPE TEL/EMAIL; a photo-bearing card (assert stripped);
   a 10k-card file for the streaming path. Unique temp dirs.
5. Sequencing: P1, early — it instantly widens coverage (iCloud, Outlook,
   CardDAV, Google Takeout in one importer) and exercises the contacts
   raw layer before the contract pass.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| .vcf drop | ✅ unit-tested | export from iCloud.com; drop on the import box; confirm `contacts/vcard/contacts.jsonl` rows + hub last-data |
| Google Takeout fallback | ✅ unit-tested | Takeout → Contacts → `contacts.vcf`; same drop; fields land incl. labels |
| Idempotence | ✅ unit-tested | drop the same file twice; row count unchanged (UID/composite dedupe) |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§vCard / VCF File Import (L728-734) + §Google Contacts Takeout fallback
(L744-750, rides this importer); at-a-glance L656, L658; cross-cutting
notes 1-2, 4. Feasibility 🟢 high, "build now". Covers the Takeout
fallback for the already-shipped `google-contacts` def when OAuth isn't
wanted. Not time-sensitive.

# Entity Resolution (cross-source identity merge)

- **id:** `entity-resolution`
- **domains:** `contacts` (contract: **not yet ratified** — Phase 3 drafts
  the contacts shape; this layer defines the merged-person record that
  sits beside it)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (re-runnable local derivation over vault data;
  no external source)
- **connection:** none
- **evidence:** deterministic local compute — no API. `phonenumber` Rust
  crate for E.164 normalization; Python `recordlinkage` and `nomenklatura`
  as algorithm references (Trove implements a simpler Rust-native version)
- **effort / priority:** L / P0
- **needs:** Needs-David (merged-person schema sign-off; contract:
  contacts) — no permissions beyond what each source already holds; not
  privacy-flagged (derives from data already in the vault, nothing new
  leaves or arrives)

## What it is

The infrastructure that makes the person layer coherent: it turns
`j.doe@gmail.com` in email, `+14155551234` in iMessage, and "John Doe" in
the address book into one person. Input: every contact record from every
source (`apple-contacts`, `google-contacts`, LinkedIn, vCard, CRMs) plus
the identifier space of `correspondence/` (emails, phone numbers,
iMessage handles). Output: a unified persons file each record of which
carries all known identities and source references. Everything
relationship-shaped downstream (interaction graph, "who matters most")
depends on it.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Exact-match merge | none (ships first) | persons keyed by shared E.164 phone / lowercased email / iMessage handle | deterministic |
| Fuzzy name merge | none — but **user-confirmed only, never automatic** | merge *suggestions* for records lacking a shared exact identifier | research note 9 |
| Stable person ids | none | `person_id` stable across re-runs (derived from primary identity or assigned once and persisted) | research notes |

## Access & auth

- Pure in-process computation over vault JSONL. No network, no TCC, no
  credentials. Standalone-clean by construction.
- Normalization rules: phones → E.164 via the `phonenumber` crate; emails
  lowercased + trimmed; iMessage handles that look like emails are treated
  as emails for matching.

## Vault mapping

- **Raw layer:** none of its own — it *reads* `contacts/<source>/` folders
  and correspondence identifiers. Per-source raw records stay untouched so
  the resolver is re-runnable without data loss (research cross-cutting
  note 2).
- **Derived layer:** `contacts/persons.jsonl` — one merged person per
  line: `person_id`, `canonical_name`, `identities[]`
  ({type: email|phone|imessage_handle|linkedin_url|apple_contact_id,
  value, source}), `merged_from[]`. `guid` = `person_id`.
  User-confirmed merge decisions are durable data (vault, not `.trove/`);
  match indexes are rebuildable.
- **Contract layer:** the Phase 3 contacts contract should ratify this
  merged-person shape alongside the per-source record shape.

## Build plan

1. Spike first: define the normalized identity schema (the Phase 3
   contract input) — this is the Needs-David gate.
2. Module `crates/trove-core/src/entity_resolution.rs` (def id
   `entity-resolution`): `DEF` (Periodic — re-derive after contact/
   correspondence syncs). No connection. One registration line.
3. **Exact-match layer ships with the first contacts collector**
   (sequenced with `apple-contacts`): E.164 + lowercased-email +
   handle matching. This alone handles ~80% of cases.
4. Fuzzy name matching (edit distance/soundex) lands later as
   **suggestions only** — surfaced for user confirmation, never
   auto-merged without a shared exact identifier ("John Smith" at two
   companies must not collapse).
5. Fixtures: multi-source contact sets with shared phones/emails,
   year-less variants, deliberate same-name-different-person traps;
   stability test (`person_id` unchanged across re-runs); unique temp
   dirs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Exact-match merge | — | vault with apple-contacts + vcard rows sharing a phone; run; confirm one `contacts/persons.jsonl` person with both sources in `merged_from` |
| Re-run stability | — | run twice; `person_id`s identical, no duplicate persons |
| Fuzzy suggestions | — | same-name records without shared identifiers stay split; suggestion (not merge) surfaced in UI |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§Entity Resolution Layer (L760-766); at-a-glance L660; cross-cutting
notes 1-2, 4, 9. Feasibility 🟢 high — all data local, deterministic,
re-runnable; the risk is algorithm quality, hence exact-match-first and
the never-auto-merge rule. P0 because it unblocks the domain: the
interaction graph and every cross-source person view key off
`persons.jsonl`. Not time-sensitive.

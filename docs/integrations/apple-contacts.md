# Apple Contacts

- **id:** `apple-contacts`
- **domains:** `contacts` (contract: **not yet ratified** — Phase 3 drafts
  the contacts shape from Apple Contacts + Google Contacts + LinkedIn +
  vCard together; this is the anchor source)
- **status:** 🧪 built (fixture-tested; `CNContactStore` via `objc2-contacts`; first-in-domain binding of `contacts`; needs the Contacts TCC grant to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (poll ~6h, or react to AddressBook-directory file
  events)
- **connection:** none — TCC `kTCCServiceAddressBook` (standard one-time
  system prompt)
- **evidence:** official-docs — Apple Contacts.framework / `CNContactStore`
  (incl. `CNContactBirthdayKey`, `CNContactDatesKey`); `objc2-contacts`
  crate for zero-shim Rust FFI; Trove's own `eventkit.rs` TCC bridge as the
  in-repo precedent
- **effort / priority:** M / P0
- **needs:** Needs-David (Contacts **TCC** grant — `kTCCServiceAddressBook` —
  for live validation; standard one-time system dialog). The `contacts`
  contract is now **bound** (this collector bound it — no longer a contract
  park). Not privacy-flagged (names/numbers, not message bodies).

## What it is

The macOS address book — the anchor of Trove's whole person layer. One TCC
prompt yields every contact across every account the user syncs (iCloud +
Google + Exchange + CardDAV + LDAP federate into one `CNContactStore`),
including birthdays, anniversaries, social handles, and postal addresses.
Everything else in the contacts domain (interaction graph, entity
resolution, CRM imports) keys off this pull.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Contact records | all accounts in one store | names, org, phones, emails, addresses, URLs, social profiles, notes | official docs |
| Birthdays & anniversaries | same pull, zero extra permission | `CNContactBirthdayKey` (year may be nil), `CNContactDatesKey` labeled dates | official docs |
| Cross-account dedup | native to CNContactStore | unified person per linked contact | official docs |

All optional in the eventual contract; year-less birthdays stored as MM-DD
strings.

## Access & auth

- `CNContactStore` via the `objc2-contacts` crate (no Swift shim);
  `NSContactsUsageDescription` in Info.plist; TCC
  `kTCCServiceAddressBook` one-time dialog — follow the `eventkit.rs`
  bridge pattern exactly.
- The raw-SQLite alternative (`AddressBook-v22.abcddb`) is rejected: needs
  the heavier FDA grant, loses native cross-account dedup, and is
  schema-fragile. Framework path only.
- Zero network; standalone-clean. Both trove (UI prompt) and troved (poll)
  reuse the same crate code; remember the build-signing rule so the TCC
  grant survives rebuilds.

## Vault mapping

- **Raw layer:** `contacts/apple-contacts/contacts.jsonl` — one person per
  line, full fidelity (all labeled values, birthday, `anniversaries[]`,
  notes), snapshot-style rewrite per sync (contacts are state, not
  events). *(Research doc said a flat `contacts/contacts.jsonl`; the
  taxonomy + identity convention win: per-source folder under
  `contacts/`.)*
- **Contract layer:** `contacts/` per the pending Phase 3 contacts
  contract — Google Contacts already writes this domain, so the contract
  pass normalizes both. `guid` = `CNContact.identifier` (stable across
  syncs); overflow in `extra`.
- Downstream consumers (interaction-graph, entity resolution) read this
  folder; keep raw records per-source so the resolver is re-runnable
  without data loss (research cross-cutting note 2).

## Build plan

1. Module `crates/trove-core/src/apple_contacts.rs` (def id
   `apple-contacts`): `DEF` (Periodic), permission hook = contacts TCC
   status (prompt copy batched/adjacent to the calendar prompt — note 3),
   last-data + pull hooks. No connection.
2. Registration line in `INTEGRATIONS`.
3. Fetch keys: names, org, phones, emails, postal, URLs, social profiles,
   birthday, dates, note. Include the birthday/anniversary fields in this
   same pull — **no separate "birthdays" integration** (the research doc's
   derivation row rides here; the EventKit synthetic Birthdays calendar is
   already covered by the built `calendar` def).
4. Fixtures: serialized contact fixtures incl. year-less birthday, custom
   date labels, multi-account duplicates; unique temp dirs.
5. Sequencing: first contacts collector → ships with it the exact-match
   slice of entity resolution (E.164 via `phonenumber` crate, lowercased
   emails) per the entity-resolution brief; interaction-graph builds on
   both.
6. Parked behind **Needs-David (contract: contacts)** for the contract
   layer; the raw layer can land first (per-source raw is always allowed).

## Build status — 🧪 2026-06-14

Shipped (`apple_contacts.rs`, INDEX #8 — also the **first-in-domain binding** of
the `contacts` contract). `Behavior::Periodic` (6h), no connection. Uses
**`objc2-contacts` (`CNContactStore`)** — `#[cfg(target_os="macos")]` FFI + a
non-macOS stub, mirroring `eventkit.rs`. TCC `kTCCServiceAddressBook`: the
permission hook reports authorization status; the pull requests access (system
dialog) and degrades gracefully when denied. The macOS FFI extracts each
`CNContact` into a plain `RawAppleContact`; a pure, cross-platform
`map_contact()` then produces the contract row — the testable core (the FFI
itself is validated live).

Output: snapshot `contacts/apple-contacts/contacts.jsonl` (atomic rewrite,
sorted by `id`). `id` = `CNContact.identifier`; `source` = `apple-contacts`;
emails lowercased/deduped, phones E.164 where derivable (else verbatim) via the
shared `contacts::normalize_*` helpers; `name`/`given`/`family`/`orgs`;
**year-less birthday → `extra.birthdays` with no `year` key**;
anniversaries/addresses/urls/social/note → `extra` (full fidelity). **Photo
omitted** in v1 (CNContact gives image *data*; the contract bans inlined base64
— `// TODO(apple-contacts photo)` for a real-URL path). `other`=false (Apple
contacts are user-saved); `account` omitted (one unified federated store).

Contract binding (first collector in `contacts/`): added the `Contact` struct +
`normalize_email`/`normalize_phone` (`contacts.rs`), `ContractKind::Snapshot` +
manifest-scan handling, the `contacts` `DOMAINS` entry (`required: source,id`),
and promoted the fixtures from the draft test to the ratified triad (5/5). The
already-built `google-contacts` was brought into conformance in the same change
(`resource`→`id`, `source`, folder `contacts/google-contacts/`).

Adversarial-verify: 0 blocking + 3 minor (legacy-folder orphan, a doc/bindings
kind list, brief drift) — fixed.

Gate: trove-core 472/0, `cargo check` clean, `schedule_doc` regenerated
(apple-contacts Periodic), `bindings.ts` regenerated (+`"snapshot"` kind).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| TCC prompt + pull | 🧪 (Needs-David: TCC) | fresh vault; enable card; approve the system dialog; Sync now; confirm `contacts/apple-contacts/contacts.jsonl` + hub last-data |
| Multi-account federation | 🧪 (Needs-David: TCC) | Mac with iCloud + Google accounts in System Settings; confirm both account's contacts appear once each |
| Birthdays/anniversaries | 🧪 (Needs-David: TCC) | contact with year-less birthday + custom anniversary; confirm MM-DD handling (no `year` key) and labeled dates in `extra` |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§macOS Contacts (L664-670) + §Birthdays & Anniversary Derivation
(L736-742); at-a-glance L648, L657; cross-cutting notes 1-5, 8.
Feasibility 🟢 high. P0 because it unblocks the rest of the domain:
interaction graph and entity resolution are force multipliers that need
this anchor. iCloud CardDAV catalogued separately (`icloud-contacts`) as a
niche alternative for users with Contacts sync off — for everyone else
this pull is strictly better. Not time-sensitive.

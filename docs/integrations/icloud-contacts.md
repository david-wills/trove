# iCloud Contacts (CardDAV)

- **id:** `icloud-contacts`
- **domains:** `contacts/` (contract: **Phase 3 pending** — contacts contract
  drafted from Apple Contacts + Google Contacts + LinkedIn + vCard + CRMs)
- **status:** 🧪 built (fixture-tested; Needs-login to validate — see Build status + validation matrix below)
- **unavailable_reason:** none
- **behavior:** Periodic (poll the CardDAV collection; read-only)
- **connection:** `icloud-contacts` — TokenPaste (Apple ID + app-specific
  password generated at appleid.apple.com; iCloud 2FA doesn't work over
  DAV). Potentially shareable with a future iCloud CalDAV def — record the
  intent, don't pre-build the sharing.
- **evidence:** community-schema — no official Apple CardDAV docs for
  third-party apps; verified working via vdirsyncer's regular testing
  against `contacts.icloud.com` (medium confidence)
- **effort / priority:** L / P2
- **needs:** Needs-login (Apple ID + app-specific password to validate) ·
  spike-first decision gate (see build plan)

## Build status

**Status:** 🧪 built — Periodic (6h), contacts domain snapshot, NEW `icloud-contacts` TokenPaste connection. All 20 tests pass.

**Narrower than brief:** no ETag-based incremental sync (fetches all vCards on every pass, like apple-contacts' full CNContactStore enumeration). The brief noted this as acceptable at the cadence chosen (6h). CTag/ETag watermarking deferred to a future pass.

**Connection:** NEW `icloud-contacts` ConnectionDef (TokenPaste, two lines: Apple ID email + app-specific password). Integrator must add `&crate::icloud_contacts::CONNECTION,` to CONNECTIONS in integrations.rs.

---

## What it is

iCloud's contact store over the standard CardDAV protocol. For most users
this is **the same data** as the macOS Contacts pull (`apple-contacts`,
P0) — CNContactStore federates iCloud locally with one TCC prompt and no
credential management. This def only earns its place for users who have
iCloud Contacts sync turned *off* on the Mac, or who refuse the
app-wide TCC grant but will paste an app-specific password.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Address book collections | free iCloud | vCard 3.0/4.0 records: names, phones, emails, addresses, birthdays, URLs | community (vdirsyncer) |

All optional in the contract; omit-if-empty.

## Access & auth

- CardDAV: `https://contacts.icloud.com/` — PROPFIND to discover address
  book collections, GET individual vCard resources. Read-only for Trove
  (CardDAV *write* has known 403 issues against iCloud — irrelevant here).
- Auth: Apple ID username + app-specific password. The connect card must
  explain the generate-a-password step (disabled-controls-need-affordance:
  link the user to appleid.apple.com with inline copy).
- Parsing: `calcard` (stalwartlabs, vCard 3.0/4.0 + iCalendar) — same crate
  the generic `vcard` importer would use.
- No TCC. Standalone-clean (plain HTTPS). Undocumented server: expect
  quirks (collection-naming minimums; collections should be created from
  Apple clients — we only read).

## Vault mapping

- **Raw layer:** `contacts/icloud/contacts.jsonl` — snapshot of parsed
  vCard records, full fidelity (base64 PHOTO blobs stripped, never image
  copies in contact rows).
- **Contract layer:** Phase 3 contacts contract pending — one row per
  person, `source: icloud-contacts`, overflow in `extra`.
- **Dedupe:** vCard `UID` as `guid`; fall back to name+email composite for
  UID-less cards.

## Build plan

1. **Spike first** (the research recommendation): confirm via a real
   Apple ID that PROPFIND/GET against `contacts.icloud.com` works with an
   app-specific password, and measure overlap with the `apple-contacts`
   pull. If the data is fully redundant for the validating account, park
   this behind the free fallback: iCloud.com → Contacts → Export vCard →
   the generic `vcard` importer (zero code).
2. If built: module `crates/trove-core/src/icloud_contacts.rs` — `DEF`
   (Periodic, slow tick), `CONNECTION` (TokenPaste with the app-specific-
   password help copy), `pull` hook.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures: hand-built vCard 3.0 and 4.0 samples (community-verified
   shapes, no official docs) — parser + store tests, unique temp dirs.
5. Hub copy should steer users toward `apple-contacts` when Contacts sync
   is on — this def is the fallback, not the default.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Collection discovery + vCard pull | — | paste Apple ID + app-specific password; Sync now; confirm rows in `contacts/icloud/` + hub last-data; diff against an `apple-contacts` pull for overlap |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§iCloud Contacts (CardDAV) (L688–L694). Feasibility 🟡 medium — works but
undocumented by Apple; vdirsyncer is the community evidence base. Strictly
dominated by CNContactStore for users with sync enabled, hence P2 and the
spike gate. The `.vcf` manual export through the generic `vcard` importer
is the free alternative either way.

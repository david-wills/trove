# Google Contacts

- **id:** `google-contacts`
- **domains:** `contacts/` (contract: **Phase 3 pending** — drafted from
  Apple Contacts + Google Contacts + LinkedIn + vCard + personal CRMs
  together; per-source raw rows are already in place)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (incremental via People API `syncToken`)
- **connection:** `google` — OAuth, the shared Google connection (one login,
  six defs: gmail, calendar, contacts, tasks, youtube, books). Multi-account
  by `sub` per the Google-integration model.
- **evidence:** official-docs — Google People API v1 (replaced the deprecated
  Contacts API in 2022, stable); `contacts.readonly` is a non-sensitive
  scope; Takeout `contacts.vcf` is standard vCard 3.0
- **effort / priority:** M / P1
- **needs:** extension: `otherContacts` endpoint (auto-saved people) ·
  Takeout `.vcf` fallback rides the generic `vcard` importer once it exists

## What it is

Google's address book — for many users the primary contact store, and it
often does *not* sync to macOS Contacts unless the Google account is added
in System Settings. Names, emails, phones, birthdays, organizations, URLs,
photos. A pillar of the person layer alongside Apple Contacts; shipped
2026-06-12 as part of the Google integration.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Connections (saved contacts) | free | names, emails, phones, birthdays, addresses, orgs, urls, bios, photos | official docs |
| Other contacts (auto-saved) | free — separate endpoint, not yet pulled | name + email of people you've corresponded with | official docs |
| Takeout vCard fallback | free, zero-auth | vCard 3.0 standard fields | official docs |

All optional in the (pending) contract; omit-if-empty.

## Access & auth

- REST: `people.googleapis.com/v1/people/me/connections?personFields=…`,
  Bearer token, scope `contacts.readonly` (non-sensitive — no verification
  needed). `otherContacts` lives at a separate endpoint.
- Incremental: `syncToken` change tokens; on token expiry the existing code
  re-lists in full. Rate limit 90 req/min/user — trivial for periodic pulls.
- Stable identity: `resourceName` (`people/c<id>`) + `etag`.
- No TCC, no local files. Standalone-clean (plain HTTPS via the shared
  OAuth plumbing in `sync/`).

## Vault mapping

- **Layer:** `contacts/google-contacts/<sub>.jsonl` (snapshot per account,
  keyed by the OAuth `sub`) + `contacts/index.md` human summary. Raw and
  contract layers coincide: rows are written in the **bound `contacts`
  contract** shape (`id` = `resourceName`, `source: google-contacts`,
  `account` = email, names/emails/phones/org normalized — emails lowercased,
  phones E.164 where derivable — overflow in `extra`; `otherContacts` →
  `other:true`). The entity-resolution layer merges across sources at read
  time — this def never merges, only reports.
- **Dedupe:** `id` (the `resourceName` value) as the stable key across syncs.

## Build plan

Shipped pre-pipeline (`google_contacts.rs`, registered on the shared
`google` connection). Remaining work items:

1. Add the `otherContacts` pull (auto-saved people — often contains everyone
   you've emailed but never formally added).
2. When the generic `vcard` importer ships, document Takeout
   (`takeout.google.com` → Contacts → `contacts.vcf`) as the zero-auth
   fallback path — no new code, prefer vCard over the non-standard
   `google.csv`.
3. **Done (2026-06-14, with apple-contacts):** rows conform to the bound
   `contacts` contract (`resource`→`id`, added `source`, folder
   `contacts/google/`→`contacts/google-contacts/`, clean handles; a one-time
   best-effort cleanup removes the orphaned legacy `contacts/google/` folder).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connections pull | 🧪 shipped pre-pipeline | Connect Google, enable the Contacts sub-toggle, Sync now; confirm `contacts/google-contacts/<sub>.jsonl` rows (contract shape: `id`/`source`) + hub last-data. David promotes to ✅ |
| Incremental syncToken | 🧪 shipped pre-pipeline | second Sync after editing a contact picks up only the change; expired-token path re-lists in full |
| otherContacts | — | not yet built |
| Takeout vCard fallback | — | blocked on the `vcard` importer |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§Google Contacts (People API) (L680–L686) and §Google Contacts Takeout
(L744–L750). Feasibility 🟢 high. Cross-cutting notes: one `contacts.rs`-ish
module should own the shared schema and write path; entity resolution is
exact-match-first (E.164 phone, lowercased email), fuzzy merge only
user-confirmed. Research vault paths predate the taxonomy — `contacts/`
per the README table governs.

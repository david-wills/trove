# Domain: contacts

Every address book and personal CRM, in one normalized store. Google
Contacts already writes this domain; macOS/iCloud Contacts, a dropped
`.vcf`, a LinkedIn connections export, and personal CRMs (Monica, Dex,
Clay, a Notion/Airtable base) all write the same per-person shape. Each
source keeps its own folder of full-fidelity rows; the read-time
entity-resolution layer merges people *across* sources by their clean
handles — collectors only report, they never merge.

- **Layout:** `contacts/<source>/<account>.jsonl` (one line per contact;
  `<account>` is whatever partitions the source — Google's OAuth `sub`, an
  Apple ID, or just `contacts` for a single-store import like a `.vcf`)
- **Kind:** snapshot (rewritten whole, atomically: sibling tmp + rename)
- **Schema:** [`schemas/contacts.contact.schema.json`](../schemas/contacts.contact.schema.json)
- **Dedupe key:** `id` — the source's stable per-contact id (Google
  `resourceName`, `CNContact.identifier`, vCard `UID`, a CRM row id, or a
  name+email composite where the export carries none). Re-syncs/re-imports
  upsert by `id`; a snapshot rewrite drops what the source no longer
  returns.

## Contact

A current-state record, not an event — there is no `ts`. Only
`source`+`id` are required; a sparse source (an other-contact with
just an email, a LinkedIn row with no email) writes a minimal line, a rich
address book fills more. Handles are clean and raw: emails lowercased,
phones E.164 where the country is derivable; the contact's own display name
rides `name`, never decorated onto a handle.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `source` | string | ✔ | collector id, = the folder name (may be `""` in sparse lines; the folder then names it) |
| `id` | string | ✔ | source-native stable contact id, the dedupe key |
| `account` | string | | the connected account this contact belongs to (Google account address; absent for single-store imports) |
| `name` | string | | display name, from the primary name entry |
| `given` | string | | given name |
| `family` | string | | family name |
| `emails` | string[] | | email handles, lowercased, in source order, dupes removed |
| `phones` | string[] | | phone handles, E.164 where derivable, else verbatim |
| `orgs` | object[] | | organizations / job titles: `{name?, title?}` (LinkedIn Company/Position, vCard ORG/TITLE) |
| `photo` | string | | profile photo URL (real photos only; base64 blobs are stripped, never inlined) |
| `other` | bool | | auto-collected rather than user-saved (Google's "Other contacts" — everyone you've corresponded with) |
| `updated` | string | | RFC3339 local time the contact last changed at the source, if known |
| `extra` | object | | everything source-specific — birthdays, addresses, URLs, notes, relationship context (`connected_on`, `last_talked_to`), CRM tags/enrichment — full fidelity |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"source":"google-contacts","id":"people/c8523451","account":"me@gmail.com","name":"Alice Example","given":"Alice","family":"Example","emails":["alice@example.com","alice@work.com"],"phones":["+14155550142"],"orgs":[{"name":"Example Corp","title":"CTO"}],"photo":"https://lh3.googleusercontent.com/contacts/real.jpg","updated":"2026-06-02T00:00:00Z","extra":{"birthdays":[{"date":{"month":3,"day":14}}],"urls":[{"value":"https://alice.example"}]}}
{"source":"linkedin","id":"https://www.linkedin.com/in/bob-jones-4a1b2c3","name":"Bob Jones","given":"Bob","family":"Jones","orgs":[{"name":"Globex","title":"Head of Design"}],"extra":{"connected_on":"2021-09-14"}}
{"source":"google-contacts","id":"otherContacts/o4471","account":"me@gmail.com","emails":["stranger@example.com"],"other":true}
```

## Read-time semantics (FYI for writers)

The contacts reader scans `contacts/*/` — creating your source folder is the
registration. Emails and phones are the join keys the exact-match
entity-resolution layer keys on, which is why the handle convention is
enforced hard at write time (lowercase emails, E.164 phones): a wrong merge
baked into a row is permanent, a clean handle is joinable forever. This
contract is deliberately the *proof* that the identity convention suffices
for exact-match resolution — but resolution itself is read-time work; a
collector writes only what it observed and never merges across sources.
Saved contacts and auto-collected ones (`other:true`) live in the same
store; views decide which to surface.

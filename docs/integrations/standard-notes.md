# Standard Notes

- **id:** `standard-notes`
- **domains:** `notes/` (contract: **Phase 3 pending** — `notes`; collected
  notes land here, `artifacts/` stays the user-curated layer)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user-initiated decrypted ZIP export)
- **connection:** none — the export is a local file the user supplies; no login.
- **evidence:** official-docs — decrypted ZIP export (Preferences > Backups >
  Download Backup) gives individual plain-text note files; E2EE means no
  readable local DB
- **effort / priority:** S / P2
- **needs:** none

## What it is

End-to-end-encrypted notes app (open source, AGPL; self-hostable sync server).
Used by privacy-minded note-takers. Because everything is encrypted at rest —
the account master key lives in the Keychain — there is no server or local
plaintext DB to query. The only readable surface is the user-initiated
**decrypted** ZIP export: a folder of plain-text/markdown note files plus a
decrypted backup file.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Note bodies | all plans | plain-text/markdown content per note | official docs |
| Note metadata | all plans | title, created/updated timestamps | official docs |
| Extension note types | extension-gated | rich text / spreadsheet / code formats | official docs |

All optional in the contract (omit-if-empty). The encrypted JSON export can be
archived as a vault file but offers no indexable content — only the decrypted
ZIP is parseable.

## Access & auth

- File paths / export mechanism: Preferences > Backups > Download Backup →
  **Decrypted ZIP** (decrypted backup file + folder of individual plain-text
  notes). The Encrypted JSON option requires the account password to decrypt
  and is not indexable — store-only if at all.
- No auth, no API, no TCC. Standalone-clean: Trove reads a file the user hands
  it via the generic import box.

## Vault mapping

- **Raw layer:** `notes/standard-notes/raw/` — the imported note files /
  decrypted backup, full fidelity.
- **Contract layer:** `notes/standard-notes/` per the (pending Phase 3) notes
  contract — expected shape: one row per note (`ts` = created/updated,
  `source`, `guid` = note uuid, `title`, `body`, `tags[]`), overflow in
  `extra`. Extension note types (rich text, spreadsheet, code) carry a
  `format`/`content_type` marker and degrade to their best plain-text
  representation rather than failing the row.
- **Dedupe:** `guid` = note uuid; re-imports of a fresh export are idempotent
  by uuid (updated notes overwrite, new notes append).

## Build plan

1. Module `crates/trove-core/src/standard-notes.rs`: `DEF` (Import), no
   `CONNECTION`. Register one line in `INTEGRATIONS`.
2. Parser walks the decrypted ZIP's note folder; reads file content + the
   decrypted backup file's metadata for timestamps/tags/uuids.
3. Graceful handling of extension note types — detect non-plain formats, keep
   raw, surface a best-effort text body. `letterboxd.rs` is the reference
   import pattern.
4. Fixtures: a decrypted-ZIP sample with a plain note, a tagged note, and one
   extension (rich-text) note; parser + store tests, unique temp dirs.
5. Vault writes via `store` helpers once the notes contract is ratified; until
   then parked behind the pending-contract flag (raw fidelity lands regardless).

## Build notes (2026-06-17)

- Behavior: `Import` — accepts `.zip`, `.txt`, `.json`; reads the backup JSON from
  the ZIP root (first root-level `.txt`/`.json` that starts with `{`), or the bare
  file if not a ZIP.
- JSON shape confirmed from SN source (`PurePayload.ts`): `uuid`, `content_type`,
  `created_at`, `updated_at`, `deleted`, `content.{title,text,references,noteType,editorIdentifier}`.
- Tags inverted from Tag items' `content.references` (each Tag lists its notes;
  we build a note→tags map). Tags sorted per note for deterministic output.
- Extension note types (`noteType != "plain-text"` or `editorIdentifier` set)
  carry `extra.note_type` / `extra.editor_identifier`; body verbatim.
- Deleted items excluded (not written at all, not even to raw).
- Raw layer: full item JSON (`uuid`, `content_type`, `created_at`, `updated_at`,
  `deleted`, `content` verbatim) + `source`, `id`, `_created` (immutable partition).
- Contract layer partitioned by month of `created`; deduped/upserted by `uuid`.
- Re-import: existing uuids are "updated" in the headline (still upserted),
  new uuids are "imported".
- 7 unit tests, all green. `cargo check` clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Note bodies + metadata | ✅ built | download a decrypted ZIP; drop it in the import box; confirm rows in `notes/standard-notes/` + hub last-data |
| Extension note types | ✅ built | include a Super/Code note in the export; confirm `extra.note_type` present, body preserved |
| Tag resolution | ✅ built | tag a note; confirm `tags` array on contract row matches tag title |
| Re-import idempotent | ✅ built | re-import the same ZIP; confirm row counts unchanged and no duplicates |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Standard Notes (L2891–L2897). Feasibility 🟢 high. E2EE is the whole story:
no local plaintext DB ever exists, so direct-DB collection (the path Bear /
Apple Notes use) is impossible — import-only by design. Extension formats are
the one parser gotcha. Open-source and self-hostable, so low shutdown risk.

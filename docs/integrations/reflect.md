# Reflect

- **id:** `reflect`
- **domains:** `notes/` (contract: **bound** — `crate::notes::Note`; first
  collector: Bear; all notes sources share `notes/<source>/YYYY-MM.jsonl`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (export ZIP; or watch the automatic daily local backup
  folder)
- **connection:** none — the read path is a user-initiated export ZIP or the
  local backup folder. (Reflect's API exists but is **write-only/append-only**:
  the app is E2EE, so the server never sees plaintext and there is no read
  endpoint.)
- **evidence:** sample-required — export ZIP (md/HTML/JSON) format and the
  automatic-backup folder layout are community-described but not officially
  documented; needs a real sample to lock the parser.
- **effort / priority:** S / P2
- **needs:** Needs-sample (export/backup layout) · Needs-David (backup folder
  location not publicly documented — ask the user to locate it)

## What it is

Reflect is a networked-thought / daily-notes app (the Roam/Obsidian lineage),
end-to-end encrypted by design. Notes are linked daily entries and pages. For
Trove the value is the user's journaling and idea graph — captured locally
from their own export or backup, since the E2EE model means no cloud read is
possible.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Notes / daily entries | all (export) | title, body (markdown), backlinks | community |
| Tags / links | all (export) | tags, page references | community |
| Timestamps | all (export) | created/modified | community |

All optional in the contract (omit-if-empty).

## Access & auth

- Export: app menu → Export → Markdown, HTML, or JSON ZIP. Well-formed
  Markdown.
- Automatic daily backups: Reflect writes backups to a local folder. Trove can
  watch that folder for a hands-off path — but the **location is not publicly
  documented** (likely `~/Downloads` or a user-configured path). Ask the user.
- API: write/append only (E2EE) — **not a read path**, do not wire it.
- No TCC beyond reading a user-chosen folder. Standalone-clean.

## Vault mapping

- **Raw layer:** `notes/reflect/raw/…` — copies of the exported/backed-up note
  files, full fidelity.
- **Contract layer:** `notes/reflect/…` — one `crate::notes::Note` row per
  note (`source`, `id`, `title`, `body`, `created`, `modified`, `tags[]`,
  backlinks in `extra`). Contract is bound (`notes` domain, `crate::notes::Note`).
- **Dedupe:** `guid` = note id; re-import is idempotent (replace on matching
  id).

## Build plan

1. Module `crates/trove-core/src/reflect.rs`: `DEF` (Import), import-box pull
   hook accepting the export ZIP (and optionally a backup-folder watch path).
2. One registration line in `INTEGRATIONS`. No `CONNECTION` (import only).
3. **Parser-last / Needs-sample:** the export ZIP and backup-folder layouts
   aren't officially documented — obtain a real export from the user, inspect
   the JSON/Markdown shape, then write the parser. Flag Needs-sample until a
   sample lands.
4. Backup-folder path is undocumented — surface a "locate your backup folder"
   step (Needs-David).
5. Vault writes via `store` helpers once the `notes/` contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Notes import | — | export a real ZIP from Reflect; drop it in the import box; confirm rows in `notes/reflect/` + hub last-data |
| Backup-folder watch | — | locate the daily-backup folder; point Trove at it; confirm new backups import without re-export |

## Build notes (2026-06-17)

Built as `Behavior::Import` accepting `.zip` or `.md` files. Contract layer via
`notes/reflect/YYYY-MM.jsonl` + raw layer via `notes/reflect/raw/YYYY-MM.jsonl`.
Tests green, cargo check clean.

**Primary sources consulted:**
- `team-reflect/reflect-import` (official repo): Convertor interface defines
  `subject/html/createdAt/updatedAt/backlinkedNoteIds` — Reflect's internal
  data model. Confirms Reflect has no frontmatter concept in its own schema
  (importing Obsidian-style frontmatter causes Reflect to treat the `---` block
  as the note title).
- `reflect.academy/import-export-backups`: four export formats — "Reflect JSON",
  "Reflect CSV", "Markdown zip", "HTML zip".
- `reflect.academy/using-backlinks-and-tags`: backlinks use `[[Entity Name]]`
  syntax inline; tags use `#tagname` inline.

**parser_parked_needs_sample = true**: The exact Markdown zip layout (whether
any frontmatter fields are emitted, exact key names, ZIP structure) is not
officially documented. The parser now handles BOTH layouts defensively:

1. **No-frontmatter (likely real format):** filename stem `YYYY-MM-DD` → date
   (daily notes are date-named in Reflect). First H1 or file stem → title.
   `#tags` and `[[backlinks]]` extracted inline from the body.
2. **Frontmatter present (bonus):** fields `id`/`uuid`, `created`/`date`,
   `updated`/`modified`, `tags`, `backlinks` read from the frontmatter block
   if present. Filename-date is still preferred for date-named files.

**Silent-zero detection:** If a ZIP is imported but contains no `.md` files
and does contain `.html`/`.json`/`.csv` files, an error is returned directing
the user to re-export as "Markdown zip" instead of silently reporting 0 notes.

**Needs-David**: Backup folder path is not publicly documented. Once located,
a watch mode could be added to avoid manual re-export.

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Reflect Notes (L2907–L2913). Feasibility 🟢 high for the export/backup path —
S effort, well-formed Markdown. The crucial constraint: **the API is
write-only** (E2EE), so export or local backup is the only read path. Backup
location is undocumented; build parser-last once a real sample is in hand.

# Domain: notes

Collected notes from every notes, journaling, and personal-knowledge app, in
one normalized store. Apple Notes, Bear, Drafts, Obsidian, Logseq, Notion,
OneNote, Evernote, Google Keep, Craft, Capacities, Reflect, Roam, Simplenote,
Standard Notes, Ulysses — and the journaling apps Day One and Stoic — all write
this one shape; a journal entry, a daily note, a quick capture, a checklist, and
a long-form sheet are all *a titled body of text with timestamps*. This is the
**collected** layer (a snapshot of what lives in the user's note apps);
`artifacts/` stays the separate user-curated layer Trove authors itself, and the
reader keeps the two apart. Each source writes its own folder; the reader scans
them all into one notes view, reconciling cross-source overlap (the same note
synced to two apps) at read time.

- **Layout:** `notes/<source>/YYYY-MM.jsonl` (per-source snapshot, month of
  `created`)
- **Kind:** snapshot (per-affected-month whole-file atomic rewrite)
- **Schema:** [`schemas/notes.note.schema.json`](../schemas/notes.note.schema.json)
- **Key:** `id` (source-native, stable) within a source. Re-runs replace the
  note in place by `id`; an overlapping re-import or re-scan never duplicates.

## The note — `notes/<source>/YYYY-MM.jsonl`

The **current state** of the user's notes, one note per line, partitioned by the
month of `created`. Each sync/import rewrites only the months whose notes
changed (atomically: sibling tmp + rename); a note that loses no field keeps its
line. Only `source` and `id` are required — a plain-text note with no title
writes `body` + `created`; a richly-tagged, foldered, pinned note fills more. A
title-only or checklist-only note simply omits `body`.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `source` | string | ✔ | collector id, = the folder name |
| `id` | string | ✔ | source-native note id (Bear `ZUNIQUEIDENTIFIER`, Day One / Notion / Roam uuid, Keep filename, vault-relative path) |
| `title` | string | | note title (explicit, or the filename / first line / H1 where the source has no title field); omit when the source has none |
| `body` | string | | the full note text, verbatim — Markdown / plain text / HTML→Markdown (full fidelity; trimming or rendering is a read-time opinion) |
| `created` | string | | RFC3339 local time the note was created (falls back to file mtime where that is all the source exposes) |
| `modified` | string | | RFC3339 local time the note was last edited |
| `tags` | string[] | | note tags, verbatim (Bear/Drafts tags, Day One/Keep labels, frontmatter tags) |
| `folder` | string | | the note's one place: folder / notebook / collection / group name, or vault-relative path for folder-tree sources |
| `pinned` | bool | | pinned / flagged |
| `archived` | bool | | archived (out of the active set, not deleted) |
| `trashed` | bool | | in trash / soft-deleted (kept for fidelity; a read-time filter decides whether to surface) |
| `extra` | object | | everything source-specific (full fidelity): Day One location/weather/mood, Stoic mood/metrics, Keep checklist items + color, Roam/Reflect backlinks, frontmatter, content-type/format marker, … |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"source":"bear","id":"4F8A2C1E-0B7D-4E2A-9F3C-1A2B3C4D5E6F","title":"Garden planting plan","body":"# Garden planting plan\n\n- Tomatoes in the south bed\n- #garden #spring","created":"2026-03-14T09:12:00-07:00","modified":"2026-04-02T18:40:00-07:00","tags":["garden","spring"],"folder":"Home","pinned":true,"extra":{"ZUNIQUEIDENTIFIER":"4F8A2C1E"}}
{"source":"day-one","id":"A1B2C3D4E5F60718293A4B5C6D7E8F90","body":"Long run along the coast this morning. Felt clear-headed for the first time in weeks.","created":"2026-06-08T07:05:11-07:00","modified":"2026-06-08T07:31:44-07:00","tags":["running","reflection"],"extra":{"journal":"Daily","location":{"placeName":"Lands End","latitude":37.7806,"longitude":-122.5111},"weather":{"conditions":"Foggy","temperatureCelsius":13.0},"mood":"good"}}
{"source":"apple-notes","id":"x-coredata://A1F2/ICNote/p5512","title":"Wifi password — back of the router"}
```

## Read-time semantics (FYI for writers)

The notes reader scans `notes/*/*.jsonl` into one notes view; creating your
source folder is the registration. It keeps the collected `notes/` layer
distinct from the user-curated `artifacts/` layer, and reconciles cross-source
overlap (the same note synced to two apps, an export re-imported beside a live
DB read) by `id` within a source and by content at read time — write your own
folder with stable ids and let the reader decide. `trashed`/`archived` are
honest state, not a delete instruction: carry them and let the view filter.
Journaling apps fit the same shape — a Day One or Stoic entry is a note, with
its location / weather / mood / metrics preserved in `extra` (a read-time view,
not a separate contract, may surface those). Full source fidelity always
survives in each source's own `notes/<source>/raw/` regardless of what the
normalized columns capture.

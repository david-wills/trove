# Day One

- **id:** `day-one`
- **domains:** `notes/` (contract: **Phase 3 pending** — notes shape, drafted
  with Apple Notes / Bear / Drafts / Capacities / Obsidian / Logseq)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user drops the JSON export ZIP; re-runnable, dedup by
  entry UUID)
- **connection:** none (import only)
- **evidence:** official-docs — File > Export > JSON produces a stable,
  well-structured ZIP (Journal.json + media subfolders); widely parsed by
  third-party tools (dayone-to-obsidian etc.)
- **effort / priority:** S / P1
- **needs:** none (notes contract ratified; follower build complete)

## What it is

The dominant journaling app (~10M users). Entries are markdown with rich
first-class metadata: creation/modified timestamps, location, weather,
tags, and attached photos/videos/audio. Journal entries are personal and
reflective — high-value, otherwise never captured locally in a structured
form. The official JSON export is stable and re-runnable, making this a
clean import.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Entries | all plans | uuid, creationDate, modifiedDate, text (markdown), tags | official export |
| Location/weather | all plans | per-entry geo + weather snapshot | official export |
| Media references | optional (export toggle) | photos/videos/audios arrays + media subfolders | official export |

All optional in the contract; an entry without media or location simply
omits those fields.

## Access & auth

- **Export (the path):** File > Export > JSON → `.zip` containing
  `Journal.json` (array of entries) plus media subfolders. Timestamps are
  ISO8601; text is markdown; tags and location first-class. Media included
  only if the user toggles "Include Attachments".
- **Local DB:** `~/Library/Group Containers/5U8NS4GX82.dayoneapp2/` exists
  but Day One discourages direct access and the schema is unpublished —
  **avoid it**, use the export.
- TCC: only ordinary file-read on the dropped ZIP. Standalone-clean.

## Vault mapping

- **Raw layer:** `notes/day-one/raw/` — the parsed `Journal.json` entries,
  full fidelity (location, weather, media refs preserved).
- **Contract layer:** `notes/day-one/YYYY-MM.jsonl` per the (pending) notes
  contract — expected shape: one row per entry (`ts` = creationDate,
  `source`, `guid` = entry uuid, `body` = text markdown, `tags[]`,
  `modified` = modifiedDate), location/weather/media refs in `extra`.
  Media is referenced, never copied as image bytes into the contract.
- **Dedupe:** entry uuid as `guid` — re-importing a fresh export updates in
  place, no duplicates.

## Build plan

1. Reuse the generic import path (`letterboxd.rs` is the reference import
   example): `DEF` is `Import` behavior, `import` hook unzips and parses
   `Journal.json`.
2. Registration line in `INTEGRATIONS`.
3. Fixtures from a real JSON export (entries with/without location, weather,
   media; a re-import overlap); parser + store + dedup-by-uuid tests, unique
   temp dirs.
4. Bundle the parser shape with Stoic (similar journal-entry JSON).
5. Vault writes via `store` helpers once the notes contract is ratified;
   until then **parked behind Needs-David (contract)**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Entry import | ✅ tested | export JSON from Day One, drop the ZIP in the import box; confirm rows in `notes/day-one/` + hub last-data |
| Re-import dedup | ✅ tested | re-export and re-import; confirm entries update by uuid, no duplicates |
| Media references | ✅ tested | export with attachments; confirm media refs land in `extra`, no image bytes copied |
| Multi-journal ZIP | ✅ tested | ZIP with multiple `<Name>.json` journals; each entry carries `extra.journal` with the correct name |
| Starred entries | ✅ tested | entries with `starred:true` carry `extra.starred:true` |
| Bare JSON import | ✅ tested | bare `Journal.json` (no zip) accepted via the `.json` extension |

## Build notes (Phase B follower)

- **Contract fit:** notes domain, reuse-bound. `Note` type from `crate::notes`
  (pioneer: bear). Same `upsert_notes_by_month` / `upsert_raw_by_month` pattern.
- **Format confirmed** against a real 1,024-entry export (2014–2026):
  `uuid` (stable id), `creationDate`/`modifiedDate` (UTC `Z` suffix), `text`
  (Markdown), `tags` (string array), `isPinned`, `starred`, `location`, `weather`.
- **Multi-journal:** a Day One ZIP contains multiple `<JournalName>.json` files;
  the journal name (filename sans `.json`) is preserved in `extra.journal`.
- **richText** field (large redundant JSON blob) stripped from the raw layer.
- No new dependencies. No new connection. Import accepts `.zip` or bare `.json`.
- 6 unit tests; all green. `cargo check` green.

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Day One (L2827–L2833). Feasibility 🟢 high. JSON export is official and
widely parsed; the local group container is discouraged and unpublished, so
the export is the right path. ~10M users — worth the P1 priority. Pairs
with Stoic on a shared journal-JSON parser shape.

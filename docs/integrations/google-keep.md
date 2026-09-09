# Google Keep

- **id:** `google-keep`
- **domains:** `notes/` (contract: **Phase 3 pending** — drafted with Apple
  Notes, Bear, Drafts, Day One, Obsidian, Logseq together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (manual Google Takeout download; no API exists)
- **connection:** none — the user downloads a Takeout ZIP (Google account
  login happens on Google's site, not in Trove). No stored credential.
- **evidence:** community-schema — the Takeout per-note JSON is stable and
  well-documented in the community (high confidence; one JSON + one HTML file
  per note)
- **effort / priority:** S / P2
- **needs:** none

## What it is

Google's lightweight notes/checklist app. Holds short notes, checklists,
colors, pins/archives, and image attachments. Unlike the rest of Google
Workspace, Keep has **no API** — Takeout is the only programmatic path, which
makes this an import rather than a periodic pull.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Notes | all accounts | title, `textContent`, color, `isPinned`/`isArchived`/`isTrashed`, `userEditedTimestampUsec` | community schema |
| Checklists | all accounts | `listContent[]` items (`text`, `isChecked`) | community schema |
| Attachments | all accounts | image filenames via `attachments[].filePath` (separate files in the ZIP) | community schema |

All optional in the contract (omit-if-empty).

## Access & auth

- Google Takeout (`takeout.google.com`) → select **Keep** → download ZIP.
  Contents: one HTML + one JSON file per note. No API, no token, no rate
  limit.
- Timestamps are **microseconds** (`userEditedTimestampUsec`) — divide by
  1000 for ms at parse time.
- Color enum: `DEFAULT, RED, ORANGE, YELLOW, GREEN, TEAL, BLUE, CERULEAN,
  PURPLE, PINK, GRAY, WHITE`.
- No TCC. Standalone-clean (parse a local ZIP). Pairs with the other Takeout
  imports (YouTube history, Maps Saved Places) on one import infrastructure.

## Vault mapping

- **Raw layer:** `notes/google-keep/raw/` — the per-note JSON verbatim, full
  fidelity (image files copied alongside or referenced).
- **Contract layer:** `notes/…` per the (pending) notes contract — expected
  one row per note (`ts` = edited time, `source`, `guid` = note id/filename,
  `title`, `body` from `textContent` or rendered `listContent`, flags), with
  checklist structure and color in `extra`. Parked behind the contract draft
  (Needs-David).
- **Dedupe:** Takeout note filename (stable) as `guid`; re-import replaces
  by guid.

## Build plan

1. Module `crates/trove-core/src/google-keep.rs`: `DEF` (Import / behavior
   `Import`), `last_data` hook; no `CONNECTION` (Takeout has no auth in-app).
2. Registration line in `INTEGRATIONS`.
3. Wire to the generic Takeout/ZIP import box — detect the `Keep/` subtree,
   parse per-note JSON, normalize microsecond timestamps, flatten
   `listContent[]`.
4. Fixtures: a sample Keep Takeout subtree (a plain note, a checklist note, a
   note with an image attachment); parser + store tests, unique temp dirs.
5. Vault writes via `store` helpers once the notes contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Notes + checklists | ✅ unit-tested | export a real Keep Takeout, drop the ZIP into the import box; confirm notes + checklist items in `notes/google-keep/` + hub last-data |
| Attachments | ✅ unit-tested | confirm image-bearing notes reference their files correctly in extra.attachments |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Google Keep (L2899–L2905). Feasibility 🟢 high. No Keep API exists — Takeout
JSON is the only path and is community-documented and stable. Watch the
microsecond timestamps and the `listContent[]` checklist shape. Shares the
notes contract with Apple Notes, Bear, Drafts, Day One, Obsidian, Logseq.

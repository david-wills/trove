# Stoic

- **id:** `stoic`
- **domains:** `notes/` (contract: **notes** — bound; journaling entries land
  here alongside Day One)
- **status:** 🧪 built (parser parked — Needs-sample to verify field names)
- **unavailable_reason:** none
- **behavior:** Import (user-initiated JSON full-backup export)
- **connection:** none — the export is a local file the user supplies; no login.
- **evidence:** official-docs — structured JSON full-backup export (app menu >
  Import & Export → JSON), re-runnable, dedup by entry UUID
- **effort / priority:** S / P2
- **needs:** none

## What it is

iOS-first journaling app (Mac app arrived 2025) blending guided journaling with
mood/metric tracking. Syncs across iPhone, Mac, iPad, and Apple Watch via
iCloud. Smaller user base than Day One but structurally near-identical — the
JSON full backup carries entry text, timestamps, an attachments flag, and
mood/metrics, so it bundles with the Day One parser.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Journal entries | all plans | text, created timestamp | official docs |
| Mood / metrics | all plans | mood + metric values per entry | official docs |
| Attachments flag | export toggle | whether media is present | official docs |

All optional in the contract (omit-if-empty). Photos are included only if the
user toggles **Include Attachments** at export; otherwise entries carry an
attachments flag but no media.

## Access & auth

- File paths / export mechanism: app menu > Import & Export → **JSON** (full
  backup with metadata, re-importable) or TXT (text-only). JSON is the
  collection target. No known local DB path — direct-DB collection is not
  available; import-only.
- No auth, no API, no TCC. Standalone-clean: Trove reads a file the user hands
  it via the generic import box.

## Vault mapping

- **Raw layer:** `notes/stoic/raw/` — the imported JSON backup, full fidelity.
- **Contract layer:** `notes/stoic/` per the (pending Phase 3) notes contract —
  expected shape: one row per entry (`ts` = created, `source`, `guid` = entry
  uuid, `body` = entry text, `tags[]`), mood/metric values in `extra`.
  Attachments are referenced (flag/path), never copied into the vault as image
  blobs.
- **Dedupe:** `guid` = entry uuid; re-imports of a fresh backup are idempotent
  by uuid (the export is explicitly re-runnable).

## Build plan

1. Module `crates/trove-core/src/stoic.rs`: `DEF` (Import), no `CONNECTION`.
   Register one line in `INTEGRATIONS`.
2. Parser reads the JSON full backup; bundle with / mirror the Day One JSON
   parser (similar structure). `letterboxd.rs` is the reference import pattern.
3. Map entry text/timestamps to the notes rows; mood/metrics into `extra`.
4. Fixtures: a JSON backup sample with a plain entry, a mood-tagged entry, and
   one with the attachments flag set; parser + store tests, unique temp dirs.
5. Vault writes via `store` helpers once the notes contract is ratified; until
   then parked behind the pending-contract flag (raw fidelity lands regardless).

## Build status (fan-out #178)

- **Behavior:** `Import` (JSON / zip; re-runnable, dedup by UUID)
- **Contract:** `notes` (reuse-bound; `Note` rows in `notes/stoic/YYYY-MM.jsonl`)
- **Raw:** `notes/stoic/raw/YYYY-MM.jsonl` (full fidelity, always written)
- **Parser status:** PARKED — scaffold built against inferred field names
  (`uuid`, `text`, `createdAt`, `modifiedAt`, `tags`, `hasAttachments`); mood
  and metrics land in `extra` via unknown-key pass-through. Serde aliases cover
  Day One–style names (`id`/`creationDate`/`modifiedDate`/`body`) as fallbacks.
  Timestamp fields (`createdAt`/`modifiedAt`) accept **both** string ISO 8601
  and integer epoch (seconds or milliseconds) encoding — `to_local_rfc3339_value`
  uses a 10-vs-13-digit heuristic; `title`/`journal`/`folder` fields mapped to
  `Note.title`/`Note.folder` when present.
  **Needs a real export sample to verify exact field names before shipping.**
- **Tests:** 8 passing (unique temp dirs, plausible fixture shape; includes
  `epoch_timestamps_are_accepted` and `to_local_rfc3339_value_unit` regression
  tests for the silent-zero-collection fix)
- **No new connection, no new dep, no shared contract files touched**

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Journal entries | 🧪 scaffold | export JSON full backup; drop it in the import box; confirm rows in `notes/stoic/` + hub last-data |
| Mood / metrics | 🧪 scaffold | confirm mood/metric values land in `extra` on entries that have them |
| Field names | ❌ Needs-sample | obtain a real Stoic export; verify `uuid`/`text`/`createdAt` match (or correct aliases) |
| Timestamp encoding | ❌ Needs-sample | check whether `createdAt`/`modifiedAt` are **strings** (ISO 8601) or **integers** (epoch-seconds or epoch-ms); the parser handles both, but verify the real type with `jq 'type' <(jq '.entries[0].createdAt' export.json)`; if a field was missing/wrong-type in older scaffold, the entry was silently skipped — regression test `epoch_timestamps_are_accepted` covers both numeric forms |
| Title / folder | ❌ Needs-sample | if real export carries a `title` or `journal`/`folder` field, verify it lands in `note.title` / `note.folder`; until sample confirmed, these fields survive verbatim in `raw` + `extra` |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Stoic (journal app) (L2947–L2953). Feasibility 🟢 high. JSON is structured and
re-runnable with UUID dedup. Smaller user base than Day One drives the P2
priority; the right move is to sequence it right after Day One and reuse that
parser. TXT export is text-only and lossy — prefer JSON. No local DB, so
import is the only path.

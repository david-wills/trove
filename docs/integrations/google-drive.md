# Google Drive

- **id:** `google-drive`
- **domains:** `files/` (raw-only per the taxonomy — no shared contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (scan the local Drive-for-Desktop mirror; diff against
  previous snapshot)
- **connection:** none for the primary local-folder path. The existing `google`
  connection (OAuth, already shared by six Google defs) is the natural carrier
  for a *future* Drive API v3 fallback / Docs-content export — recorded here,
  not in initial scope.
- **evidence:** well-known local path (`~/Library/CloudStorage/Google Drive/`,
  File Provider macOS 12.1+) — 🟢 high per the research doc; Drive API v3
  officially documented (docs current May 2026 per research)
- **effort / priority:** S / P1
- **needs:** none

## What it is

Google's cloud drive via the Drive for Desktop app. Same File Provider local
mirror as Dropbox/OneDrive, so it's another root for the generic cloud-folder
watcher. Distinct service from the six shipped Google defs (Gmail, Calendar, …)
— connection sharing doesn't merge it into them; it gets its own brief and def.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| File metadata index | none (Drive for Desktop installed) | rel path, name, ext, size, created/modified ts | well-known path |
| Change events over time | none | added / modified / removed since last scan | snapshot diff |
| Docs/Sheets/Slides stubs | none | name + ext (`.gdoc`/`.gsheet`/`.gslides`) from stub files; placeholder=true in stream mode; no content | research doc |
| Docs content (future) | `google` OAuth | exported text/docx via `files.export` | official Drive API v3 docs |

Stubs are first-class metadata rows with their Google type distinguishable by
extension (`.gdoc` → Google Doc, `.gsheet` → Google Sheet, etc.). In stream
mode they are recorded with `placeholder=true, size=0`; the extension is
always preserved. Content export is the API-fallback capability, deferred.

## Access & auth

- Local mirror: `~/Library/CloudStorage/Google Drive/` (File Provider,
  macOS 12.1+). "Stream files" mode (the default) leaves most files as
  placeholders; "Mirror files" keeps all local — index what's there either way.
- TCC: covered by Trove's existing Full Disk Access grant. No new prompt.
- **Stub gotcha:** Google Docs/Sheets/Slides are never real local files — only
  `.gdoc`/`.gsheet`/`.gslides` web stubs. The local path yields their names and
  structure only; content requires Drive API `files.export`.
- Fallback/extension: Drive API v3 (`GET /drive/v3/files`, `files.export`) on
  the existing `google` connection; generous rate limit (12k queries/min/user).
  Deferred — local folder first. Drive for Desktop is detected, never required.

## Vault mapping

- **Raw layer:** `files/google-drive/` — metadata snapshots + month-partitioned
  change events (JSONL). Metadata only; contents never copied into the vault.
- **Contract layer:** none — `files/` is raw-only per the taxonomy.
- **Dedupe:** `guid` = hash of (rel path + change kind + mtime); cursor in
  `.trove/`, rebuildable from the latest snapshot.

## Build plan

1. Reuse the generic cloud-folder watcher (built for `icloud-drive`).
2. Module `crates/trove-core/src/google_drive.rs`: `DEF` (Periodic); path
   detection for `~/Library/CloudStorage/Google Drive*/` (account-suffixed
   variants exist); stub-file recognition (`.gdoc`/`.gsheet`/`.gslides`);
   permission hook = path exists + readable, card hint when Drive for Desktop
   isn't installed.
3. One registration line in `INTEGRATIONS`. No new connection.
4. Fixtures: temp-dir trees with real files, placeholders, and stub files;
   tests per shape (unique temp dirs).
5. (Later, demand-driven) Drive API v3 def on `connection: Some("google")`
   for users without the desktop app + Docs content export.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Metadata snapshot | ✅ | on a Mac with Drive for Desktop, enable + Sync now; rows in `files/google-drive/<account>/` match the folder |
| Change events | ✅ | add/modify/delete a test file; re-sync; confirm event kinds in `files/google-drive/<account>/events/` |
| Stub handling | ✅ | .gdoc/.gsheet/.gslides appear as metadata rows with ext captured; in stream mode (default) stubs are SF_DATALESS → placeholder=true, size=0, ext preserved |
| Stream-mode placeholders | ✅ | SF_DATALESS bit (0x40000000 in st_flags) detected via real_is_dataless; placeholder=true, size=0 |
| Multi-account | ✅ | All GoogleDrive-* accounts indexed under separate vault sub-paths; Shared drives as secondary root |

## Build notes (2026-06-15)

- Reused `cloud_folder::scan_and_diff` + `real_is_dataless` (same engine as Dropbox).
- Path probe: all `~/Library/CloudStorage/GoogleDrive-<account>/` directories, indexing `My Drive/` and optionally `Shared drives/`.
- Confirmed on real install: two accounts present (`GoogleDrive-dwills@example.com` and `GoogleDrive-personal@gmail.com`), both with `My Drive/` subdirectory.
- Google stub files (.gdoc/.gsheet/.gslides etc.): in stream mode (the default) these stubs ARE SF_DATALESS — empirically 99 % of stubs on a real install show `st_flags=0x40000060` (SF_DATALESS | SF_COMPRESSED | …). Only already-materialized stubs show `st_flags=0x40` (no SF_DATALESS). So in production nearly all stubs are recorded `placeholder=true, size=0`; their extension (gdoc/gsheet/…) is still captured, distinguishing them from binary placeholders.
- Stream-mode placeholder files (large binary, not downloaded): same SF_DATALESS bit (0x40000000). `real_is_dataless` correctly identifies both stub and binary placeholders via the same flag check.
- contract_mode: raw-only (files/ domain has no contract per taxonomy).
- 8/8 unit tests green; cargo check clean.

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Google Drive for Desktop (L2851–L2857) + cross-cutting note 1 (L2997).
Feasibility 🟢 high. Research notes the Google worktree's OAuth plumbing makes
the API path a natural extension — record it, don't build it yet. Google Drive
*file storage* is unrelated to `google-books`/`google-youtube` data; only the
login would be shared.

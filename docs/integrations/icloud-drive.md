# iCloud Drive

- **id:** `icloud-drive`
- **domains:** `files/` (raw-only per the taxonomy — no shared contract; vault-wide
  conventions still apply: guids, timestamps, partitions)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (scan the local mirror on an interval; diff against the
  previous snapshot)
- **connection:** none — it is just the filesystem. No login, no API.
- **evidence:** well-known local path (`~/Library/Mobile Documents/com~apple~CloudDocs/`),
  plain filesystem — community-documented, high confidence per the research doc (🟢)
- **effort / priority:** S / P1
- **needs:** none

## What it is

Apple's cloud file storage, on by default for most Mac users. The local mirror is
a real directory of real files — the cheapest possible window into what documents
a user keeps and when they change. Anchor of the generic **cloud-folder watcher**:
one scan/diff mechanism with per-service path detection covers iCloud Drive,
Dropbox, Google Drive, and OneDrive (research cross-cutting note 1).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| File metadata index | none | rel path, name, ext, size, created/modified ts | well-known path, plain FS |
| Change events over time | none | added / modified / removed since last scan | snapshot diff, no OS API needed |
| App-specific containers | none (opt-in subfolders) | per-app documents under `com~apple~*` bundle dirs | research doc |

All optional in the per-source record shape; placeholders (un-downloaded files)
yield name + placeholder flag, no size/content — never a special code path, just
omitted fields.

## Access & auth

- Local mirror: `~/Library/Mobile Documents/com~apple~CloudDocs/`; app-specific
  data at `~/Library/Mobile Documents/<bundle-id>/`.
- TCC: the path is under `~/Library` — covered by the Full Disk Access grant
  Trove already holds (Safari, iMessage, Biome). No new prompt.
- No network, no auth, standalone-clean.
- **Placeholder gotcha:** with "Optimize Mac Storage" / stream mode, un-downloaded
  files appear in two forms: on Sonoma 14+ File Provider keeps the original filename
  and sets the `SF_DATALESS` `st_flags` bit (Apple TN3150); on older macOS the file
  is replaced with a hidden stub named `.<original_name>.icloud` (leading dot, no
  tilde). Reading either can trigger a download. The scanner detects placeholders via
  the `SF_DATALESS` flag OR the `.<name>.icloud` name pattern and indexes metadata
  only — never opens them.

## Vault mapping

- **Raw layer:** `files/icloud-drive/` — periodic metadata snapshots + change
  events (JSONL, month-partitioned for events). **Metadata only — Trove never
  copies file contents into the vault** (same principle as photos: index, don't
  duplicate).
- **Contract layer:** none — `files/` is raw-only per the taxonomy.
- **Dedupe:** `guid` = hash of (rel path + change kind + mtime); scan cursor in
  `.trove/`, rebuildable from the latest snapshot.

## Build plan

1. Build the **generic cloud-folder watcher** in `crates/trove-core` (this is
   its first instance): scan a root, skip/log placeholders, diff vs. last
   snapshot, emit events + snapshot via `store` helpers.
2. Module `crates/trove-core/src/icloud_drive.rs`: `DEF` (Periodic), permission
   hook = path-exists + readable check (detects iCloud Drive disabled).
3. One registration line in `INTEGRATIONS`.
4. UI: optional subfolder scoping (whole drive can be huge); default to the
   user-visible root, surface placeholder counts in Recent data.
5. Fixtures: synthetic temp-dir trees incl. `.icloud` placeholder names; tests
   for diff correctness and placeholder skip (unique temp dirs).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Metadata snapshot | ✅ | enable on a Mac with iCloud Drive on; Sync now; confirm snapshot rows in `files/icloud-drive/` match Finder |
| Change events | ✅ | add/modify/delete a test file; re-sync; confirm three event kinds appear |
| Placeholder handling | ✅ | `SF_DATALESS` `st_flags` bit (Sonoma 14+) OR `.<name>.icloud` legacy stub pattern detected; placeholder=true, size=0, content never read |

## Build notes (2026-06-15)

- Replaced NotWired stub with real Periodic implementation.
- Reuses `cloud_folder::scan_and_diff` (same engine as `dropbox.rs`) — lstat-only, silent baseline on first scan, month-partitioned events, atomic snapshot rewrite.
- iCloud-specific placeholder detection: combines `cloud_folder::real_is_dataless` (SF_DATALESS `st_flags` bit, Apple TN3150; primary on Sonoma 14+) with `.<name>.icloud` legacy stub pattern check (`is_icloud_name_placeholder`). Both are checked via OR; the filename pattern is injectable in tests.
  - Corrected: the real iCloud stub format is `.<name>.icloud` (leading dot only), NOT `.~<name>.icloud` (which does not occur in practice). Verified against production iCloud Drive content.
  - Added `resolve_icloud_stub_name` helper: strips leading `.` and `.icloud` suffix for UI display of legacy stub names. Vault stores the raw stub filename; resolution is a UI-layer concern.
- SF_DATALESS injection test added: `scan_records_placeholder_via_dataless_flag_side` exercises the flag-side OR branch via closure injection, proving Sonoma-style eviction detection works without needing real SF_DATALESS files.
- No new crate deps, no new connection, no contract layer (files/ is raw-only).
- 11 unit tests all green; cargo check clean.

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§iCloud Drive (L2835–L2841) + cross-cutting note 1 (L2997). Feasibility 🟢 high.
Build this first of the four cloud drives — it needs no app installed and no
account, and the watcher it produces makes Dropbox/Google Drive/OneDrive
near-zero marginal cost. App-specific `Mobile Documents` containers (Pages,
Ulysses, …) are separate per-app schemas — out of scope here; other briefs
(e.g. Ulysses) may point at them.

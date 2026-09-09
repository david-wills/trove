# macOS Downloads

- **id:** `macos-downloads`
- **domains:** `files/macos-downloads/` (raw-only — `files/` has no
  contract per the taxonomy)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (local SQLite read, copy-then-read; OS writes it,
  Trove only reads)
- **connection:** none
- **evidence:** community-schema, high confidence — the LSQuarantineEvent
  table is a well-known, stable, fully-enumerated schema
  (`~/Library/Preferences/com.apple.LaunchServices.QuarantineEventsV2`)
- **effort / priority:** S / P2
- **needs:** none

## What it is

macOS's quarantine system logs every file downloaded by Safari, Chrome,
Firefox, Mail, and any quarantine-aware app into a single home-dir SQLite
database — including the **source URL and the referring page**, and the
records persist even after the file is deleted or moved. It answers "what
did I download, from where, and via what" — a richer complement to browser
history than a Downloads-folder listing, with zero permissions and zero
setup.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Download events | none — every Mac has this | ts, source URL, referrer URL, origin page title, downloading app (bundle id + name), sender name/address (Mail/AirDrop), type | well-known LSQuarantineEvent schema |

All columns optional in the raw rows (Mail-only fields like sender are
usually empty for browser downloads — omit-if-empty).

## Access & auth

- SQLite at `~/Library/Preferences/com.apple.LaunchServices.QuarantineEventsV2`,
  single table `LSQuarantineEvent` (UUID identifier, timestamp, agent
  bundle id/name, data URL, origin URL, origin title, sender name/address,
  type number).
- No TCC prompt (`~/Library/Preferences` is accessible without FDA).
  Copy-then-read; never write — the OS owns this file.
- Timestamps are Apple epoch (seconds since 2001-01-01 UTC) — reuse the
  existing `SAFARI_EPOCH_OFFSET_S` constant from `browser.rs`.
- Records persist indefinitely unless the user clears them; first sync is
  a deep backfill for free.

## Vault mapping

- **Raw layer:** `files/macos-downloads/YYYY-MM.jsonl` — one row per
  download event: ts, url, referrer_url, origin_title, app_bundle_id,
  app_name, plus sender fields when present. (The research entry's
  `developer/downloads/` path predates the taxonomy; `files/` governs —
  these are file-system activity records, not developer activity.)
- **Contract layer:** none — `files/` is raw-only.
- **Dedupe:** `LSQuarantineEventIdentifier` UUID as `guid`; incremental
  cursor on the Apple-epoch timestamp, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/macos_downloads.rs` (def id
   `macos-downloads`): `DEF` Periodic — copy DB to temp, query rows newer
   than the cursor, write via `store` helpers.
2. One registration line in `INTEGRATIONS`. No connection, no permission
   flow (permission hook = file-exists/readable).
3. Fixture: a small synthetic QuarantineEventsV2 SQLite with browser-,
   Mail-, and sparse-column rows; epoch-conversion + dedupe + cursor
   tests, unique temp dirs.
4. Reuse/share the Apple-epoch constant rather than redefining it.

## Implementation notes

- Schema confirmed from real DB (PRAGMA table_info): 11 columns, `LSQuarantineTimeStamp` is REAL (float seconds since 2001-01-01 UTC).
- Real data shows `LSQuarantineDataURLString` and `LSQuarantineOriginURLString` are often empty for Chrome downloads in this env; URL fields are optional.
- `LSQuarantineOriginAlias` (BLOB) is always NULL in practice; intentionally omitted from raw rows.
- Cursor is a f64 (Apple REAL epoch) stored in `.trove/macos-downloads-sync.json`.
- `import_via_copy` from `browser.rs` reused for the copy-then-open pattern.
- 4 unit tests: epoch round-trip, incremental import + dedupe, sync state persist/reload, zero-timestamp row skip.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Download events | ✅ built | enable, Sync now; download a file in Safari and one in Chrome; re-sync; confirm both rows in `files/macos-downloads/` with correct URL + referrer + app, timestamps in correct era (not 2001-shifted); third sync adds nothing |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §macOS
Download History (QuarantineEventsV2) (L1380–L1386). Feasibility 🟢 high,
"build now" recommendation — zero-permission S effort, surprisingly rich.
Users can clear the table via Finder's "Clear Downloads," so synced vault
rows may outlive the source records (fine — the vault is the durable copy).
This is also the sanctioned answer to "file activity" asks that the
unavailable FSEvents-journal and Recent-Files entries point at.

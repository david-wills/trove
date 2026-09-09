# Dropbox

- **id:** `dropbox`
- **domains:** `files/` (raw-only per the taxonomy — no shared contract)
- **status:** 🧪 built (fixture-tested; local-mirror watcher + reusable `cloud_folder` engine; needs the Dropbox app + a real folder to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (scan the local mirror; diff against previous snapshot)
- **connection:** none for the primary local-folder path. A Dropbox OAuth
  connection (API v2) exists only as a *future fallback* for users without the
  desktop app — not part of the initial build.
- **evidence:** well-known local path (`~/Library/CloudStorage/Dropbox/`,
  post-File-Provider macOS 12.5+) — community-documented, 🟢 high per the
  research doc; API v2 officially documented as fallback
- **effort / priority:** S / P1
- **needs:** none

## What it is

The most widely-installed third-party cloud drive. Since the 2023 File Provider
migration its local mirror is a standard filesystem path, so it folds straight
into the generic cloud-folder watcher built for iCloud Drive — same scan/diff
code, different root.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| File metadata index | none (desktop app installed) | rel path, name, ext, size, created/modified ts | well-known path |
| Change events over time | none | added / modified / removed since last scan | snapshot diff |
| API listing (fallback) | Dropbox OAuth app | same metadata via `files/list_folder` | official API v2 docs |

Online-only files appear as placeholders — indexed with a placeholder flag and
no size/content, no special code path.

## Access & auth

- Local mirror: `~/Library/CloudStorage/Dropbox/` (File Provider, macOS 12.5+).
  Legacy pre-migration path `~/Dropbox` — **probe both**, prefer CloudStorage.
- TCC: `~/Library/CloudStorage` is under the Full Disk Access grant Trove
  already holds. No new prompt.
- Dropbox warns against *modifying* the CloudStorage folder; **reading is safe**
  per the research doc. The watcher is strictly read-only.
- Fallback: API v2 (`api.dropboxapi.com/2/files/list_folder`, OAuth2) for users
  without the desktop app — clean but needs app registration; deferred until
  demand. Standalone rule: the desktop app is *detected*, never required —
  if absent, the card explains the local path needs the Dropbox app (or the
  future API fallback).

## Vault mapping

- **Raw layer:** `files/dropbox/` — metadata snapshots + month-partitioned
  change events (JSONL). Metadata only; file contents are never copied into
  the vault.
- **Contract layer:** none — `files/` is raw-only per the taxonomy.
- **Dedupe:** `guid` = hash of (rel path + change kind + mtime); cursor in
  `.trove/`, rebuildable from the latest snapshot.

## Build plan

1. Reuse the generic cloud-folder watcher (built for `icloud-drive`).
2. Module `crates/trove-core/src/dropbox.rs`: `DEF` (Periodic); path detection
   probes CloudStorage then legacy `~/Dropbox`; permission hook = path exists +
   readable, with a card hint when the desktop app isn't installed.
3. One registration line in `INTEGRATIONS`.
4. Fixtures: temp-dir trees incl. placeholder-shaped entries; tests for both
   path locations (unique temp dirs).
5. (Later, demand-driven) `CONNECTION` for OAuth API fallback — TokenPaste/OAuth
   per ConnectSpec; out of initial scope.

## Build status — 🧪 2026-06-15

Shipped (`dropbox.rs` + the new **reusable `cloud_folder.rs` engine**, INDEX #20 —
the **first cloud-folder watcher**). `Behavior::Periodic` (15 min), no connection
(the OAuth API is deferred per the brief), rides the existing FDA grant
(`~/Library/CloudStorage` is covered — no new prompt). **Default-on** (file
**metadata only** — names/paths/sizes/timestamps; contents are NEVER read or
copied — so it sits with contacts/calendar, not with email bodies which are
opt-in). *David may opt the whole `files/` watcher class out — pending his call.*

- **`cloud_folder.rs`** (provider-agnostic, inherited by iCloud Drive / OneDrive /
  Google Drive / Box): `scan_and_diff(vault, vault_path, root, is_dataless)` walks
  a mirror tree **lstat-only — never `open()`s a file** (reading content would
  fetch the user's whole drive), snapshots `files/<source>/snapshot.jsonl`
  (path/name/ext/size/created/modified/is_dir/placeholder), diffs vs the prior
  snapshot → `added`/`modified`/`removed` events in
  `files/<source>/events/YYYY-MM.jsonl`. `guid` = `sha256(path|kind|mtime|size)`.
  **First scan = silent baseline** (write the snapshot, emit no events — mirrors
  `books.rs`/`podcasts.rs`; avoids a thousands-of-`added` flood on first sync).
- **Online-only / "dataless" placeholders:** detected via the BSD **`SF_DATALESS`
  `st_flags` bit** (Apple TN3150) read through `std::os::darwin::fs::MetadataExt`
  (`#[cfg(macos)]`; non-mac stub → false). A dataless **directory** is recorded but
  **not descended into** (recursing would materialize the subtree). Placeholders
  carry `placeholder:true`, size 0, no content access.
- **`dropbox.rs`** owns only: the path probe (`~/Library/CloudStorage/Dropbox` →
  `Dropbox (Personal)` → `Dropbox (*)` excluding `Old`/timestamp noise → legacy
  `~/Dropbox`; `TROVE_HOME` test override), the `DEF`, and the hooks. Symlinks are
  skipped (so the legacy `~/Dropbox`→CloudStorage symlink can't double-scan).

Evidence: paths + the `SF_DATALESS` download-safe detection confirmed against
Apple TN3150 + community sources (spike). Adversarial-verify (Opus): **1 BLOCKING
+ 1 minor + 1 test-gap, all fixed** — (B) first scan emitted an `added` per file
(flood) → now a silent baseline keyed on the snapshot FILE existing; (m) guid
omitted size (collision) → added; (gap) the production `SF_DATALESS` read was
untested → covered. No-content-read, the dataless-dir skip, `SF_DATALESS` value +
API, symlink-loop safety, and the reuse factoring were all independently confirmed.

Gate (my run, serial): trove-core 625/0 (+ cloud_folder/dropbox tests),
`cargo check` clean, `schedule_doc` regenerated (dropbox Periodic), `bindings.ts`
up to date. **Deferred:** the OAuth API fallback (for users without the desktop
app — noted in the DEF caveats, not wired).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Metadata snapshot | 🧪 (needs the Dropbox app) | on a Mac with the Dropbox app synced, enable + Sync now; rows in `files/dropbox/snapshot.jsonl` match the folder; first sync writes the snapshot with NO event flood |
| Change events | 🧪 (needs the Dropbox app) | add/modify/delete a test file; re-sync; confirm `added`/`modified`/`removed` in `files/dropbox/events/` |
| Legacy path | 🧪 | on a pre-migration install (or a `TROVE_HOME` symlinked fixture), confirm `~/Dropbox` is found when CloudStorage is absent |
| Online-only files | 🧪 (needs a real online-only file) | mark a file online-only in Dropbox; confirm `placeholder:true`, size 0, and NO download triggered (the file stays online-only) |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Dropbox (L2843–L2849) + cross-cutting note 1 (L2997). Feasibility 🟢 high.
Sequence right after `icloud-drive` — it is the same watcher with a different
root and a two-path probe. The API fallback is the only piece with real cost;
ship without it.

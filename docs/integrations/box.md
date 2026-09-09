# Box

- **id:** `box`
- **domains:** `files/` (raw-only per the taxonomy — no shared contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (scan the local Box Drive mirror; diff against previous
  snapshot)
- **connection:** none for the local-folder path (Box Drive app). A Box OAuth2
  connection (developer-app registration; personal accounts support OAuth2
  only, no JWT/CCG) would be needed for the API path — deferred, not shared
  with any other def.
- **evidence:** local path via File Provider (`~/Library/CloudStorage/Box-Box/`,
  older versions `~/Box/`) — 🟡 medium per the research doc; Box API v2
  officially documented but gated on dev-app registration
- **effort / priority:** M / P2
- **needs:** none

## What it is

Enterprise-leaning cloud storage. Rare on personal Macs (the research doc
recommends icebox for the API path), but when the Box Drive desktop app *is*
installed it exposes the same File Provider local mirror as the other cloud
drives — so the cheap local-folder slice rides the generic watcher for almost
nothing, and only that slice is initially in scope.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| File metadata index | none (Box Drive installed) | rel path, name, ext, size, created/modified ts | File Provider path, research doc |
| Change events over time | none | added / modified / removed since last scan | snapshot diff |
| API listing/download (deferred) | Box dev-app OAuth2 | `GET /folders/{id}/items`, file content; 1,000 calls/min | official API v2 docs |

Online-only files are placeholders — indexed with a flag, no special code path.

## Access & auth

- Local mirror: `~/Library/CloudStorage/Box-Box/` (Box Drive on macOS 12.5+;
  Apple's File Provider framework names dirs `<provider>-<domain>`, and Box
  Drive uses provider "Box" + domain display name "Box", yielding "Box-Box").
  Older Box Drive versions used `~/Box/` — probe both.
- TCC: covered by Trove's existing Full Disk Access grant. No new prompt.
- API path: Box API v2 with OAuth2 via a registered Box developer app —
  personal/free accounts cannot use CCG or JWT. Registration + review overhead
  for a niche personal-Mac audience is why the research doc iceboxes it;
  revisit on community demand. Box Drive is detected, never required.

## Vault mapping

- **Raw layer:** `files/box/` — metadata snapshots + month-partitioned change
  events (JSONL). Metadata only; contents never copied into the vault.
- **Contract layer:** none — `files/` is raw-only per the taxonomy.
- **Dedupe:** `guid` = hash of (rel path + change kind + mtime); cursor in
  `.trove/`, rebuildable from the latest snapshot.

## Build plan

1. Reuse the generic cloud-folder watcher (built for `icloud-drive`).
2. Module `crates/trove-core/src/box_drive.rs` (module name avoids the Rust
   `box` keyword; def id stays `box`): `DEF` (Periodic); path probe for
   CloudStorage then legacy `~/Box/`; permission hook = path exists + readable,
   card hint when Box Drive isn't installed.
3. One registration line in `INTEGRATIONS`. No connection in initial scope.
4. Fixtures: temp-dir trees for both path variants + placeholders (unique temp
   dirs).
5. The M effort and P2 priority reflect the *API* path (OAuth app registration,
   personal-account constraints) — the local-folder slice alone is S; ship it
   and leave the API def iceboxed per research.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Metadata snapshot | ✅ unit tested | `cargo test -p trove-core box_::` — `collect_scan_baseline_and_event` baseline pass |
| Change events | ✅ unit tested | same test — second scan with new file yields `added` event |
| Legacy path | ✅ unit tested | `probe_finds_legacy_box` |
| Modern CloudStorage path (Box-Box) | ✅ unit tested | `probe_finds_box_box_canonical` |
| Canonical-over-alphabetical preference | ✅ unit tested | `probe_prefers_box_box_over_alphabetically_earlier_glob` |
| Bare Box forward-compat | ✅ unit tested | `probe_finds_cloud_storage_box` |
| Glob fallback | ✅ unit tested | `probe_glob_fallback_finds_other_box_variant` |
| Timestamped/Old exclusion | ✅ unit tested | `probe_skips_old_and_timestamped_dirs` |
| Not-installed graceful miss | ✅ unit tested | `probe_finds_nothing_when_box_not_installed` |
| Real user acceptance | Needs-login | Needs a Box account + Box Drive installed — David doesn't use Box; real-user run can validate |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Box (L2971–L2977) + cross-cutting note 1 (L2997). Feasibility 🟡 medium —
entirely because of the API's dev-app gate; the local folder is as solid as
Dropbox's. Catalog row notes "icebox-leaning: primarily enterprise, niche on
personal Macs" — hence P2, sequenced after the three P1 drives, local-folder
slice only.

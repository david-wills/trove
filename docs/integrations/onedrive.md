# OneDrive

- **id:** `onedrive`
- **domains:** `files/` (raw-only per the taxonomy — no shared contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (scan the local mirror; diff against previous snapshot)
- **connection:** none for the primary local-folder path. A future `microsoft`
  OAuth connection (Graph API, delegated) would be **shared with OneNote** and
  any later Microsoft defs — recorded here, not in initial scope.
- **evidence:** well-known local path (`~/Library/CloudStorage/OneDrive-Personal/`,
  File Provider macOS 12.5+) — 🟢 high per the research doc; Microsoft Graph
  API officially documented as fallback
- **effort / priority:** S / P1
- **needs:** none

## What it is

Microsoft's cloud drive, ubiquitous for anyone in the Microsoft 365 ecosystem
(less common than iCloud/Dropbox on personal Macs, but a large population).
Another File Provider root for the generic cloud-folder watcher — near-zero
marginal cost once that exists.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| File metadata index | none (OneDrive app installed) | rel path, name, ext, size, created/modified ts | well-known path |
| Change events over time | none | added / modified / removed since last scan | snapshot diff |
| Work-account drives | none | same metadata under `OneDrive-<org>/` roots | research doc |
| Graph API listing (future) | `microsoft` OAuth | `GET /me/drive/root/children`, file content | official Graph docs |

Online-only files are placeholders — indexed with a flag, no special code path.

## Access & auth

- Local mirror: `~/Library/CloudStorage/OneDrive-Personal/` for personal
  accounts; `~/Library/CloudStorage/OneDrive-<OrgName>/` per signed-in work
  account — enumerate all `OneDrive-*` roots and index each as a labeled
  sub-source.
- TCC: covered by Trove's existing Full Disk Access grant. No new prompt.
- Fallback: Microsoft Graph API (OAuth 2.0 delegated — app-only auth was
  removed March 2025) for users without the desktop app. Requires an Azure AD
  app registration; bundle with OneNote when/if a `microsoft` connection is
  built (research cross-cutting note 5). Deferred. The OneDrive app is
  detected, never required.

## Vault mapping

- **Raw layer:** `files/onedrive/` — metadata snapshots + month-partitioned
  change events (JSONL), with an account label per `OneDrive-*` root. Metadata
  only; contents never copied into the vault.
- **Contract layer:** none — `files/` is raw-only per the taxonomy.
- **Dedupe:** `guid` = hash of (account root + rel path + change kind + mtime);
  cursor in `.trove/`, rebuildable from the latest snapshot.

## Build plan

1. Reuse the generic cloud-folder watcher (built for `icloud-drive`).
2. Module `crates/trove-core/src/onedrive.rs`: `DEF` (Periodic); path detection
   globs `~/Library/CloudStorage/OneDrive-*/`; multi-root handling (personal +
   org accounts); permission hook = any root exists + readable, card hint when
   the OneDrive app isn't installed.
3. One registration line in `INTEGRATIONS`. No connection yet.
4. Fixtures: temp-dir trees with two `OneDrive-*` roots + placeholder entries;
   tests for multi-root labeling (unique temp dirs).
5. (Later, demand-driven) `microsoft` ConnectionDef + Graph-API def, shared
   with a OneNote def — one Azure registration, one login, many defs (the
   Google model).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Metadata snapshot | ✅ unit-tested | `scan_writes_per_account_vault_paths` — two OneDrive roots in temp dir, snapshot written to separate sub-paths |
| Change events | ✅ unit-tested | `change_events_emitted_after_baseline` — add file after baseline, event appears in events/YYYY-MM.jsonl |
| Multi-account roots | ✅ unit-tested | `finds_multiple_roots` — personal + work roots found and labeled; `roots_sorted_by_label` sorts deterministically |
| Online-only files | ✅ unit-tested | `placeholder_recorded_size_zero` — injected seam marks file as dataless, recorded with placeholder=true, size=0 |
| Label slugification | ✅ unit-tested | `label_slugifies_spaces` — "Contoso Ltd" → "contoso-ltd" |
| No-roots graceful | ✅ unit-tested | `no_roots_when_absent`, `last_data_none_when_empty_vault` |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§OneDrive (L2963–L2969) + cross-cutting notes 1 (cloud-drive convergence,
L2997) and 5 (Microsoft stack bundling, L3005). Feasibility 🟢 high. Research
suggested "build later" relative to iCloud/Dropbox, but as the third root on a
shared watcher the cost is trivial — keep it in the same wave; only the Graph
API piece stays deferred.

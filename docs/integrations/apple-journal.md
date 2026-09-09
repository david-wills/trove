# Apple Journal

- **id:** `apple-journal`
- **domains:** `notes/` (contract: **Phase 3 pending** — collected notes land
  here; `artifacts/` stays the user-curated layer)
- **status:** 🚫 unavailable
- **unavailable_reason:** Apple Journal only arrives on the Mac with macOS 26
  (fall 2026). Its local data format is undocumented and it has no export
  feature or API yet. We'll investigate as soon as the app ships.
- **behavior:** Unavailable (NotWired — never toggleable, never default_on)
- **connection:** none
- **evidence:** low — Mac app ships with macOS Tahoe 26 (fall 2026);
  container/DB format not yet documented · sample-required (post-ship spike)
- **effort / priority:** M / P2
- **needs:** Needs-sample (post-ship container introspection) — and the
  notes contract is not yet ratified (Phase 3)

## What it is

Apple Journal is Apple's first-party journaling app — iOS/iPadOS-only since
December 2023, coming to the Mac with macOS Tahoe 26 (fall 2026). Entries are
likely rich text / markdown with photos, backed by a CloudKit-synced
container. For Trove it would be a notes source — first-party personal
journaling that's otherwise locked in Apple's silo. It is catalogued as
unavailable today because the Mac app does not yet exist, has no documented
local format, and offers no export or API.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Journal entries | (speculative — pending Mac app) | entry text (rich text/markdown), timestamps, attached photos | low (iOS pattern only) |

All speculative until the Mac app ships and its container is introspected.

## Access & auth

- **None available today.** No export feature confirmed as of June 2026, no
  API. The iOS `JournalingSuggestions` framework is iOS-only and not a
  data-retrieval path.
- Expected post-ship: a local container — likely a CloudKit-backed SQLite
  under `~/Library/Containers/com.apple.journal/` — readable with **Full Disk
  Access** (speculative). Journal must be enabled in System Settings → Apple
  ID → iCloud for data to be present.
- Standalone-rule: when built, this would read a local Apple container
  directly (no cloud calls, no external app dependency) — clean.

## Vault mapping

- **Raw layer (planned):** `notes/apple-journal/raw/` — entries parsed from
  the local container at full fidelity, partitioned by entry month.
- **Contract layer (planned):** `notes/apple-journal/YYYY-MM.jsonl` per the
  (pending) notes contract — one row per entry (`ts`, `source`, `guid` =
  entry id, `title`/`body`, attachments as sidecar references), overflow in
  `extra`. Collected notes land in `notes/`; `artifacts/` stays the
  user-curated layer.
- **Dedupe (planned):** entry id as `guid`. Out-of-scope entry note: this is
  unavailable today, so no vault path is wired yet.

## Build plan

1. **Spike first (post-ship):** once macOS Tahoe 26 ships, introspect the
   container path and DB format. Use the iOS Journal format research
   (`tallmike/AppleJournaltoDayOne` on GitHub) as a starting clue — the iOS
   store is the likely template for the Mac one.
2. Determine the access shape: local SQLite under the app container (M3, Full
   Disk Access) if found, or an import path if Apple ships an export feature
   (M1) — whichever materializes.
3. Then write `crates/trove-core/src/apple_journal.rs` against a real sample:
   `DEF`, parser, store hooks. Parser-last / Needs-sample by necessity.
4. Until the app ships, this stays `Behavior::Unavailable` — rendered as a
   greyed card with the reason, never toggleable, never default_on.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Journal entries | 🚫 unavailable | blocked until macOS 26 ships and the container format is reverse-engineered; no validation possible today |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Apple Journal (L2955–L2961). Feasibility 🟠 low — the Mac app was announced
June 2025 (WWDC) and ships fall 2026; local DB format not yet documented, no
export API known. Queue a spike right after macOS 26 ships: introspect the
container path/DB; the iOS CloudKit-backed store is the likely template.
Until then it is honestly catalogued as unavailable so the in-app card can
answer "why isn't Apple Journal available?"

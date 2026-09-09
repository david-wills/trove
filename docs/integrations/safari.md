# Safari

- **id:** `safari` (shipped def: `safari-history`)
- **domains:** `browser/` (visit shape — document contract; shipped) ·
  `reading/` (Reading List — contract: **Phase 3 pending**, highlights/saves
  shape; this slice is the queued extension)
- **status:** 🧪 built (visit history shipped pre-pipeline as
  `safari-history`; David promotes to ✅ after real-data validation.
  Reading List import is the remaining unbuilt mechanism.)
- **unavailable_reason:** none
- **behavior:** Periodic (LocalSync — polls Safari's local files; no
  network, no account)
- **connection:** none (local file reads; Full Disk Access is the gate)
- **evidence:** well-known local paths — `~/Library/Safari/History.db`
  (shipped, fixture-tested) and `~/Library/Safari/Bookmarks.plist` (binary
  plist; Reading List entries under the `com.apple.ReadingList` node; the
  `plist` crate parses it)
- **effort / priority:** S / P2 (the Reading List extension)
- **needs:** extension — Reading List import from `Bookmarks.plist` →
  `reading/` · reading contract not yet ratified (Needs-David, for that
  slice only)

## What it is

Apple's default Mac browser, read entirely from its local container. Two
mechanisms, one provider: **visit history** (already shipped — for
non-extension users it's the densest web-activity stream on the machine)
and the **Reading List** (read-later saves, iCloud-synced so it reflects
saves made on iPhone/iPad too). One FDA grant covers both.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Visit history | none | url, title, visit time, visit counts | known History.db schema; shipped |
| Reading List | none | URLString, title (URIDictionary), DateAdded, DateLastViewed, PreviewText | known plist node; research entry |
| Cached article content | none | `~/Library/Safari/ReadingListArchives/` page snapshots | research entry (likely skip — bulky, metadata suffices) |

All optional in the contract shapes; no tiering (local data).

## Access & auth

- `~/Library/Safari/History.db` (SQLite) and
  `~/Library/Safari/Bookmarks.plist` (binary plist) — the Safari container
  is protected, so **Full Disk Access** is required; the shipped
  `safari-history` def already establishes and surfaces this grant, and the
  Reading List read rides the same one.
- No network, no account, standalone-clean. Copy-before-read applies (Safari
  holds locks while running), as the shipped def already does.

## Vault mapping

- **Visits (shipped):** `browser/` per the visit shape the taxonomy marks
  "document" — Phase 3 writes the spec page for what's already in code, no
  redesign.
- **Reading List (queued):** raw layer `reading/safari/` — one row per
  Reading List item (`guid` = URL hash, `ts` = DateAdded, title, url,
  preview text, DateLastViewed in `extra`); joins the pending Phase 3
  reading contract when ratified. Items are mutable (DateLastViewed
  updates, deletions) — periodic snapshot-diff, latest state wins per guid.
- **Dedupe:** visits as shipped; Reading List by URL-hash guid.

## Build plan

(Reading List extension only — visits are done.)

1. Extend `crates/trove-core/src/safari_history.rs` (or sibling module
   sharing the def's permission hook) to parse `Bookmarks.plist` with the
   `plist` crate and extract the `com.apple.ReadingList` node.
2. No new registry entry needed if shipped as a capability of the existing
   def; if a separate def reads cleaner, it's one line in `INTEGRATIONS`
   and shares the FDA story. Decide in the loop; bias to the existing card
   with a sub-toggle.
3. Fixtures: a real-shaped binary plist with Reading List entries
   (including an entry lacking PreviewText — optional fields lie); parser +
   store tests, unique temp dirs.
4. Skip `ReadingListArchives/` page content for now — metadata-only,
   consistent with the never-copy-bulk-artifacts instinct; note it as a
   possible later opt-in.
5. Contract rows for `reading/` wait on Phase 3 ratification; raw rows can
   land before that (per-source raw is always allowed).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Visit history | 🧪 shipped pre-pipeline (`safari-history`) | David validates against his real Safari history and promotes to ✅ |
| Reading List | — | save a page to Reading List on Mac + one from iPhone (iCloud sync); Sync now; confirm both rows in `reading/safari/` + hub last-data |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Safari
Reading List (macOS) (L1648–L1655). Feasibility 🟢 high — cheap, same FDA
permission as the already-built history pull, natural complement to it.
iCloud sync makes the Reading List a cross-device read-later signal despite
being a local file. Catalog combine-by-provider note: history + Reading
List are one Safari entry, one card.

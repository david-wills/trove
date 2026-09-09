# macOS Recent Files

- **id:** `macos-recent-files`
- **domains:** `files/` (raw-only — would apply if ever buildable)
- **status:** 🚫 unavailable
- **unavailable_reason:** macOS stores recent-file lists as opaque
  NSKeyedArchiver Bookmark blobs with no readable paths; parsing them is
  forensics-grade reverse engineering. Spotlight last-used metadata answers
  the same question cleanly.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** community — Eclectic Light Company analysis: SFL2 files are
  NSKeyedArchiver binary plists wrapping opaque Bookmark data
  ("inscrutable UUIDs and chunks of gibberish text"), no plain paths
- **effort / priority:** L / P2
- **needs:** none

## What it is

The "Recent Documents" lists macOS keeps per app and globally
(`com.apple.sharedfilelist` SFL2 files) — in principle a record of which
files the user opened recently. In practice the on-disk format makes it
not worth reading.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| none — format opaque, skipped by decision | — | — | research L1436–L1442 |

## Access & auth

The files themselves are readable without FDA
(`~/Library/Application Support/com.apple.sharedfilelist/`, per-app
`…ApplicationRecentDocuments/<bundle-id>.sfl2` plus global
`RecentDocuments.sfl2` etc.) — access isn't the block. The SFL2 payload is
NSKeyedArchiver-encoded Bookmark data (opaque binary, no plain file
paths); resolving Bookmarks to paths needs an Objective-C/Swift bridge or
reverse-engineering the Bookmark binary format. That's forensics-tool
territory (AXIOM-class parsers), not a Rust collector.

## Vault mapping

Would be `files/macos-recent-files/` raw rows. Not applicable while
skipped.

## Build plan

None. Ships as a catalogued unavailable card (`Behavior::Unavailable`)
with the reason above. The sanctioned alternative if the recent-files
signal is ever wanted: Spotlight `kMDItemLastUsedDate` queries (mdfind /
NSMetadataQuery) return actual paths sorted by last use with none of the
NSKeyedArchiver complexity — that would be a **new, separate** catalog
entry (Spotlight recents are already named in the taxonomy's `files/`
row), not a revival of this one.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Unavailable card | — | hub shows macOS Recent Files greyed, sorted last in Files, with the reason copy above |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §macOS Recent
Files (SFL2 / SharedFileList) (L1436–L1442). Feasibility 🟠 low,
recommendation "skip" — high implementation cost for data that largely
overlaps Spotlight's `kMDItemLastUsedDate`. A deliberate skip, not a hard
external block: revisit only if a maintained Rust Bookmark-resolution
library appears *and* the Spotlight route proves insufficient.

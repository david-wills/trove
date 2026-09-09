# FSEvents Journal

- **id:** `macos-fsevents`
- **domains:** `files/` (raw-only — would apply if ever buildable)
- **status:** 🚫 unavailable
- **unavailable_reason:** Reading the raw /.fseventsd/ journal requires
  root access, and it records every file-system event — forensic-level
  noise. Git activity and download history answer "what files changed" far
  better.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** community — `puffyCid/macos-fseventsd` Rust crate exists
  (crates.io) and documents the gzip-compressed binary format, but
  `/.fseventsd/` itself needs root/FDA to read
- **effort / priority:** L / P2
- **needs:** none

## What it is

The on-disk FSEvents journal each APFS volume keeps at `/.fseventsd/` — a
binary log of every file create/modify/delete/rename on the volume. The
raw material of disk forensics, and exactly the wrong granularity for a
personal vault.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| none — root-gated + noise, skipped by decision | — | — | research L1452–L1458 |

## Access & auth

Gzip-compressed binary files at `/.fseventsd/` per volume; a parser exists
in Rust (`puffyCid/macos-fseventsd`), so the format isn't the block — the
access model and signal quality are. Reading the directory requires root
(beyond even normal FDA expectations for a user-level app), and the
content is every event on the volume: massive volume, near-zero personal
signal per row. A privacy-first user app demanding root to hoover the
whole disk journal is the wrong shape on every axis.

## Vault mapping

Would be `files/macos-fsevents/` raw rows. Not applicable while skipped.

## Build plan

None. Ships as a catalogued unavailable card (`Behavior::Unavailable`)
with the reason above. The research notes one genuinely different future
idea: a **live** kqueue/FSEventStream watcher on user-chosen directories
(~/Documents, ~/Desktop, ~/Downloads) needs no root and fires targeted
callbacks — that would be a separate spike-first catalog entry (a
file-activity collector), not a parse of this journal. Meanwhile the
underlying question is already covered: web-origin downloads by
`macos-downloads` (QuarantineEventsV2), code changes by git/developer
sources.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Unavailable card | — | hub shows FSEvents Journal greyed, sorted last in Files, with the reason copy above |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §FSEvents
File System Journal (L1452–L1458). Feasibility 🟠 low, recommendation
"skip" — FDA/root requirement plus massive noise-to-signal ratio. A
deliberate skip on product grounds (not an external hard block); the
revisit path is the kqueue live-watcher idea above, never the raw journal.

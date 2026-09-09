# JetBrains IDEs

- **id:** `jetbrains`
- **domains:** `developer/` (raw-only — heterogeneous shapes, no contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (read local XML on a schedule)
- **connection:** none (local files)
- **evidence:** mixed — `recentProjects.xml` is plain XML (trivially
  parseable, the build target); the Local History edit timeline is a
  **proprietary binary format** (iceboxed)
- **effort / priority:** S / P2
- **needs:** none

## What it is

The JetBrains IDE family (IntelliJ IDEA, WebStorm, PyCharm, GoLand, etc.)
— the most popular non-VS-Code IDE line. The IDEs keep no queryable
coding-time logs natively, but each one records recently opened projects
with timestamps, answering "what JetBrains projects did I open and when" —
the same slice the VS Code workspaces provider gives for that editor.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Recent projects | all editions (CE + paid) | project path, last-opened ts, per IDE | plain XML, community-known location |
| File-edit timeline (Local History) | all editions | per-file edit events | proprietary binary — **iceboxed** |
| Coding time | only via plugins (WakaTime etc.) | — | covered by the `wakatime` provider, not here |

Only the first row is in scope for v1.

## Access & auth

- `~/Library/Application Support/JetBrains/<IDEName><Version>/options/recentProjects.xml`
  — plain XML per IDE+version directory; enumerate all
  `JetBrains/*/options/recentProjects.xml` so every installed IDE and
  version is covered.
- Local History lives in the caches dir
  (`~/Library/Caches/JetBrains/<IDEName><Version>/`) in a proprietary
  binary format — reverse-engineering is explicitly not planned.
- No TCC beyond troved's existing FDA grant for `~/Library/`. No network.
  Standalone-clean.

## Vault mapping

- **Raw layer:** `developer/jetbrains/YYYY-MM.jsonl` — one row per
  (project, opened-at) observation: ts, project_path, project_name, ide,
  ide_version. `developer/` is raw-only per the taxonomy; no contract
  applies.
- **Dedupe:** `guid` = hash(ide, project_path, ts) — the XML holds
  last-opened times, so periodic re-reads only append when a timestamp
  advances.

## Build plan

1. Pair with the VS Code workspaces provider (same "what projects did I
   open" shape, same periodic local-read pattern); build in that batch.
2. Module `crates/trove-core/src/jetbrains.rs`: `DEF` (Periodic), glob
   over `JetBrains/*/options/recentProjects.xml`, lightweight XML parse
   (quick-xml or similar already-vetted crate).
3. One registration line in `INTEGRATIONS`. No connection.
4. Fixtures: real-shaped `recentProjects.xml` samples from two IDE
   versions (the format has minor variations across versions — tolerate
   unknown attributes); unique-temp-dir tests.
5. Scope discipline: Local History binary stays iceboxed; coding-time
   stays WakaTime's job. Don't grow this module beyond recent-projects.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Recent projects | ✅ built (needs live run) | on a machine with any JetBrains IDE: open a project, Sync now, confirm a row in `developer/jetbrains/` + hub last-data (David may not run JetBrains — any real user's run validates) |
| Multi-IDE | ✅ built (needs live run) | with two IDEs installed, confirm rows carry distinct `ide` values |
| Legacy format (pre-2019.2) | ✅ built | paths with no additionalInfo entry are collected with activation_ts = epoch 0 |

## Build notes

- Periodic Behavior (hourly), like local-git and claude-code.
- Globs `~/Library/Application Support/JetBrains/*/options/recentProjects.xml`.
- XML parsed with quick-xml (already in Cargo.toml) via a lightweight state machine.
- `$USER_HOME$` macro in project paths is expanded to the real home dir.
- IDE name and version extracted from the config dir name (e.g., `IntelliJIdea2024.1` → ide=`IntelliJIdea`, ide_version=`2024.1`).
- Guid: stable hash of `(ide, project_path, activation_ts_ms)`.
- Cursor: `.trove/jetbrains-sync.json` (mtime per XML file).
- Upserts by guid into month-partitioned `developer/jetbrains/YYYY-MM.jsonl`.
- Legacy paths (no additionalInfo, pre-2019.2 IDEs) have ts = epoch; they land in a 1970 partition.
- Local History binary format stays iceboxed as planned.
- 12 tests pass; fixtures in `tests/fixtures/jetbrains/`.

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §JetBrains
IDE Activity (L1460–L1466). Feasibility 🟡 medium for the domain overall,
but the research recommendation splits it: icebox the binary Local
History, build the S-effort `recentProjects.xml` piece "alongside VS Code
workspaces" — that split is exactly this brief's scope, and is why the
catalog carries S where the at-a-glance row said M. Time tracking for
JetBrains users routes through the WakaTime plugin → `wakatime` provider.

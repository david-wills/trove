# Org-mode / Plain-text task files

- **id:** `org-mode`
- **domains:** `tasks/` (contract: **✅ ratified** — tasks)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (watch a user-picked folder for `.org` files; re-parse
  on change)
- **connection:** none — the user picks a folder; no login. Files-as-truth, so
  nothing to authenticate.
- **evidence:** official format spec at orgmode.org; **no mature Rust crate**
  (org-rs immature) — a minimal TODO/timestamp parser must be written
- **effort / priority:** M / P2
- **needs:** Needs-sample (format is well-specified, but real `.org` files vary;
  validate the parser against actual user files before promoting)

## What it is

Org-mode is Emacs's plain-text outliner/task format — `.org` files with TODO
keywords, priorities, tags, and `SCHEDULED`/`DEADLINE`/`DONE` timestamps.
Niche but dedicated: Emacs users, beorg (iCloud-synced iOS), Logseq's org
backend, and Doom/Spacemacs. Because it's plain text the user already owns, a
watch-folder collector respects the files-as-truth principle perfectly.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| TODO items | none (local files) | heading, TODO/DONE keyword, priority cookie, tags | format spec |
| Scheduling | none | SCHEDULED, DEADLINE timestamps | format spec |
| Completion / clock | none | DONE timestamp, LOGBOOK drawer state changes | format spec |

All optional in the tasks contract; files without timestamps simply carry no
due/done dates.

## Access & auth

- Mechanism: **watch-folder** over a user-configured directory (e.g. `~/org`,
  `~/Documents/notes`, or beorg's iCloud path
  `~/Library/Mobile Documents/iCloud~com~appsonthemove~beorg/Documents/`).
- Parse TODO keywords, `SCHEDULED`/`DEADLINE`/`DONE` timestamps, priority
  cookies (`[#A]`), tags (`:tag:`), and LOGBOOK drawers via text parsing.
- No network, no key, no TCC beyond read access to the user's chosen folder.
  Standalone-clean.

## Vault mapping

- **Raw layer:** `tasks/org-mode/raw/` — copies or parsed snapshots of the
  source `.org` content (full fidelity; the user's files remain canonical).
- **Contract layer:** `tasks/org-mode/YYYY-MM.jsonl` per the **ratified tasks
  contract**: one row per task (`ts`, `source`, `guid`, `title`, `status`,
  `due`, `completed_at`, `priority`, `tags[]`), org-specific bits (LOGBOOK,
  drawer properties) in `extra`.
- **Dedupe `guid`:** stable per-task identity is the gotcha — org has no native
  IDs unless `:ID:` properties are set. Prefer the `:ID:` property when present;
  otherwise hash (file path + heading text + outline position). Record the
  fallback in the parser so re-parses are idempotent.

## Build plan

1. Module `crates/trove-core/src/org_mode.rs`: `DEF` (Periodic, watch-folder),
   no `CONNECTION` (folder pick only).
2. **Parser-first is not possible blind** — write a minimal TODO/timestamp/tag
   parser (no mature Rust crate exists; do not depend on org-rs). Cover TODO
   keywords, the three timestamp forms, priority cookies, tags, LOGBOOK.
3. Fixtures from the orgmode.org spec examples AND **real sample files**
   (Needs-sample) — beorg, Logseq-org, and hand-written Emacs variants differ.
4. Store + dedupe tests with unique temp dirs; verify idempotent re-parse.
5. Vault writes via `store` helpers against the ratified tasks contract.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| TODO + scheduling | 🧪 built | point the collector at a folder of real `.org` files; confirm `tasks/org-mode/` rows with correct status/due; edit a file and confirm idempotent re-parse |
| Completion / LOGBOOK | 🧪 built | mark a task DONE in Emacs/beorg; re-sync; confirm `completed_at` populates and no duplicate row appears |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Org-mode / Plain-text task files (L2714–L2720). Feasibility 🟡 medium — format
is well-specified plain text and Rust parsing is straightforward, but **no
mature crate**, so a minimal parser is the real cost and a real-file sample is
required before promotion. Clean fit with files-as-truth; covers an
otherwise-unreachable Emacs/beorg/Logseq audience.

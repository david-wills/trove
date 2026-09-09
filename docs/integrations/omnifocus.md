# OmniFocus

- **id:** `omnifocus`
- **domains:** `tasks/` (contract: ✅ **ratified** — the shared tasks record)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (read the local `.ofocus` container; mtime watermark).
  TaskPaper/JSON export is the Import fallback.
- **connection:** none — local on-disk database in the app's container
- **evidence:** community-schema — `tomzx/ofocus-format` (GitHub) documents the
  ZIP+XML transaction-log format; medium confidence, no vendor schema guarantee
- **effort / priority:** L / P2
- **needs:** Needs-sample (the `.ofocus` format is community-documented only —
  introspect a real container before trusting the parser)

## What it is

OmniFocus is a heavyweight Mac/iOS GTD task manager favored by power users. Its
data is on disk but in a proprietary `.ofocus` format — a directory of ZIP
bundles, each containing a `contents.xml` of transaction deltas. There is no
standard SQLite and no REST API; reconstructing current state means replaying
the transaction log. Worth building for the OmniFocus power-user audience, but
non-trivial — hence L effort and a spike-first plan.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Tasks / actions | all (local file) | title, note, project, tags, defer/due dates, completed, flagged | community schema |
| Projects & folders | all (local file) | project name, status, folder hierarchy | community schema |

All optional in the contract. No tiering — local file access is unconditional.

## Access & auth

- File: `~/Library/Containers/com.omnigroup.OmniFocus4/Data/Library/Application
  Support/OmniFocus/OmniFocus.ofocus` — a directory of ZIP files; each holds
  `contents.xml` with transaction deltas. Must replay all transactions to
  rebuild current state; no stable schema guarantee.
- TCC: Full Disk Access (sandboxed container path). Copy-then-read to avoid
  reading a file mid-write.
- Standalone-clean: pure local file parsing. The Omni Automation JavaScript API
  requires the app to be running (violates the standalone rule) — not used.
  OmniFocus offers no REST API.
- Fallback: File > Export → TaskPaper/JSON is an Import path (M1) if the
  transaction-replay parser proves too costly.

## Vault mapping

- **Raw layer:** `tasks/omnifocus/raw/…` — extracted transaction objects /
  reconstructed-state snapshot, full fidelity.
- **Contract layer:** `tasks/omnifocus/YYYY-MM.jsonl` per the **ratified tasks
  contract** — map title→`title`, note→body/`extra`, project/tags→the
  contract's project/tags fields, defer/due/completed→the date fields, OmniFocus
  id→`guid`. Overflow (flagged, perspective metadata, repeat rules) in `extra`.
- **Dedupe:** OmniFocus persistent object id as `guid`; mtime watermark in
  `.trove/omnifocus-sync.json`, rebuildable by re-reading the container.

## Build plan

1. **Spike first:** introspect a real `.ofocus` container — unzip a bundle,
   confirm `contents.xml` shape against `tomzx/ofocus-format`, validate that
   transaction replay reconstructs current state. **Needs-sample** gates the
   parser; build it parser-last against a captured sample.
2. Module `crates/trove-core/src/omnifocus.rs`: `DEF` (Periodic), no
   `CONNECTION`. FDA-gated container read with copy-then-read.
3. Registration line in `INTEGRATIONS`.
4. Fixtures from the captured `.ofocus` sample (multiple transaction bundles
   incl. completions/deletions); parser + replay + store + dedupe tests, unique
   temp dirs.
5. TaskPaper/JSON Import fallback wired if replay proves unreliable.
6. Vault writes via `store` helpers against the ratified tasks contract.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Tasks / actions | Needs-sample | grant FDA; Sync now against a real OmniFocus install; confirm task rows in `tasks/omnifocus/` + hub last-data; spot-check defer/due/completed |
| Replay correctness | Needs-sample | confirm a recently completed/deleted action reflects correct current state (not a stale earlier transaction) |
| Multi-tag (OF4) | Needs-sample | create task with 2+ tags in OF4; confirm both appear in `tags[]` in vault output |
| Nested subtasks | Needs-sample | create subtask under an action (not directly under project); confirm `project` field resolves to the ancestor project name |

## Build notes (2026-06-21 + defect fixes 2026-06-21)

Parser built from the `tomzx/ofocus-format` community spec (v1 + v2 README,
confirmed via GitHub API). Both quick-xml and zip crates were already deps.
Key decisions:
- **Periodic/hourly** with mtime watermark on the max zip mtime. Full replay on
  any new ZIP or master ZIP name change (compaction detection). Freshness gate
  also tracks seen ZIP names to guard against same-second writes on fast SSDs.
- **op="reference"** for context/folder elements is processed (populates the
  name lookups), but reference task nodes skip task-state overwrite.
- **Flagged → priority 5** (high); unflagged → 0.
- **Context names** (from `<context>` entities) resolved to tags. All `<context
  idref>` and `<tag idref>` children collected per task (supports OF4 multi-tag;
  exact OF4 element name unconfirmed without a real sample).
- **Nested subtask project resolution**: parent chain walked upward until an
  `is_project` ancestor is found; cycle guard at 50 steps.
- **project nodes** (is_project=true) appear as ProjectInfo only, not as Task rows.
- **completed tasks** go into the fate closure as `TaskFate::Completed`; they
  don't appear in the `fresh` open list.
- **Zero-parse guard**: if ZIPs with parseable content yield zero tasks, cursor
  is NOT advanced. Returns a Needs-sample warning instead of silently advancing
  past a structurally-wrong container.
- **Self-closing context/folder deletes** handled in `Event::Empty` branch.
- **Needs-sample flag stays**: the parser was built from spec only, not a real
  .ofocus container. Must validate field names/encoding against a real install.
  Until then: element names, `all_day` assumption, and OF4 tag element name are
  all unverified.
- XML parsing uses `Reader::from_str` + explicit element-name path stack to
  handle namespaced OF XML correctly without depth arithmetic bugs.
- 21 tests green (cargo test -p trove-core omnifocus::).

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§OmniFocus (L2650–L2656). Feasibility 🟡 medium — on disk and parseable but
NOT standard SQLite; custom ZIP+XML transaction-log parser required, replay all
transactions to reconstruct state, no stable schema guarantee. Community schema
spec: `tomzx/ofocus-format`. Omni Automation JS API needs the app running
(standalone violation, skipped). Export to TaskPaper/JSON is the Import
fallback.

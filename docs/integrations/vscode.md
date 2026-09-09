# VS Code

- **id:** `vscode`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes, no
  shared contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (copy-then-read local SQLite state)
- **connection:** none (local files)
- **evidence:** community-schema — the `state.vscdb` ItemTable key layout
  (`history.recentlyOpenedPathsList`) is well-documented by the VS Code
  community; no official docs
- **effort / priority:** S / P2
- **needs:** none

## What it is

VS Code's recently-opened workspaces and file-activity state — "what
projects and files did I open in VS Code, when" without installing
WakaTime. Lightweight complement to local git (which sees only commits)
and shell history. Path-level metadata only, no file contents, hence no
privacy flag.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Recently opened paths | none (local files) | path, type (file/folder/workspace), ts | community-schema (`history.recentlyOpenedPathsList` in ItemTable) |
| Per-workspace file opens | none | files opened per workspace, line counts | community-schema (`workspaceStorage/<hash>/state.vscdb`, codelens/cache2 key) — lower confidence |

All optional; v1 can ship on the recently-opened list alone.

## Access & auth

- `~/Library/Application Support/Code/User/globalStorage/state.vscdb` —
  SQLite ItemTable; key `history.recentlyOpenedPathsList`.
- Per-workspace: `~/Library/Application Support/Code/User/workspaceStorage/<hash>/state.vscdb`.
- No TCC permission (home-dir Application Support is readable).
  **Copy-then-read** — VS Code holds a write lock while running (WAL
  usually allows concurrent reads, but the copy pattern is the codebase
  standard and avoids the edge cases).
- No network, no auth. Standalone-clean.
- Same paths exist under `.../Cursor/` and other VS Code forks — the def
  should take the product dir as an internal parameter so forks are a
  config row, not a fork of the module.

## Vault mapping

- **Raw layer:** `developer/vscode/YYYY-MM.jsonl` — fields: `ts`, `path`,
  `type` (file/folder/workspace). Path from the taxonomy (`developer/`,
  raw-only).
- **Contract layer:** none — `developer/` carries native shapes.
- **Dedupe:** `guid` = `<path>:<ts>`; the recently-opened list is a
  rolling snapshot, so each sync diffs against previously-written rows
  (seen set in `.trove/vscode-sync.json`) and appends only new
  (path, ts) pairs. Note the list is capped — entries that age out before
  a sync are simply missed; this is a coarse signal, not a complete log.

## Build plan

1. Module `crates/trove-core/src/vscode.rs`: `DEF` (Periodic), pull hook
   copies `state.vscdb` to temp, reads the JSON value of
   `history.recentlyOpenedPathsList` via rusqlite.
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. v1 = global recently-opened only; the per-workspace codelens/cache2
   slice is a follow-up (lower-confidence schema — treat as
   parser-last within the provider).
4. Fixtures: construct a miniature state.vscdb in the test with the
   documented key/JSON shape, plus an unknown-shape case asserting
   graceful skip (community schema can drift across VS Code releases).
5. Sequence after the M3 copy-then-read pattern is well-worn (it already
   is — chrome-history et al.), and ideally alongside `cursor`, which
   shares the storage layout.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Recently opened | ✅ built | open a few folders/files in real VS Code/Cursor; Sync now; rows in `developer/vscode/` match File → Open Recent; hub last-data updates |
| While-running read | ✅ built | sync with VS Code open; confirm copy-then-read succeeds (no lock error) |

## Build notes (2026-06-17)

- Schema verified against real Cursor 1.x `state.vscdb` on this machine: the
  `history.recentlyOpenedPathsList` key holds `{"entries":[{"folderUri":"file://…"},
  {"fileUri":"file://…"}]}`.  VS Code's own `state.vscdb` on this machine did not
  have the key yet (the install is rarely used), which validates the graceful-skip
  path (`query_row().ok()` → empty vec).
- No timestamps in the recently-opened list; `ts = collection time` is written.
- Dedup key is `path` (decoded URI), tracked in `.trove/vscode-sync.json`.
- Multi-product scanning: Code, Cursor, VSCodium, Windsurf, Code - OSS,
  Code - Insiders — all supported via a fixed product-dir list.
- Brief said `guid = <path>:<ts>` — corrected to `guid = path` (no ts available).
- 10 unit tests: all pass (includes uri decoding, percent-decode, missing-key,
  remote-uri skip, seen dedup, multi-product, label round-trip, back-compat).

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §VS Code
Recent Workspaces & File Activity (L1332–L1338). Feasibility 🟢 high,
build-later per research (P2). Copilot chat sessions in
`workspaceStorage/<hash>/chatSessions/` are the separate `github-copilot`
provider, and Cursor's chat stores are the separate `cursor` provider —
this brief is deliberately the thin path/workspace signal. Backups dir
(`.../Code/Backups/`, unsaved edits) noted in research but skipped:
content capture is out of scope for this provider's shape.

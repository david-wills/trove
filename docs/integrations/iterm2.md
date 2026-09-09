# iTerm2

- **id:** `iterm2`
- **domains:** `developer/` (raw-only — heterogeneous shapes, no contract)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (copy-then-read of the local store, once located)
- **connection:** none (local files)
- **evidence:** low — the storage path is **not publicly documented**;
  needs filesystem inspection on a machine with shell integration enabled.
  sample-required.
- **effort / priority:** M / P2
- **needs:** privacy (terminal commands can hold secrets — opt-in with
  explicit acknowledgement, command-prefix exclusion list) · Needs-sample
  (storage format undiscovered) · Needs-David (**icebox decision** —
  `shell-history` covers the same commands; only unique value is
  cd/directory history)

## What it is

The dominant third-party macOS terminal. With Shell Integration enabled
and "Save copy/paste history and command history to disk" turned on, it
keeps per-user command history (capped at ~200 commands per user/hostname)
and — uniquely — directory-change history that `~/.zsh_history` doesn't
record. For command text itself it is strictly worse than the
`shell-history` provider: capped, app-dependent, undocumented format.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Command history | requires Shell Integration + save-to-disk setting | command, ts, host/user (≤200 per user/host) | low — path/format undocumented |
| Directory history | same gating | cd events with timestamps | low — undocumented; the unique slice |

All optional; rows simply absent for users without shell integration.

## Access & auth

- Files live somewhere under `~/Library/Application Support/iTerm2/`
  (scripts/profiles confirmed there; the history store itself — likely
  SQLite or plist — is not in official docs). `~/.iterm2_shell_integration.zsh`
  is the hook script, not the data.
- Discovery step required: `find ~/Library/Application\ Support/iTerm2` on
  a machine with shell integration + save-to-disk enabled.
- No TCC beyond what troved's FDA grant already covers for `~/Library/`.
  No network. Standalone-clean once the file is found.

## Vault mapping

- **Raw layer:** `developer/iterm2/YYYY-MM.jsonl` — command rows and
  directory-change rows as typed records, native shape. `developer/` is
  raw-only per the taxonomy; no contract applies.
- **Dedupe:** against `developer/shell/` rows is a **read-time** concern
  (raw layers stay complete); within-source guid from (ts, command/dir,
  session) hash once the real format is known.

## Build plan

1. **Parked behind Needs-David (icebox call).** Recommendation from the
   research doc: don't build — `shell-history` (P0, S, documented format)
   captures the same commands without the 200-command cap. Revisit only if
   users specifically ask for directory history.
2. If un-iceboxed: spike first — locate and inspect the store on a real
   machine (Needs-sample); parser is written **last**, after a sample
   confirms the format.
3. Then module `crates/trove-core/src/iterm2.rs` (`DEF`, Periodic,
   copy-then-read like browser.rs), one registration line, fixtures from
   the captured sample, unique-temp-dir tests.
4. Privacy gate: ships opt-in (commands can contain secrets); share the
   command-prefix exclusion-list mechanism with `shell-history`.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Command history | — | enable iTerm2 shell integration + save-to-disk; run commands; Sync now; confirm rows in `developer/iterm2/` |
| Directory history | — | cd around in an integrated session; confirm cd-event rows with timestamps |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §iTerm2
Command & Directory History (L1428–L1434). Feasibility 🟡 medium, but the
research's own recommendation is **icebox**: shell history is a strictly
better command source; iTerm2's marginal value (cd history, mark metadata)
serves a very small audience for an M-effort reverse-engineering job. The
catalog keeps it queued-with-flags rather than unavailable because nothing
hard-blocks it — it's a priority call, not a feasibility wall.

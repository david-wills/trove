# Shell History

- **id:** `shell-history`
- **domains:** `developer/` (contract: **raw-only** — heterogeneous developer
  shapes each write their own native form in their own folder)
- **status:** 🧪 built (fixture-tested; opt-in + parse-time secret redaction; no login)
- **unavailable_reason:** none
- **behavior:** Periodic (re-scan the history file(s) on a cursor; plain local
  files, no daemon needed)
- **connection:** none (local files in the user's home directory; no auth)
- **evidence:** official — plain text at `~/.zsh_history`; the EXTENDED_HISTORY
  format (`: <unix_ts>:0;command`) is documented zsh behaviour and confirmed
  stable by OSS `zsh_history-analysis` tooling. Bash/fish formats equally
  well-known.
- **effort / priority:** S / P0
- **needs:** privacy (commands can embed secrets — exclusion/redaction list at
  parse time; raw vault otherwise complete)

## What it is

The terminal command log — `~/.zsh_history` and friends. Noted in the research
as "trivially easy, surprisingly rich": zero permissions, plain text, and a
direct complement to the activity watcher for answering "what did I do/build
today?". With EXTENDED_HISTORY enabled, every line carries a per-command Unix
timestamp, giving a real time series rather than just ordering. Part of the P0
developer-activity wave (shell history + local git + Claude Code transcripts +
GitHub).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Timestamped commands (zsh extended) | n/a | ts, command, session | official format |
| Ordered commands (no extended flag) | n/a | command, line order (ts absent) | official format |
| Per-session files | macOS Catalina+ | `~/.zsh_sessions/*.history` (dedupe source) | official |
| Bash history | bash users | `~/.bash_history` (no ts unless HISTTIMEFORMAT) | official |
| Fish history | fish users | `~/.local/share/fish/fish_history` (YAML: cmd + `when` ts) | official |

All optional in the brief; a user without EXTENDED_HISTORY simply gets rows
with no `ts`, ordered by file position. No special code path for tiering.

## Access & auth

- Plain text files in `$HOME`: `~/.zsh_history` (primary), `~/.zsh_sessions/*.history`
  (per-session, macOS), `~/.bash_history` (bash fallback),
  `~/.local/share/fish/fish_history` (fish, YAML).
- No TCC prompt (home-directory dotfiles, not a protected location), no OAuth,
  no network. Standalone-clean.
- Read-only; Trove never writes to these files.

## Vault mapping

- **Raw layer:** `developer/shell/YYYY-MM.jsonl` — one row per command:
  `ts` (from the `: ts:0;` prefix when present), `command`, `duration_secs`
  (the zsh 0-field — always 0 in practice), `session`, `shell` (zsh/bash/fish).
  Partitioned by month off the parsed timestamp; commands without a ts go in a
  bounded `developer/shell/undated.jsonl` keyed by file offset.
- **Contract layer:** none — `developer/` is raw-only (heterogeneous shapes).
  Each row stays in shell's native form; no shared developer record.
- **Dedupe:** commands appear in both the main history and per-session files —
  dedupe on `(ts, command)` when ts present, else `(session, command, offset)`.
  Incremental cursor = last-imported line offset (or max ts) per source file,
  in `.trove/shell-history-sync.json`, rebuildable by re-scanning.

## Build plan

1. Module `crates/trove-core/src/shell_history.rs`: `DEF` (Periodic), parser
   for the three formats (zsh extended/plain, bash, fish YAML), `pull` hook for
   Sync-now. No connection.
2. Registration line in `INTEGRATIONS`.
3. Privacy gate: ships opt-in (commands can carry tokens/passwords inline). A
   configurable exclusion list (regex/keyword: `export *_TOKEN=`, `--password`,
   AWS keys, etc.) redacts or drops matching commands **at parse time** so the
   raw vault never persists obvious secrets; the analysis layer can flag the
   rest. Note: the research recorded "no redaction at collection time" but the
   pipeline's privacy rule overrides — secret-bearing commands are a known
   hazard here, so redaction is mandatory at parse time.
4. Fixtures: synthetic history files in each format (extended ts present and
   absent, a per-session dupe, a bash file, a fish YAML file, a secret-bearing
   line that must be redacted); parser + dedupe + cursor + redaction tests with
   unique temp dirs.

## Build status — 🧪 2026-06-14

Shipped (`shell_history.rs`, INDEX #7): `Behavior::Periodic` (hourly), no
connection. Parses zsh (extended `: ts:elapsed;cmd` + plain, with
backslash-continuation multi-line entries), bash (bare commands + `#<epoch>`
timestamp comment lines), and fish YAML; sources `~/.zsh_history`,
`~/.zsh_sessions/*.history`, `~/.bash_history`,
`~/.local/share/fish/fish_history` (resolved under `TROVE_HOME` else HOME so
tests use a temp home). One row per command → `developer/shell/YYYY-MM.jsonl`
(dated) or `developer/shell/undated.jsonl` (no ts): `ts`, `command`
(post-redaction), `duration_secs`, `session`, `shell`, `redacted`. Dedupe on
`(ts, command)` / `(session, command, offset)` via guid upsert-into-partition;
truncation-safe per-file byte-offset cursor in `.trove/shell-history-sync.json`.

**Privacy — mandatory parse-time secret redaction (always on).** The
integration ships opt-in (🔒). Before any command is written, a built-in
redactor scrubs obvious secrets to `***` (or replaces the whole command with
`[redacted]`) and sets `redacted: true`; the raw secret bytes are never
persisted. Covered: named env-var assignments (`*_TOKEN=`, `*_SECRET=`,
`PGPASSWORD=`, `AWS_SECRET_ACCESS_KEY=`, …), `--password`/`--token`/`--api-key`/
`--secret`/`--auth` flags, credential-tool `-p`/`-a` values (**tool-scoped** to
mysql/mongo/redis-cli/… so `mkdir -p`/`ssh -p` stay untouched), URL userinfo
*and* query-string secrets, and AWS access-key ids. The dedupe guid is keyed on
the **pre-redaction** command hash (a SipHash — stores no secret), so two
distinct secret commands never merge. **Best-effort for OBVIOUS secrets** (the
brief's bar; generic high-entropy detection is out of scope — the analysis
layer flags the rest). The exclusion list is a fixed default in v1;
**user-configurable redaction rules are a deferred follow-up.**

Adversarial-verify caught + fixed **2 blocking** (`mysql -phunter2` concat +
`mysql -p hunter2` space forms leaked the password) **+ 3 minor** (URL
query-param token leak; a trailing `;` corrupted the scrubbed command; the
guid-over-redacted-command merged distinct secrets) — all with regression tests.

Gate: trove-core 453/0 (+24 shell_history tests, run serially — the pre-existing
`runner::lock_is_exclusive_until_released` flakes under full-suite parallelism,
unrelated), `cargo check` clean, `schedule_doc` regenerated, `bindings.ts` up to
date. `developer/` raw-only — no contract changes.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| zsh extended import | 🧪 | enable; Sync now; confirm `developer/shell/YYYY-MM.jsonl` rows carry real `ts`; hub last-data updates |
| Dedupe across sessions | 🧪 | with `~/.zsh_sessions` populated, confirm no duplicate `(ts, command)` rows |
| Secret redaction | 🧪 | seed an `export FOO_TOKEN=...` line; confirm it is redacted (`***`/`[redacted]`, `redacted:true`) in the vault and the raw secret is absent |
| Bash / fish fallback | 🧪 | point at a bash/fish history fixture; confirm rows parse with the right `shell` tag |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Shell History
(L1292–L1298). Feasibility 🟢 high; P0 in the priority matrix (L68) as part of
the developer-activity wave. Per-command timestamps require EXTENDED_HISTORY;
fall back to line ordering when absent. Fish uses YAML (`- cmd:` / `when:`).
The research suggested no collection-time redaction (raw vault) — the pipeline
privacy rule supersedes this for the obvious-secret case. Sequence alongside
local-git and Claude Code transcripts to land the developer domain as one wave.

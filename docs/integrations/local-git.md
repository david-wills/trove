# Local Git Activity

- **id:** `local-git`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes, no
  shared contract)
- **status:** 🧪 built (fixture-tested; default HOME scan, no login)
- **unavailable_reason:** none
- **behavior:** Periodic (walk configured repo roots; per-repo commit-ts cursor)
- **connection:** none (pure local file reads)
- **evidence:** official-docs level — `gix` (gitoxide) crate is pure Rust,
  production-quality, used by Cargo itself; well-documented commit-walk API
- **effort / priority:** S / P0
- **needs:** none

## What it is

The commit history of every repository on the user's machine — the "what did
I build" trail. Walks user-configured repo roots (e.g. `~/Projects`,
`~/Code`) and extracts the commit log from each `.git` directly via the
`gix` crate. Zero permissions, zero network, high-value signal that pairs
naturally with the activity watcher, shell history, and Claude Code
transcripts.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Commit log per repo | none (local files) | repo path, branch, sha (short), ts, author email, subject | gix docs; format is git itself |
| Diff stats | none | insertions, deletions, files changed | gix-traverse |

All optional in the contract sense (raw-only stream); diff stats can be
skipped on huge repos if walk cost bites — fields omit-if-empty.

## Access & auth

- Local filesystem only: user configures a list of repo root dirs (or a
  parent dir to scan) in Trove settings. No TCC permission needed for
  normal home-dir project folders.
- `gix` handles packed-refs, shallow clones, and worktrees gracefully; skip
  submodule `.git` dirs so commits aren't read twice.
- Standalone-clean: no external git binary dependency — `gix` compiles into
  the binary per the absorb-as-library rule.

## Vault mapping

- **Raw layer:** `developer/git/YYYY-MM.jsonl` — one row per commit:
  `ts`, `repo`, `branch`, `sha`, `author_email`, `subject`, `insertions`,
  `deletions`, `files_changed`. Path from the taxonomy (`developer/` is
  raw-only; no contract applies).
- **Contract layer:** none — `developer/` carries native shapes.
- **Dedupe:** `guid` = `<repo>:<sha>`; incremental by per-repo
  highest-imported commit timestamp in `.trove/git-sync.json`
  (rebuildable by scanning output files).

## Build plan

1. Module `crates/trove-core/src/local_git.rs`: `DEF` (Periodic), pull hook
   walks configured roots with `gix`, last-data hook reads newest row.
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. Settings surface for repo roots — the registry doesn't give per-def
   config for free; check how `activity` exposes settings and follow it.
4. Fixtures: build a tiny throwaway repo in a unique temp dir inside the
   test (git fixture-by-construction, not checked-in binaries); test
   cursor advance, submodule skip, multi-root merge.
5. Author-identity note: capture all commits in the repo or only the
   user's? Research suggests `author` filter for the cloud twin; locally,
   store all authors (raw fidelity) — read layer can filter by the
   configured email.

## Build status — 🧪 2026-06-14

Shipped (`local_git.rs`, INDEX #6): `Behavior::Periodic` (hourly), no
connection. Uses **`gix` (gitoxide), pure-Rust, compiled in** — never shells to
the `git` CLI (standalone rule; git-CLI appears only in test fixtures);
`Cargo.toml` keeps only the `sha1`/`zlib-rs`/`blob-diff` features (no
network/C). Repo discovery = a bounded (depth 5), pruned scan of the user's
HOME (`TROVE_HOME` override for tests), pruning the subtree at any dir
containing `.git` (auto-skips submodules/nested repos). One row per commit →
`developer/git/YYYY-MM.jsonl` (`ts` with the commit's own offset, `repo`,
`branch`, `sha`, `author_email`/`author_name`, `subject`,
`insertions`/`deletions`/`files_changed`); `guid=<repo>:<sha>`; **all authors
stored** (raw fidelity — the read layer filters by the configured email later);
upsert-into-partition dedup; per-repo committer-time cursor in
`.trove/git-sync.json`. Diff stats via `gix` blob-diff, omitted on
merge/root/error.

**No user-facing repo-roots config UI** (the registry gives no per-def config,
and a bespoke screen is out of scope) — v1 uses the default HOME scan,
following `activity`'s default-config precedent. **Configurable scan roots are a
deferred follow-up** (the `Access & auth` design above describes the eventual
config; the shipped v1 is the home scan).

Adversarial-verify caught + fixed **3 blocking (one root cause):** the commit
walk used a committer-time *cutoff* that prunes DAG descent, dropping commits
behind an older node (feature branches merged with an older merge-commit time;
amended/rebased/backdated HEAD) and stranding the tail past the per-repo cap.
Now a full-ancestry walk + strict `ts > cursor` write-filter + `guid` upsert
(idempotent); a capped pass holds the cursor instead of advancing past
un-imported commits.

Gate: trove-core 429/0 (+15 local_git tests), `cargo check` clean,
`schedule_doc` regenerated, `bindings.ts` up to date. `developer/` raw-only —
no contract/`DOMAINS`/`spec_validation` changes.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Commit log | 🧪 | enable local-git; Sync now; confirm rows in `developer/git/YYYY-MM.jsonl` match `git log` for a spot-checked repo under your home dir; hub last-data updates |
| Incremental cursor | 🧪 | make a new commit in any scanned repo; Sync now again; exactly one new row appears (no dups) |
| Configurable scan roots | 🚫 deferred | follow-up — v1 scans HOME by default; per-user root config is a later UI/settings slice |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Local Git
Activity (L1300–L1306). Feasibility 🟢 high; was already "planned" in
data-sources.md. Complements (does not duplicate) the `github` provider:
local git misses PRs/issues/stars; GitHub misses uncloned-anywhere local
repos and uncommitted-to-remote work. The gitoxide-core-tools-query
auto-maintained SQLite analytics DB exists but is unnecessary — we write
our own vault rows.

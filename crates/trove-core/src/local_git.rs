//! Local Git activity — a pure-local, no-login commit collector.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/local-git.md.
//!
//! A **Periodic** local filesystem collector (the [`crate::claude_code`] shape:
//! a bounded scan of the user's home dir, no network and no connection). It
//! walks the machine for git repos and writes **every** commit it finds as one
//! raw row to `developer/git/YYYY-MM.jsonl`, partitioned by the commit's own
//! month. Stores all authors' commits at full fidelity — the read layer (later)
//! filters to the user; the raw stream stays complete.
//!
//! **`developer/` is raw-only** (taxonomy decision): no media-plays/domain
//! contract, no normalized struct, no spec validation — this module owns its
//! row shape.
//!
//! ## Library
//!
//! Uses the pure-Rust [`gix`] (gitoxide) crate — the same library Cargo uses —
//! compiled in, so it never spawns the `git` binary (the standalone rule). We
//! open each repo read-only, resolve HEAD + the branch short-name, walk commit
//! ancestry from HEAD newest-first, read per-commit metadata, and compute
//! single-parent tree-diff line stats.
//!
//! ## Discovery (default, bounded, pruned)
//!
//! The default scan root is the user's home dir (honoring `TROVE_HOME`/`HOME`
//! the way [`crate::claude_code`] honors `CLAUDE_CONFIG_DIR`, so tests point it
//! at a temp dir). The walk is bounded to [`MAX_DEPTH`] levels and prunes
//! heavy/irrelevant directories by name ([`is_pruned_dir`]). When a directory
//! contains a `.git` (dir *or* file) it's recorded as a repo and the whole
//! subtree is skipped — which auto-skips submodules and nested repos (they live
//! inside an already-pruned working tree). Unreadable directories are skipped,
//! never fatal. There is **no per-repo config surface** (the registry gives a
//! def no config UI, and a bespoke one is forbidden): like [`crate::activity`]
//! it runs on sensible hardcoded defaults. Configurable scan roots are a
//! follow-up.
//!
//! ## Cursor / incremental
//!
//! `.trove/git-sync.json` (non-secret, rebuildable, *not* under `.trove/sync/`)
//! maps repo path → highest committer-time imported (epoch seconds). Each sync,
//! for each repo, we walk the **full** HEAD ancestry and write commits whose
//! committer-time is **strictly greater** than the cursor — the walk has *no*
//! commit-time cutoff (a cutoff prunes DAG descent and silently drops commits
//! that sit behind an older node, since committer-time isn't monotonic in the
//! DAG), and the strict `>` filter alone decides what's written. Rows are
//! upserted into their month partitions **by `guid`** (so a full re-walk never
//! duplicates), then the cursor advances to the max committer-time written —
//! only after the batch is written, and only if the walk wasn't truncated by
//! the [`MAX_COMMITS_PER_REPO`] cap. First run (no cursor): import the full HEAD
//! ancestry.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local};
use serde::{Deserialize, Serialize};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Hourly, like the other always-on local collectors. Commits trickle in; an
/// hourly sweep keeps the log fresh without churn.
pub const LOCAL_GIT_SYNC_SECS: u64 = 3600;

/// Contract-free raw stream: one row per commit.
const DIR: &str = "developer/git";
/// Rebuildable cursor: repo path → highest committer-time imported (epoch s).
const SYNC_FILE: &str = ".trove/git-sync.json";

/// How many directory levels below the scan root to descend before giving up.
/// Repos a developer cares about live within a handful of levels of home; a
/// shallow bound keeps the scan from wandering the whole disk.
const MAX_DEPTH: usize = 5;

/// Safety cap on commits imported from a single repo per pass. A pathological
/// monorepo (kernel-sized) won't stall the watcher owner loop. When the cap is
/// hit the cursor is *held* (not advanced) so the un-imported tail isn't
/// stranded; see [`Vault::sync_one_repo`].
const MAX_COMMITS_PER_REPO: usize = 50_000;

/// The per-repo commit cap: `TROVE_GIT_MAX_COMMITS` when set, parseable, and
/// > 0, else [`MAX_COMMITS_PER_REPO`]. The env seam (mirroring how the scan root
/// honors `TROVE_HOME`) lets tests exercise the cap path without 50k commits.
fn max_commits_per_repo() -> usize {
    std::env::var("TROVE_GIT_MAX_COMMITS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(MAX_COMMITS_PER_REPO)
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_local_git()?;
    Ok(crate::registry::CollectOutcome::note_if(s.commits > 0, || {
        format!("local git synced — {} commits from {} repos", s.commits, s.repos_with_new)
    }))
}

// Manual "Sync now": the same scan, surfacing a human headline.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_local_git()?;
    let headline = if s.commits == 0 {
        format!("Local Git is up to date — no new commits ({} repos scanned)", s.repos_scanned)
    } else {
        format!("Local Git synced — {} commits from {} repos", s.commits, s.repos_with_new)
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("commits", s.commits),
            ("repos", s.repos_scanned),
        ]),
    })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "local-git",
        name: "Local Git Activity",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Indexes every commit in the Git repositories on this Mac — \
                      timestamp, branch, author, subject, and per-commit \
                      insertions/deletions — by scanning your home folder. \
                      Pure-local: it reads the repos directly, nothing is sent \
                      anywhere.",
        domain: "developer",
        vault_path: "developer/git/",
        toggleable: true,
        setup: &[
            "Reads the Git repositories already on your Mac — nothing to install or connect.",
            "Scans your home folder (to a bounded depth) for repos; heavy folders like Library and node_modules are skipped.",
            "Set TROVE_HOME to scan a different root than your home directory.",
        ],
        caveats: "Scans your home folder for repos, so a repo outside it (or behind a skipped \
                  folder like Library or node_modules) won't be indexed. Stores commits from all \
                  authors in each repo, not only yours — the raw stream is kept complete. Only the \
                  current HEAD branch of each repo is followed. Merge commits and root commits are \
                  recorded without insertion/deletion stats.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(LOCAL_GIT_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// The raw commit row (this module's own shape — no contract).

/// One row in `developer/git/YYYY-MM.jsonl`: a single commit, raw fidelity.
/// `guid == "<repo>:<sha>"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CommitRow {
    /// Committer time as RFC3339, preserving the commit's *own* UTC offset
    /// (full fidelity — not normalized to local). Partition key is its month.
    pub ts: String,
    /// Repo path: HOME-relative when under the scan root's home, else absolute.
    pub repo: String,
    /// HEAD branch short-name (e.g. "main"). Empty for a detached HEAD.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub branch: String,
    /// Full hex commit id.
    pub sha: String,
    /// Short (12-char) sha prefix, for convenience.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub short_sha: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author_email: String,
    /// First line of the commit message.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subject: String,
    /// Full commit message (kept — it's cheap and raw fidelity wants it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Lines added in the single-parent diff. Omitted for merge/root commits
    /// or on any diff error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insertions: Option<u64>,
    /// Lines removed. Omitted alongside `insertions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    /// Files touched. Omitted alongside `insertions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<u64>,
    /// `<repo>:<sha>` — the upsert/dedupe key.
    pub guid: String,
}

/// Result of one scan pass, for logging/status.
#[derive(Debug, Clone, Default)]
pub struct LocalGitStats {
    /// Repos discovered and opened this pass.
    pub repos_scanned: u64,
    /// Repos that yielded at least one new commit.
    pub repos_with_new: u64,
    /// Commits written this pass.
    pub commits: u64,
}

// ---------------------------------------------------------------------------
// Scan root.

/// The scan root: `TROVE_HOME` when set and non-empty, else the home dir. The
/// `TROVE_HOME` override mirrors claude_code's `CLAUDE_CONFIG_DIR` so tests can
/// point the scan at a temp tree.
fn scan_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

// ---------------------------------------------------------------------------
// Discovery (pure, fixture-tested).

/// Directory names to prune: heavy caches, app bundles, build output, and dirs
/// whose contents aren't source repos worth scanning. Plus the convention
/// "skip dotted dirs" handled in [`discover_repos`] (the root itself is never
/// pruned). Over-pruning is safer than scanning the world.
fn is_pruned_dir(name: &str) -> bool {
    matches!(
        name,
        ".Trash"
            | "Library"
            | "Applications"
            | "node_modules"
            | "target"
            | ".cache"
            | ".cargo"
            | ".rustup"
            | "Pictures"
            | "Movies"
            | "Music"
            | "vendor"
            | "Pods"
            | ".venv"
            | "venv"
            | "__pycache__"
            | "DerivedData"
    )
}

/// Does this directory hold a git repo? True when it contains a `.git` entry,
/// either a directory (normal repo) or a file (worktree / submodule gitlink).
fn dir_is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Walk `root` to [`MAX_DEPTH`] levels, returning every git repo's canonical
/// path, deduped. A directory containing a `.git` is recorded and **not**
/// descended into (so submodules and nested repos inside its working tree are
/// skipped). Pruned and dotted directories (except the root) are not entered.
/// Unreadable directories are skipped silently after a single log line. Pure —
/// no vault, no gix — so it's exercised with a synthetic tree.
fn discover_repos(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut logged_unreadable = false;
    // Iterative DFS with explicit depth so a deep tree can't blow the stack.
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, depth)) = stack.pop() {
        // A repo root: record it once (canonicalized) and prune the subtree.
        if dir_is_repo(&dir) {
            let canon = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if seen.insert(canon.clone()) {
                found.push(canon);
            }
            continue;
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                // Permission denied / vanished: skip gracefully, log once.
                if !logged_unreadable {
                    eprintln!("trove local-git: skipping unreadable dir {}: {e}", dir.display());
                    logged_unreadable = true;
                }
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Only descend into real directories (not symlinks — avoid cycles
            // and escaping the scan root).
            let is_dir = entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false);
            if !is_dir {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Never enter a `.git` internals dir, any pruned name, or a dotted
            // dir (dotfiles dirs are config/state, not source trees).
            if name == ".git" || is_pruned_dir(&name) || name.starts_with('.') {
                continue;
            }
            stack.push((path, depth + 1));
        }
    }
    // Deterministic order regardless of readdir/stack ordering.
    found.sort();
    found
}

// ---------------------------------------------------------------------------
// Per-commit extraction (gix).

/// One commit's metadata extracted from gix, before it's turned into a row.
struct CommitMeta {
    /// Committer epoch seconds — the cursor watermark and partition driver.
    committed_secs: i64,
    /// Committer time as RFC3339 preserving the commit's own offset.
    ts: String,
    sha: String,
    author_name: String,
    author_email: String,
    subject: String,
    message: String,
    insertions: Option<u64>,
    deletions: Option<u64>,
    files_changed: Option<u64>,
}

/// RFC3339 from epoch seconds + a UTC offset in seconds, preserving the offset
/// the commit recorded (full fidelity). Falls back to UTC if the offset is out
/// of range.
fn rfc3339_with_offset(secs: i64, offset_secs: i32) -> String {
    let dt = DateTime::from_timestamp(secs, 0).unwrap_or_default();
    match FixedOffset::east_opt(offset_secs) {
        Some(off) => dt.with_timezone(&off).to_rfc3339(),
        None => dt.to_rfc3339(),
    }
}

/// First line of a commit message, trimmed of trailing whitespace.
fn subject_of(message: &str) -> String {
    message.lines().next().unwrap_or("").trim_end().to_string()
}

/// Single-parent tree-diff stats `(insertions, deletions, files_changed)`.
/// Returns `None` for merge commits (>1 parent), root commits (0 parents), or
/// any diff error — never propagates (a commit log without stats is fine).
fn diff_stats(repo: &gix::Repository, commit: &gix::Commit<'_>) -> Option<(u64, u64, u64)> {
    let parents: Vec<_> = commit.parent_ids().collect();
    if parents.len() != 1 {
        return None; // merge or root: omit stats
    }
    let parent = repo.find_commit(parents[0].detach()).ok()?;
    let parent_tree = parent.tree().ok()?;
    let commit_tree = commit.tree().ok()?;
    let mut changes = parent_tree.changes().ok()?;
    let stats = changes.stats(&commit_tree).ok()?;
    Some((stats.lines_added, stats.lines_removed, stats.files_changed))
}

/// Pull everything we keep out of one gix commit. Tolerant: a field that fails
/// to decode is left empty rather than dropping the commit.
fn commit_meta(repo: &gix::Repository, id: gix::ObjectId) -> Option<CommitMeta> {
    let commit = repo.find_commit(id).ok()?;
    // Committer time drives the partition + cursor; without it we can't file
    // the row, so this is the one hard requirement.
    let time = commit.time().ok()?;
    let committed_secs = time.seconds;
    let ts = rfc3339_with_offset(committed_secs, time.offset);

    let (author_name, author_email) = match commit.author() {
        Ok(sig) => (
            // SignatureRef name/email are &BStr (bytes); decode lossily then
            // trim the whitespace gix preserves verbatim.
            String::from_utf8_lossy(sig.name).trim().to_string(),
            String::from_utf8_lossy(sig.email).trim().to_string(),
        ),
        Err(_) => (String::new(), String::new()),
    };

    let message = commit
        .message_raw()
        .map(|m| m.to_string())
        .unwrap_or_default();
    let message = message.trim_end().to_string();
    let subject = subject_of(&message);

    let (insertions, deletions, files_changed) = match diff_stats(repo, &commit) {
        Some((i, d, f)) => (Some(i), Some(d), Some(f)),
        None => (None, None, None),
    };

    Some(CommitMeta {
        committed_secs,
        ts,
        sha: id.to_hex().to_string(),
        author_name,
        author_email,
        subject,
        message,
        insertions,
        deletions,
        files_changed,
    })
}

/// The HEAD branch short-name, or "" for a detached HEAD / no ref.
fn head_branch(repo: &gix::Repository) -> String {
    match repo.head_ref() {
        Ok(Some(r)) => r.name().shorten().to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Cursor.

/// Incremental-sync state, persisted in `.trove/git-sync.json`. `cursors`
/// (repo path → highest committer-time imported, epoch seconds) is the real
/// state; it's rebuildable by re-scanning (a lost cursor just re-imports every
/// commit, and the guid upsert keeps that idempotent).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalGitSyncState {
    /// RFC3339 local time of the last sync pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// repo path → newest committer-time imported (epoch seconds).
    #[serde(default)]
    pub cursors: BTreeMap<String, i64>,
}

impl Vault {
    fn read_local_git_sync(&self) -> LocalGitSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_local_git_sync(&self, state: &LocalGitSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    // -----------------------------------------------------------------------
    // The scan.

    /// One incremental scan pass over the git repos under the scan root.
    /// Silently a no-op when there's no scan root.
    pub fn collect_local_git(&self) -> Result<LocalGitStats> {
        let Some(root) = scan_root() else {
            return Ok(LocalGitStats::default());
        };
        self.collect_local_git_from(&root)
    }

    /// The pass itself, scan-root-injected for tests. Walks every discovered
    /// repo, importing commits newer than each repo's cursor. The per-repo cap
    /// comes from [`max_commits_per_repo`] (the `TROVE_GIT_MAX_COMMITS` env seam).
    pub(crate) fn collect_local_git_from(&self, root: &Path) -> Result<LocalGitStats> {
        self.collect_local_git_with(root, max_commits_per_repo())
    }

    /// Like [`Self::collect_local_git_from`] but with the per-repo commit `cap`
    /// injected directly. This is the cap's test seam (parallel to how
    /// `collect_local_git_from` injects the scan root) — it avoids mutating the
    /// process-global `TROVE_GIT_MAX_COMMITS` env in tests, which would race the
    /// other import tests running concurrently in the same process.
    pub(crate) fn collect_local_git_with(&self, root: &Path, cap: usize) -> Result<LocalGitStats> {
        let mut stats = LocalGitStats::default();
        let repos = discover_repos(root);
        if repos.is_empty() {
            return Ok(stats);
        }
        // HOME, for forming repo-relative paths. The scan root is treated as
        // "home" for display (in tests it's the temp root). Canonicalize it so
        // it shares the realpath form that `discover_repos` gave the repos
        // (macOS resolves /var → /private/var); otherwise strip_prefix misses.
        let home = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let home = home.as_path();

        let mut state = self.read_local_git_sync();
        let mut changed = false;

        for repo_path in repos {
            stats.repos_scanned += 1;
            let repo_key = display_repo(&repo_path, home);
            match self.sync_one_repo(&repo_path, &repo_key, &mut state, cap) {
                Ok(n) if n > 0 => {
                    stats.commits += n;
                    stats.repos_with_new += 1;
                    changed = true;
                }
                Ok(_) => {}
                Err(e) => {
                    // A single bad repo (corrupt, mid-rebase, unreadable) never
                    // aborts the whole scan — log and move on. Cursor untouched
                    // → retried next pass.
                    eprintln!("trove local-git: skipping repo {}: {e:#}", repo_path.display());
                }
            }
        }

        if changed {
            state.updated = Local::now().to_rfc3339();
            self.write_local_git_sync(&state)?;
        }
        Ok(stats)
    }

    /// Import one repo's new commits. Walks the **full** HEAD ancestry
    /// newest-first (no commit-time cutoff), taking commits whose committer-time
    /// is **strictly greater** than the repo's cursor, upserts them into their
    /// month partitions by guid, then advances the cursor to the max
    /// committer-time written (only after the write) — but only if the walk
    /// completed without hitting the per-repo cap. Returns the number of commits
    /// written.
    fn sync_one_repo(
        &self,
        repo_path: &Path,
        repo_key: &str,
        state: &mut LocalGitSyncState,
        cap: usize,
    ) -> Result<u64> {
        let repo = gix::open(repo_path)
            .with_context(|| format!("opening repo {}", repo_path.display()))?;
        let branch = head_branch(&repo);

        let Ok(head_id) = repo.head_id() else {
            // Unborn HEAD (freshly `git init`, no commits) — nothing to do.
            return Ok(0);
        };
        let cursor = state.cursors.get(repo_key).copied();

        // Full-ancestry newest-first walk — NO commit-time cutoff. A ts cutoff
        // prunes DAG descent and misses commits behind older nodes (merged-in
        // feature branches, amended/rebased/backdated HEAD); must walk full
        // ancestry and filter on write. Git committer-time is not monotonic in
        // the DAG, so a newer commit can sit behind an older one and a cutoff
        // would never reach it. The strict `committed_secs > cursor` write-
        // filter below alone decides what gets written; this walk only
        // guarantees every newer commit is *visited*. The guid upsert keeps a
        // full re-walk idempotent.
        let walk = {
            use gix::traverse::commit::simple::CommitTimeOrder;
            let platform = repo
                .rev_walk(Some(head_id.detach()))
                .sorting(gix::revision::walk::Sorting::ByCommitTime(
                    CommitTimeOrder::NewestFirst,
                ));
            platform.all().with_context(|| format!("walking {}", repo_path.display()))?
        };

        let mut rows: Vec<CommitRow> = Vec::new();
        let mut max_secs: Option<i64> = cursor;
        let mut hit_cap = false;
        for info in walk {
            let info = match info {
                Ok(i) => i,
                Err(_) => continue, // a single bad object — skip it
            };
            let Some(meta) = commit_meta(&repo, info.id) else {
                continue;
            };
            // Strictly newer than the cursor — the boundary commit (== cursor)
            // is excluded so it isn't re-counted; the guid upsert would dedupe
            // it anyway, this just avoids the work.
            if let Some(c) = cursor {
                if meta.committed_secs <= c {
                    continue;
                }
            }
            max_secs = Some(max_secs.map_or(meta.committed_secs, |m| m.max(meta.committed_secs)));

            let guid = format!("{repo_key}:{}", meta.sha);
            rows.push(CommitRow {
                ts: meta.ts,
                repo: repo_key.to_string(),
                branch: branch.clone(),
                short_sha: meta.sha.chars().take(12).collect(),
                sha: meta.sha,
                author_name: meta.author_name,
                author_email: meta.author_email,
                subject: meta.subject,
                message: meta.message,
                insertions: meta.insertions,
                deletions: meta.deletions,
                files_changed: meta.files_changed,
                guid,
            });
            if rows.len() >= cap {
                hit_cap = true;
                break;
            }
        }

        let written = self.upsert_commit_rows(&rows)?;

        if hit_cap {
            // The walk was truncated before exhausting ancestry. Newest-first +
            // a forward cursor can never backfill an older tail, so advancing
            // the cursor would strand the un-imported commits forever. Hold it.
            // TODO(local-git): >cap repos refresh only their most-recent {cap}
            // commits each pass and don't backfill the ancient tail; a full fix
            // needs oldest-first paging or a stored-HEAD-sha diff.
            eprintln!(
                "local-git: repo {repo_key} exceeds {cap} commits — \
                 imported the most recent batch; cursor held"
            );
            return Ok(written);
        }

        // Advance the cursor ONLY after the batch is written, so an interrupted
        // write re-imports rather than skips.
        if let Some(m) = max_secs {
            let entry = state.cursors.entry(repo_key.to_string()).or_insert(m);
            *entry = (*entry).max(m);
        }
        Ok(written)
    }

    /// Upsert commit rows into their month partitions, keyed by `guid`. Groups
    /// by partition, reads each touched partition once, replaces any matching
    /// guid and appends the rest, and rewrites the partition atomically. Returns
    /// the count of rows that were genuinely new (not already present by guid).
    fn upsert_commit_rows(&self, rows: &[CommitRow]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        // Group incoming rows by their commit-month partition key.
        let mut by_key: BTreeMap<String, Vec<&CommitRow>> = BTreeMap::new();
        for r in rows {
            let key = Partition::Month.key(&r.ts).with_context(|| {
                format!("commit {} has an unpartitionable ts {:?}", r.guid, r.ts)
            })?;
            by_key.entry(key.to_string()).or_default().push(r);
        }

        let stream = self.stream(DIR, Partition::Month);
        let mut new_count = 0u64;
        for (key, incoming) in by_key {
            let mut existing: Vec<CommitRow> = stream.read(&key)?;
            let mut index: std::collections::HashMap<String, usize> = existing
                .iter()
                .enumerate()
                .map(|(i, r)| (r.guid.clone(), i))
                .collect();
            for r in incoming {
                match index.get(&r.guid) {
                    Some(&i) => existing[i] = r.clone(), // upsert in place
                    None => {
                        index.insert(r.guid.clone(), existing.len());
                        existing.push(r.clone());
                        new_count += 1;
                    }
                }
            }
            self.write_snapshot(&format!("{DIR}/{key}.jsonl"), &existing)?;
        }
        Ok(new_count)
    }

    // -----------------------------------------------------------------------
    // Reads.

    /// All commit rows for one month (`YYYY-MM`), in file order.
    pub fn local_git_commits(&self, month: &str) -> Result<Vec<CommitRow>> {
        self.stream(DIR, Partition::Month).read(month)
    }
}

/// Display path for a repo: HOME-relative when under `home`, else absolute.
fn display_repo(repo: &Path, home: &Path) -> String {
    match repo.strip_prefix(home) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        // The repo IS the home root, or isn't under it: use the absolute path.
        _ => repo.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-localgit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A unique temp dir to build fixture repos under.
    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-lgroot-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Is the `git` binary available? Tests that build fixture repos shell to
    /// `git` for hermeticity (the *shipped* collector uses gix); when git is
    /// absent the test skips gracefully rather than failing.
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// `git` in `dir` with a fixed identity and deterministic environment, so
    /// commit ids and authorship are reproducible and independent of the host's
    /// global git config.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Fixture Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Fixture Author")
            .env("GIT_COMMITTER_EMAIL", "author@example.com")
            .env("GIT_AUTHOR_DATE", "2026-06-10T12:00:00+00:00")
            .env("GIT_COMMITTER_DATE", "2026-06-10T12:00:00+00:00")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        assert!(status.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&status.stderr));
    }

    /// Init a repo at `path` on a fixed branch with deterministic config.
    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.name", "Fixture Author"]);
        git(path, &["config", "user.email", "author@example.com"]);
        git(path, &["config", "commit.gpgsign", "false"]);
    }

    /// Write a file and commit it with a deterministic committer date. The
    /// `date` is an RFC3339-ish string git accepts (e.g. "2026-06-10T12:00:00+00:00").
    fn commit_file(path: &Path, file: &str, body: &str, msg: &str, date: &str) {
        fs::write(path.join(file), body).unwrap();
        git(path, &["add", file]);
        let status = Command::new("git")
            .current_dir(path)
            .args(["commit", "-q", "-m", msg])
            .env("GIT_AUTHOR_NAME", "Fixture Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Fixture Author")
            .env("GIT_COMMITTER_EMAIL", "author@example.com")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(status.status.success(), "commit: {}", String::from_utf8_lossy(&status.stderr));
    }

    // ----- pure discovery (no git needed) -----

    #[test]
    fn discovers_repos_prunes_subtree_and_skips_nested() {
        let root = temp_root("discover");
        // A repo with a normal .git dir.
        let repo_a = root.join("projects/alpha");
        fs::create_dir_all(repo_a.join(".git")).unwrap();
        // A NESTED repo inside repo_a's working tree — must NOT be discovered
        // separately (the parent subtree is pruned at the repo boundary).
        fs::create_dir_all(repo_a.join("subproj/.git")).unwrap();
        // A submodule-style gitlink: a `.git` *file* (not dir) also counts.
        let repo_b = root.join("projects/beta");
        fs::create_dir_all(&repo_b).unwrap();
        fs::write(repo_b.join(".git"), "gitdir: /elsewhere/.git/modules/beta").unwrap();
        // A pruned heavy dir holding a stray repo — must be skipped.
        fs::create_dir_all(root.join("Library/Caches/somepkg/.git")).unwrap();
        fs::create_dir_all(root.join("projects/web/node_modules/dep/.git")).unwrap();
        // A plain non-repo dir.
        fs::create_dir_all(root.join("projects/notes")).unwrap();

        // discover_repos canonicalizes repo paths (macOS /var → /private/var);
        // canonicalize the root the same way so display paths come out relative.
        let croot = fs::canonicalize(&root).unwrap();
        let repos = discover_repos(&root);
        let rel: HashSet<String> = repos
            .iter()
            .map(|p| display_repo(p, &croot))
            .collect();
        assert!(rel.contains("projects/alpha"), "found alpha: {rel:?}");
        assert!(rel.contains("projects/beta"), "found beta (gitlink file): {rel:?}");
        assert!(!rel.iter().any(|r| r.contains("subproj")), "nested repo pruned: {rel:?}");
        assert!(!rel.iter().any(|r| r.contains("Library")), "Library pruned: {rel:?}");
        assert!(!rel.iter().any(|r| r.contains("node_modules")), "node_modules pruned: {rel:?}");
        assert_eq!(repos.len(), 2, "exactly alpha + beta: {rel:?}");
    }

    #[test]
    fn depth_is_bounded() {
        let root = temp_root("depth");
        // Bury a repo deeper than MAX_DEPTH levels — it must not be found.
        let mut deep = root.clone();
        for i in 0..(MAX_DEPTH + 2) {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(deep.join(".git")).unwrap();
        // And one within bounds.
        fs::create_dir_all(root.join("a/b/.git")).unwrap();
        let croot = fs::canonicalize(&root).unwrap();
        let repos = discover_repos(&root);
        let rel: HashSet<String> = repos.iter().map(|p| display_repo(p, &croot)).collect();
        assert!(rel.contains("a/b"), "shallow repo found: {rel:?}");
        assert!(!rel.iter().any(|r| r.contains("d5") || r.contains("d6")), "deep repo pruned: {rel:?}");
    }

    #[test]
    fn subject_takes_first_line() {
        assert_eq!(subject_of("one line"), "one line");
        assert_eq!(subject_of("subject\n\nbody text\nmore"), "subject");
        assert_eq!(subject_of("trailing  \n"), "trailing");
        assert_eq!(subject_of(""), "");
    }

    #[test]
    fn rfc3339_preserves_the_commit_offset() {
        // 2026-06-10T12:00:00Z with a -07:00 offset → local-offset RFC3339.
        let ts = rfc3339_with_offset(1_780_056_000, -7 * 3600);
        assert!(ts.ends_with("-07:00"), "offset preserved: {ts}");
        assert_eq!(
            DateTime::parse_from_rfc3339(&ts).unwrap().timestamp(),
            1_780_056_000
        );
        // A +05:30 offset round-trips too.
        let ts2 = rfc3339_with_offset(1_780_056_000, 5 * 3600 + 1800);
        assert!(ts2.ends_with("+05:30"), "half-hour offset: {ts2}");
    }

    #[test]
    fn cursor_back_compat_old_and_empty_deserialize() {
        let empty: LocalGitSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.cursors.is_empty());
        assert_eq!(empty.updated, "");
        // An "old" cursor that only wrote `updated` still deserializes.
        let old: LocalGitSyncState =
            serde_json::from_str(r#"{"updated":"2026-01-01T00:00:00-08:00"}"#).unwrap();
        assert_eq!(old.updated, "2026-01-01T00:00:00-08:00");
        assert!(old.cursors.is_empty());
        // A CommitRow with only the required fields (old sparse line) decodes.
        let sparse: CommitRow = serde_json::from_str(
            r#"{"ts":"2026-06-10T12:00:00+00:00","repo":"a","sha":"abc","guid":"a:abc"}"#,
        )
        .unwrap();
        assert_eq!(sparse.guid, "a:abc");
        assert_eq!(sparse.insertions, None);
        assert_eq!(sparse.branch, "");
    }

    #[test]
    fn missing_root_is_a_quiet_noop() {
        let v = temp_vault("missing");
        let root = temp_root("missing-empty"); // exists but holds no repos
        let stats = v.collect_local_git_from(&root).unwrap();
        assert_eq!(stats.commits, 0);
        assert!(!v.root().join(DIR).exists());
    }

    // ----- end-to-end with real fixture repos (need git) -----

    #[test]
    fn imports_commits_with_correct_fields_and_partitions() {
        if !git_available() {
            eprintln!("skipping imports_commits_*: git binary not available");
            return;
        }
        let v = temp_vault("import");
        let root = temp_root("import");
        let repo = root.join("proj");
        init_repo(&repo);
        // Three commits across two months (so partitioning is exercised).
        commit_file(&repo, "a.txt", "line1\nline2\n", "first commit", "2026-05-20T09:00:00+00:00");
        commit_file(&repo, "b.txt", "x\n", "second commit", "2026-06-01T10:00:00+00:00");
        commit_file(&repo, "a.txt", "line1\nline2\nline3\n", "third: add a line", "2026-06-02T11:00:00+00:00");

        let stats = v.collect_local_git_from(&root).unwrap();
        assert_eq!(stats.repos_scanned, 1);
        assert_eq!(stats.commits, 3);

        // Partitioned by commit month.
        let may = v.local_git_commits("2026-05").unwrap();
        let jun = v.local_git_commits("2026-06").unwrap();
        assert_eq!(may.len(), 1, "one May commit");
        assert_eq!(jun.len(), 2, "two June commits");

        // Field correctness on a known row.
        let first = &may[0];
        assert_eq!(first.repo, "proj", "HOME-relative repo path");
        assert_eq!(first.branch, "main");
        assert_eq!(first.subject, "first commit");
        assert_eq!(first.author_email, "author@example.com");
        assert_eq!(first.author_name, "Fixture Author");
        assert_eq!(first.guid, format!("proj:{}", first.sha), "guid == <repo>:<sha>");
        assert_eq!(first.sha.len(), 40, "full hex sha");
        assert_eq!(first.short_sha, &first.sha[..12]);
        // The root commit (no parent) carries no diff stats.
        assert_eq!(first.insertions, None, "root commit: stats omitted");
        // ts preserves the +00:00 offset the commit recorded.
        assert!(first.ts.starts_with("2026-05-20T09:00:00"), "ts: {}", first.ts);
        assert_eq!(DateTime::parse_from_rfc3339(&first.ts).unwrap().timestamp(),
                   DateTime::parse_from_rfc3339("2026-05-20T09:00:00+00:00").unwrap().timestamp());

        // The "add a line" commit has a single parent → real diff stats.
        let third = jun.iter().find(|c| c.subject.starts_with("third")).unwrap();
        assert_eq!(third.insertions, Some(1), "one line added");
        assert_eq!(third.deletions, Some(0));
        assert_eq!(third.files_changed, Some(1));
    }

    #[test]
    fn incremental_cursor_imports_only_new_commits() {
        if !git_available() {
            eprintln!("skipping incremental_cursor_*: git binary not available");
            return;
        }
        let v = temp_vault("incr");
        let root = temp_root("incr");
        let repo = root.join("proj");
        init_repo(&repo);
        commit_file(&repo, "a.txt", "1\n", "c1", "2026-06-10T09:00:00+00:00");
        commit_file(&repo, "a.txt", "1\n2\n", "c2", "2026-06-10T09:05:00+00:00");

        let first = v.collect_local_git_from(&root).unwrap();
        assert_eq!(first.commits, 2);
        assert_eq!(v.local_git_commits("2026-06").unwrap().len(), 2);

        // Re-sync with no new commits → nothing imported, no dups.
        let noop = v.collect_local_git_from(&root).unwrap();
        assert_eq!(noop.commits, 0, "cursor: unchanged repo yields nothing");
        assert_eq!(v.local_git_commits("2026-06").unwrap().len(), 2, "no duplicates");

        // Add exactly ONE new commit (strictly later) and re-sync.
        commit_file(&repo, "a.txt", "1\n2\n3\n", "c3", "2026-06-10T09:10:00+00:00");
        let second = v.collect_local_git_from(&root).unwrap();
        assert_eq!(second.commits, 1, "exactly one new commit imported");
        let rows = v.local_git_commits("2026-06").unwrap();
        assert_eq!(rows.len(), 3, "three total, no dups across the boundary");
        assert!(rows.iter().any(|r| r.subject == "c3"));
    }

    #[test]
    fn submodule_nested_repo_recorded_once() {
        if !git_available() {
            eprintln!("skipping submodule_*: git binary not available");
            return;
        }
        let v = temp_vault("nested");
        let root = temp_root("nested");
        // Outer repo with a commit.
        let outer = root.join("outer");
        init_repo(&outer);
        commit_file(&outer, "top.txt", "hi\n", "outer commit", "2026-06-10T08:00:00+00:00");
        // A real nested repo inside the outer working tree (a submodule lives
        // here too). It has its own commit. Discovery must record `outer` once
        // and never descend to read the inner repo.
        let inner = outer.join("vendored");
        init_repo(&inner);
        commit_file(&inner, "in.txt", "x\n", "inner commit — must not appear", "2026-06-10T08:30:00+00:00");

        let stats = v.collect_local_git_from(&root).unwrap();
        assert_eq!(stats.repos_scanned, 1, "only the outer repo is discovered");
        let rows = v.local_git_commits("2026-06").unwrap();
        assert_eq!(rows.len(), 1, "only the outer commit imported");
        assert_eq!(rows[0].subject, "outer commit");
        assert!(
            !rows.iter().any(|r| r.subject.contains("inner")),
            "inner repo not double-read"
        );
    }

    #[test]
    fn multiple_repos_under_root_all_appear() {
        if !git_available() {
            eprintln!("skipping multiple_repos_*: git binary not available");
            return;
        }
        let v = temp_vault("multi");
        let root = temp_root("multi");
        let one = root.join("code/one");
        let two = root.join("code/two");
        init_repo(&one);
        init_repo(&two);
        commit_file(&one, "f.txt", "a\n", "one's commit", "2026-06-11T09:00:00+00:00");
        commit_file(&two, "f.txt", "b\n", "two's commit", "2026-06-11T10:00:00+00:00");

        let stats = v.collect_local_git_from(&root).unwrap();
        assert_eq!(stats.repos_scanned, 2, "both repos discovered");
        assert_eq!(stats.commits, 2);
        let rows = v.local_git_commits("2026-06").unwrap();
        let repos: HashSet<&str> = rows.iter().map(|r| r.repo.as_str()).collect();
        assert!(repos.contains("code/one"), "repo one present: {repos:?}");
        assert!(repos.contains("code/two"), "repo two present: {repos:?}");
    }

    #[test]
    fn merge_commit_diff_stats_omitted_gracefully() {
        if !git_available() {
            eprintln!("skipping merge_commit_*: git binary not available");
            return;
        }
        let v = temp_vault("merge");
        let root = temp_root("merge");
        let repo = root.join("proj");
        init_repo(&repo);
        // base commit
        commit_file(&repo, "base.txt", "base\n", "base", "2026-06-10T09:00:00+00:00");
        // branch off and commit on a side branch
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        commit_file(&repo, "feature.txt", "feat\n", "feature work", "2026-06-10T09:05:00+00:00");
        // back to main, diverge
        git(&repo, &["checkout", "-q", "main"]);
        commit_file(&repo, "main.txt", "main\n", "main work", "2026-06-10T09:06:00+00:00");
        // merge feature into main (creates a 2-parent merge commit), no-ff
        git(
            &repo,
            &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
        );

        // Must not panic; the merge commit lands with no stats.
        let stats = v.collect_local_git_from(&root).unwrap();
        assert!(stats.commits >= 4, "base+feature+main+merge imported: {}", stats.commits);
        let rows = v.local_git_commits("2026-06").unwrap();
        let merge = rows
            .iter()
            .find(|r| r.subject == "merge feature")
            .expect("merge commit present");
        assert_eq!(merge.insertions, None, "merge commit: stats omitted, no panic");
        assert_eq!(merge.deletions, None);
        assert_eq!(merge.files_changed, None);
        // A normal single-parent commit on the branch still has stats.
        let feat = rows.iter().find(|r| r.subject == "feature work").unwrap();
        assert_eq!(feat.insertions, Some(1));
    }

    /// Commit a file at an explicit committer epoch (seconds, +0000), via the
    /// `@<unixtime> <tz>` form git's date parser accepts — so committer-time is
    /// exactly the integer we pass (the cursor watermark).
    fn commit_at(path: &Path, file: &str, body: &str, msg: &str, secs: i64) {
        commit_file(path, file, body, msg, &format!("@{secs} +0000"));
    }

    /// Run a raw git command (returning whether it succeeded) under the fixed,
    /// host-independent identity/environment, with an explicit committer date.
    fn git_at(dir: &Path, secs: i64, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Fixture Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Fixture Author")
            .env("GIT_COMMITTER_EMAIL", "author@example.com")
            .env("GIT_AUTHOR_DATE", format!("@{secs} +0000"))
            .env("GIT_COMMITTER_DATE", format!("@{secs} +0000"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    // ----- FIX 1: full-ancestry walk (no committer-time cutoff) -----

    /// REGRESSION (bug 1b): a merge brings in feature commits NEWER than the
    /// cursor, but the merge itself is committed with an OLDER time. A ts cutoff
    /// stops descending at the (old) HEAD and never reaches the newer feature
    /// commits behind it; a full-ancestry walk must still find them.
    #[test]
    fn incremental_does_not_miss_commits_behind_an_older_merge() {
        if !git_available() {
            eprintln!("skipping incremental_does_not_miss_commits_behind_an_older_merge: git binary not available");
            return;
        }
        let v = temp_vault("behind-merge");
        let root = temp_root("behind-merge");
        let repo = root.join("proj");
        init_repo(&repo);
        // main: M1@1000, M2@2000. First import sets cursor = 2000 naturally.
        commit_at(&repo, "m1.txt", "m1\n", "M1", 1000);
        commit_at(&repo, "m2.txt", "m2\n", "M2", 2000);
        let first = v.collect_local_git_from(&root).unwrap();
        assert_eq!(first.commits, 2, "first import: M1+M2");

        // Feature branch off M2: F1@3000, F2@3100 (both NEWER than cursor 2000).
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        commit_at(&repo, "f1.txt", "f1\n", "F1", 3000);
        commit_at(&repo, "f2.txt", "f2\n", "F2", 3100);
        // Merge into main with an OLDER committer time (1500) than the cursor.
        git(&repo, &["checkout", "-q", "main"]);
        git_at(&repo, 1500, &["merge", "-q", "--no-ff", "-m", "MERGE", "feature"]);

        // Re-sync. Before fix: the cutoff at HEAD=MERGE@1500 prunes the walk →
        // 0 imported. After fix: the full walk reaches F1+F2 (the key assertion);
        // MERGE@1500 is correctly filtered (1500 < cursor 2000).
        let second = v.collect_local_git_from(&root).unwrap();
        // Fixtures use small epoch seconds (1970-01 partitions), so gather every
        // imported subject across all touched month partitions.
        let subjects = all_subjects(&v);
        assert!(subjects.contains("F1"), "F1 (ts>cursor, behind older merge) imported: {subjects:?}");
        assert!(subjects.contains("F2"), "F2 (ts>cursor, behind older merge) imported: {subjects:?}");
        assert!(!subjects.contains("MERGE"), "MERGE@1500 filtered (< cursor 2000): {subjects:?}");
        assert!(second.commits >= 2, "at least F1+F2 imported this pass: {}", second.commits);
    }

    /// REGRESSION (bug 1c): HEAD is a backdated commit whose PARENT is newer
    /// than the cursor (amend/rebase that rewrote the tip with an old date). A
    /// ts cutoff stops at the backdated HEAD and misses the newer parent.
    #[test]
    fn incremental_does_not_miss_a_newer_commit_under_a_backdated_head() {
        if !git_available() {
            eprintln!("skipping incremental_does_not_miss_a_newer_commit_under_a_backdated_head: git binary not available");
            return;
        }
        let v = temp_vault("backdated-head");
        let root = temp_root("backdated-head");
        let repo = root.join("proj");
        init_repo(&repo);
        // Seed a real cursor of 5000 by importing a base commit at t=5000.
        commit_at(&repo, "base.txt", "base\n", "BASE", 5000);
        let first = v.collect_local_git_from(&root).unwrap();
        assert_eq!(first.commits, 1, "first import: BASE (cursor → 5000)");

        // CNEW@6000 (newer than cursor) then COLD@2000 on top of it (HEAD is the
        // backdated commit; its parent CNEW is newer than the cursor).
        commit_at(&repo, "cnew.txt", "cnew\n", "CNEW", 6000);
        commit_at(&repo, "cold.txt", "cold\n", "COLD", 2000);

        // Re-sync. Before fix: cutoff at HEAD=COLD@2000 (≤ cursor 5000) prunes →
        // CNEW never visited → 0 imported. After fix: full walk reaches CNEW.
        let second = v.collect_local_git_from(&root).unwrap();
        let subjects = all_subjects(&v);
        assert!(subjects.contains("CNEW"), "CNEW@6000 (>cursor, under backdated HEAD) imported: {subjects:?}");
        assert!(!subjects.contains("COLD"), "COLD@2000 filtered (< cursor 5000): {subjects:?}");
        assert!(second.commits >= 1, "CNEW imported this pass: {}", second.commits);
    }

    /// Every commit subject imported into the vault, across all month
    /// partitions (fixtures here span 1970-01 — small epoch seconds — so we
    /// can't assume one fixed month string).
    fn all_subjects(v: &Vault) -> HashSet<String> {
        let dir = v.root().join(DIR);
        let mut out = HashSet::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(month) = name.strip_suffix(".jsonl") {
                    for r in v.local_git_commits(month).unwrap_or_default() {
                        out.insert(r.subject);
                    }
                }
            }
        }
        out
    }

    // ----- FIX 2: capped pass holds the cursor (no stranding) -----

    /// REGRESSION (bug 2): on hitting the per-repo cap, the cursor used to jump
    /// to the newest committer-time seen, permanently stranding the older
    /// un-imported tail (newest-first + a forward cursor can never backfill it).
    /// With the cap HIT, the cursor must be HELD so a later pass (here, a raised
    /// cap) still sees the un-imported commits — no permanent skip. The cap is
    /// injected via `collect_local_git_with` (not the process-global
    /// `TROVE_GIT_MAX_COMMITS` env, which would race concurrent import tests).
    #[test]
    fn capped_repo_holds_cursor_instead_of_stranding() {
        if !git_available() {
            eprintln!("skipping capped_repo_holds_cursor_instead_of_stranding: git binary not available");
            return;
        }
        let v = temp_vault("capped");
        let root = temp_root("capped");
        let repo = root.join("proj");
        init_repo(&repo);
        // 5 commits, strictly increasing committer-time.
        for (i, secs) in [(1, 1000), (2, 2000), (3, 3000), (4, 4000), (5, 5000)] {
            commit_at(&repo, &format!("c{i}.txt"), &format!("c{i}\n"), &format!("C{i}"), secs);
        }

        // First pass with cap = 2: at most `cap` rows written, cursor NOT advanced.
        let first = v.collect_local_git_with(&root, 2).unwrap();
        assert!(first.commits <= 2, "capped pass writes at most the cap: {}", first.commits);
        let state = v.read_local_git_sync();
        // The cursor for this repo must NOT have jumped to the newest (5000).
        // Holding it = absent, or at most ≤ the oldest commit (never the newest).
        match state.cursors.get("proj").copied() {
            None => {}
            Some(c) => assert!(
                c < 5000,
                "cursor must be held, not advanced to newest 5000 (got {c})"
            ),
        }

        // A later pass with a RAISED cap must still see the un-imported tail —
        // proving the first pass didn't permanently skip anything.
        let _ = v.collect_local_git_with(&root, 100).unwrap();
        let subjects = all_subjects(&v);
        for s in ["C1", "C2", "C3", "C4", "C5"] {
            assert!(subjects.contains(s), "{s} eventually imported (no stranding): {subjects:?}");
        }
    }

    /// The `TROVE_GIT_MAX_COMMITS` env seam parses correctly and falls back to
    /// the const default. Kept env-isolated by restoring immediately (parallel
    /// to `claude_code`'s `config_dir_override_is_followed`).
    #[test]
    fn max_commits_env_seam_overrides_default() {
        let prev = std::env::var("TROVE_GIT_MAX_COMMITS").ok();
        std::env::set_var("TROVE_GIT_MAX_COMMITS", "7");
        let got = max_commits_per_repo();
        std::env::set_var("TROVE_GIT_MAX_COMMITS", "0"); // invalid → fallback
        let zero = max_commits_per_repo();
        match prev {
            Some(v) => std::env::set_var("TROVE_GIT_MAX_COMMITS", v),
            None => std::env::remove_var("TROVE_GIT_MAX_COMMITS"),
        }
        assert_eq!(got, 7, "env value honored");
        assert_eq!(zero, MAX_COMMITS_PER_REPO, "0 is invalid → const fallback");
    }
}

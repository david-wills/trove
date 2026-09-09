//! Google Tasks collector — open and completed tasks from every connected
//! Google account into the same normalized task store as TickTick and Apple
//! Reminders (`tasks/google-tasks/`, see [`crate::tasks`]). The OAuth side
//! (connect, token store, refresh, reconnect flagging) is owned by
//! [`crate::sync::google`]; this module only asks it for a fresh token per
//! account via [`crate::sync::google::fresh_token`]. See [`crate::gmail`]
//! for the worked per-account-pull template.
//!
//! **Why Google Tasks is the easy task source.** TickTick's API exposes only
//! open tasks, so its completion stream exists purely by diffing snapshots
//! and probing fates. Google's `tasks.list` (with `showCompleted`,
//! `showHidden`, `showDeleted`) returns completed tasks *with their real
//! completion times* and deleted tasks flagged `deleted: true` — so when a
//! task vanishes from the open set, its fate is read straight off the same
//! pull, no follow-up lookups: present-as-completed → `Completed(true time)`,
//! present-as-deleted → `Deleted`, gone entirely → `Deleted` (Google purged
//! it, or its whole list was deleted — anything that still exists is
//! returned under those three flags). Fates fall back to `Unknown`
//! (carry-forward, the never-guess doctrine) only when the account or list
//! pull itself failed — and for tasks belonging to a since-disconnected
//! account, which freeze in the snapshot rather than being mislogged as
//! deleted (the vault never discards data on disconnect; they resolve if
//! the account reconnects).
//!
//! **History reachable.** Completed tasks stay listable (`showHidden=true`
//! covers ones the UI has "cleared") until the user deletes them or Google
//! purges them — Google documents no retention guarantee, and first-party
//! clients only surface recent completions. So on each account's **first
//! sync the visible completion history is seeded** into the event stream at
//! its true times (landing in the right `events/YYYY-MM.jsonl` months — the
//! same backfill-into-true-months spirit as the diff's `created` stamping),
//! and from then on the append-only stream is the durable record that
//! outlives Google's retention. Deletions before the first sync are
//! unreachable (no deletion timestamps exist). The per-account seeded flag
//! lives in `.trove/google-tasks-sync.json`; deleting that file would
//! re-seed and duplicate completion events, so don't.
//!
//! **Multi-account.** Task, and task-list, ids are namespaced as
//! `"<sub>:<google id>"` (Google's `sub` is the stable numeric account id
//! and never contains `:`, so splitting on the first `:` recovers it
//! unambiguously) — two accounts' tasks can never collide in the one shared
//! snapshot. The raw id, the owning account's email, and the raw task-list
//! id ride in `extra` (`google_id`, `account`, `list_id`). When more than
//! one account is connected, project names get an ` (email)` suffix so two
//! accounts' default "My Tasks" lists don't merge in the human checklists.
//!
//! **Subtasks.** Google subtasks are *full tasks* carrying a `parent`
//! pointer — unlike TickTick's `items`, which are bare checklist lines and
//! fold into `Task::subtasks`. Folding Google children the same way would
//! discard their notes/due/ids (raw vault data stays complete) and, worse,
//! log a false `deleted` event whenever the user indents an existing task
//! (its standalone id would vanish from the snapshot). So children stay
//! first-class tasks — own lifecycle events, own completion times — with
//! the raw parent id in `extra.parent`; readers can rebuild the tree.
//!
//! Like the other two task sources this syncs every 15 minutes inside the
//! watcher owner loop, flowing through the shared `tasks.rs` pipeline:
//! snapshot diff → `tasks/google-tasks/tasks.jsonl` + `events/YYYY-MM.jsonl`
//! + regenerated markdown, with the per-source status row in
//! `.trove/tasks-sync.json` under `"google-tasks"`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::tasks::{ProjectInfo, Task, TaskEvent, TaskFate, TasksSyncStats, TASKS_SYNC_SECS};
use crate::vault::Vault;

/// The source id: folder name under `tasks/`, key in `.trove/tasks-sync.json`,
/// and every task's `source` field.
const SOURCE: &str = "google-tasks";
const TASKS_API: &str = "https://tasks.googleapis.com";
/// Per-account seeded-history flags + last per-account errors.
const STATE_FILE: &str = ".trove/google-tasks-sync.json";
/// Kept short so a hung connection can't stall the watcher owner loop for
/// long (the `tasks.rs`/`oura.rs` reasoning).
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Page size for `tasklists.list` and `tasks.list` (`maxResults`, API max 100).
const PAGE_SIZE: u32 = 100;

// A silent no-op when no Google account is connected; per-account failures
// are recorded without aborting the other accounts.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_google_tasks()?;
    Ok(CollectOutcome::note_if(
        s.created + s.completed + s.deleted > 0,
        || {
            format!(
                "google tasks synced — {} completed, {} created, {} deleted",
                s.completed, s.created, s.deleted
            )
        },
    ))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::tasks::source_last_data(vault, SOURCE)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-tasks",
        name: "Google Tasks",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Open and completed Google Tasks into the same normalized task store as TickTick and Apple Reminders.",
        domain: "tasks",
        vault_path: "tasks/google-tasks/",
        toggleable: true,
        setup: &[],
        caveats: "Completed and hidden tasks are included.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(TASKS_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("google"),
    pull: Some(pull),
};

/// [`crate::registry::IntegrationDef::pull`] adapter:
/// [`Vault::google_tasks_pull`] mapped into the generic outcome shape.
/// Per-account failures freeze their tasks without aborting the rest (the
/// pass only errors when *every* account failed) — they land in
/// `.trove/google-tasks-sync.json` — so the headline re-reads the state to
/// surface them rather than reporting a clean sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.google_tasks_pull()?;
    let errors = vault
        .read_google_tasks_state()
        .accounts
        .values()
        .filter(|a| a.error.is_some())
        .count() as u64;
    let mut headline = if s.created + s.completed + s.deleted > 0 {
        format!(
            "{} completed, {} created, {} deleted — {} open tasks",
            s.completed, s.created, s.deleted, s.open
        )
    } else {
        format!("{} open tasks across {} lists", s.open, s.projects)
    };
    if errors > 0 {
        headline.push_str(&format!(
            " — {errors} account{} failed",
            if errors == 1 { "" } else { "s" }
        ));
    }
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("projects", s.projects as u64),
            ("open", s.open),
            ("created", s.created),
            ("completed", s.completed),
            ("deleted", s.deleted),
            ("account_errors", errors),
        ]),
    })
}

/// Per-account collector state, persisted in [`STATE_FILE`] keyed by Google
/// `sub`. Entries for disconnected accounts are deliberately *kept*: dropping
/// `history_seeded` would re-seed (and duplicate) completion events on a
/// reconnect.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AccountState {
    /// Display address (the map key is the `sub`).
    #[serde(default)]
    email: String,
    /// This account's visible completion history has been seeded into the
    /// event stream (first successful full pull).
    #[serde(default)]
    history_seeded: bool,
    /// Why this account's last pass failed, for debugging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The whole Google Tasks collector state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GoogleTasksState {
    /// RFC3339 local time of the last sync attempt.
    #[serde(default)]
    updated: String,
    accounts: BTreeMap<String, AccountState>,
}

/// One task list, as `tasklists.list` reports it.
#[derive(Debug, Clone)]
struct TaskList {
    id: String,
    title: String,
}

/// Thin Tasks API client. The base URL is injected so orchestration below it
/// stays testable without a network (the `gmail.rs` pattern).
struct GoogleTasksClient {
    base: String,
    token: String,
}

impl GoogleTasksClient {
    fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let mut req = ureq::get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HTTP_TIMEOUT);
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| anyhow!("parsing response: {e}")),
            Err(ureq::Error::Status(401, _)) => Err(anyhow!(
                "Google rejected the token (401) — reconnect from the Integrations tab"
            )),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(anyhow!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                ))
            }
            Err(e) => Err(anyhow!("{e}")),
        }
    }

    /// Every page of one paged collection's `items`.
    fn get_all(&self, path: &str, base_params: &[(&str, String)]) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut params = base_params.to_vec();
            params.push(("maxResults", PAGE_SIZE.to_string()));
            if let Some(t) = &page {
                params.push(("pageToken", t.clone()));
            }
            let v = self.get(path, &params)?;
            if let Some(items) = v.get("items").and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(t) => page = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    /// All of the account's task lists (`tasklists.list`).
    fn tasklists(&self) -> Result<Vec<TaskList>> {
        Ok(self
            .get_all("/tasks/v1/users/@me/lists", &[])?
            .into_iter()
            .filter_map(|it| {
                Some(TaskList {
                    id: it.get("id")?.as_str()?.to_string(),
                    title: it
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect())
    }

    /// Every task of one list, raw — completed, hidden (= cleared in
    /// first-party clients), and deleted ones included, so a disappeared
    /// task's fate is readable off this same pull.
    fn list_tasks(&self, list_id: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!("/tasks/v1/lists/{list_id}/tasks"),
            &[
                ("showCompleted", "true".to_string()),
                ("showHidden", "true".to_string()),
                ("showDeleted", "true".to_string()),
            ],
        )
    }
}

/// A Google instant ("2026-06-10T18:00:00.000Z") → RFC3339 local.
/// Unparseable values pass through verbatim rather than being dropped.
fn time_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// A Google `due` → RFC3339 local **midnight of the same calendar day**.
/// Google dues are date-only (the API "records date information; the time
/// portion of the timestamp is discarded" — always `T00:00:00.000Z`), so a
/// naive instant conversion would shift the day for any negative-UTC-offset
/// timezone. Keep the date, re-anchor it locally.
fn due_local(s: &str) -> String {
    s.get(..10)
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|dt| Local.from_local_datetime(&dt).earliest())
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| s.to_string())
}

/// A normalized task plus the one flag that decides which bucket of the pull
/// it lands in (deleted tasks never reach the snapshot or the done map).
struct NormTask {
    task: Task,
    deleted: bool,
}

/// A raw Tasks API task object → normalized [`Task`]. Mapped fields are
/// consumed; everything else (`etag`, `position`, `parent`, `hidden`,
/// `links`, `webViewLink`, …) survives in `extra`, joined by `google_id`
/// (the un-namespaced id), `account` (owner email), and `list_id`. Returns
/// None for objects without an id; Google allows empty titles, so a missing
/// title normalizes to `""` rather than dropping the task.
fn normalize_google(
    value: Value,
    sub: &str,
    email: &str,
    list: &TaskList,
    project: &str,
) -> Option<NormTask> {
    let Value::Object(mut obj) = value else {
        return None;
    };
    let raw_id = crate::tasks::take_str(&mut obj, "id")?;
    let title = crate::tasks::take_str(&mut obj, "title").unwrap_or_default();
    let status = match obj.remove("status") {
        Some(Value::String(s)) if s == "completed" => "done".to_string(),
        _ => "open".to_string(),
    };
    let deleted = obj
        .get("deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let due = crate::tasks::take_str(&mut obj, "due").map(|s| due_local(&s));
    let task = Task {
        source: SOURCE.into(),
        id: format!("{sub}:{raw_id}"),
        title,
        project: project.to_string(),
        notes: crate::tasks::take_str(&mut obj, "notes").unwrap_or_default(),
        status,
        // Google Tasks has no priority concept; 0 = none on the shared scale.
        priority: 0,
        // Date-only in Google — see `due_local`.
        all_day: due.is_some(),
        due,
        start: None,
        // No recurrence in the API: Google materializes each instance of a
        // repeating task as its own task with its own id.
        recurrence: None,
        tags: Vec::new(),
        // Children are first-class tasks here, not embedded lines — see the
        // module docs.
        subtasks: Vec::new(),
        // The API exposes no creation time, only `updated`.
        created: None,
        modified: crate::tasks::take_str(&mut obj, "updated").map(|s| time_local(&s)),
        completed: crate::tasks::take_str(&mut obj, "completed").map(|s| time_local(&s)),
        extra: {
            obj.insert("google_id".into(), Value::String(raw_id));
            obj.insert("account".into(), Value::String(email.to_string()));
            obj.insert("list_id".into(), Value::String(list.id.clone()));
            obj
        },
    };
    Some(NormTask { task, deleted })
}

/// One account's pull, normalized and bucketed. Pure data — built by
/// [`build_pull`] from raw API values, so tests drive the whole downstream
/// pipeline from JSON fixtures without a network.
#[derive(Default)]
struct AccountPull {
    projects: Vec<ProjectInfo>,
    /// Open tasks → snapshot candidates.
    open: Vec<Task>,
    /// Completed tasks visible this pull, by namespaced id — fate lookups
    /// read true completion times here; first-sync seeding reads the tasks.
    done: BTreeMap<String, Task>,
    /// Namespaced ids flagged `deleted: true`.
    deleted: HashSet<String>,
    /// Raw ids of lists whose `tasks.list` failed — their previously-known
    /// tasks must be carried forward, not mislogged as deleted.
    failed_lists: HashSet<String>,
}

/// Bucket one account's fetched lists. `suffix_account` disambiguates
/// project names (everyone's default list is "My Tasks") when more than one
/// account is connected. A `deleted` flag wins over a completed status — a
/// task deleted after completion is, today, deleted.
fn build_pull(
    sub: &str,
    email: &str,
    lists: Vec<(TaskList, Result<Vec<Value>>)>,
    suffix_account: bool,
) -> AccountPull {
    let mut pull = AccountPull::default();
    for (list, result) in lists {
        let project = if suffix_account {
            format!("{} ({email})", list.title)
        } else {
            list.title.clone()
        };
        pull.projects.push(ProjectInfo {
            id: format!("{sub}:{}", list.id),
            name: project.clone(),
        });
        let values = match result {
            Ok(v) => v,
            Err(e) => {
                eprintln!("trove tasks: google list {:?} failed: {e}", list.title);
                pull.failed_lists.insert(list.id.clone());
                continue;
            }
        };
        for norm in values
            .into_iter()
            .filter_map(|v| normalize_google(v, sub, email, &list, &project))
        {
            if norm.deleted {
                pull.deleted.insert(norm.task.id);
            } else if norm.task.status == "done" {
                pull.done.insert(norm.task.id.clone(), norm.task);
            } else {
                pull.open.push(norm.task);
            }
        }
    }
    pull
}

/// All connected accounts' pulls merged, ready for one diff pass over the
/// shared snapshot.
#[derive(Default)]
struct CombinedPull {
    projects: Vec<ProjectInfo>,
    open: Vec<Task>,
    done: BTreeMap<String, Task>,
    deleted: HashSet<String>,
    /// Currently-connected subs — a snapshot task whose sub isn't here
    /// belongs to a disconnected account and freezes (Unknown fate).
    live_subs: HashSet<String>,
    /// Connected subs whose pull failed this pass — their tasks carry
    /// forward too.
    failed_subs: HashSet<String>,
    /// `(sub, raw list id)` pairs whose task pull failed.
    failed_lists: HashSet<(String, String)>,
    /// First-sync completion-history seed events (true completion times).
    seed: Vec<TaskEvent>,
}

impl CombinedPull {
    /// Merge one successfully-pulled account in. `seed_history` = this is
    /// the account's first fully-successful pull, so its visible completed
    /// tasks become backdated `completed` events.
    fn add_account(&mut self, sub: &str, pull: AccountPull, seed_history: bool) {
        self.live_subs.insert(sub.to_string());
        for lid in pull.failed_lists {
            self.failed_lists.insert((sub.to_string(), lid));
        }
        if seed_history {
            self.seed.extend(pull.done.values().map(|t| TaskEvent {
                time: t
                    .completed
                    .clone()
                    .unwrap_or_else(|| Local::now().to_rfc3339()),
                kind: "completed".into(),
                task: t.clone(),
            }));
        }
        self.projects.extend(pull.projects);
        self.open.extend(pull.open);
        self.done.extend(pull.done);
        self.deleted.extend(pull.deleted);
    }

    /// Record an account whose pull failed entirely: still live (its tasks
    /// must not read as deleted), contributing no fresh data.
    fn add_failed_account(&mut self, sub: &str) {
        self.live_subs.insert(sub.to_string());
        self.failed_subs.insert(sub.to_string());
    }
}

/// The account (`sub`) prefix of a namespaced task id.
fn id_sub(id: &str) -> &str {
    id.split_once(':').map(|(s, _)| s).unwrap_or("")
}

impl Vault {
    /// One Google Tasks sync pass across every connected account: fetch all
    /// lists' tasks (completed/hidden/deleted included), seed first-sync
    /// completion history, then diff through the shared tasks engine into
    /// `tasks/google-tasks/`. A silent no-op when no Google account is
    /// connected; per-account and per-list failures freeze the affected
    /// tasks (Unknown fate) without aborting the rest. Errors only when
    /// *every* account failed.
    pub fn collect_google_tasks(&self) -> Result<TasksSyncStats> {
        let accounts = self.google_status()?.accounts;
        if accounts.is_empty() {
            return Ok(TasksSyncStats::default());
        }
        let mut state = self.read_google_tasks_state();
        let mut combined = CombinedPull::default();
        let mut newly_seeded: Vec<String> = Vec::new();
        let mut first_error: Option<String> = None;
        let multi = accounts.len() > 1;

        for acct in &accounts {
            let seeded = {
                let astate = state.accounts.entry(acct.sub.clone()).or_default();
                astate.email = acct.email.clone();
                astate.history_seeded
            };
            let mut fail = |state: &mut GoogleTasksState, msg: String| {
                first_error.get_or_insert(msg.clone());
                state.accounts.entry(acct.sub.clone()).or_default().error = Some(msg);
                combined.add_failed_account(&acct.sub);
            };
            // A flagged account can't refresh non-interactively; skip it
            // (the card surfaces the reconnect prompt).
            if acct.needs_reconnect {
                let msg = format!(
                    "{} needs to be reconnected from the Integrations tab",
                    acct.email
                );
                fail(&mut state, msg);
                continue;
            }
            let token = match crate::sync::google::fresh_token(self, &acct.sub) {
                Ok(t) => t.access_token,
                Err(e) => {
                    fail(&mut state, format!("{e:#}"));
                    continue;
                }
            };
            let client = GoogleTasksClient {
                base: TASKS_API.to_string(),
                token,
            };
            let lists = match client.tasklists() {
                Ok(l) => l,
                Err(e) => {
                    fail(&mut state, format!("google tasks {}: {e:#}", acct.email));
                    continue;
                }
            };
            let fetched: Vec<(TaskList, Result<Vec<Value>>)> = lists
                .into_iter()
                .map(|l| {
                    let r = client.list_tasks(&l.id);
                    (l, r)
                })
                .collect();
            let pull = build_pull(&acct.sub, &acct.email, fetched, multi);
            // Seed only off a *fully* successful pull — a partial one would
            // bake an incomplete history under the seeded flag.
            let seed_now = !seeded && pull.failed_lists.is_empty();
            if seed_now {
                newly_seeded.push(acct.sub.clone());
            }
            state.accounts.entry(acct.sub.clone()).or_default().error = None;
            combined.add_account(&acct.sub, pull, seed_now);
        }

        state.updated = Local::now().to_rfc3339();
        if combined.failed_subs.len() == accounts.len() {
            let msg =
                first_error.unwrap_or_else(|| "every Google account's pull failed".to_string());
            self.write_google_tasks_state(&state)?;
            self.record_tasks_sync(SOURCE, Some(msg.clone()))?;
            return Err(anyhow!(msg));
        }
        let stats = self.apply_google_tasks(combined)?;
        // Flags flip only after the seed events are on disk — a failure
        // above retries seeding next pass (the prev-snapshot filter in
        // `apply_google_tasks` absorbs the overlap with diff-emitted events).
        for sub in newly_seeded {
            state.accounts.entry(sub).or_default().history_seeded = true;
        }
        self.write_google_tasks_state(&state)?;
        Ok(stats)
    }

    /// Pull every connected account's Google Tasks into the vault — the
    /// manual "Sync now" path. The very same pass as the scheduled one
    /// ([`Self::collect_google_tasks`]), so there is exactly one writer of
    /// `tasks/google-tasks/`; only the no-account case differs (a clean
    /// error instead of a silent no-op). Blocking (network).
    pub fn google_tasks_pull(&self) -> Result<TasksSyncStats> {
        if self.google_status()?.accounts.is_empty() {
            bail!("no Google account is connected");
        }
        self.collect_google_tasks()
    }

    /// The network-free half of a pass: append seed history, then run the
    /// shared snapshot diff with fates read off the pull. Split from
    /// [`Self::collect_google_tasks`] so tests drive it from fixtures.
    fn apply_google_tasks(&self, pull: CombinedPull) -> Result<TasksSyncStats> {
        let CombinedPull {
            projects,
            open,
            done,
            deleted,
            live_subs,
            failed_subs,
            failed_lists,
            mut seed,
        } = pull;
        let mut seeded = 0u64;
        if !seed.is_empty() {
            // A task already in the snapshot gets its completion event from
            // the diff below — seeding it too would double-log it (this is
            // what makes a deferred first seed safe).
            let prev_ids: HashSet<String> = self
                .load_tasks_snapshot(SOURCE)?
                .into_iter()
                .map(|t| t.id)
                .collect();
            seed.retain(|e| !prev_ids.contains(&e.task.id));
            seeded = seed.len() as u64;
            self.append_task_events(SOURCE, &seed)?;
        }
        let mut stats = self.apply_tasks_sync(SOURCE, &projects, open, |t| {
            let sub = id_sub(&t.id);
            // Pull failures and disconnected accounts freeze, never guess.
            if failed_subs.contains(sub) || !live_subs.contains(sub) {
                return TaskFate::Unknown;
            }
            if let Some(lid) = t.extra.get("list_id").and_then(Value::as_str) {
                if failed_lists.contains(&(sub.to_string(), lid.to_string())) {
                    return TaskFate::Unknown;
                }
            }
            if deleted.contains(&t.id) {
                return TaskFate::Deleted;
            }
            if let Some(d) = done.get(&t.id) {
                return TaskFate::Completed(d.completed.clone());
            }
            // Absent under showCompleted+showHidden+showDeleted: Google
            // purged it, or its list was deleted.
            TaskFate::Deleted
        })?;
        stats.completed += seeded;
        Ok(stats)
    }

    /// The persisted collector state, or a fresh default.
    fn read_google_tasks_state(&self) -> GoogleTasksState {
        self.resolve(STATE_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|b| serde_json::from_str(&b).ok())
            .unwrap_or_default()
    }

    /// Atomic write so readers never see a torn file.
    fn write_google_tasks_state(&self, state: &GoogleTasksState) -> Result<()> {
        let path = self.resolve(STATE_FILE)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-gtasks-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn list(id: &str, title: &str) -> TaskList {
        TaskList {
            id: id.into(),
            title: title.into(),
        }
    }

    fn norm(json: &str) -> NormTask {
        normalize_google(
            serde_json::from_str(json).unwrap(),
            "sub1",
            "me@gmail.com",
            &list("l1", "My Tasks"),
            "My Tasks",
        )
        .unwrap()
    }

    /// A realistic open task off the live API.
    const OPEN: &str = r#"{"kind":"tasks#task","id":"dGFzazE","etag":"\"e1\"","title":"Buy milk","updated":"2026-06-10T18:00:00.000Z","selfLink":"https://www.googleapis.com/tasks/v1/lists/l1/tasks/dGFzazE","position":"00000000000000000001","notes":"the 2% kind","status":"needsAction","due":"2026-06-12T00:00:00.000Z","webViewLink":"https://tasks.google.com/task/dGFzazE","links":[]}"#;
    const COMPLETED: &str = r#"{"kind":"tasks#task","id":"dGFzazI","etag":"\"e2\"","title":"File taxes","updated":"2026-04-15T12:00:00.000Z","position":"00000000000000000002","status":"completed","completed":"2026-04-15T11:58:30.000Z","hidden":true}"#;
    const DELETED: &str = r#"{"kind":"tasks#task","id":"dGFzazM","etag":"\"e3\"","title":"Old idea","updated":"2026-06-01T09:00:00.000Z","position":"00000000000000000003","status":"needsAction","deleted":true}"#;
    const CHILD: &str = r#"{"kind":"tasks#task","id":"dGFzazQ","etag":"\"e4\"","title":"Buy bottle brush","updated":"2026-06-10T18:05:00.000Z","position":"00000000000000000001","status":"needsAction","parent":"dGFzazE"}"#;

    #[test]
    fn normalizes_open_task_with_account_namespacing() {
        let n = norm(OPEN);
        assert!(!n.deleted);
        let t = n.task;
        assert_eq!(t.source, "google-tasks");
        assert_eq!(t.id, "sub1:dGFzazE", "id is namespaced by the account sub");
        assert_eq!(t.title, "Buy milk");
        assert_eq!(t.project, "My Tasks");
        assert_eq!(t.notes, "the 2% kind");
        assert_eq!(t.status, "open");
        assert!(t.completed.is_none());
        // Date-only due: same calendar day at local midnight, all_day set.
        assert!(t.due.as_deref().unwrap().starts_with("2026-06-12T00:00:00"));
        assert!(t.all_day);
        // updated → modified, as a true instant.
        let modified = DateTime::parse_from_rfc3339(t.modified.as_deref().unwrap()).unwrap();
        assert_eq!(
            modified,
            DateTime::parse_from_rfc3339("2026-06-10T18:00:00Z").unwrap()
        );
        // Unmapped fields and the provenance trio survive in extra.
        assert_eq!(t.extra.get("google_id").and_then(Value::as_str), Some("dGFzazE"));
        assert_eq!(t.extra.get("account").and_then(Value::as_str), Some("me@gmail.com"));
        assert_eq!(t.extra.get("list_id").and_then(Value::as_str), Some("l1"));
        assert!(t.extra.contains_key("etag"));
        assert!(t.extra.contains_key("position"));
        assert!(t.extra.contains_key("webViewLink"));
    }

    #[test]
    fn normalizes_completed_task_with_true_time() {
        let t = norm(COMPLETED).task;
        assert_eq!(t.status, "done");
        let completed = DateTime::parse_from_rfc3339(t.completed.as_deref().unwrap()).unwrap();
        assert_eq!(
            completed,
            DateTime::parse_from_rfc3339("2026-04-15T11:58:30Z").unwrap()
        );
        // The hidden (cleared-in-UI) flag is fidelity, kept in extra.
        assert_eq!(t.extra.get("hidden").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn deleted_flag_is_detected() {
        let n = norm(DELETED);
        assert!(n.deleted);
        assert_eq!(n.task.id, "sub1:dGFzazM");
    }

    #[test]
    fn subtask_stays_standalone_with_parent_in_extra() {
        let t = norm(CHILD).task;
        assert_eq!(t.extra.get("parent").and_then(Value::as_str), Some("dGFzazE"));
        // It's a first-class task — never folded into the parent's subtasks.
        assert!(t.subtasks.is_empty());
    }

    #[test]
    fn build_pull_buckets_and_flags_failed_lists() {
        let lists = vec![
            (
                list("l1", "My Tasks"),
                Ok(vec![
                    serde_json::from_str(OPEN).unwrap(),
                    serde_json::from_str(COMPLETED).unwrap(),
                    serde_json::from_str(DELETED).unwrap(),
                    serde_json::from_str(CHILD).unwrap(),
                ]),
            ),
            (list("l2", "Errands"), Err(anyhow!("HTTP 500"))),
        ];
        let p = build_pull("sub1", "me@gmail.com", lists, false);
        assert_eq!(p.projects.len(), 2);
        assert_eq!(p.projects[0].id, "sub1:l1");
        let open_ids: Vec<&str> = p.open.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(open_ids, vec!["sub1:dGFzazE", "sub1:dGFzazQ"]);
        assert!(p.done.contains_key("sub1:dGFzazI"));
        assert!(p.deleted.contains("sub1:dGFzazM"));
        assert_eq!(p.failed_lists, HashSet::from(["l2".to_string()]));
    }

    #[test]
    fn multi_account_projects_get_email_suffix_and_ids_never_collide() {
        let mk = |sub: &str, email: &str| {
            build_pull(
                sub,
                email,
                vec![(list("l1", "My Tasks"), Ok(vec![serde_json::from_str(OPEN).unwrap()]))],
                true,
            )
        };
        let a = mk("sub1", "a@gmail.com");
        let b = mk("sub2", "b@gmail.com");
        assert_eq!(a.open[0].id, "sub1:dGFzazE");
        assert_eq!(b.open[0].id, "sub2:dGFzazE");
        assert_ne!(a.open[0].id, b.open[0].id, "same Google id, two accounts");
        assert_eq!(a.projects[0].name, "My Tasks (a@gmail.com)");
        assert_eq!(b.projects[0].name, "My Tasks (b@gmail.com)");
        assert_eq!(a.open[0].project, a.projects[0].name);
    }

    /// The full fixture-driven pass: first sync seeds visible completion
    /// history into its true month and snapshots the open tasks; the second
    /// sync logs a completion at its true time and a purge as a deletion.
    #[test]
    fn full_sync_pass_writes_snapshot_and_true_time_events() {
        let v = temp_vault("sync");
        let pull = build_pull(
            "sub1",
            "me@gmail.com",
            vec![(
                list("l1", "My Tasks"),
                Ok(vec![
                    serde_json::from_str(OPEN).unwrap(),
                    serde_json::from_str(CHILD).unwrap(),
                    serde_json::from_str(COMPLETED).unwrap(),
                ]),
            )],
            false,
        );
        let mut combined = CombinedPull::default();
        combined.add_account("sub1", pull, true);
        let stats = v.apply_google_tasks(combined).unwrap();
        assert_eq!(stats.open, 2);
        assert_eq!(stats.created, 2);
        assert_eq!(stats.completed, 1, "seeded history counts");

        // The seeded completion landed in its *true* month, true time.
        assert!(v.root().join("tasks/google-tasks/events/2026-04.jsonl").exists());
        let seeded = v.task_events("2026-04-01", "2026-04-30").unwrap();
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].kind, "completed");
        assert_eq!(seeded[0].task.title, "File taxes");
        assert_eq!(
            DateTime::parse_from_rfc3339(&seeded[0].time).unwrap(),
            DateTime::parse_from_rfc3339("2026-04-15T11:58:30Z").unwrap()
        );

        // Snapshot holds the two open tasks; sync state records the source.
        let snap = v.load_tasks_snapshot("google-tasks").unwrap();
        assert_eq!(snap.len(), 2);
        let sync = v.read_tasks_sync().unwrap();
        assert!(sync.sources["google-tasks"].error.is_none());
        assert!(!sync.sources["google-tasks"].updated.is_empty());
        // Human layer regenerated with the pretty source name.
        let index = fs::read_to_string(v.root().join("tasks/google-tasks/index.md")).unwrap();
        assert!(index.starts_with("# Google Tasks\n"));
        assert!(index.contains("| [My Tasks](my-tasks.md) | 2 |"));

        // Second pass: "Buy milk" now completed (with its true time), the
        // child task gone entirely (purged) → deletion.
        let done_milk = r#"{"id":"dGFzazE","title":"Buy milk","status":"completed","completed":"2026-06-11T09:30:00.000Z","updated":"2026-06-11T09:30:00.000Z"}"#;
        let pull2 = build_pull(
            "sub1",
            "me@gmail.com",
            vec![(
                list("l1", "My Tasks"),
                Ok(vec![
                    serde_json::from_str(done_milk).unwrap(),
                    serde_json::from_str(COMPLETED).unwrap(),
                ]),
            )],
            false,
        );
        let mut combined2 = CombinedPull::default();
        combined2.add_account("sub1", pull2, false);
        let stats2 = v.apply_google_tasks(combined2).unwrap();
        assert_eq!(stats2.open, 0);
        assert_eq!(stats2.completed, 1);
        assert_eq!(stats2.deleted, 1);

        // Completions carry Google's true (June fixture) time, but deletions
        // are stamped at sync time — the window must reach the real clock.
        let to = (chrono::Local::now() + chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let june = v.task_events("2026-06-01", &to).unwrap();
        let completed: Vec<_> = june.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].task.title, "Buy milk");
        assert_eq!(
            DateTime::parse_from_rfc3339(&completed[0].time).unwrap(),
            DateTime::parse_from_rfc3339("2026-06-11T09:30:00Z").unwrap(),
            "completion carries Google's real time, not sync time"
        );
        let deleted: Vec<_> = june.iter().filter(|e| e.kind == "deleted").collect();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].task.title, "Buy bottle brush");
        // The already-seeded April completion was not double-logged.
        assert_eq!(v.task_events("2026-04-01", "2026-04-30").unwrap().len(), 1);
    }

    /// A deferred seed (first full success after a partial pass) must not
    /// double-log tasks the diff already resolves.
    #[test]
    fn deferred_seed_never_duplicates_diffed_completions() {
        let v = temp_vault("seed-dedupe");
        // Pass 1, unseeded (simulating a partial pull): "Buy milk" open.
        let pull1 = build_pull(
            "sub1",
            "me@gmail.com",
            vec![(list("l1", "My Tasks"), Ok(vec![serde_json::from_str(OPEN).unwrap()]))],
            false,
        );
        let mut c1 = CombinedPull::default();
        c1.add_account("sub1", pull1, false);
        v.apply_google_tasks(c1).unwrap();

        // Pass 2: it completed; seeding fires now (first full success).
        let done_milk = r#"{"id":"dGFzazE","title":"Buy milk","status":"completed","completed":"2026-06-11T09:30:00.000Z"}"#;
        let pull2 = build_pull(
            "sub1",
            "me@gmail.com",
            vec![(list("l1", "My Tasks"), Ok(vec![serde_json::from_str(done_milk).unwrap()]))],
            false,
        );
        let mut c2 = CombinedPull::default();
        c2.add_account("sub1", pull2, true);
        let stats = v.apply_google_tasks(c2).unwrap();
        assert_eq!(stats.completed, 1, "diff event only — seed was filtered");
        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == "completed").count(),
            1,
            "exactly one completion despite seed + diff both seeing it"
        );
    }

    #[test]
    fn failed_account_and_failed_list_freeze_their_tasks() {
        let v = temp_vault("freeze");
        // Baseline: one task in each of two lists.
        let other = r#"{"id":"dGFzazk","title":"In the flaky list","status":"needsAction"}"#;
        let pull = build_pull(
            "sub1",
            "me@gmail.com",
            vec![
                (list("l1", "My Tasks"), Ok(vec![serde_json::from_str(OPEN).unwrap()])),
                (list("l2", "Errands"), Ok(vec![serde_json::from_str(other).unwrap()])),
            ],
            false,
        );
        let mut c = CombinedPull::default();
        c.add_account("sub1", pull, false);
        v.apply_google_tasks(c).unwrap();

        // Pass 2: l2's pull fails — its task must carry forward, no events.
        let pull2 = build_pull(
            "sub1",
            "me@gmail.com",
            vec![
                (list("l1", "My Tasks"), Ok(vec![serde_json::from_str(OPEN).unwrap()])),
                (list("l2", "Errands"), Err(anyhow!("HTTP 503"))),
            ],
            false,
        );
        let mut c2 = CombinedPull::default();
        c2.add_account("sub1", pull2, false);
        let stats = v.apply_google_tasks(c2).unwrap();
        assert_eq!(stats.open, 2, "flaky list's task carried forward");
        assert_eq!(stats.completed + stats.deleted, 0);

        // Pass 3: the whole account fails — everything carries forward.
        let mut c3 = CombinedPull::default();
        c3.add_failed_account("sub1");
        // (some other healthy account keeps the pass alive)
        c3.add_account("sub2", AccountPull::default(), false);
        let stats = v.apply_google_tasks(c3).unwrap();
        assert_eq!(stats.open, 2);
        assert_eq!(stats.completed + stats.deleted, 0);
    }

    #[test]
    fn disconnected_accounts_tasks_freeze_instead_of_reading_as_deleted() {
        let v = temp_vault("disconnect");
        let pull = build_pull(
            "gone",
            "old@gmail.com",
            vec![(list("l1", "My Tasks"), Ok(vec![serde_json::from_str(OPEN).unwrap()]))],
            false,
        );
        let mut c = CombinedPull::default();
        c.add_account("gone", pull, false);
        v.apply_google_tasks(c).unwrap();

        // Next pass: only a different account is connected.
        let mut c2 = CombinedPull::default();
        c2.add_account("sub2", AccountPull::default(), false);
        let stats = v.apply_google_tasks(c2).unwrap();
        assert_eq!(stats.deleted, 0, "no false deletions for the gone account");
        let snap = v.load_tasks_snapshot("google-tasks").unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "gone:dGFzazE");
    }

    #[test]
    fn no_account_is_a_silent_noop() {
        let v = temp_vault("noaccount");
        let stats = v.collect_google_tasks().unwrap();
        assert_eq!(stats.open + stats.created + stats.completed + stats.deleted, 0);
        assert!(!v.root().join("tasks/google-tasks").exists());
        assert!(!v.root().join(".trove/google-tasks-sync.json").exists());
    }

    #[test]
    fn pull_without_an_account_is_a_clean_error() {
        // Unlike the silent scheduled pass, a user-triggered pull must say
        // why nothing happened.
        let v = temp_vault("pull-noaccount");
        let err = pull(&v).unwrap_err();
        assert!(err.to_string().contains("no Google account"), "{err}");
    }

    #[test]
    fn state_round_trips_and_keeps_seeded_flags() {
        let v = temp_vault("state");
        let mut state = GoogleTasksState::default();
        state.updated = "2026-06-11T10:00:00-07:00".into();
        state.accounts.insert(
            "12345".into(),
            AccountState {
                email: "me@gmail.com".into(),
                history_seeded: true,
                error: None,
            },
        );
        v.write_google_tasks_state(&state).unwrap();
        let loaded = v.read_google_tasks_state();
        assert_eq!(loaded.updated, "2026-06-11T10:00:00-07:00");
        assert!(loaded.accounts["12345"].history_seeded);
        assert_eq!(loaded.accounts["12345"].email, "me@gmail.com");
    }

    #[test]
    fn due_stays_on_its_calendar_day() {
        // Google dues are date-only midnight-UTC stamps; a naive instant
        // conversion would land 2026-06-12 on June 11 anywhere west of UTC.
        let due = due_local("2026-06-12T00:00:00.000Z");
        assert!(due.starts_with("2026-06-12T00:00:00"), "got {due}");
        // Garbage passes through verbatim rather than being dropped.
        assert_eq!(due_local("not a date"), "not a date");
    }
}

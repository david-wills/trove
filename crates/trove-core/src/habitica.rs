//! Habitica — the gamified habit/daily/to-do tracker, pulled into the bound
//! [`crate::habits`] contract. Catalogued in the Phase 2 pass; brief:
//! docs/integrations/habitica.md. **First collector in the `habits` domain** —
//! this build binds the contract (see `crate::habits` / `crate::contracts`).
//!
//! A **Periodic** cloud pull over the v3 tasks endpoint, one account login:
//!
//! - `GET /api/v3/tasks/user?type=habits` and `?type=dailys` → the user's
//!   habit and daily *definitions*. Each becomes a [`crate::habits::Habit`] in
//!   the snapshot `habits/habitica/habits.jsonl` (`id` = the task `_id`; RPG
//!   progression / counters / frequency detail ride in `extra`), and each
//!   task's inline `history[]` array yields the per-day [`crate::habits::Checkin`]
//!   rows under `habits/habitica/checkins/YYYY-MM.jsonl`.
//!
//! Habitica returns task history inline (no separate history endpoint): a habit
//! history point is `{"date": <ms-epoch>, "value": <float>, "scoredUp": n,
//! "scoredDown": n}`; a daily history point adds `completed`/`isDue`. We map the
//! ms-epoch `date` to a `YYYY-MM-DD` (the contract key) and the score direction
//! / completion to the coarse `status` (`done`/`missed`/`skipped`). To-dos and
//! rewards are **not** habits — they belong to the ratified `tasks` contract and
//! are out of this collector's scope.
//!
//! **Compressed history is skipped, not guessed.** Habitica preens old history
//! server-side (`website/server/libs/preening.js`): for free accounts every
//! entry older than 60 days (365 for subscribers) is replaced by a monthly/
//! yearly **average** of shape `{date, value}` only — every flag dropped, the
//! `value` a fractional mean. Such an averaged point is not an observed
//! habit-day (the mean hides days that were missed), so [`checkin_from`] detects
//! it (no flags survive) and emits **no** check-in row for it — the contract day
//! is *absent*, per the [`crate::habits`] "don't invent a fate you can't
//! observe" rule. The full averaged point still lands verbatim in the raw layer.
//! This is the brief's "connect early" warning made structural: only points
//! Habitica still keeps at full fidelity become contract check-ins.
//!
//! **Day attribution is host-local (a documented v1 approximation).** The
//! ms-epoch `date` is bucketed to a `YYYY-MM-DD` in the **host machine's** local
//! timezone (see [`ms_to_local`]). Habitica's own day boundary is the account's
//! `preferences.dayStart` + `timezoneOffset`; a score near a custom day-rollover,
//! or a vault synced on a machine in a different timezone than the Habitica
//! account, can therefore shift a check-in's `date` (and its month partition) by
//! one calendar day vs Habitica's attribution. Acceptable for v1; a later pass
//! can read those prefs from `GET /api/v3/user` and bucket with the account
//! offset to match exactly.
//!
//! Two layers: the **raw** task object verbatim (with its full `history`) under
//! `habits/habitica/raw/YYYY-MM.jsonl` (full fidelity, unconditional), and the
//! normalized **contract** rows — a rewritten habit snapshot plus the deduped
//! append-only check-in stream.
//!
//! The check-in stream is the part that **can't** be backfilled (Habitica
//! averages and discards older history), so we drain every history point we can
//! still see and dedupe on `(source, habit, date)` against what's on disk. We
//! persist a watermark — the max history `date` (ms) already drained — in
//! `.trove/habitica-sync.json` (non-secret, rebuildable) and only advance it
//! after a full successful drain, so a crash re-drains rather than skips.
//!
//! Auth is a secret: the user pastes `userId:apiToken` (from Habitica
//! Settings → Site Data → API), split and sent as the `x-api-user` +
//! `x-api-key` headers, with the mandatory `x-client` app identifier on every
//! request. Stored under `.trove/sync/` (0600), verified at connect with a real
//! `GET /api/v3/user`, and never logged or written to the cursor.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::habits::{Checkin, Habit};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract-layer snapshot file; check-ins nest under `checkins/`, raw under
/// `raw/`. All paths live under the `habits/habitica/` domain root folder.
const SNAPSHOT_FILE: &str = "habits/habitica/habits.jsonl";
const CHECKINS_DIR: &str = "habits/habitica/checkins";
const RAW_DIR: &str = "habits/habitica/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that's for 0600
/// secrets). Deleting it just re-drains all still-visible history.
const SYNC_FILE: &str = ".trove/habitica-sync.json";

/// Service id under `.trove/sync/` where the pasted credential is stored. The
/// `userId:apiToken` pair rides a never-expiring [`TokenSet`] (token in
/// `access_token`, userId in `scope`).
const SERVICE: &str = "habitica";

const API_BASE: &str = "https://habitica.com";
/// Mandatory `x-client` header (a unique app identifier) — Habitica rejects
/// requests without it since late 2025. A stable Trove string; not a secret.
const X_CLIENT: &str = "trove-habitica";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between syncs in the watcher loop. Hourly: habit check-ins trickle
/// in daily, and the snapshot rewrite is cheap.
pub const HABITICA_SYNC_SECS: u64 = 3600;

/// `source`/folder name for every row this collector writes.
const SOURCE: &str = "habitica";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    // Newest check-in month, falling back to the snapshot's mtime-free presence.
    crate::registry::newest_stem(&vault.root().join(CHECKINS_DIR))
        .or_else(|| vault.root().join(SNAPSHOT_FILE).exists().then(|| "habits".to_string()))
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// a missing login or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
            let interesting = c("habits") > 0 || c("checkins") > 0;
            Ok(crate::registry::CollectOutcome::note_if(interesting, || {
                format!("habitica synced — {} habits, {} new check-ins", c("habits"), c("checkins"))
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("habitica sync skipped: {e}"))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Habitica synced — {} habits, {} new check-ins", c("habits"), c("checkins")),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "habitica",
        name: "Habitica",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pulls your Habitica habits and dailies (definitions + per-day check-in history) \
             into the unified habits store via the v3 API. Connect early — Habitica averages \
             or discards old history over time.",
        domain: "habits",
        vault_path: "habits/habitica/",
        toggleable: true,
        setup: &[
            "Connect with your Habitica User ID and API Token on this card.",
            "First sync captures every habit and all history Habitica still keeps; later syncs append new days.",
        ],
        caveats: "Historical habit scores are averaged or discarded by Habitica over time, so \
                  connecting early preserves the most check-in detail. To-dos and rewards are not \
                  habits — they're out of this collector's scope.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(HABITICA_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("habitica"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = "userId:apiToken", a SECRET composite).

/// Split a pasted `userId:apiToken` into its two parts (both trimmed). The
/// token itself can't contain a colon (it's a UUID), so split once on the first
/// `:`. Returns `None` when either half is empty.
fn split_credential(pasted: &str) -> Option<(String, String)> {
    let (user, token) = pasted.trim().split_once(':')?;
    let (user, token) = (user.trim(), token.trim());
    (!user.is_empty() && !token.is_empty()).then(|| (user.to_string(), token.to_string()))
}

/// Verify the pasted credential with `GET /api/v3/user` (a 200 on success),
/// then store it (0600). A 401 bails with a clear message; the credential is
/// never logged.
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let Some((user_id, token)) = split_credential(pasted) else {
        bail!(
            "paste your Habitica credential as USER_ID:API_TOKEN — both come from \
             Habitica Settings → Site Data → API"
        );
    };
    let client = HabiticaClient::new(API_BASE.to_string(), user_id.clone(), token.clone());
    match client.verify() {
        Ok(()) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Habitica rejected the credential (401) — copy your User ID and API Token fresh \
             from Habitica Settings → Site Data → API"
        ),
        Err(e) => bail!("Habitica auth check failed: {e}"),
    }
    // The credential goes ONLY through the secret store (0600). Never the cursor.
    // Token in `access_token`, the (non-secret) userId in `scope` so it pairs.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token,
            refresh_token: None,
            token_type: Some("habitica".into()),
            scope: Some(user_id),
            expires_at: None,
        },
    )
}

/// Forget the stored credential. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = the credential is stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Habitica".to_string(),
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // the API token doesn't expire
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // No bring-your-own-app step: the User ID + API Token are self-service.
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste the
/// `userId:apiToken` composite. No app registration / OAuth — the keys are
/// found in-app and sent as `x-api-user` + `x-api-key`.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "habitica",
    display_name: "Habitica",
    methods: &[ConnectMethod::TokenPaste {
        label: "Habitica User ID : API Token",
        help: "In Habitica, open Settings → Site Data → API. Copy your User ID and API Token and \
               paste them here joined by a colon, as USER_ID:API_TOKEN. Stored locally, never sent \
               anywhere but Habitica.",
        placeholder: "b0413351-405f-…:7f3e0a2b-…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["habitica"],
    setup: &[
        "Open Habitica → Settings → Site Data → API while signed in.",
        "Copy your User ID and your API Token.",
        "Paste them here joined by a colon (USER_ID:API_TOKEN) — stored locally only.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors: 401 wants a distinct connect/skip message, 429 is
/// transient, everything else is a message.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The endpoint the pull needs. A trait so tests drive the mapping/persist
/// logic with fixtures, never the network.
trait HabiticaApi {
    /// `GET /api/v3/tasks/user?type=<task_type>` → the `data` array of task
    /// objects (each with its inline `history`). `task_type` is `habits` or
    /// `dailys`.
    fn tasks(&self, task_type: &str) -> Result<Vec<Value>, FetchError>;
}

/// Thin client; base URL injected (the readwise/github/lastfm pattern).
struct HabiticaClient {
    base: String,
    user_id: String,
    token: String,
}

impl HabiticaClient {
    fn new(base: String, user_id: String, token: String) -> Self {
        HabiticaClient { base, user_id, token }
    }

    /// Apply the three mandatory auth/identity headers to a request.
    fn auth(&self, req: ureq::Request) -> ureq::Request {
        req.set("x-api-user", &self.user_id)
            .set("x-api-key", &self.token)
            .set("x-client", X_CLIENT)
    }

    /// `GET /api/v3/user` → 200 when the credential is valid. Used at connect.
    /// Requests a single tiny field to keep the response small.
    fn verify(&self) -> Result<(), FetchError> {
        let url = format!("{}/api/v3/user", self.base);
        let req = self
            .auth(ureq::get(&url).timeout(HTTP_TIMEOUT))
            .query("userFields", "_id");
        match req.call() {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!("HTTP {code}: {}", body.chars().take(200).collect::<String>())))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl HabiticaApi for HabiticaClient {
    fn tasks(&self, task_type: &str) -> Result<Vec<Value>, FetchError> {
        let url = format!("{}/api/v3/tasks/user", self.base);
        let req = self
            .auth(ureq::get(&url).timeout(HTTP_TIMEOUT))
            .query("type", task_type);
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok(parse_tasks(v))
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!("HTTP {code}: {}", body.chars().take(300).collect::<String>())))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

/// Pull the `data` array out of a Habitica `{"success":true,"data":[…]}`
/// envelope. A bare array is tolerated; anything else yields no tasks.
fn parse_tasks(v: Value) -> Vec<Value> {
    match v {
        Value::Object(o) => o.get("data").and_then(Value::as_array).cloned().unwrap_or_default(),
        Value::Array(a) => a,
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max history `date` (ms epoch) already drained into the check-in stream —
    /// the lower bound for the next drain. New history points have a strictly
    /// larger `date`. `None` on a first sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkins_after_ms: Option<i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_habitica_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_habitica_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity task object). The on-disk line is the verbatim
// task object (flattened — no synthetic columns), tagged with a ts purely so
// the month-partition writer files it under the right month. Only `value` is
// serialized.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// A top-level string field, trimmed; "" when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// An RFC3339-ish timestamp → RFC3339 local. Unparseable/empty values pass
/// through verbatim rather than being dropped (the github/todoist idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// A ms-epoch history `date` → (local `YYYY-MM-DD`, full local RFC3339). The
/// history `date` is the only time a check-in carries; the contract keys on the
/// calendar day. Bucketed in the **host machine's** local timezone — a v1
/// approximation of Habitica's account `dayStart`/`timezoneOffset` day boundary
/// (see the module doc's "Day attribution" note). `None` when out of range.
fn ms_to_local(ms: i64) -> Option<(String, String)> {
    let local = DateTime::from_timestamp_millis(ms)?.with_timezone(&Local);
    Some((local.format("%Y-%m-%d").to_string(), local.to_rfc3339()))
}

/// A task's stable id: prefer `_id` (the Mongo id Habitica returns), fall back
/// to `id`. `None` when neither is a non-empty string (can't key the snapshot
/// or attribute a check-in).
fn task_id(t: &Value) -> Option<String> {
    let id = str_field(t, "_id");
    let id = if id.is_empty() { str_field(t, "id") } else { id };
    (!id.is_empty()).then_some(id)
}

/// Render a daily's `repeat` map (`{"m":true,"t":false,…}`) as a stable,
/// source-verbatim weekday list (`"m,w,f"`) for the `schedule` field. Empty
/// when nothing is set. Keys are emitted in week order, not map order.
fn repeat_schedule(repeat: &Value) -> String {
    const DAYS: [&str; 7] = ["su", "m", "t", "w", "th", "f", "s"];
    let Some(map) = repeat.as_object() else { return String::new() };
    DAYS.iter()
        .filter(|d| map.get(**d).and_then(Value::as_bool) == Some(true))
        .copied()
        .collect::<Vec<_>>()
        .join(",")
}

/// One Habitica task (habit or daily) → a contract [`Habit`]. `None` when the
/// task has no usable id. RPG / counter / frequency detail that the normalized
/// columns don't carry rides in `extra` (full fidelity stays in the raw layer).
fn habit_from(t: &Value) -> Option<Habit> {
    let id = task_id(t)?;
    let ttype = str_field(t, "type"); // "habit" | "daily"

    // schedule: a daily's repeat-map becomes a weekday list; otherwise the raw
    // `frequency` word ("daily"/"weekly"/"monthly") — never interpreted.
    let schedule = {
        let repeat = repeat_schedule(t.get("repeat").unwrap_or(&Value::Null));
        if !repeat.is_empty() {
            repeat
        } else {
            str_field(t, "frequency")
        }
    };

    let mut extra = Map::new();
    extra.insert("type".into(), Value::String(ttype.clone()));
    // Counters/direction (habits) and frequency/cadence detail (dailies) are
    // the source's own numbers, kept for provenance, never a contract column.
    for key in ["frequency", "attribute"] {
        let val = str_field(t, key);
        if !val.is_empty() {
            extra.insert(key.into(), Value::String(val));
        }
    }
    for key in ["counterUp", "counterDown", "streak", "everyX", "priority", "value"] {
        if let Some(n) = t.get(key).and_then(Value::as_f64) {
            if let Some(num) = serde_json::Number::from_f64(n) {
                extra.insert(key.into(), Value::Number(num));
            }
        }
    }
    for key in ["up", "down", "completed", "isDue"] {
        if let Some(b) = t.get(key).and_then(Value::as_bool) {
            extra.insert(key.into(), Value::Bool(b));
        }
    }
    // Tags (UUIDs) and the source's own _id mirror — useful for cross-ref.
    if let Some(tags) = t.get("tags").and_then(Value::as_array) {
        if !tags.is_empty() {
            extra.insert("tags".into(), Value::Array(tags.clone()));
        }
    }

    Some(Habit {
        source: SOURCE.into(),
        id,
        title: str_field(t, "text"),
        schedule,
        // Habitica habits/dailies aren't measurable in the goal+unit sense
        // (they're yes/no or +/- scored), so no goal/unit; a future measurable
        // source fills these.
        goal: None,
        unit: String::new(),
        // Habitica has no per-habit display color (color is derived from value);
        // leave empty.
        color: String::new(),
        // A Habitica habit is never "archived" in the API sense we model;
        // leave unset rather than guess.
        archived: None,
        created: {
            let c = str_field(t, "createdAt");
            if c.is_empty() { String::new() } else { to_local(&c) }
        },
        modified: {
            let m = str_field(t, "updatedAt");
            if m.is_empty() { String::new() } else { to_local(&m) }
        },
        extra,
    })
}

/// One history point of a task → a contract [`Checkin`]. `task_id` attributes
/// it; `is_daily` selects the status vocabulary. `None` when the point has no
/// usable `date`, **or when it's a compressed aggregate** (see below).
///
/// **Compression awareness (the load-bearing rule).** Habitica preens old
/// history server-side: for free accounts every entry older than 60 days
/// (365 for subscribers) is replaced by a *monthly/yearly average* of shape
/// `{date, value}` only — `value` is the arithmetic mean of the bucket, with
/// `completed`/`isDue`/`scoredUp`/`scoredDown` all dropped (Habitica server
/// `website/server/libs/preening.js` `_aggregate`). That averaged point is
/// **not** an observed habit-day: a fractional mean encodes days that were
/// actually missed, so asserting `done` (or any status) for it would "invent a
/// fate we can't observe" (the [`crate::habits`] hard rule — an unrecorded day
/// is *absent*, not a guessed status, and there is no `unknown` status value).
/// A raw, un-compressed point always carries its flags: a habit score push
/// always sets both `scoredUp` and `scoredDown` (one of them to 1), and a daily
/// history push (manual completion or cron) always carries `completed` and
/// `isDue` (Habitica `scoreTask`/`cron`). So a point that has **neither**
/// score flag (habit) / **neither** `completed` nor `isDue` (daily) is a
/// compressed aggregate — we drop it from the contract check-in stream (it
/// survives verbatim, mean and all, in the raw layer) and return `None`.
///
/// Status mapping for the *observed* (un-compressed) points (the coarse
/// normalization; the raw flags ride in `extra`):
/// - **daily**: `completed:true` → `done`; `completed:false` + `isDue:true` →
///   `missed`; `isDue:false` → `skipped` (not due that day = excused).
/// - **habit**: a `+` press (`scoredUp >= 1`) → `done`; a `-` press
///   (`scoredDown >= 1`, no `+`) → `missed`. A habit `-` press is an *observed*
///   negative scoring event ("did the bad thing" / failed the good habit), so
///   it's a real outcome, not an absence — mapped to `missed` deliberately (the
///   coarsest fit in the three-value enum; the exact `scoredDown` ride in
///   `extra`). A point with both flags 0 cannot occur for a raw habit push, so
///   it is treated as compressed above.
fn checkin_from(point: &Value, task_id: &str, is_daily: bool) -> Option<Checkin> {
    let ms = point.get("date").and_then(Value::as_i64).or_else(|| {
        // Tolerate a float ms or an RFC3339 string date defensively.
        point.get("date").and_then(Value::as_f64).map(|f| f as i64)
    })?;
    let (date, ts) = ms_to_local(ms)?;

    // Detect a preened (averaged) point by the ABSENCE of the flags every raw
    // point carries — a compressed entry is `{date, value}` only. Key presence,
    // not value: this drops the aggregate before any status is fabricated.
    let has_score_flag = point.get("scoredUp").is_some() || point.get("scoredDown").is_some();
    let has_daily_flag = point.get("completed").is_some() || point.get("isDue").is_some();
    let compressed = if is_daily { !has_daily_flag } else { !has_score_flag };
    if compressed {
        // A monthly/yearly average is not an observed habit-day: emit no check-in
        // (the day is ABSENT in the contract; full fidelity stays in raw/).
        return None;
    }

    let scored_up = point.get("scoredUp").and_then(Value::as_i64).unwrap_or(0);
    let scored_down = point.get("scoredDown").and_then(Value::as_i64).unwrap_or(0);
    let completed = point.get("completed").and_then(Value::as_bool);
    let is_due = point.get("isDue").and_then(Value::as_bool);

    let status = if is_daily {
        match completed {
            Some(true) => "done",
            Some(false) => match is_due {
                Some(false) => "skipped", // not due that day — deliberately excused
                _ => "missed",            // due and not completed — an observed miss
            },
            // A daily point with `isDue` but no `completed` shouldn't occur (raw
            // pushes carry both); treat the due-flag presence as the signal and
            // fall back to the isDue arm. Reaching here means a flag was present
            // (else `compressed` returned None), so this is an observed day.
            None => match is_due {
                Some(false) => "skipped",
                _ => "missed",
            },
        }
    } else {
        // Habit: score direction. scoredDown with no scoredUp → a "-" → missed
        // (an observed negative scoring event, not an absence). Otherwise a "+".
        if scored_down > 0 && scored_up == 0 {
            "missed"
        } else {
            "done"
        }
    };

    let mut extra = Map::new();
    if let Some(n) = point.get("value").and_then(Value::as_f64) {
        if let Some(num) = serde_json::Number::from_f64(n) {
            extra.insert("value".into(), Value::Number(num)); // the running task value at that point
        }
    }
    for (k, val) in [("scoredUp", scored_up), ("scoredDown", scored_down)] {
        if val > 0 {
            extra.insert(k.into(), Value::from(val));
        }
    }
    if let Some(c) = completed {
        extra.insert("completed".into(), Value::Bool(c));
    }
    if let Some(d) = is_due {
        extra.insert("isDue".into(), Value::Bool(d));
    }
    extra.insert("stamp_ms".into(), Value::from(ms));

    Some(Checkin {
        date,
        source: SOURCE.into(),
        habit: task_id.to_string(),
        status: status.into(),
        ts,
        // The contract `value` is the per-day *logged amount* for a measurable
        // habit; Habitica habits/dailies aren't measurable that way (the
        // `value` here is the RPG running score, kept in extra), so leave unset.
        value: None,
        note: String::new(),
        // History points carry no native id; synthesize a stable one so a row
        // is traceable, while the contract dedupe key stays (source,habit,date).
        guid: format!("{task_id}:{ms}"),
        extra,
    })
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the credential and sync. Missing login ⇒ a quiet skip on the
/// periodic path (mirror readwise/todoist/lastfm), a clear error on the manual
/// path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let tok = vault
        .load_sync_token(SERVICE)?
        .filter(|t| !t.access_token.trim().is_empty())
        .context("Habitica is not connected — add your User ID and API Token in the Integrations tab")?;
    let user_id = tok
        .scope
        .clone()
        .filter(|s| !s.trim().is_empty())
        .context("Habitica login is missing its User ID — reconnect from the Integrations tab")?;
    let client = HabiticaClient::new(API_BASE.to_string(), user_id, tok.access_token.clone());
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl HabiticaApi) -> Result<PullOutcome> {
    let mut state = vault.read_habitica_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- Fetch both task types; tag each with whether it's a daily. --------
    // A fetch failure aborts BEFORE any write so the watermark never advances
    // on a partial pull (a crash re-drains).
    let mut tasks: Vec<(Value, bool)> = Vec::new();
    for (ttype, is_daily) in [("habits", false), ("dailys", true)] {
        let batch = api.tasks(ttype).map_err(|e| fetch_err(ttype, e))?;
        tasks.extend(batch.into_iter().map(|t| (t, is_daily)));
    }

    // --- Snapshot: every habit/daily definition, rewritten whole. ----------
    let habit_rows: Vec<Habit> = tasks.iter().filter_map(|(t, _)| habit_from(t)).collect();
    vault.write_snapshot(SNAPSHOT_FILE, &habit_rows)?;
    counts.insert("habits", habit_rows.len() as u64);

    // --- Raw: the full task objects (with history), partitioned by updatedAt
    // month (falling back to createdAt, then now), full fidelity, append-only
    // & deduped by the task id so a re-sync doesn't pile up duplicate raw rows.
    write_raw(vault, &tasks)?;

    // --- Check-ins: drain every history point newer than the watermark. ----
    let after = state.checkins_after_ms;
    let mut new_checkins: Vec<Checkin> = Vec::new();
    let mut drained_max: Option<i64> = after;
    for (t, is_daily) in &tasks {
        let Some(id) = task_id(t) else { continue };
        let Some(history) = t.get("history").and_then(Value::as_array) else { continue };
        for point in history {
            let Some(ms) = point.get("date").and_then(Value::as_i64) else { continue };
            // Strictly-after the watermark; the rest is caught by the on-disk
            // (source,habit,date) dedupe below (a same-day re-score updates in
            // place is a read-time concern — we append once per day).
            if after.is_some_and(|w| ms <= w) {
                continue;
            }
            if let Some(c) = checkin_from(point, &id, *is_daily) {
                new_checkins.push(c);
            }
            drained_max = Some(drained_max.map_or(ms, |cur| cur.max(ms)));
        }
    }
    let written = append_checkins(vault, new_checkins)?;
    counts.insert("checkins", written);

    // Advance the watermark only after the full drain, and only forward — but
    // only when at least one task carried history (an empty/again pull must not
    // reset it). `drained_max` started at the prior watermark, so it never
    // regresses.
    if let Some(w) = drained_max {
        state.checkins_after_ms = Some(state.checkins_after_ms.map_or(w, |cur| cur.max(w)));
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_habitica_sync(&state)?;

    let h = counts.get("habits").copied().unwrap_or(0);
    let c = counts.get("checkins").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("{h} habits, {c} new check-ins"),
        counts,
    })
}

/// Append the full task objects to the raw layer, deduped by task id against
/// what's already stored, partitioned by the month of `updatedAt` (then
/// `createdAt`, then now). Re-running never piles up duplicate raw rows.
fn write_raw(vault: &Vault, tasks: &[(Value, bool)]) -> Result<()> {
    let raw = vault.stream(RAW_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in raw.partitions()? {
        for v in raw.read::<Value>(&key)? {
            if let Some(id) = task_id(&v) {
                seen.insert(id);
            }
        }
    }
    let now = Local::now().to_rfc3339();
    let mut rows: Vec<RawLine> = Vec::new();
    for (t, _) in tasks {
        let Some(id) = task_id(t) else { continue };
        if !seen.insert(id) {
            continue; // already archived
        }
        let when = {
            let u = str_field(t, "updatedAt");
            let c = str_field(t, "createdAt");
            let raw_ts = if !u.is_empty() {
                to_local(&u)
            } else if !c.is_empty() {
                to_local(&c)
            } else {
                now.clone()
            };
            // Must yield a month partition; otherwise file under `now`.
            if Partition::Month.key(&raw_ts).is_some() { raw_ts } else { now.clone() }
        };
        rows.push(RawLine { ts: when, value: t.clone() });
    }
    raw.append(&rows, |r| &r.ts)
}

/// Append new check-in rows, deduped on `(source, habit, date)` against every
/// check-in already on disk (the contract dedupe key — re-runnable: a re-pull
/// of an overlapping window never duplicates a habit-day). Returns the count
/// written.
fn append_checkins(vault: &Vault, rows: Vec<Checkin>) -> Result<u64> {
    let stream = vault.stream(CHECKINS_DIR, Partition::Month);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            let habit = str_field(&v, "habit");
            let date = str_field(&v, "date");
            if !habit.is_empty() && !date.is_empty() {
                seen.insert((habit, date));
            }
        }
    }
    let mut fresh: Vec<Checkin> = Vec::new();
    for c in rows {
        if seen.insert((c.habit.clone(), c.date.clone())) {
            fresh.push(c);
        }
    }
    stream.append(&fresh, |c| &c.date)?;
    Ok(fresh.len() as u64)
}

/// Map a [`FetchError`] at the top of the pull into an anyhow error with a
/// clear reconnect message for 401.
fn fetch_err(what: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Habitica rejected the credential (401) fetching {what} — reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Habitica rate limited the {what} fetch (429) — it'll retry on the next sync"
        ),
        other => anyhow::anyhow!("Habitica {what} fetch failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-habitica-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (the documented v3 /tasks/user shapes) -----------------

    /// A habit task with a two-point history — the documented minimal history
    /// shape (`{date, value}`) plus a richer point with score directions.
    /// Modeled on the apidoc GET /tasks/user + GET /tasks/challenge examples.
    fn habit_task() -> Value {
        json!({
            "_id": "2b774d70-ec8b-4f6b-9c3a-1f0e2d3c4b5a",
            "userId": "b0413351-405f-416f-8787-947ec1c85199",
            "text": "Drink water",
            "type": "habit",
            "notes": "stay hydrated",
            "tags": ["8a9d461b-f5eb-4a16-97d3-c03380c422a3"],
            "value": 12.5,
            "priority": 1.5,
            "attribute": "con",
            "up": true,
            "down": true,
            "counterUp": 3,
            "counterDown": 0,
            "frequency": "daily",
            "challenge": {},
            "createdAt": "2026-01-04T08:00:00.000Z",
            "updatedAt": "2026-06-10T14:00:00.000Z",
            "history": [
                {"date": 1748563200000i64, "value": 11.0, "scoredUp": 1, "scoredDown": 0},
                {"date": 1749600000000i64, "value": 12.5, "scoredUp": 1, "scoredDown": 0}
            ],
            "id": "2b774d70-ec8b-4f6b-9c3a-1f0e2d3c4b5a"
        })
    }

    /// A daily task with a repeat map (M/W/F), a streak, and a daily history
    /// where one day was completed and one was due-but-missed.
    fn daily_task() -> Value {
        json!({
            "_id": "a1f4c0de-2b77-4f3e-9c11-8d6e0f5a2b34",
            "userId": "b0413351-405f-416f-8787-947ec1c85199",
            "text": "Meditate",
            "type": "daily",
            "notes": "",
            "tags": [],
            "value": 5.0,
            "priority": 1,
            "attribute": "int",
            "frequency": "weekly",
            "everyX": 1,
            "startDate": "2026-01-01T00:00:00.000Z",
            "repeat": {"su": false, "m": true, "t": false, "w": true, "th": false, "f": true, "s": false},
            "streak": 4,
            "completed": false,
            "isDue": true,
            "checklist": [],
            "createdAt": "2026-01-01T09:00:00.000Z",
            "updatedAt": "2026-06-10T07:30:00.000Z",
            "history": [
                {"date": 1748563200000i64, "value": 6.0, "completed": true, "isDue": true},
                {"date": 1749600000000i64, "value": 5.0, "completed": false, "isDue": true}
            ],
            "id": "a1f4c0de-2b77-4f3e-9c11-8d6e0f5a2b34"
        })
    }

    // --- pure mapping tests ----------------------------------------------

    #[test]
    fn parse_tasks_reads_data_envelope_and_bare_array() {
        let env = parse_tasks(json!({"success": true, "data": [{"_id": "a"}, {"_id": "b"}], "notifications": []}));
        assert_eq!(env.len(), 2);
        let bare = parse_tasks(json!([{"_id": "x"}]));
        assert_eq!(bare.len(), 1);
        assert!(parse_tasks(json!({"success": false})).is_empty());
        assert!(parse_tasks(json!(7)).is_empty());
    }

    #[test]
    fn maps_habit_definition_with_provenance_in_extra() {
        let h = habit_from(&habit_task()).unwrap();
        assert_eq!(h.source, "habitica");
        assert_eq!(h.id, "2b774d70-ec8b-4f6b-9c3a-1f0e2d3c4b5a", "id is the task _id (stable)");
        assert_eq!(h.title, "Drink water");
        assert_eq!(h.schedule, "daily", "habit frequency word, verbatim");
        assert!(h.goal.is_none() && h.unit.is_empty(), "Habitica habits aren't goal+unit measurable");
        assert!(h.archived.is_none(), "no archived guess");
        // created/modified converted to local, same instant as the source.
        assert_eq!(
            DateTime::parse_from_rfc3339(&h.created).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-01-04T08:00:00.000Z").unwrap().timestamp(),
        );
        // RPG/counter provenance rides in extra (the source's own numbers).
        assert_eq!(h.extra.get("type"), Some(&json!("habit")));
        assert_eq!(h.extra.get("counterUp"), Some(&json!(3.0)));
        assert_eq!(h.extra.get("value"), Some(&json!(12.5)));
        assert_eq!(h.extra.get("up"), Some(&json!(true)));
        assert_eq!(h.extra.get("attribute"), Some(&json!("con")));
        assert!(h.extra.get("tags").is_some(), "tags preserved");
    }

    #[test]
    fn maps_daily_repeat_map_to_weekday_schedule() {
        let h = habit_from(&daily_task()).unwrap();
        assert_eq!(h.schedule, "m,w,f", "repeat map → week-ordered weekday list");
        assert_eq!(h.extra.get("type"), Some(&json!("daily")));
        assert_eq!(h.extra.get("streak"), Some(&json!(4.0)));
        assert_eq!(h.extra.get("isDue"), Some(&json!(true)));
        assert_eq!(h.extra.get("completed"), Some(&json!(false)));
    }

    #[test]
    fn habit_history_point_maps_to_done_checkin_with_local_date() {
        let t = habit_task();
        let point = &t["history"][0];
        let c = checkin_from(point, "task-1", false).unwrap();
        assert_eq!(c.source, "habitica");
        assert_eq!(c.habit, "task-1");
        assert_eq!(c.status, "done", "a habit score event is a 'done'");
        // date is the LOCAL calendar day of the ms-epoch stamp.
        let want = DateTime::from_timestamp_millis(1748563200000).unwrap().with_timezone(&Local);
        assert_eq!(c.date, want.format("%Y-%m-%d").to_string());
        // ts carries the same instant, in local tz.
        assert_eq!(
            DateTime::parse_from_rfc3339(&c.ts).unwrap().timestamp_millis(),
            1748563200000,
        );
        assert!(c.value.is_none(), "RPG running value is not a measurable per-day amount");
        assert_eq!(c.guid, "task-1:1748563200000", "stable synthesized id");
        assert_eq!(c.extra.get("value"), Some(&json!(11.0)), "running value preserved in extra");
        assert_eq!(c.extra.get("scoredUp"), Some(&json!(1)));
        assert_eq!(c.extra.get("stamp_ms"), Some(&json!(1748563200000i64)));
    }

    #[test]
    fn habit_minus_press_is_missed() {
        let down = json!({"date": 1749600000000i64, "value": -2.0, "scoredUp": 0, "scoredDown": 1});
        let c = checkin_from(&down, "task-1", false).unwrap();
        assert_eq!(c.status, "missed", "a '-' press with no '+' is a missed");
        assert_eq!(c.extra.get("scoredDown"), Some(&json!(1)));
        assert!(c.extra.get("scoredUp").is_none(), "zero scoredUp omitted");
    }

    #[test]
    fn compressed_habit_point_is_skipped_not_guessed() {
        // A bare {date, value} habit point (no scoredUp/scoredDown keys) is a
        // PREENED monthly/yearly AVERAGE (Habitica preening.js _aggregate), not
        // an observed scoring event — `value` is a fractional mean that hides
        // missed days. The contract forbids inventing a status, so no check-in
        // is emitted (the day is ABSENT; full fidelity stays in the raw layer).
        let bare = json!({"date": 1748563200000i64, "value": 18.53});
        assert!(
            checkin_from(&bare, "task-1", false).is_none(),
            "an averaged habit point must not become a fabricated 'done'",
        );
        // A real raw point — even a pure '+' with scoredDown:0 present — still
        // maps (key presence, not value, distinguishes raw from compressed).
        let raw_plus = json!({"date": 1748563200000i64, "value": 18.53, "scoredUp": 1, "scoredDown": 0});
        assert_eq!(checkin_from(&raw_plus, "task-1", false).unwrap().status, "done");
    }

    #[test]
    fn daily_history_status_vocabulary() {
        let completed = json!({"date": 1748563200000i64, "value": 6.0, "completed": true, "isDue": true});
        assert_eq!(checkin_from(&completed, "d1", true).unwrap().status, "done");
        let missed = json!({"date": 1749600000000i64, "value": 5.0, "completed": false, "isDue": true});
        assert_eq!(checkin_from(&missed, "d1", true).unwrap().status, "missed");
        let excused = json!({"date": 1749700000000i64, "value": 5.0, "completed": false, "isDue": false});
        assert_eq!(checkin_from(&excused, "d1", true).unwrap().status, "skipped", "not due = excused");
        // A daily point with ONE flag present is still observed (raw pushes carry
        // both, but tolerate a sparse-but-flagged point): isDue:true alone → a
        // due, uncompleted day = missed.
        let due_only = json!({"date": 1749750000000i64, "value": 5.0, "isDue": true});
        assert_eq!(checkin_from(&due_only, "d1", true).unwrap().status, "missed");
        // Older daily history with NEITHER flag → a PREENED average ({date,value}
        // only), not a recorded day → skipped from the contract (no row).
        let compressed = json!({"date": 1749800000000i64, "value": 5.0});
        assert!(
            checkin_from(&compressed, "d1", true).is_none(),
            "an averaged daily point must not become a fabricated 'done'",
        );
    }

    #[test]
    fn checkin_skips_point_without_usable_date() {
        assert!(checkin_from(&json!({"value": 1.0}), "t", false).is_none(), "no date → no row");
        assert!(checkin_from(&json!({"date": "not-ms"}), "t", false).is_none());
    }

    #[test]
    fn repeat_schedule_is_week_ordered() {
        // Out-of-order map keys still emit in week order.
        let r = json!({"f": true, "m": true, "su": false, "w": true});
        assert_eq!(repeat_schedule(&r), "m,w,f");
        assert_eq!(repeat_schedule(&json!({})), "");
        assert_eq!(repeat_schedule(&Value::Null), "");
    }

    // --- a scripted mock API ---------------------------------------------

    struct MockApi {
        habits: RefCell<Vec<Value>>,
        dailys: RefCell<Vec<Value>>,
        fail: Option<&'static str>, // task type whose fetch errors
    }

    impl MockApi {
        fn new(habits: Vec<Value>, dailys: Vec<Value>) -> Self {
            MockApi { habits: RefCell::new(habits), dailys: RefCell::new(dailys), fail: None }
        }
    }

    impl HabiticaApi for MockApi {
        fn tasks(&self, task_type: &str) -> Result<Vec<Value>, FetchError> {
            if self.fail == Some(task_type) {
                return Err(FetchError::Other("boom".into()));
            }
            Ok(match task_type {
                "habits" => self.habits.borrow().clone(),
                "dailys" => self.dailys.borrow().clone(),
                _ => Vec::new(),
            })
        }
    }

    #[test]
    fn full_pull_writes_snapshot_checkins_raw_dedupes_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(vec![habit_task()], vec![daily_task()]);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("habits"), Some(&2), "one habit + one daily in the snapshot");
        assert_eq!(out.counts.get("checkins"), Some(&4), "2 history points each");

        // Snapshot: both definitions, rewritten whole, one line each.
        let snap = std::fs::read_to_string(v.root().join("habits/habitica/habits.jsonl")).unwrap();
        assert_eq!(snap.lines().count(), 2);
        assert!(snap.contains("\"id\":\"2b774d70-ec8b-4f6b-9c3a-1f0e2d3c4b5a\""));
        assert!(snap.contains("\"schedule\":\"m,w,f\""), "daily weekday list on disk: {snap}");
        assert!(snap.contains("\"source\":\"habitica\""));

        // Check-ins: partitioned by the LOCAL month of the history date.
        let want_month = DateTime::from_timestamp_millis(1748563200000)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m")
            .to_string();
        let ci = std::fs::read_to_string(
            v.root().join(format!("habits/habitica/checkins/{want_month}.jsonl")),
        )
        .unwrap();
        assert!(ci.contains("\"status\":\"done\""));
        assert!(ci.contains("\"habit\":\"2b774d70-ec8b-4f6b-9c3a-1f0e2d3c4b5a\""));

        // Raw: full task objects with their history, verbatim fields the
        // contract drops.
        let raw_files = v.stream(RAW_DIR, Partition::Month).partitions().unwrap();
        assert!(!raw_files.is_empty(), "raw partitions exist");
        let raw_any: String = raw_files
            .iter()
            .map(|k| std::fs::read_to_string(v.root().join(format!("{RAW_DIR}/{k}.jsonl"))).unwrap())
            .collect();
        assert!(raw_any.contains("\"counterUp\":3"), "raw keeps counters the contract drops");
        assert!(raw_any.contains("\"history\""), "raw keeps the full history blob");

        // Watermark advanced to the max history ms drained.
        let state = v.read_habitica_sync();
        assert_eq!(state.checkins_after_ms, Some(1749600000000), "max history date (ms)");
        assert!(state.updated.is_some());

        // The cursor file carries NO credential.
        let cursor = std::fs::read_to_string(v.root().join(".trove/habitica-sync.json")).unwrap();
        assert!(!cursor.contains("token") && !cursor.contains("x-api"), "no secret in cursor");

        // Re-run identical input → check-in dedupe (0 new), snapshot rewritten
        // identically, raw not duplicated.
        let again = pull_with(&v, &MockApi::new(vec![habit_task()], vec![daily_task()])).unwrap();
        assert_eq!(again.counts.get("checkins"), Some(&0), "no duplicate habit-days");
        assert_eq!(again.counts.get("habits"), Some(&2), "snapshot always rewritten whole");
        let snap2 = std::fs::read_to_string(v.root().join("habits/habitica/habits.jsonl")).unwrap();
        assert_eq!(snap, snap2, "snapshot byte-identical after re-run");
        let raw_files2 = v.stream(RAW_DIR, Partition::Month).partitions().unwrap();
        let raw_lines2: usize = raw_files2
            .iter()
            .map(|k| {
                std::fs::read_to_string(v.root().join(format!("{RAW_DIR}/{k}.jsonl")))
                    .unwrap()
                    .lines()
                    .count()
            })
            .sum();
        assert_eq!(raw_lines2, 2, "raw deduped by task id — still 2 rows");
    }

    #[test]
    fn pull_drops_preened_history_but_keeps_it_raw_and_advances_watermark() {
        // The realistic free-account case: a habit's history is mostly PREENED
        // monthly/yearly averages ({date,value} only) with a couple of recent,
        // still-raw scored points. Only the OBSERVED points may become contract
        // check-ins; the averages are absent from the check-in stream but stay
        // verbatim in the raw layer, and the watermark still advances past them.
        let v = temp_vault("preened");
        let mut habit = habit_task();
        // Replace history: two old aggregates (no flags) + two recent raw points.
        habit["history"] = json!([
            {"date": 1700000000000i64, "value": 7.41},                                  // yearly avg
            {"date": 1740000000000i64, "value": 12.08},                                 // monthly avg
            {"date": 1748563200000i64, "value": 11.0, "scoredUp": 1, "scoredDown": 0},  // observed +
            {"date": 1749600000000i64, "value": 12.5, "scoredUp": 1, "scoredDown": 0},  // observed +
        ]);

        let out = pull_with(&v, &MockApi::new(vec![habit.clone()], vec![])).unwrap();
        assert_eq!(
            out.counts.get("checkins"),
            Some(&2),
            "only the two flag-bearing (observed) points became check-ins; the two averages are dropped",
        );

        // No check-in row exists for either aggregate's calendar day.
        let stream = v.stream(CHECKINS_DIR, Partition::Month);
        let mut dates: HashSet<String> = HashSet::new();
        for key in stream.partitions().unwrap() {
            for c in stream.read::<Checkin>(&key).unwrap() {
                dates.insert(c.date);
            }
        }
        let day = |ms: i64| ms_to_local(ms).unwrap().0;
        assert!(!dates.contains(&day(1700000000000)), "no check-in for the yearly average");
        assert!(!dates.contains(&day(1740000000000)), "no check-in for the monthly average");
        assert!(dates.contains(&day(1748563200000)), "observed day present");
        assert!(dates.contains(&day(1749600000000)), "observed day present");

        // Raw keeps the FULL history, averages included (full fidelity).
        let raw_any: String = v
            .stream(RAW_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .iter()
            .map(|k| std::fs::read_to_string(v.root().join(format!("{RAW_DIR}/{k}.jsonl"))).unwrap())
            .collect();
        assert!(raw_any.contains("7.41"), "the preened average survives verbatim in raw");
        assert!(raw_any.contains("12.08"), "the preened average survives verbatim in raw");

        // Watermark advanced to the newest history ms — even though the aggregate
        // points emitted no check-in, the drain still saw them, so we don't keep
        // re-scanning them every sync.
        assert_eq!(
            v.read_habitica_sync().checkins_after_ms,
            Some(1749600000000),
            "watermark = max history date drained, aggregates included",
        );
    }

    #[test]
    fn incremental_pull_only_appends_newer_history() {
        let v = temp_vault("incremental");
        // First sync drains both points (watermark → 1749600000000).
        pull_with(&v, &MockApi::new(vec![habit_task()], vec![])).unwrap();

        // A later task state adds a third, newer history point.
        let mut later = habit_task();
        later["history"].as_array_mut().unwrap().push(json!({
            "date": 1752278400000i64, "value": 13.0, "scoredUp": 1, "scoredDown": 0
        }));
        let out = pull_with(&v, &MockApi::new(vec![later], vec![])).unwrap();
        assert_eq!(out.counts.get("checkins"), Some(&1), "only the new point appended");

        let state = v.read_habitica_sync();
        assert_eq!(state.checkins_after_ms, Some(1752278400000), "watermark advanced to newest");
    }

    #[test]
    fn fetch_failure_aborts_without_advancing_watermark() {
        let v = temp_vault("fetchfail");
        // Seed a watermark via a good first sync.
        pull_with(&v, &MockApi::new(vec![habit_task()], vec![])).unwrap();
        let before = v.read_habitica_sync().checkins_after_ms;

        // Now the dailys fetch errors mid-pull → the whole pull errors, nothing
        // committed, watermark unchanged.
        let mut api = MockApi::new(vec![habit_task()], vec![daily_task()]);
        api.fail = Some("dailys");
        let err = pull_with(&v, &api).unwrap_err().to_string();
        assert!(err.contains("dailys"), "error names the failing fetch: {err}");
        assert_eq!(v.read_habitica_sync().checkins_after_ms, before, "watermark not advanced");
    }

    #[test]
    fn cursor_back_compat_empty_and_unknown_fields() {
        // An empty cursor file deserializes to all-None (a first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.checkins_after_ms.is_none());
        // A future cursor field is tolerated (additive evolution — old loads).
        let fut: SyncState =
            serde_json::from_str(r#"{"checkins_after_ms":123,"future":"x"}"#).unwrap();
        assert_eq!(fut.checkins_after_ms, Some(123));
    }

    #[test]
    fn split_credential_parses_user_and_token() {
        assert_eq!(
            split_credential("  abc-123 : def-456  "),
            Some(("abc-123".to_string(), "def-456".to_string()))
        );
        assert_eq!(split_credential("no-colon"), None);
        assert_eq!(split_credential(":only-token"), None);
        assert_eq!(split_credential("only-user:"), None);
    }

    // --- connection tests -------------------------------------------------

    #[test]
    fn connection_stores_credential_0600_and_absent_from_cursor() {
        let v = temp_vault("conn");
        // Store directly (def_connect needs the network for /user).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "secret-token-xyz".into(),
                refresh_token: None,
                token_type: Some("habitica".into()),
                scope: Some("user-id-abc".into()),
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Habitica");
        assert_eq!(status.accounts[0].key, "habitica");

        // The credential is NOT in any non-secret file (the cursor).
        v.write_habitica_sync(&SyncState { checkins_after_ms: Some(1), updated: Some("2026-06-15T00:00:00-07:00".into()) })
            .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/habitica-sync.json")).unwrap();
        assert!(!cursor.contains("secret-token-xyz"), "token never in the cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("secret-token-xyz") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret credential file must be 0600");
                }
            }
            assert!(found, "the credential was stored under .trove/sync");
        }

        def_disconnect(&v, "habitica").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn empty_login_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "no-colon-here").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "habitica");
        assert_eq!(DEF.connection, Some("habitica"));
    }
}

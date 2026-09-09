# Fix Health-tab slowness / app-wide hang

## Context

Opening the Health tab freezes the entire app for seconds. Two compounding root causes, both verified against the real vault (~36 MB of Oura JSONL: 29 MB / 294,886 heartrate records across 10 month files, 4.5 MB daily_activity, 3 MB sleep):

1. **All 7 health Tauri commands are synchronous** (`src-tauri/src/lib.rs:198–293`: `health_series`, `health_metrics_unified`, `health_series_unified`, `oura_overview`, `oura_sleep_nights`, `oura_heartrate_range`, `health_workouts`). Tauri 2 runs plain `fn` commands on the main thread, so the file I/O blocks the whole UI — and they hold `state.vault.lock()` for the entire read, blocking every other command too.
2. **Unbounded re-parsing in `crates/trove-core/src/health_unified.rs`.** `health_metrics_unified()` loops 16 `OURA_METRICS` defs and fully parses each def's collection per def: sleep.jsonl 4×, daily_activity 3×, daily_readiness/daily_stress 2× each, and `load_heartrate_months()` parses all 29 MB just to compute count/first/last. Selecting the heart-rate metric (`oura_metric_series`) re-parses all 29 MB on every metric/bucket click.

The Apple Health side is already fast because the import persists rebuildable indexes (`.trove/health-summary.json` + per-metric `daily.csv`). The Oura side has no equivalent — we mirror that pattern. `docs/vault-spec/conventions.md` sanctions machine summaries at `.trove/<domain>-summary.json` (rebuildable), which fits the "files are source of truth, databases are rebuildable indexes" hard rule.

Options considered and rejected: in-memory cache (app and future troved are separate processes; Vault is re-opened per command), in-vault `daily.csv` for Oura (continuously-refreshed derived data belongs in `.trove/`, not the vault), heartrate-only fix (one unified index covers catalog + all series with less machinery), date-range params on series commands (unnecessary once daily rollups exist — day-bucket output is only ~hundreds of points).

## Step 1 — Async-ify the 7 health commands (removes the app-wide hang; independently shippable)

**File: `src-tauri/src/lib.rs`** (lines ~198–293)

Convert each command to the established `oura_pull` pattern (lib.rs:546): `async fn` + `State<'_, AppState>`, extract `state.vault.lock().unwrap().root().to_path_buf()`, then `tauri::async_runtime::spawn_blocking(move || { let vault = Vault::open_or_create(root)…; vault.<method>()… }).await`. (`Vault` is not `Clone`; this root-reopen is the existing idiom.) Parse the `bucket` string inside the closure for the two series commands.

The mutex is then held only for `root().to_path_buf()` — contention gone even during syncs. TS surface unchanged (calls are already Promise-based); regenerate `src/bindings.ts`.

## Step 2 — `.trove/oura-summary.json`: one rebuildable index serving catalog + series

**File: `crates/trove-core/src/health_unified.rs`** (lives next to `OURA_METRICS`, which defines its contents)

Key insight: daily aggregates are tiny regardless of raw volume (294k heartrate samples → ~1,000 daily rows). Storing per-metric daily `(day, count, sum)` rows serves **both** the catalog (records = Σcount, first/last = min/max day) and every series read (`fold_buckets` at health_unified.rs:332 consumes exactly a per-day `(sum, count)` map), bit-identical to today's values.

### Data shape

```rust
const OURA_SUMMARY_REL: &str = ".trove/oura-summary.json";
const OURA_SUMMARY_VERSION: u32 = 1;   // bump when OURA_METRICS defs change

struct FileStamp { size: u64, mtime_ms: i64 }   // equality compare, never ">"
struct DailyRow { day: String, count: u64, sum: f64 }

struct OuraSummary {
    version: u32,
    /// keyed by path relative to health/oura/: "sleep.jsonl", "heartrate/2026-06.jsonl"
    files: BTreeMap<String, FileStamp>,
    /// slug -> daily rows, for every non-heartrate def
    metrics: BTreeMap<String, Vec<DailyRow>>,
    /// "YYYY-MM" -> that month file's daily rows (heart-rate metric)
    heartrate_months: BTreeMap<String, Vec<DailyRow>>,
}
```

Heartrate is keyed per month file so an hourly sync touching only the current month re-parses ~1 MB, not 29 MB.

### `ensure_oura_summary(&self) -> Result<OuraSummary>`

1. Read the summary; missing / unparseable / wrong `version` → start from default (full rebuild).
2. Enumerate the 8 distinct collection files referenced by `OURA_METRICS` plus every `health/oura/heartrate/*.jsonl`. Stat (capture `FileStamp`) **before** parsing, so a mid-read sync just looks stale next time.
3. For each new or stamp-changed file: parse once via the existing `load_oura_records` (oura.rs:669) and recompute its daily rows — one pass per collection file recomputes **all** defs reading it (sleep → 4 defs, daily_activity → 3, …). Deleted files: drop stamps and rows (metric disappears, matching today).
4. If anything changed, write back via the existing `crate::store::write_json_atomic` (store.rs:222). Fresh-path cost: ~10 stats + one small JSON read.

### Rewire reads

- `health_metrics_unified` (line 409): replace the per-def load loop with `ensure_oura_summary()` + derive `UnifiedSourceInfo` from daily rows (heart-rate concatenates `heartrate_months`). Defs with no rows stay absent.
- `oura_metric_series` (line 683): summary daily rows → `BTreeMap<NaiveDate,(f64,f64)>` → existing `fold_buckets`.
- Delete `oura_metric_info` (line 702) and `load_heartrate_months` (line 736) — no other callers.
- Leave `oura_overview`, `oura_sleep_nights`, `health_workouts`, `oura_heartrate_range` on raw files: they need fields the rollup doesn't carry, touch small or month-windowed data, and are off the main thread after Step 1.
- Update the module doc (lines 1–8, "nothing new is persisted" is no longer true).

### Sync-time freshness

**File: `crates/trove-core/src/oura.rs`** — at the end of `collect_oura` (after `write_oura_index`, ~line 453), call `self.ensure_oura_summary()?`. Both the watcher loop and manual "Sync now" funnel through `collect_oura`, and it's in trove-core so troved gets it for free. This moves re-parse cost of fresh data into the sync pass; the lazy mtime check in reads remains the correctness mechanism.

## Step 3 — Generalize: make both fixes house patterns, not health one-offs

The app will host massive data in every domain; these two principles must hold everywhere — **reads cost O(what's displayed), not O(what's stored)** and **vault I/O never runs on the main thread**.

1. **Extract the index machinery for reuse.** `FileStamp` (size + mtime_ms, equality compare), the stat-before-parse staleness walk, and the atomic read-check-rebuild-write loop go in `crates/trove-core/src/store.rs` (next to `write_json_atomic`) as a small generic helper — e.g. `fn ensure_index<T>(vault, rel, version, files, rebuild_fn) -> Result<T>`. `ensure_oura_summary` becomes its first caller; future domain rollups (finance overview, ads summaries, calendar) plug in the same way.
2. **Sweep the remaining sync commands.** ~37 commands in `src-tauri/src/lib.rs` lock the vault synchronously (timelines, finance_overview, ads_summary/ads_daily, search_artifacts, tasks_list, …). Convert every command that reads/writes vault *files* to the async + `spawn_blocking` pattern — mechanical, same shape as Step 1. Tiny `.trove` status reads (`*_sync_info`) may stay sync; everything else goes async so no file parse can ever stall the UI again.
3. **Document the conventions** so new integrations inherit them: in `docs/vault-spec/conventions.md`, add the read-side rule (partition by date/month; views read newest partitions until the page fills; cross-partition aggregates come from a `.trove/<domain>-summary.json` rebuildable index via the shared helper) and in `CLAUDE.md` hard rules, one line: vault-touching Tauri commands are `async fn` + `spawn_blocking`; read paths are O(displayed).

This step is mechanical and low-risk but broad; it lands after health is verified (Steps 1–2 prove the pattern on the worst offender first).

## Step 4 — Tests & verification

`cargo test -p trove-core` (unique temp dirs via existing `temp_vault(name)` helpers in health_unified.rs):

- Existing tests (`oura_only_metrics_list_and_series`, `heartrate_range_filters_and_downsamples`, `apple_and_oura_merge_under_one_slug`, …) are the regression net — they now exercise the summary path and must pass unmodified.
- New: summary written on first read and reused; hand-edited jsonl (different size) detected and reflected; deleted collection drops the metric; corrupt/wrong-version summary self-repairs; two heartrate months with one modified still yield correct series.
- In oura.rs sync tests: after a stub-server `collect_oura`, assert `.trove/oura-summary.json` is fresh and consistent.

Then: `cargo check` (workspace), regenerate/commit `src/bindings.ts`, rebuild per memory rule (`npm run tauri dev`), open Health tab against the real ~/Trove vault — first open does one full rebuild (~0.5–1 s, off-thread, no hang), subsequent opens are instant; metric/bucket switching on heart-rate is instant. After the Step 3 sweep, spot-check the other heavy views (Finance, Ads, Activity) for regressions.

## Risks / edge cases

- **First run after upgrade**: no summary → one full rebuild on a blocking thread (spinner, no hang); sync keeps it fresh thereafter.
- **Hand-edited files**: size+mtime equality (not newer-than) catches edits, clock skew, and restored backups. Same-size-same-mtime edits are invisible — acceptable for an index; deleting `.trove/oura-summary.json` is the escape hatch.
- **Concurrent app + troved**: both write atomically (tmp+rename); worst case momentary staleness self-healed by the next fingerprint check. No locking.
- **Float determinism**: per-day accumulation order matches today's file order → identical outputs.
- **Frontend**: zero changes; HealthView's request pattern is already right.

---

# Appendix A — Supporting evidence (verified during exploration)

## Measured vault volume (real `~/Trove`, 2026-06)

| File / dir | Size | Records |
|---|---|---|
| `health/oura/heartrate/` (10 month files, 2025-09 → 2026-06) | 29 MB | 294,886 |
| `health/oura/daily_activity.jsonl` | 4.5 MB | — |
| `health/oura/sleep.jsonl` | 3.0 MB | — |
| `health/oura/daily_readiness.jsonl` | 252 KB | — |
| `health/oura/daily_sleep.jsonl` | 156 KB | — |
| `health/oura/sleep_time.jsonl` | 120 KB | — |
| `health/oura/daily_stress.jsonl` | 92 KB | — |
| `health/oura/daily_spo2.jsonl` | 88 KB | — |

This is ~9 months of one user's data. It only grows; heartrate dominates and grows ~1 MB/month.

## Cost of the current catalog build (`health_metrics_unified`)

`OURA_METRICS` has 16 defs across 8 distinct collections. `oura_metric_info` fully loads + JSON-parses the def's collection **once per def**, so per Health-tab open the same files are re-parsed:

- `sleep.jsonl` (3 MB) → 4× (slugs: `sleep`, `resting-heart-rate`, `hrv`, `respiratory-rate`)
- `daily_activity.jsonl` (4.5 MB) → 3× (`activity-score`, `steps`, `active-energy`)
- `daily_readiness.jsonl` → 2× (`readiness-score`, `temperature-deviation`)
- `daily_stress.jsonl` → 2× (`stress-high`, `recovery-high`)
- `load_heartrate_months()` concatenates **all** 29 MB / 294,886 records just to compute count + first/last date for the `heart-rate` catalog row.

Then selecting `heart-rate` in the Metrics or Overview tab calls `oura_metric_series`, which re-parses all 29 MB again on **every** metric/bucket click.

## Key file/line map

| What | Location |
|---|---|
| 7 sync health commands | `src-tauri/src/lib.rs:198–293` |
| Reference async pattern (`oura_pull`) | `src-tauri/src/lib.rs:546` |
| `AppState { vault: Mutex<Vault> }` | `src-tauri/src/lib.rs:13` |
| `health_metrics_unified` (catalog) | `crates/trove-core/src/health_unified.rs:409` |
| `health_series_unified` | `crates/trove-core/src/health_unified.rs:453` |
| `oura_metric_series` (per-click re-parse) | `crates/trove-core/src/health_unified.rs:683` |
| `oura_metric_info` (per-def re-parse) — delete | `crates/trove-core/src/health_unified.rs:702` |
| `load_heartrate_months` (29 MB concat) — delete | `crates/trove-core/src/health_unified.rs:736` |
| `fold_buckets` (per-day → bucket fold) | `crates/trove-core/src/health_unified.rs:332` |
| `oura_heartrate_range` (good month-windowed read) | `crates/trove-core/src/health_unified.rs:564` |
| `OURA_METRICS` defs | `crates/trove-core/src/health_unified.rs:169–314` |
| `load_oura_records` (read_to_string + per-line serde) | `crates/trove-core/src/oura.rs:669` |
| `collect_oura` (sync entrypoint; hook point) | `crates/trove-core/src/oura.rs:430–453` |
| `write_json_atomic` (tmp + rename) | `crates/trove-core/src/store.rs:222` |
| Apple fast path: `list_health_metrics` (reads summary json) | `crates/trove-core/src/health.rs:184` |
| Apple fast path: `health_series` (reads `daily.csv` rollup) | `crates/trove-core/src/health.rs:196` |
| `.trove/health-summary.json` const | `crates/trove-core/src/health.rs:85` |
| HealthView (frontend) | `src/components/HealthView.tsx` |

## Frontend data flow (already correct — no change needed)

`HealthView.tsx` loads the catalog + Oura overview once on mount (`refresh()`, line ~120), and fetches a metric's series only when it's selected (`MetricsSection`/`OverviewSection` effects at lines ~281 / ~357). Sleep loads 90 nights (`ouraSleepNights(90)`); each selected night fetches its intraday HR via the already-windowed `ouraHeartrateRange(start, end, 600)`. Workouts load the newest 200. The request pattern is fine — the only problem is backend latency, which Steps 1–2 fix. All `invoke` calls route through `src/api.ts`.

---

# Appendix B — Design-validation notes (corrections found while validating the draft)

These refine the original draft approach; they are already folded into the plan above but recorded here for the implementer:

1. **It's 7 health commands, not 6.** `list_health_metrics` (lib.rs:191) reads only the small `.trove/health-summary.json` and can stay synchronous; everything else in the 198–293 block goes async.
2. **`Vault` is not `Clone`** (`crates/trove-core/src/vault.rs:21` is just `{ root: PathBuf }` with no derive). The existing async commands do **not** clone it — they extract `root().to_path_buf()` and call `Vault::open_or_create(root)` on the blocking thread. Copy that exactly.
3. **Steps B and C of the draft collapse into one index.** Daily `(day, count, sum)` rows serve the catalog *and* every series/bucket read, because the catalog's `records/first/last` are just `Σcount / min day / max day` of the same rows that `fold_buckets` already consumes. One file, one staleness check, one rebuild path — strictly less machinery than a separate count-index + series-cache.
4. **Don't put Oura's rollup in the vault.** Apple's in-vault `daily.csv` is a one-shot import artifact; a continuously-refreshed derived file belongs under `.trove/` per `docs/vault-spec/conventions.md`. (In-memory caching is also out — app and troved are separate processes and the Vault is re-opened per command.)
5. **Atomic writes are already solved** by `store::write_json_atomic` (tmp sibling + rename), which is what `.trove/oura-sync.json` already uses. Concurrent app/troved writers are safe: last-writer-wins with valid content, and any staleness self-heals on the next fingerprint check — no locking needed.
6. **One sync hook covers both sync paths.** The watcher loop and the manual "Sync now" button both funnel through `Vault::collect_oura` (oura.rs:430), and it lives in trove-core, so the future troved daemon inherits the freshness hook for free.
7. **Use equality, not newer-than, on `FileStamp`.** Comparing size+mtime for *inequality* (rather than "stored mtime is older") makes clock skew, restored backups, and hand-rollbacks all trigger a correct rebuild instead of a silent miss.
8. **Module doc must change.** `health_unified.rs:1–8` currently says "nothing new is persisted… derived on demand." After this change a rebuildable `.trove/` index *is* persisted (which the conventions explicitly sanction) — update the comment.

---

*Plan authored 2026-06-12; implementation to be carried out on branch `worktree-health-refactor` (worktree `.claude/worktrees/health-refactor`). This document lives on `main` as the canonical spec.*

# S7-health handoff — 2026-09-16

For the agent picking up Trove after the session of 2026-09-16. Read
`CLAUDE.md`, then `docs/roadmap.md` (governing), then this. `README.md` is
current on status; `HANDOFF.md` is the June architecture reference and is
accurate on internals but stale on status.

Repo: `~/Local/trove`, branch `main`, public at github.com/david-wills/trove.
Working tree is clean at `7f3d50e` apart from this file.

## 1. Where the project is

Trove is a local-first Mac app (Rust core + Tauri 2 + React 19) that
collects personal data into a plain-file vault at `~/Documents/Trove`.
The September 2026 roadmap (`docs/roadmap.md`) is a re-baselining: prune
the catalog, get the always-on watcher out of the app, expose the vault to
agents over MCP, then do one data-type pass at a time. Sequence and status:

| Step | What | Status |
|---|---|---|
| S1 | Double-clickable `Trove.app` + `Trove Dev.app` | done 2026-09-14 |
| S2 | Prune catalog to the keep list | done 2026-09-14 |
| S3 | Catalog view in the hub | done 2026-09-14 |
| M1 | Vault MCP server (`crates/trove-mcp`) | done 2026-09-15 |
| S4 | Extract watcher to `trove-collector`; delete `troved`; app syncs on open | **done 2026-09-16, commit `0ccfc01`** |
| S5 | Measure each periodic sync's memory in isolation | not started; independent of S7 |
| S6 | UI baseline: navigation, theme, layout | **next** (thin scope, see §5) |
| S7-health | First data-type pass: build both read-side shapes on real Oura + Apple Health data, pick one | **groundwork done 2026-09-16, commit `7f3d50e`**; views wait on S6 |
| S7+ | Remaining passes in the shape S7 settled: messages, browser, calendar, activity, music | |
| later | Scheduled insights over MCP; vault-wide search / timeline / entity resolution | |

## 2. What happened on 2026-09-16

### Commit `0ccfc01` — S4 extraction

Committed the already-built S4 work after verifying `cargo check`,
`npm run build`, and `cargo test -p trove-core` (563 tests) were green.
Summary, also recorded under "S4 outcome" in the roadmap:

- `crates/troved`, `extension/`, `sampler.rs`, `music_listener.rs`, and
  the write halves of `activity`, `music`, `browser_ext`, `ads` are gone.
  Read halves, defs, and heartbeat types stay.
- `Behavior::Live` and `NativeHost` became one `Behavior::External { collector }`.
- `runner.rs`'s owner loop is now `run_sync`, holds `.trove/sync.lock`,
  first pass ~5 s after launch. There is no daemon in this repo.
- The hub reads the collector heartbeat (`.trove/watcher-state.json`)
  through `Vault::collector_status`.
- Spec pages + schemas + fixtures for the three collector-owned streams
  (`activity`, `browser-visits`, `ads`).
- The watcher lives at `~/Local/trove-collector` (public repo, own
  `scripts/build.sh`).

### Commit `7f3d50e` — S7 groundwork

Everything in S7 that did not depend on the UI baseline:

- **`docs/vault-spec/domains/health.md`** — spec page for the `health/`
  raw layer (was "spec page planned"): Apple per-metric CSVs
  (`health/<metric>/YYYY-MM.csv`, header `start,end,value,unit,source`,
  Apple's own timestamp format recorded as the one non-RFC3339
  exception; `daily.csv`; `index.md`; `.trove/health-summary.json`) and
  Oura verbatim API records (`health/oura/<collection>.jsonl`,
  `heartrate/YYYY-MM.jsonl`, `.trove/oura-sync.json`, `.trove/oura-summary.json`).
- **`health-sleep`, the first promoted noun contract.** It qualified
  under the roadmap's rule (two real sources report it *and* a real
  cross-source question needs it). Files:
  - `docs/vault-spec/domains/health-sleep.md` (spec)
  - `docs/vault-spec/schemas/health-sleep.session.schema.json`
  - `crates/trove-core/tests/fixtures/spec/health-sleep.session.jsonl`
    (must equal the page's example block byte-for-byte; a test enforces it)
  - `crates/trove-core/src/health_sleep.rs` — `Session` type,
    `oura_session` (raw Oura record → Session), `AppleInterval` +
    `apple_sessions` (stitch `SleepAnalysis` intervals into sessions per
    origin, <1 h gap), `origin_slug`, `dedupe_relays`,
    `Vault::write_sleep_sessions`, `Vault::rebuild_oura_sleep_sessions`,
    `Vault::sleep_sessions(from, to)` (reads only month partitions in range)
  - `contracts.rs` DOMAINS entry `health-sleep` (root `health/sleep`,
    month partition, required `day,start,end,source,guid`)
  - `spec_validation.rs` wiring (round-trip, doc-verbatim, required-list)
  - `crates/trove-mcp/src/spec.rs` — `describe_type` now serves `health`
    and `health-sleep`
  - `lib.rs` re-exports `SleepSession`, `AppleInterval`, `apple_sessions`,
    `dedupe_relays`, `oura_session`
- **Writers** (both are projections of raw the crate already holds and
  rewrite their own `health/sleep/<source>/` folder whole, atomically):
  - `oura.rs::apply_oura_records` — when the `sleep` collection is
    rewritten, projects the merged raw list and calls
    `write_sleep_sessions("oura", …, replace_all = true)`.
  - `health.rs` — `ImportState.sleep` collects every `SleepAnalysis`
    interval; `finish()` calls `apple_sessions` and writes
    `apple-health` with `replace_all = true` (a full export replaces).
- **Real vault populated** by a one-off run (throwaway example, since
  deleted): 913 Oura sessions (2024-06..2026-09) and 4065 Apple sessions
  (2016-09..2026-09) under `~/Documents/Trove/health/sleep/`. David
  imported a fresh Apple Health export the same day
  (`~/Downloads/export 2.zip`, imported 13:00, data through 2026-09-16).
- **Roadmap** gained an "S7-health groundwork" section with the findings
  below and the S7 row now reads "groundwork 2026-09-16; views wait on S6".
- **Trove.app rebuilt and installed** via `scripts/build-app.sh` (signed,
  `/Applications/Trove.app` + `./Trove.app`, MCP binary alongside). David
  was asked to relaunch so the running app picks up the Oura writer.

### Findings from real data (these shape the views)

1. **Apple Health is a relay.** The sleep export carries seven origins
   (AutoSleep 1619 sessions, iPhone 820+218, Apple Watch 629+3, Oura 255,
   Clock 238, Pillow 198, Withings 85). Oura nights arrive twice: directly
   in `oura/` and again via Apple with `origin: "Oura"`, identical to the
   second on stage totals. Contract rule: both rows are written;
   `dedupe_relays` hides the relay row at read time when the device writes
   its own folder. Two *different* origins on one night are both shown,
   never averaged. This is the strongest concrete argument so far for
   "source-native by default": a merged view needs this rule on day one.
2. **`day` is the source's attribution, not the date of `end`.** Oura's
   sleep day turns over at 18:00, so an evening nap belongs to the next
   day, and that day is the key its `daily_sleep` score joins on. 101 real
   rows differ from the end date, all Oura naps/rests after 18:00. Spec,
   schema, and Rust doc were corrected; views must group by `day`.
3. **Chart seeds.** 14 nights in the last 90 days have no Oura long-sleep
   row (the "missing nights" annotation has real data). Oura's raw
   `sleep.jsonl` is sorted by id, not day; it is not a gap.
4. **Three Oura collections are empty on a 401** (`daily_cardiovascular_age`,
   `daily_resilience`, `vo2_max`): the account never granted their scope.
   Fix is David reconnecting with every permission checked. Not a bug.

## 3. Decisions settled with David (2026-09-16)

- **Order:** thin S6 first, then S7 views; S5 in parallel or after.
- **Sleep contract in S7:** yes (done).
- **Boards are files:** markdown with YAML frontmatter under `boards/`,
  one panel per entry. Not built yet; needs a spec page when it is.
- **Judging the two shapes:** both ship in one build behind a toggle;
  David uses them for about a week against the five chart wants from the
  M1 outcome (health trend around a date with the gap visible; sleep
  score vs calendar event count on a dual axis; weekly sleep duration vs
  weekly event total; a month heatmap of event counts; a missing-nights
  annotation over event bars). Each shape must produce all five. The
  decision is recorded in `docs/roadmap.md`.

## 4. The two read-side shapes (from the roadmap, unchanged)

- **A. Merged.** Health tab → one normalized view with every source
  folded in; needs precedence wherever two sources report the same thing.
  *State:* `health_unified.rs` is already most of this — a canonical
  metric catalog (`health_metrics_unified`) serving per-source series
  side by side (`health_series_unified`), plus `oura_overview`,
  `oura_sleep_nights`, `oura_heartrate_range`, `health_workouts`.
  `HealthView.tsx` (729 lines, tabs overview/metrics/sleep/workouts)
  renders it with `MultiChart.tsx` on uPlot. What A lacks: a per-metric
  preferred source so an overview card shows one number, and reading
  sleep from the contract instead of `oura_sleep_nights`.
- **B. Source-native.** Health is a category; inside it the user picks
  the source (Oura, Apple Health); each source gets per-type views
  (a sleep view over that source's contract rows), falling back to a
  generic table; a **generic chart** (source → stream → numeric column,
  including `extra`, → chart type) with no mapping; a user-curated
  **pinned board** as the only merged view. *State:* nothing exists.
  Needs a new core read over any JSONL/CSV stream with a rebuildable
  column index (use `ensure_index` in `store.rs`; reads stay O(displayed)),
  the board file format, and the panes.

Constraints that hold either way: normalize nouns not metrics; generic
chart over any numeric column; boards are files; merged views stay easy
where sources are disjoint.

## 5. What to do next, in order

1. **Thin S6.** Scope: navigation grouping, a theme variable set (dark
   mode included), one layout grid. No view redesign. Current shell:
   `src/App.tsx` (187 lines) is a sidebar with 17 flat entries;
   `src/App.css` is 3137 lines with 12 theme variables and no
   `prefers-color-scheme`. Bring David a proposal before touching it; he
   wants pushback and enumerated choices with recommended defaults, not
   open questions.
2. **S7 views, both shapes behind a toggle.** Build B's core pieces first
   (generic column index + chart read, sleep pane over the contract,
   board file + spec page), then adapt A (preferred-source precedence,
   contract-backed sleep). Wire Tauri commands through `src/api.ts`
   (`bindings.ts` is specta-generated); every vault-touching command is
   `async fn` + `spawn_blocking`. Make the five charts producible in
   each shape, hand to David for the week, record the decision.
3. **Spec-fidelity check** is part of the pass: re-run the schema check
   against real files after any writer change (the ad-hoc Python check
   from today validated required fields, RFC3339, partition month, uint
   seconds, `asleep ≤ in_bed`, `kind` enum, scalars-only `extra`).
4. **S5** whenever: measure each periodic sync's RSS in isolation, the
   same way `trove-collector` logs its own (`rss_mb` in the heartbeat).

## 6. Gotchas learned today

- **Rebuild via `scripts/build-app.sh`, never bare `cargo build`** for the
  app; TCC grants are keyed to the signing identity. Same for the
  collector's `scripts/build.sh`. Rebuild after code changes, not just
  typecheck (David's standing rule).
- **`crates/trove-core/examples/` has seven tracked examples.** Deleting
  the directory to remove a throwaway removes them all; I did that and
  restored from git. Put throwaways elsewhere or delete by name.
- **Oura raw files are snapshots sorted by key** (id for event
  collections), so first/last-line checks lie about date ranges.
- **Apple export timestamps** are `YYYY-MM-DD HH:MM:SS ±HHMM`, parsed by
  `health.rs::parse_date`; the contract layer converts to RFC3339 via
  `to_rfc3339()`.
- **Apple origins carry a non-breaking space** ("David’s Apple Watch");
  `origin_slug` collapses it. Match on slug, not raw string.
- **Bindings fields lie**: `skip_serializing_if` fields arrive `undefined`
  despite required-looking TS types; optional-chain in views.
- **MCP `structuredContent` must be an object**, and tool pages need a
  byte budget (M1 lesson; already handled in `trove-mcp`).
- **Git identity is derived from the remote**; never set `user.email`.
  This repo is personal (`git@github-personal:`).

## 7. Verify before you start

```bash
source "$HOME/.cargo/env"
cargo test -p trove-core          # 568 passed, 2 ignored at 7f3d50e
cargo test -p trove-mcp           # 8 passed
cargo check --workspace
npm run build
ls ~/Documents/Trove/health/sleep/            # apple-health/ oura/ + Apple's *.csv
cat ~/Documents/Trove/.trove/oura-sync.json | head -5   # the 401 note until David reconnects
```

## 8. Open on David's side

- Reconnect Oura in the Integrations tab with every permission checked
  (fills the three empty collections).
- Relaunch Trove after the 2026-09-16 rebuild if not already done.
- Approve the thin S6 proposal when it is brought to him.

## 9. Pointers

- `docs/roadmap.md` — governing; "Read-side shape" section, M1 and S4
  outcomes, S7 groundwork section.
- `docs/vault-spec/` — README, conventions, domains, schemas; the
  round-trip harness is `crates/trove-core/tests/spec_validation.rs`.
- `docs/integration-schedule.md` — cadence of every periodic sync.
- `~/Local/trove-collector` — the external watcher; its spec pages are
  `activity`, `browser-visits`, `ads`.
- Claude Code memory for this repo lives at
  `~/.claude/projects/-Users-davidwills-Local-Studio-trove/memory/`
  (`MEMORY.md` is the index; `s7-health-groundwork.md` is today's note).

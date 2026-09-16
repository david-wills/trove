# Trove — Roadmap

*2026-09-14, decided with David; M1 + read-side section added 2026-09-15, M1 shipped the same day. Supersedes `docs/post-wave-roadmap.md` for
sequencing. The write-time doctrine in `docs/integration-pipeline.md`
(contracts, conventions, evidence hierarchy) still stands; the vault spec
in `docs/vault-spec/` is unchanged and remains the product's core contract.*

## Where this leaves off

The prototype paused in July 2026 with collection and storage working,
viewing thin, and analysis unbuilt (see `README.md`, "Where this is
going"). Two things were wrong with the shape it paused in:

1. **Breadth ran ahead of depth.** 279 integration modules had never seen a
   real account. They cost compile time, review surface, and hub clutter
   for no user-facing value, and they made "what does Trove actually do"
   hard to answer.
2. **The always-on daemon was one process doing two jobs.** `troved` ran
   the sampling watcher (app/window activity, the browser-extension host,
   the Music scrobbler, screenshots) *and* every periodic cloud sync
   (Fathom, Google Drive, iMessage, Oura, Calendar, …). It reached 5–6 GB
   of RAM at times, which is unacceptable on a daily machine. The error log
   shows the periodic syncs were the busy part; the watcher itself wrote
   11 MB of activity data in a year. The leak has not been located.

## Decisions (settled 2026-09-14 — do not relitigate)

1. **Trove is a baseline data collection, storage, and analysis app,
   built out one data type at a time.** Each data type gets a deliberate
   pass: the best view for it, a read-path index that keeps it
   O(displayed), and spec fidelity. Depth over breadth, in that order.
2. **The watcher becomes its own project.** Everything that needs a 24/7
   process — the CoreGraphics activity sampler, the Chrome-extension
   native-messaging host, the Music scrobbler, the ads observer, the
   screenshot capturer — leaves this repo and becomes a standalone
   collector that writes to the vault under the vault spec. It appears in
   Trove as one external-collector card in the hub. This is the first real
   exercise of the spec's central claim that any program can write to the
   vault. Its first job is to measure its own memory.
3. **The daemon goes away with the watcher.** Once the watcher is out,
   nothing in Trove needs an always-on process. Periodic pulls run while
   the app is open (sync-on-open). If that turns out to be a real gap, a
   scheduler is a later addition, not a carry-over. The sync-on-open path
   inherits whatever memory problem the daemon had, so each periodic sync
   gets measured in isolation before it is trusted.
4. **The catalog is pruned to sources that have touched real data.** Main
   keeps the local Mac readers, the cloud connections David has actually
   connected, and the watcher-class modules until they move out. The other
   ~316 modules are removed from the tree and remain restorable from git
   (`docs/integrations/INDEX.md` records the commit for each). The briefs
   stay in `docs/integrations/` as documentation. New integrations from
   here are demand-driven: a real user with a real account.
5. **The pruned catalog stays visible in-app.** The hub gets a Catalog
   view rendered from `docs/integrations/INDEX.md` at build time: every
   source ever briefed, its domain, its status (in this build / built but
   not included / queued / unavailable), and for pruned ones the commit
   that has the module. Restoring one is checking out that file and adding
   its registry line.
6. **UI gets one shared baseline first, then per-type views inside it.**
   Navigation, theme, and layout are cross-cutting and get settled once;
   individual views are then designed in each data type's pass. No global
   redesign ahead of the first data pass.

## Next up: the vault MCP server (M1) — settled 2026-09-15

**Decision.** Trove's "ask" component is not built into the window. It is
an MCP server over the vault that any agent (Claude Code, Claude Desktop,
anything speaking MCP) can query. The window stays the visual layer; the
model is the question layer; both read the same files. This is also the
prerequisite for scheduled insights later (an agent reads through the same
door and writes findings back into the vault as markdown).

Live service connectors (Google, TickTick, …) are *not* Trove's concern:
the user attaches those to their MCP client directly. Trove's own pulls
are for **retention**, not freshness — slow and correct is fine.

**Brief for whoever builds it** (self-contained; read `CLAUDE.md` first):

- New workspace crate `crates/trove-mcp`, one binary `trove-mcp`, stdio
  transport. Depends on `trove-core` only; no Tauri. Add it to
  `Cargo.toml` `members` and to `scripts/build-app.sh` so it ships inside
  `Trove.app` (`Contents/MacOS/trove-mcp`). Suggested SDK: the official
  Rust MCP crate (`rmcp`) — verify current name/version before adding.
- **Read-only in v1.** No tool writes to the vault. Every path goes
  through `Vault::resolve`; `.trove/` is never readable (same jail as the
  `read_stream` Tauri command in `src-tauri/src/lib.rs`).
- Vault root: `Vault::default_root()`, overridable with `--vault <path>`
  (and `HOME` still works for the temp-vault test pattern).
- Tools, each a thin wrapper over an existing `trove-core` read path
  (extract shared logic into `trove-core` where the Tauri command
  currently holds it — e.g. the newest-first pagination in `read_stream`
  — rather than duplicating; the Tauri command then calls the same fn):
  - `list_sources` — every registry def (`INTEGRATIONS`), its domain,
    the vault dirs it writes, and whether those dirs have any data.
  - `list_streams` — every date-partitioned JSONL dir present in the
    vault, with partition range and record count (cheap: partitions only).
  - `read_stream(dir, from?, to?, limit, offset)` — newest-first records
    from a stream, optional date bounds on the partition key. Cap `limit`.
  - `describe_type(domain)` — returns the vault-spec doc for a domain
    (`docs/vault-spec/domains/<domain>.md`, embedded at build time via
    `include_str!`) so the agent knows the record shape before reading.
  - `search_artifacts(query)` / `read_artifact(path)` — over the notes
    layer, wrapping `Vault::search_artifacts` / `read_artifact`.
  - `health_metrics()` / `health_series(slug, bucket)` — wrapping the
    existing `list_health_metrics` / `health_series`. One worked example
    of a typed, aggregated read; other domains get theirs in their
    data-type pass.
- Every tool response is JSON. Large reads are paginated, never
  truncated silently; return `next_offset`.
- Register: document the Claude Code (`claude mcp add trove -- <path>`)
  and Claude Desktop config snippets in `README.md`; add a "Copy MCP
  config" affordance in the app's settings later, not in M1.
- Tests: `cargo test -p trove-mcp` against a temp vault with the spec
  example lines; one test per tool; one test proving `.trove/` and `..`
  paths are refused.
- **Done when:** with the server registered in Claude Code, "what did I
  do last Tuesday across every source in my vault" and "compare my Oura
  sleep to my calendar density last month" both return grounded answers
  with no per-question code. Note in this file which questions felt like
  they wanted a chart — that list seeds the data-type passes.

### M1 outcome (2026-09-15)

Built as briefed: `crates/trove-mcp` (rmcp 3.4, stdio), eight read-only
tools, ships as `Trove.app/Contents/MacOS/trove-mcp` via
`scripts/build-app.sh`, registered in Claude Code at user scope. The
generic reads it needed (`read_stream_page`, `list_streams`,
`list_sources`) were extracted into `crates/trove-core/src/query.rs`; the
Tauri `read_stream` command now calls the same function. Ten tests
(`cargo test -p trove-mcp` + the `query` unit tests), including the
`.trove/`/`..` jail and a wire-level stdio test.

Both done-when questions returned grounded, cited answers with no
per-question code ("last Tuesday across every source": ~12 streams read
and cross-referenced; "Oura sleep vs calendar density last month": daily
sleep score + weekly duration against daily event counts). The first live
run found two things the brief did not anticipate, both fixed:

- **MCP `structuredContent` must be a JSON object.** Every list-shaped
  tool failed client-side validation until wrapped (`{streams: [...]}`).
- **Pages need a byte budget and record-level date bounds.** Month
  partitions made "one day" cost a whole month of paging, and 100 email
  records (or 3 raw Oura sleep sessions with their 5-minute arrays)
  exceeded Claude Code's tool-result limit no matter the range. Day
  bounds now also filter records by `ts`/`occurrence`/`start`/`day`, and
  a page ends early past ~160 KB with `next_offset` pointing at the rest.

Wanted a chart (seeds for the data-type passes, from the two answers):

- A day timeline with calendar blocks and message bursts on one time axis
  (S7+ messages/calendar).
- Message volume by contact and hour; screen time stacked by app across a
  day; browser activity as topic clusters over time (messages, activity,
  browser passes).
- Health trend around a date with the data gap visible; sleep score vs
  event count on a dual axis; weekly duration vs weekly event total; a
  month heatmap of event counts; a missing-nights annotation over event
  bars (S7-health — the cross-source overlay is exactly the merged vs
  source-native question).

Not done in M1, by design: the "Copy MCP config" affordance in settings;
any typed aggregate beyond health. The Oura raw `sleep` rows are the first
concrete case for the generic-chart constraint below (a numeric column
under a non-contract stream that someone wanted plotted).

## Read-side shape (leaning, to be settled by S7-health)

Discussed 2026-09-15; not yet decided. The Health pass (S7) builds
**both** candidates on real Oura + Apple Health data and picks one:

- **A. Merged.** Health tab → one normalized view (sleep, readiness,
  activity, …) with every connected source folded in. Needs precedence
  rules wherever two sources report the same thing.
- **B. Source-native.** Health is a category; inside it the user picks
  the source (Oura, Apple Health); each source gets a pane built from
  *per-type* views (a sleep view rendering that source's sleep records),
  falling back to a generic table for anything without a view. A
  user-curated **pinned board** is the only merged view; the user picks
  the winner per metric, so there is no conflict logic.

Constraints that hold either way:

- **Normalize nouns, not metrics.** The shared record shape per domain
  stays small (a sleep is start/end/source + a few columns); everything
  else lives under `extra` at full fidelity. A column is promoted only
  when two real sources have it *and* a real cross-source question needs
  it. The 18 ratified domain contracts are the ceiling, not the floor.
- **Generic chart.** Any numeric column in any stream (including `extra`)
  can be charted over time with no mapping: pick source → table → column
  → chart type. Pinned boards compose these. This is what keeps the long
  tail self-serve rather than gated on someone deciding what matters.
- **Boards are files** in the vault, so they are shareable; a field that
  recurs across shared boards is the demand signal for a designed view.
- Merged views stay easy where sources are disjoint (a unified inbox
  across iMessage, Slack, email); "source-native by default" is a default,
  not a global rule.

## What is kept in this build

- **Local Mac readers:** Apple Health export, Chrome and Safari history,
  iMessage, Calls, Calendar and Reminders (EventKit), Screen Time and Now
  Playing (Biome), Music library, Podcasts, Books, Apple Mail, folder scans
  (Downloads, Dropbox, iCloud Drive, Google Drive), the finance file
  importer, and the `.mbox` / Slack / Letterboxd one-shot imports
  (Letterboxd stays as the reference import example).
- **Cloud connections:** Google (Gmail, Calendar, Contacts, Tasks,
  YouTube, Books), TickTick, Oura, Fathom, SimpleFIN, Weather (Open-Meteo).
- **Watcher-class, left with decision 2 (S4):** activity, browser extension
  host, ads observer and identify, Music scrobbler — now written by
  `trove-collector`, shown here as `Behavior::External` cards. Screenshots
  stayed, converted to a periodic scan (a screenshot is a file; nothing is
  lost between passes).

## Sequence

| Step | What | Status |
|---|---|---|
| S1 | Double-clickable `Trove.app` + `Trove Dev.app` launcher | ✅ 2026-09-14 |
| S2 | Prune the catalog to the keep list; INDEX records restore commits | ✅ 2026-09-14 (`b0e2bce`) |
| S3 | Catalog view in the hub, rendered from INDEX at build time | ✅ 2026-09-14 (`60bc571`) |
| M1 | **Vault MCP server** (`crates/trove-mcp`) — see "Next up" above; the ask component lives here, not in the window | ✅ 2026-09-15 (outcome below) |
| S4 | Extract the watcher to its own project; delete `troved`; app syncs on open | ✅ 2026-09-16 (outcome below) |
| S5 | Measure each periodic sync's memory in isolation; fix what the daemon leaked | |
| S6 | UI baseline: navigation, theme, layout | |
| S7-health | First data-type pass. Build **both** read-side shapes (merged vs source-native, see above) on real Oura + Apple Health data; pick one; record the decision here | |
| S7+ | Remaining data-type passes, one at a time, in the shape S7-health settled: messages, browser, calendar, activity (via the external watcher), music | |
| later | Scheduled insights: an agent reads the vault through M1 on a schedule and writes findings back as markdown; the window shows them like any other type | |
| later | The read layer from the old R4: vault-wide search, unified timeline, entity resolution, then LLM analysis over the search index | |

Each data-type pass is: best view for the type → read-path index →
spec-fidelity check against real files → done. A pass is not done until
the view is one a new user would keep open.

### S4 outcome (2026-09-16)

- `crates/troved`, `extension/`, `sampler.rs`, `music_listener.rs`, and the
  write halves of `activity`, `music`, `browser_ext`, `ads` are gone from
  this repo. The read halves, the defs, and the sidecar/heartbeat types
  stay. `Behavior::Live` and `NativeHost` were replaced by one
  `Behavior::External { collector }`; the `LiveCollector` trait is gone.
- troved never had a scheduler: every periodic def already ran from
  `runner.rs`'s owner loop, which the app was running in a thread. That
  loop is now `run_sync`, holds its own `.trove/sync.lock`, and never
  contends with the collector's `.trove/watcher.lock`. Opening the app is
  the sync; the first pass fires ~5 s after launch.
- The hub reads the collector's heartbeat (`.trove/watcher-state.json`:
  pid, last tick, in-progress activity event, resident memory) through
  `Vault::collector_status`; the Trove Collector group shows running /
  installed / memory and the five external toggles.
- The three streams the collector owns exclusively had no spec page. They
  do now (`domains/activity.md`, `browser-visits.md`, `ads.md`), with
  schemas, fixtures, and the anti-drift round-trip test — the contract is
  the only thing the two programs share.
- `~/Local/trove-collector` is public at github.com/david-wills/trove-collector
  and installed via its `scripts/build.sh`. Its first job (measure its own
  memory) is built in: `rss_mb` in the heartbeat and a log line every ten
  minutes. S5 measures the app's periodic syncs the same way.

## Carried forward unchanged

- Files are the source of truth; every index is rebuildable. Vault-touching
  commands are `async` + `spawn_blocking`; reads are O(displayed).
- Standalone: capabilities are compiled in, never a runtime dependency on
  another app or service. The external watcher is a *collector*, not a
  dependency — Trove works without it.
- Built for anyone: nothing user-specific hardcoded; David is the first
  user, not the design target.
- The registry and the vault spec are the product core; integrations are
  content on top of them. Registry generalization (compiled defs, external
  collectors, mapping artifacts rendered uniformly) happens as S4 needs it,
  not as a standalone phase.

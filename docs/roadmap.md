# Trove — Roadmap

*2026-09-14, decided with David. Supersedes `docs/post-wave-roadmap.md` for
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

## What is kept in this build

- **Local Mac readers:** Apple Health export, Chrome and Safari history,
  iMessage, Calls, Calendar and Reminders (EventKit), Screen Time and Now
  Playing (Biome), Music library, Podcasts, Books, Apple Mail, folder scans
  (Downloads, Dropbox, iCloud Drive, Google Drive), the finance file
  importer, and the `.mbox` / Slack / Letterboxd one-shot imports
  (Letterboxd stays as the reference import example).
- **Cloud connections:** Google (Gmail, Calendar, Contacts, Tasks,
  YouTube, Books), TickTick, Oura, Fathom, SimpleFIN, Weather (Open-Meteo).
- **Watcher-class, leaving with decision 2:** activity, browser extension
  host, ads observer and identify, Music scrobbler, screenshots.

## Sequence

| Step | What | Status |
|---|---|---|
| S1 | Double-clickable `Trove.app` + `Trove Dev.app` launcher | ✅ 2026-09-14 |
| S2 | Prune the catalog to the keep list; INDEX records restore commits | in progress |
| S3 | Catalog view in the hub, rendered from INDEX at build time | next |
| S4 | Extract the watcher to its own project; delete `troved`; app syncs on open | paused 2026-09-14: `~/Local/trove-collector` scaffolded (standalone binary + extension, compiles, 36 tests; local repo, not pushed). Trove side untouched — still carries troved + watcher modules |
| S5 | Measure each periodic sync's memory in isolation; fix what the daemon leaked | |
| S6 | UI baseline: navigation, theme, layout | |
| S7+ | Data-type passes, one at a time, starting with streams that already have real data: health, messages, browser, calendar, activity (via the external watcher), music | |
| later | The read layer from the old R4: vault-wide search, unified timeline, entity resolution, then LLM analysis over the search index | |

Each data-type pass is: best view for the type → read-path index →
spec-fidelity check against real files → done. A pass is not done until
the view is one a new user would keep open.

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

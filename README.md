# trove

A local-first vault for your own data: a Mac app and a background daemon that pull your personal data out of the apps and services that hold it, write it to plain files on your disk, and keep it there — so that you can finally ask questions of it.

The arc is **collect → store → see → ask**. Collection and storage work. Seeing is thin. Asking — pointing a language model, ideally one running on your own machine, at the whole corpus — is the point of the project and is *not built yet*. This README is specific about which is which.

**Status: prototype, resumed September 2026.** Six weeks of concentrated work in mid-2026, a pause to ship Beat Journal, and now a second pass with a narrower brief: a baseline collect/store/analyze app built out one data type at a time ([`docs/roadmap.md`](docs/roadmap.md)). The importers listed below work and are tested; the ones marked planned are not written.

The thesis is short. The companies that hold your data have every incentive to keep it, and the exports they offer are an afterthought. The counter-move is a program on your own machine that collects everything, writes it in formats any tool can read, and never sends it anywhere unless you say so. Trove is that program, or the start of one.

But collection is the prerequisite, not the goal. The reason to want every message, play, visit, workout, purchase and calendar occurrence in one folder in one set of shapes is that a corpus like that is *answerable*. Each company holding a piece of your life can answer a sliver of a question about it, and does — that is what their models are trained to do. Nobody can answer across the pieces, and you least of all: your own life is the one corpus you cannot query. A complete vault on your disk plus a model you control is the only arrangement where questions like *what actually preceded the months I slept badly*, or *which people drop out of my life when work gets heavy*, or *what did I spend the year of my attention on* can be asked at all without handing the answer's raw material to somebody else in order to get the answer back.

## Where this is going

The end state is a place where your data lives *and* where you interrogate it. Four parts, and the honest status of each:

| | | Status |
|---|---|---|
| **Collect** | 24/7 daemon plus one-shot importers, pulling from the services and files that hold your life | **Works.** The tiers below say exactly which sources have been run against real data |
| **Store** | plain JSONL, CSV and markdown in a folder you own, under an open spec any program can write to | **Works.** This is the strongest part of the project |
| **See** | charts, tables and per-domain views over the streams | **Thin.** Functional, one dark theme, a generic table for anything without a bespoke view |
| **Ask** | a language model pointed at the whole vault — search, a unified timeline, entity resolution, then analysis | **Not built.** This is the destination |

The last row is the reason for the other three. Collection came first because you cannot analyze what you have not gathered, and gathering turned out to be most of the work — but a vault you can only browse is a filing cabinet, and a new user today gets impressive breadth and a weak "so what."

**On which model.** The bias is local — a model running on your own machine, over a corpus that never leaves it, is the only configuration where the privacy claim in this README survives contact with the analysis step. That is the default the design aims at. It is a bias, not a restriction: the vault is plain files, so you can point anything at it, and if you would rather send a scoped slice to a frontier API because it answers better, that is your call to make explicitly. The commitment is that the choice is yours and the default is the private one, never that some capable model is off-limits.

This is a stated goal with a design behind it, not a shipped feature. The sequencing lives in [`docs/roadmap.md`](docs/roadmap.md): split the always-on watcher into its own project, settle a UI baseline, then one deliberate pass per data type, and after that the read layer — vault-wide search, unified timeline, entity resolution, and LLM analysis built on the search index. Nothing in that last phase is written. Judge the repo on the first three rows; the fourth is what it is for.

## What is here

A Rust workspace and a Tauri app:

- [`crates/trove-core/`](crates/trove-core/) is everything: the vault, every collector and importer, the integration registry, the read paths. About 54k lines of Rust after the September 2026 prune (it was 330k; see the next tier below).
- [`crates/troved/`](crates/troved/) is the always-on collector daemon, a small headless binary registered with `launchd`. It and the app share the core crate and coordinate through a file lock so exactly one process collects at a time.
- [`src-tauri/`](src-tauri/) and [`src/`](src/) are the desktop app: Tauri 2, React 19, a thin command layer over the core crate with TypeScript bindings generated from the Rust types.
- [`extension/`](extension/) is a Chrome extension that streams tab activity to the daemon over native messaging. Local-only; it has no network permission.
- [`docs/vault-spec/`](docs/vault-spec/) is the file-format spec: the vault's conventions, one page per data domain, a JSON Schema per record type, and a guide to writing a collector in any language. It is the part of the project I would keep if I had to throw the rest away.

The vault is a folder, `~/Documents/Trove`, of JSONL, CSV and markdown. The files are the source of truth; every index the app builds is under `.trove/` and rebuildable from them. Anything that can read a text file can read a vault, including you, `grep`, and whatever model you point at it.

```
~/Documents/Trove/
  correspondence/<source>/YYYY-MM.jsonl     every message and call, one shape
  tasks/<source>/tasks.jsonl + events/      open-task snapshot + append-only completion stream
  calendar/events/ + calendar/changes/      occurrences + a change stream (reschedules, cancels)
  media/plays/<source>/YYYY-MM.jsonl        listens and watches, one shape
  health/<metric>/YYYY-MM.csv + daily.csv   Apple Health, raw plus a per-day aggregate
  activity/YYYY-MM-DD.jsonl                 app and window spans from the live watcher
  browser/YYYY-MM-DD.jsonl                  visits with duration, from history and the extension
  ...                                       22 domains; see docs/vault-spec/README.md
  .trove/                                   rebuildable indexes, cursors, settings, secrets (0600)
```

## What it ingests

There are three tiers, and the difference between them is the honest part of this README.

### Run against my own data

These were built first, one at a time, and each was validated against the real thing on my machine before the next one was started.

| Source | How | Needs |
|---|---|---|
| Apple Health `export.zip` | streaming XML parse, never extracted; ~600k records/s | nothing |
| Mac app and window activity | CoreGraphics sampler, AFK back-dating, merged spans; the daemon runs it 24/7 | Screen Recording for other apps' titles |
| Chrome and Safari history | copy-then-read of the SQLite files, incremental cursors that rebuild from the vault | Full Disk Access for Safari |
| Browser tab spans | the Chrome extension, via native messaging into `troved` | load unpacked |
| Apple Music plays | observes the `playerInfo` distributed notification; a scrobbler, since Music keeps no history | nothing |
| Screen Time and Now Playing from iPhone/iPad | reads Apple's Biome SEGB segments synced to the Mac, with my own protobuf walker | Full Disk Access |
| iMessage and SMS | `chat.db`, with my own typedstream decoder (the GPL one was off-limits) | Full Disk Access |
| Calls and FaceTime | `CallHistory.storedata` | Full Disk Access |
| Email `.mbox`, Slack export zips | one-shot imports into the correspondence stream | a file |
| TickTick | OAuth pull; the completion stream exists only because each sync diffs against the last snapshot | an app registration |
| Apple Calendar and Reminders | EventKit via `objc2`, all synced accounts, month-sharded snapshot plus a change stream | Calendar/Reminders consent |
| Weather at your location | Open-Meteo hourly, CoreLocation rounded to ~1 km | Location consent, or type a place |

Apple Music library, Apple Podcasts and Apple Books readers were also written against my real databases (the schema findings are in each module's header comment); their scheduled snapshot-and-diff path was not exercised live before the pause.

### Built from documentation and fixtures, never run against a real account — now pruned from the tree

After the core above existed, I wrote the file-format spec, ratified a shared record contract for each domain, and then built out the catalog: **279 provider modules** across health, finance, media, reading, notes, home, developer tools, social, travel and more, each one module plus one registration line. They were written against official API docs and community-documented export formats, and each has fixture tests; 4,400-odd test functions in the crate, most of them here. None of them has been connected to a live account by me. The Google pulls (Gmail, Calendar, Contacts, Tasks, YouTube, Books) and Oura, SimpleFIN and the other OAuth connectors are in this tier: the flows are implemented and unit-tested but no client credentials are compiled in and no real backfill has run. Treat every module here as a starting point that a first real user will find bugs in.

On 2026-09-14 I removed 248 of these from the build (commit `b0e2bce`) to keep Trove to sources that have touched real data. Their code lives at commit `33bda15`, the briefs stay in `docs/integrations/`, and the INDEX marks each one 📦 with the checkout command that brings it back. The app lists them too, under Integrations → Catalog. The Google pulls, Oura, Fathom and SimpleFIN stayed because I have connected them.

The full list, with status per provider, is [`docs/integrations/INDEX.md`](docs/integrations/INDEX.md); each row links to a brief that records the evidence it was built from.

### Planned, or not possible

32 providers are queued and unwritten (Spotify, Robinhood, Airthings, Craft, several dating apps, a password-manager metadata import, and others that were waiting on a sample file or a decision from me). 32 more are catalogued as unavailable with the reason (Apple Journal, Apple Maps history, Plaid, Life360, Kagi, etc.: no export, no API, or an entitlement I cannot get). Both lists are in the same INDEX.

## How the spec and the registry work

Two contracts hold the thing together.

**The vault spec is the data contract.** A collector is any program that writes correctly-formatted files into the vault; a Rust module compiled into the app, a Python script on a cron, an AI agent following the spec are equal citizens, and the app shows their data with no registration. Where many sources mean the same thing (a message, a task, a play, a calendar occurrence) they write one shared record shape, documented in [`docs/vault-spec/domains/`](docs/vault-spec/domains/) with a JSON Schema in [`schemas/`](docs/vault-spec/schemas/), and anything the shape has no column for goes under `extra`. Raw source data is always written first at full fidelity, so a wrong mapping is a re-projection rather than a loss. Cross-source opinions (dedupe, precedence, derived series) happen at read time only and are never persisted. A test, [`spec_validation.rs`](crates/trove-core/tests/spec_validation.rs), round-trips the Rust types, the schemas and the example lines so the spec cannot drift from the code. [`writing-a-collector.md`](docs/vault-spec/writing-a-collector.md) walks through a collector in Python, and a second test extracts that script and runs it.

**The registry is the runtime contract.** Every integration is one `IntegrationDef` static: metadata, one `Behavior` (a periodic pull, a file import, a live collector, a native-messaging host, a pointer to the def that covers it, or unavailable-with-reason), and hooks for permission preflight and "when did this last produce data". Logins are `ConnectionDef`s (OAuth or paste-a-token) that many defs can share; Google is six defs and one login. The daemon's scheduler, the hub cards and toggles, the connect and import UI, the Recent-data view and the TypeScript bindings are all derived from the registry, so adding a source touches nothing else. Invalid combinations do not compile. [`registry.rs`](crates/trove-core/src/registry.rs) has the shape and [`letterboxd.rs`](crates/trove-core/src/letterboxd.rs) is the reference import.

The last thing built before the pause was the [normalizer](docs/normalizer.md): drop an arbitrary CSV or JSONL on the app, it detects the closest contract from the headers (with an opt-in Claude call to suggest a mapping when heuristics are unsure), you confirm the field binding, and the binding persists as a mapping file so future drops of the same shape import themselves. It is a collector authored as data instead of code, and it was the direction the project was heading: the catalog covers known sources, the normalizer extends every contract to unknown ones.

## Running it

macOS only. You need Rust via `rustup` (built with 1.96), Node 22 or newer (built with 24), and the Xcode command line tools.

```bash
git clone https://github.com/david-wills/trove
cd trove
npm install
scripts/build-app.sh         # builds, signs, and installs Trove.app (/Applications and ./Trove.app)
cargo test -p trove-core     # the core tests
```

The first build compiles all of the Rust and takes a while; later builds only recompile what changed. For day-to-day hacking, `npm run tauri dev` runs the app with live reload instead, and `scripts/make-dev-launcher.sh` wraps that in a double-clickable `Trove Dev.app` so you never need a terminal open. The app creates `~/Documents/Trove` on first launch.

The daemon, if you want collection to continue when the app is closed:

```bash
cargo build --release -p troved
./target/release/troved install    # writes a launchd agent; `status` and `uninstall` also exist
```

macOS ties Full Disk Access and Screen Recording grants to a binary's signature, and an ad-hoc-signed `cargo` build changes on every rebuild, so the grants get revoked each time. [`scripts/build-troved.sh`](scripts/build-troved.sh) signs with a stable identity (`TROVED_SIGN_ID`, an Apple Development certificate in your keychain) so you grant once. Without it you re-grant after every build.

Cloud sources need their own app registration: set `TROVE_<SERVICE>_CLIENT_ID` and `_SECRET` at build time, or paste them into the connect card. No credentials of mine are compiled in; the one baked-in pair is Eight Sleep's community-published client id, the same one Home Assistant ships. [`docs/oauth-distribution.md`](docs/oauth-distribution.md) explains the model.

## What it deliberately does not do

- **No network by default.** The app makes no calls of its own. Data crosses the wire only when you connect a cloud source (and then it flows in), plus three narrow cases: the weather collector's Open-Meteo request, an off-by-default lookup that resolves who paid for an ad the extension's observer saw, and the normalizer's opt-in Claude suggestion, which needs your own API key.
- **No sync, no accounts, no telemetry.** The vault is a folder. Back it up like one.
- **No runtime dependency on another app.** No Ollama, no ActivityWatch install, no sidecar processes. Capabilities are absorbed as libraries compiled into the binary: an IMAP client, a FIT parser, an EXIF reader, a Realm reader, a git reader, a PDF extractor, EventKit and iTunesLibrary bindings. This rule survives the local-inference goal above rather than being an exception to it: when a model is wired to the vault it should be compiled in the same way, so a default install has working local analysis with nothing else to install. Pointing an external runtime you already run at the vault stays possible — the files are plain — but it is not to become a prerequisite for the feature to work.
- **No encryption at rest.** Files are plain on purpose; that is what makes them readable by everything. Disk encryption is FileVault's job, secrets live under `.trove/` with `0600`, and the SQLCipher dependency in the tree is for reading Signal Desktop's archive, not for writing ours.
- **No analysis _yet_ — and this one is not a non-goal.** Everything else in this list is a standing design decision. This is sequencing: there is no vault-wide search, no unified timeline, no entity resolution and no model wired to the vault, because collection had to come first. It is the intended destination, not something ruled out — see [Where this is going](#where-this-is-going). The app today is a good collector and a thin viewer.

## Limitations

- **No CI.** Tests run on my machine. `cargo test -p trove-core` and `npm run build` were green at the pause.
- **macOS only, single user, dev builds.** There are no signed or notarized builds and no installer; you build it. Windows and Linux are architecturally possible through Tauri and were never attempted. Apple Health is only reachable through the iPhone's export zip, because HealthKit has no Mac API.
- **Breadth ran ahead of depth, then got cut back.** 279 provider modules that had never seen a real account were produced in a batched, agent-driven build loop against documented API shapes ([`docs/integration-pipeline.md`](docs/integration-pipeline.md) is the doctrine, [`docs/integrations/`](docs/integrations/) the briefs). I reviewed the mechanism closely and the leaf modules lightly; the registry design is what lets any one of them be removed with one line.
- **The permission story is rough.** Grants are per-binary, some panes have no manual-add button so the prompt is the only path, and the fixes for that (embedded usage strings in the daemon, the signing script) came late and were not all validated live. Expect to visit System Settings more than once.
- **Read paths were the last thing fixed.** The Health tab used to hang the app on open; it was refactored to rebuildable indexes and async commands just before the pause, and that convention has not been applied to every view.
- **The frontend is functional, not designed.** One dark theme, one chart wrapper over uPlot, a generic table for any stream without a bespoke view.
- **Fixture-derived tests can pass while the real world has moved.** Apple's on-disk formats (Biome, `chat.db`, the Podcasts store) are undocumented and change with OS releases. Tests pin the formats as observed on macOS 26; a later OS may break a reader silently.

## What this was extracted from

This is my private working repository, exported as a fresh history with employer email addresses replaced by placeholders, the internal handoff documents and session reports left out, and one device-derived test fixture removed. What remains under [`docs/`](docs/) is the design record: the vault spec, the integration doctrine and catalog, the source map, and the plans for the pieces that were specced but not built.

## License

MIT.

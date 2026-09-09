# Vault Conventions — the invariants

Every file a collector writes into a Trove vault must hold these. They are
what make the vault readable by anything, mergeable across collectors, and
safe under concurrent readers.

## Timestamps

- **RFC3339 with the local UTC offset**, e.g. `2026-06-10T14:03:01-07:00`.
  Local time is deliberate: personal data is lived in local time.
- RFC3339 strings **sort lexically** — readers rely on prefix comparisons
  (`ts[..10]` is the day, `ts[..7]` the month). Never write a format that
  breaks this.
- Date-only values are `YYYY-MM-DD`.

## Partitioned JSONL streams

The default shape for anything event-like:

- One file per local **day** (`<dir>/YYYY-MM-DD.jsonl`) for high-volume
  streams, or per **month** (`<dir>/YYYY-MM.jsonl`) for lower volume. A
  record belongs to the partition of its own timestamp.
- One JSON object per line, **newline-terminated**, no pretty-printing.
- **Append-only.** Never rewrite, reorder, or rewrite-to-dedupe a stream
  file. If you might re-run, dedupe *before* appending (see below).
- Sort order inside a file is arrival order; readers sort by `ts` when they
  need strict chronology.

## Snapshot files

For "current state" (open tasks, library contents): a single JSONL file
**rewritten whole, atomically** on every update.

- Atomicity rule, any language: write the full new content to a sibling
  temp file (`<name>.tmp` in the same directory), then **rename it over the
  target**. Never truncate-and-write in place — a reader or crash mid-write
  would see a torn file.
- Pair a snapshot with an append-only `events/` stream when history matters
  (the snapshot+events pattern: `tasks/`, `music/library/`, `podcasts/`).

## Concurrent writers

- Single-line `O_APPEND` writes of ≤ a few KB are atomic enough for streams
  with one writer process (Trove's own collectors coordinate via a global
  single-writer lock).
- A stream with **multiple legitimate writer processes** (`browser/`: the
  history sync plus extension host processes) takes an exclusive `flock` on
  the partition file around its appends. Hold it for the write only.
- External collectors should write only their **own** folders
  (`<domain>/<your-source>/`), which sidesteps contention entirely. Never
  write into `activity/`'s root day files (the live watcher's owned
  stream) or another collector's source folder — an observed-span source
  writes `activity/<your-source>/` like any other domain.

## Fields

- **Omit empty fields** rather than writing `""`/`null`/`0` — files stay
  small and diffs stay honest. (Trove's serializers use omit-if-empty
  throughout.)
- **Unknown fields must be tolerated by every reader** — skip what you
  don't know, never error. Trove's readers also skip unparseable lines.
- Source-specific data the normalized shape has no column for goes under an
  **`extra` object** (string keys, any JSON values). Full fidelity at write
  time: never drop what the source gave you.
- Evolution is **additive only**. A breaking change means a new folder name,
  not a new meaning for an old field. There is no per-record version field —
  the path *is* the schema identifier.

## People: handles, not identities

Anywhere a record references a person — sender, recipient, attendee,
caller, organizer, contact — it records the **raw handle, consistently
shaped**. Resolving handles to *people* (the person graph) is read-time
work for the entity-resolution layer; a wrong merge baked into a stream at
write time is permanent, while a raw handle is joinable forever.

- **Phone numbers: E.164** — a leading `+` then digits only
  (`+14155551234`); no spaces, dashes, or parentheses. Normalize when the
  country is known or derivable (source metadata, an explicit prefix). A
  number that can't be confidently normalized is written as the source gave
  it — never guess a country code.
- **Email addresses: lowercase, trimmed, address only.** Display-name
  decoration (`Ana <ana@example.com>`) never goes in a handle field.
- **Service-native handles** (Slack/Discord/Telegram user ids, social
  usernames): write the source's **stable id** verbatim. Prefer the
  immutable id over the display username where the source distinguishes
  them; lowercase only what the service itself treats as case-insensitive.
- **Display names ride in a sibling `*_name` field** (the
  `sender`/`sender_name` pattern from correspondence) — names are
  presentation, handles are identity, and the two never share a field.
- **No write-time resolution.** No contact lookups, no cross-source merges,
  no "this number is the same person as that email". Collectors write what
  they observed.

## Overlapping sources

Two sources observing the same underlying events (a Gmail pull and an
.mbox import; phone-synced Screen Time and the this-Mac watcher) **both
write, each to its own folder, each with its own stable guids**. A writer
never knows about other sources:

- Never dedupe against *another source's* rows at write time — within-source
  dedupe (below) is mandatory, cross-source dedupe is forbidden.
- Never write into another source's folder.
- Reconciliation — precedence, cross-source dedupe, merging — happens at
  read time in the domain's reader module, where the opinion can change
  without touching data.

One recorded exception predates the rule: `finance/` is a single canonical
ledger — bank syncs and statement imports dedupe into shared per-account
files at write time, because a transaction's identity belongs to the
account, not the observing source. It stays as built; new domains follow
the per-source-folder rule.

## Naming

- Source ids: **lowercase ASCII letters, digits, dashes** (`my-todo-script`,
  `letterboxd`). The source id is the folder name and the `source` field
  value — keep them identical.
- **Sources self-register by folder.** Readers scan `domain/*/`; creating
  `tasks/<your-source>/tasks.jsonl` *is* the registration.

## Dedupe and re-runnability

- Imports must be **re-runnable**: carry a source-unique `guid`/`id` per
  record and skip records already present before appending (read the
  existing files, collect the guid set, append only the new). Overlapping
  exports must never duplicate.
- Incremental collectors keep a cursor in `.trove/<source>-sync.json` —
  and make the cursor **rebuildable by scanning the output files**, so a
  lost cursor never duplicates rows.

## The `.trove/` directory

- Index/state space: cursors, settings, summaries, the manifest. Everything
  here is **rebuildable** — it must never be the only copy of anything.
- Secrets (tokens, credentials) live **only** under `.trove/sync/`, written
  atomically with `0600` permissions before the rename. Never put secrets
  in data folders: the data half of a vault stays safe to share.
- External collectors may keep their own state at
  `.trove/<your-source>-sync.json`; follow the `{"updated": <RFC3339>, ...}`
  shape so the hub can show staleness.

## Read-side conventions (for analyzers and views)

- **Readers scan, never register.** Any source folder that parses is in.
- **Per-domain reader modules are the unification point**: precedence rules
  (extension rows over history rows; phone sessions over synced snapshots)
  are applied at read time, documented, and provenance (`source`) is always
  preserved in outputs. Nothing derived is persisted.
- Domains conventionally expose a reader trio — `*_timeline(date)`,
  `*_summary(from, to)`, `*_daily(from, to)` — which generic UI and future
  analyzers can assume.
- Machine summaries live at `.trove/<domain>-summary.json` (rebuildable);
  `.trove/manifest.json` is the index of indexes — what domains exist, their
  sources and date ranges.

## Read cost: O(displayed), not O(stored)

Data is partitioned by day or month (see above) precisely so reads don't
have to pay for the whole stream. Vault volume must be free to grow
unbounded without slowing any view.

- **Views read the newest partitions until the page fills**, not every
  partition on file: walk partition keys newest-to-oldest and stop once
  `limit` records are collected. A timeline or list read costs O(what's
  displayed), never O(what's stored).
- **Cross-partition aggregates don't re-scan on every read.** A metric
  catalog, a totals card, a chart spanning months — anything that needs a
  fact about the *whole* stream — comes from a rebuildable
  `.trove/<domain>-summary.json` index: small, per-day rows keyed by
  source, kept fresh by comparing each source file's size+mtime against a
  stamp and re-folding only what changed.
- Build and refresh that index with the shared `ensure_index` helper
  (`crates/trove-core/src/store.rs`) rather than hand-rolling the
  stat-then-rebuild loop per domain — it owns the staleness check, the
  atomic write, and the version-bump-triggers-full-rebuild path.

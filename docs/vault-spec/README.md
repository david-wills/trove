# The Trove Vault Format

A Trove vault is a plain folder (`~/Documents/Trove` by default) of open-format files —
JSONL, CSV, markdown. **The files are the source of truth**: every database,
index, and summary is rebuildable from them, and anything that can read a
text file can read a vault.

This spec exists because of one principle:

> **A collector is any program that writes correctly-formatted files into
> the vault.** A Rust module compiled into Trove, a Python script on a cron,
> a CLI, an AI agent — all equal citizens. If the files are right, the data
> shows up in the app, with no registration, no plugin API, no SDK.

That makes this document the vault's *real* extension surface. Read
[`conventions.md`](conventions.md) for the invariants every file must hold,
[`writing-a-collector.md`](writing-a-collector.md) for a working end-to-end
example, and [`domains/`](domains/) for the per-domain record formats.

## The two layers (lossless import)

Trove separates *what was collected* from *what it means*:

1. **Raw layer — full fidelity at write time.** A source may keep files in
   its own native shape (e.g. `health/oura/<collection>.jsonl` holds raw
   Oura API records). Nothing is projected away on import; a schema mistake
   in a normalized view can be fixed later, but data dropped at write time
   is gone forever.
2. **Contract layer — normalized, multi-source.** Where many sources mean
   the same thing (messages, tasks, media plays), they write a shared
   record shape documented in [`domains/`](domains/), with anything the
   shape has no column for preserved under an `extra` object. Readers scan
   the domain's folders — every source folder that parses is in.
3. **Opinions — read time only.** Cross-source unification, precedence
   rules, dedupe, derived series all happen when data is *read*, inside the
   app (or your own tooling). Nothing derived is persisted back, so opinions
   can change without touching the data.

## Stability promise

- Record formats evolve **additively**: new optional fields may appear,
  existing fields keep their meaning. Readers must tolerate unknown fields.
- A breaking change is expressed as a **new folder/layout**, never a
  reinterpretation of an existing one.
- The JSON Schemas in [`schemas/`](schemas/) are validated against the Rust
  types and the example lines in CI (`crates/trove-core/tests/spec_validation.rs`),
  so this spec cannot silently drift from the implementation.

## Layout at a glance

| Path | What |
|---|---|
| `correspondence/<source>/YYYY-MM.jsonl` | every message/call, normalized ([spec](domains/correspondence.md)) |
| `tasks/<source>/tasks.jsonl` + `events/YYYY-MM.jsonl` | tasks, snapshot+events ([spec](domains/tasks.md)) |
| `media/plays/<source>/YYYY-MM.jsonl` | media plays write contract ([spec](domains/media-plays.md)) |
| `calendar/events/` + `calendar/changes/` | calendar occurrences + change stream ([spec](domains/calendar.md)) |
| `activity/YYYY-MM-DD.jsonl` | Mac app/window spans ([spec](domains/activity.md)) — **single-writer, owned by the external collector**; imported observed-span histories write `activity/<source>/` subfolders instead |
| `browser/YYYY-MM-DD.jsonl` | web visits with duration ([spec](domains/browser-visits.md); multi-writer with flock, see conventions); sibling streams `browser/ads/` ([spec](domains/ads.md)) and `browser/searches/` ([spec](domains/browser-searches.md)) |
| `health/<metric>/YYYY-MM.csv` + `health/oura/` | the raw layer: Apple Health per-metric CSVs, Oura API records verbatim ([spec](domains/health.md)) |
| `health/sleep/<source>/YYYY-MM.jsonl` | sleep sessions, normalized across trackers ([spec](domains/health-sleep.md)) |
| `.trove/` | rebuildable indexes, cursors, settings, secrets — see conventions |
| `.trove/manifest.json` | rebuildable index of every data folder present |

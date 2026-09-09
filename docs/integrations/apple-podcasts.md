# Apple Podcasts

- **id:** `apple-podcasts` (shipped def id: `podcasts`)
- **domains:** taxonomy: `media/plays/`; shipped writes the
  **grandfathered** `podcasts/` path (`episodes.jsonl` snapshot +
  `events/YYYY-MM.jsonl` diffs) — in the closed grandfathered set, with the
  consolidated tidy-up migration scheduled post-wave; the set must not grow
- **status:** 🧪 built (shipped pre-pipeline; David promotes to ✅)
- **unavailable_reason:** none
- **behavior:** Periodic (local SQLite read, copy-then-read; no network)
- **connection:** none (local data; Full Disk Access TCC grant instead)
- **evidence:** community-schema, high confidence — MTLibrary.sqlite
  `ZMTEPISODE`/`ZMTPODCAST` schema community-documented and stable across
  macOS versions; the shipped collector is itself working evidence
- **effort / priority:** M / P2
- **needs:** extension — surface ZPLAYCOUNT/ZLASTDATEPLAYED play data

## What it is

Apple's built-in podcast app — the default for Mac/iPhone users who never
chose a player, so it covers the broadest slice of podcast listeners with
zero accounts or logins. Structural limit: the database keeps only the
**most recent play date and a play count** per episode, never a per-play
history — for timestamped episode-level history Overcast is strictly
better (see its brief).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Episode library snapshot | local app users | episode title, feed title, duration, play signal | shipped collector |
| Diff events | local app users | append-only changes between snapshots | shipped collector |
| Play stats (extension) | local app users | ZPLAYCOUNT (total plays), ZLASTDATEPLAYED (Core Data epoch: secs since 2001-01-01) | community schema |
| Per-play history | — | does not exist in the DB | research doc |

## Access & auth

- Local SQLite: `~/Library/Group Containers/243LU875E5.groups.com.apple.
  podcasts/Documents/MTLibrary.sqlite`; key table `ZMTEPISODE`
  (ZLASTDATEPLAYED, ZPLAYCOUNT, ZDURATION, ZTITLE) joined to `ZMTPODCAST`.
- Full Disk Access (already granted for iMessage/Safari collectors — no
  new prompt); copy-then-read against WAL, as shipped.
- Fully offline; standalone-clean by construction.

## Vault mapping

- **Raw layer / current shape:** `podcasts/episodes.jsonl` (every episode
  with play signal, atomic snapshot) + `podcasts/events/YYYY-MM.jsonl`
  (append-only diff events) — grandfathered paths; the path is the schema
  identifier, so no rename until the post-wave migration.
- **Contract layer:** none today. Post-wave, play-shaped events map onto
  the ratified media-plays contract under `media/plays/` (`category:
  "podcast"`, `seconds` from duration/progress, episode id as `guid`);
  library snapshots stay raw-only per the taxonomy.
- **Dedupe:** snapshot-diff drives the event stream; episode identity from
  the DB row.

## Build plan

Shipped (`crates/trove-core/src/podcasts.rs`, registered, Periodic).
Remaining work, in order:

1. Extension: read ZPLAYCOUNT + ZLASTDATEPLAYED (Core Data epoch
   conversion) into the snapshot/events so play stats are surfaced, not
   just durations — the catalog's flagged gap.
2. Post-wave migration (scheduled, not now): fold the grandfathered
   `podcasts/` paths into the taxonomy layout per the pipeline doc's
   After-the-wave section.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Snapshot + diff events | 🧪 built (pre-pipeline, fixture-tested; David promotes to ✅) | with FDA granted, Sync now; confirm `podcasts/episodes.jsonl` + hub last-data; play an episode, next sync appends a diff event |
| Play-stats fields | — | after the extension: play an episode twice; confirm count/last-played update in the snapshot |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§Apple Podcasts (L3270–L3276) + the podcast-depth cross-cutting note
(Overcast > Apple Podcasts > Pocket Casts > Castro). Feasibility 🟢 high.
Schema stable across macOS versions; audio cache lives in the same Group
Container but is never copied into the vault. For users wanting real
history, the in-app suggestion is Overcast's free tier.

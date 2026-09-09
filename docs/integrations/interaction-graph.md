# Interaction Graph (derived)

- **id:** `interaction-graph`
- **domains:** `contacts` (contract: **not yet ratified** — Phase 3 contacts
  pass; the graph rows are a Trove-defined derived shape, specced in the
  same pass)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (recompute on the slow tick; pure local derivation
  — no external source at all)
- **connection:** none
- **evidence:** none needed — pure derivation over vault data Trove already
  collects (correspondence, calendar, call history); no external API
- **effort / priority:** M / P0
- **needs:** Needs-David (contract: contacts / graph shape) — otherwise
  none; zero new permissions, reads only data the user already opted to
  collect

## What it is

Not a collector — a derived layer. Trove already holds iMessage, email,
Slack, call history, and calendar attendees in the vault; this pass
extracts every participant handle, counts interactions with recency decay,
and produces a ranked "who matters most" view of the user's relationships.
The highest-signal relationship intelligence in Trove, for zero new
permissions and zero new data leaving anything. It's what turns the flat
contacts list into a person layer.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Ranked interaction graph | needs ≥1 correspondence/calls/calendar source collected | normalized_id, display_name, interaction_count, last_interaction, per-source counts, strength score | derivation (research L672-678) |
| Weekly trend | same | per-person interaction counts over time | derivation |
| Identity hints | with apple-contacts present | handle → contact-record joins feeding entity resolution | derivation |

Quality scales with how many sources the user has enabled; with none it
shows an honest "enable Messages/Mail/Calls first" empty state.

## Access & auth

- Reads vault files only: `correspondence/**` (iMessage, email, Slack,
  calls rows), `calendar/`, plus `contacts/*/` records for name
  resolution. All reads through `trove-core` store paths; no TCC, no
  network, no connection.
- Normalization: E.164 phone parsing (`phonenumber` crate), lowercase+trim
  emails, `mailto:` stripped from email-shaped iMessage handles.
- Recency decay: exponential, 30-day half-life, per the research
  recommendation.
- Reads must follow the reads-scale-to-displayed rule: the recompute is a
  bounded scan (vaults are small enough per research, but keep it
  async + spawn_blocking and watermark-aware if it grows).

## Vault mapping

- **Raw/derived layer:** `contacts/interaction-graph/graph.jsonl` (one
  person per line: `normalized_id`, `display_name`,
  `interaction_count`, `last_interaction`, `sources[]`, `strength`) and
  `contacts/interaction-graph/weekly.jsonl` (trend rows). *(Research doc
  said flat `contacts/interaction-graph.jsonl`; taxonomy + identity
  convention win: per-source folder.)* Fully derived and idempotently
  regenerated — files-as-truth still holds because the inputs are vault
  files; the graph is rebuildable from them at any time, like an index
  that happens to be worth reading.
- **Contract layer:** none of the ratified contracts apply (it's not
  events, tasks, plays, or calendar); the Phase 3 contacts pass specs the
  graph shape alongside the contacts contract.
- **Dedupe:** `normalized_id` (typed key: phone | email | handle) is the
  natural key; full rewrite per recompute, no append-dedupe needed.

## Build plan

1. Module `crates/trove-core/src/interaction_graph.rs` (def id
   `interaction-graph`): `DEF` (Periodic), permission hook = trivially
   granted, last-data hook = graph file mtime, pull hook = recompute.
2. Registration line in `INTEGRATIONS`. Hub card copy explains it's
   derived ("built from your Messages, Mail, Calls and Calendar — nothing
   new is read or sent").
3. Core: `HashMap<NormalizedKey, PersonRecord>` accumulation over
   correspondence + calendar partitions; 30-day half-life scoring; join
   against `contacts/*/` records for display names (graceful when
   apple-contacts isn't built/enabled yet).
4. Sequencing: build immediately after `apple-contacts` (research note 5 —
   the graph is what makes the contacts pull useful); shares normalization
   code with the entity-resolution exact-match layer — one helper, two
   consumers.
5. Fixtures: small synthetic vault with iMessage/email/calls/calendar rows
   exercising phone-vs-email identity overlap and decay math; unique temp
   dirs.
6. Parked behind **Needs-David (graph shape sign-off)** with the contacts
   contract.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Graph build | — | vault with real iMessage + email + calls data; Sync now; confirm `contacts/interaction-graph/graph.jsonl` ranks the people David actually talks to most |
| Decay behavior | — | confirm a frequent-but-stale contact ranks below a recent regular |
| Empty state | — | fresh vault with no correspondence sources: card shows the enable-sources hint, no files written |

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§Interaction Graph (L672-678); at-a-glance L649; cross-cutting notes 2 and
5. Feasibility 🟢 high — "costs nothing new". P0 as the force multiplier
for the whole contacts domain. Distinct from `entity-resolution` (separate
catalog entry): the graph counts interactions per handle; the resolver
merges handles into persons — the graph consumes the resolver when both
exist but must degrade gracefully without it. Not time-sensitive
(recomputable forever from the vault).

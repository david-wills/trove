# Trove — Post-Wave Roadmap

*2026-07-16, designed with David. The successor to
`docs/integration-pipeline.md` for **sequencing**: the wave that doc governed
is complete, and this doc governs what comes next. The pipeline doc's
write-time doctrine (contracts, conventions, evidence hierarchy, status axes)
still stands — nothing here reopens it.*

## Where the wave left us

Phase B closed 2026-06-21 with the buildable queue driven 167 → 0. Live
catalog tally: **279 🧪 built / 32 📋 queued (all David-gated) / 32 🚫
unavailable**. The mechanism-vs-content split is healthy: the registry
machinery (`registry.rs`, `integrations.rs`, `store.rs`, `contracts.rs`,
`runner.rs`) is ~3,300 lines — about 1% of trove-core — while integration
modules are ~315k lines of *leaf code*, each removable with one registration
line. Nothing structural depends on any single integration.

What the wave did **not** build is the analyze half of "collect, organize,
and analyze": there is no vault-wide search, no unified timeline, no entity
resolution, and no LLM analysis. The Health tab still hangs the app on open
(plan specced in `docs/health-refactor.md`, unexecuted). A new user today
gets impressive breadth and a weak "so what."

## Doctrine (settled with David 2026-07-16 — do not relitigate)

1. **The wave is closed.** Do not re-launch the build loop or expand the
   catalog for completeness. New integrations from here are **demand-driven**
   (David's real use, user/contributor requests) — never catalog-driven.
2. **Depth over breadth.** The product core is two contracts: the **vault
   spec** (the open data contract — any program, any language) and the
   **registry** (the runtime contract — scheduling, auth, TCC permissions,
   hub presence). Integrations are content on top of that core. Investment
   goes into the core until the read/analyze story matches the collect story.
3. **The registry stays and generalizes.** Its next evolution is not more
   compiled entries but *kinds* of entries: compiled `DEF`s, external
   collectors (today anonymous `.trove/manifest.json` folders), and mapping
   artifacts (`.trove/mappings/`, once R2 ships) rendered uniformly in the
   hub. Compiled Rust becomes one implementation of a source, not the
   definition of one.
4. **Drop-in-anything is a return to the original vision, not a pivot.**
   "Users import their own data and create their own categories" predates the
   integration catalog. The normalizer (R2) is how the supported-set goal and
   the import-anything goal fuse: contracts cover known sources; the
   normalizer extends every contract to unknown sources of the same shape.

## The phases

Sequenced; each gate is David's. R2 and R4a are independent of each other —
R4a (search) may be pulled ahead if user-visible value is wanted sooner.

### R1 — Read-path foundation (execute `docs/health-refactor.md`)

The specced plan, all four steps: async-ify the 7 health commands, the
rebuildable `.trove/oura-summary.json` index, then **Step 3 generalized** —
the shared `ensure_index` helper in `store.rs`, the async +
`spawn_blocking` sweep of every vault-touching command, and the conventions
written into `docs/vault-spec/conventions.md` + `CLAUDE.md` hard rules.
Two principles must hold vault-wide before anything is built on top:
**reads cost O(displayed), not O(stored)** and **vault I/O never blocks the
main thread**.

*Gate:* Health tab opens instantly against the real vault; no synchronous
vault-file command remains in `src-tauri/src/lib.rs`.

### R2 — Drop-in data (the detect-and-map normalizer)

The mechanism specced in `integration-pipeline.md` § "After the wave", now
buildable because its design corpus exists: the wave's ~290 hand-written
parse→map→write mappings. v1 scope, deliberately narrow:

- User drops a CSV/JSONL file → **detect** the closest ratified contract
  (header heuristics, escalating to an at-import LLM classify-and-suggest —
  the AI-native bet) → user confirms/edits the field binding → binding
  persists as `.trove/mappings/<source>.json` → future drops of the same
  shape auto-conform.
- **Raw is always written full-fidelity first.** A wrong mapping is a
  re-projection, never data loss.
- Contract field metadata (`description` + unit + `examples` in the JSON
  Schemas — the Phase-4 carry-forward) is the normalizer's API surface.
- A mapping file is a **shareable integration authored as data** — the
  community-extension story with no plugin SDK.

*Gate:* David drops a real foreign export (a service Trove has no module
for) and reads it in the matching domain view without writing code.

### R3 — Registry generalization (three source kinds, one hub)

Hub cards, last-data probes, and toggles for all three kinds of things that
feed the vault: compiled defs, external collectors (promoted from anonymous
manifest folders to named cards), and R2's mappings. `IntegrationDef`'s
metadata shape is already what all three need; this phase makes the
non-compiled kinds first-class rather than inventing a parallel system.

*Gate:* an external collector's folder and a normalizer mapping both render
in the hub with name, freshness, and toggle — indistinguishable in dignity
from a compiled integration.

### R4 — The read layer

In value order, each designed against the full corpus per the standing
2026-06-10 decision:

- **R4a — vault-wide search** (the cheapest thing that makes ~300 streams
  feel like one product; brings the SQLite index with its trigger feature).
- **R4b — unified timeline** across domains.
- **R4c — entity resolution** (exact-match ships with the contacts
  integration as the proof the identity convention suffices).
- **R4d — LLM analysis** (the v0.2 promise: local-first inference over the
  vault, cloud by explicit choice) — built on R4a's index.

*Gate per feature;* R4 is where "analyze" stops being aspirational.

## Standing policies

- **🧪 policy — no bulk validation project.** Unvalidated integrations are
  marked honestly in the hub; promotion to ✅ happens only through real use
  (David's or users'). Some 🧪 modules will rot as third-party APIs drift —
  acceptable; fix on demand, delete on evidence of permanent breakage.
- **📋 triage — one pass, then silence.** The 32 David-gated queue items get
  a single park-or-want decision pass from David; whatever is parked stays
  parked without a standing queue to tend.
- **Punch list carry-forward.** The pipeline doc's post-wave items remain
  valid and slot in after R1: the grandfathered-path folder migration, the
  validation-findings punch list, and the thin raw viewer (subsumed by
  R2/R3).

## Decisions log (2026-07-16)

- Wave closed; catalog breadth frozen; integrations demand-driven from here.
- Registry retained — it is the runtime half of the core, not wave debris;
  its evolution is data-driven entries (R3), not deletion.
- Sequencing: R1 (read-path foundation) → R2 (normalizer) → R3 (registry
  generalization) → R4 (search → timeline → entities → LLM analysis); R4a
  may be pulled ahead of R2 at David's call.
- No bulk validation of the 🧪 pile; honest labeling + promote-by-use.
- `docs/health-refactor.md` adopted as the R1 spec (moved from repo root).

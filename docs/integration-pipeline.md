# Trove — Integration Pipeline

> **Status (2026-07-16): the wave this doc governs is complete** (Phase B
> closed 2026-06-21; queue 167→0). Post-wave sequencing now lives in
> [`post-wave-roadmap.md`](post-wave-roadmap.md). The write-time doctrine,
> evidence hierarchy, and status axes below still stand.

*2026-06-12, designed with David. The master plan for scaling Trove from ~33
integrations to the full research corpus (`docs/integrations-research.md` —
396 researched sources across 17 domain catalogs, combining to roughly 150
provider entries). The decisions below are settled; do not
relitigate them mid-pipeline. The autonomous build loop's standing contract is
`docs/collector-loop.md`; per-provider build briefs live in
`docs/integrations/`.*

## Goal & doctrine

**Every source in the research corpus becomes a visible provider entry in the
app — including infeasible ones**, which appear with the reason they're
unavailable. Overshoot, don't undershoot: the app itself answers "is X
integrated, and if not why not?" so nobody re-scours the web. Sources are
**combined by provider** wherever sensible (Telegram export + MTProto API =
one entry; Google = six defs, one connection — the model).

The architectural insight the pipeline rests on: **"organizing" splits in
two, and the halves go on opposite sides of the collection wave.**

- **Write-time organization** — domain contracts, identity conventions,
  taxonomy — must precede the wave, because retrofitting it means touching
  every module and migrating every stream.
- **Read-time organization** — person graph, unified timeline, search,
  SQLite index, dedup/precedence opinions — deliberately **follows** the
  wave. It's all derived and rebuildable (vault-spec layer 3), and designing
  it against the full corpus beats designing it against a third of it.

The raw layer is the safety net under everything: full fidelity at write
time means a wrong or missing contract is never data loss — normalized
layers can be re-derived. Expect a post-wave punch list (agents interpret
specs slightly differently), not a migration crisis.

## Evidence hierarchy (how integrations get built without David's data)

What an agent needs is **payload-shape evidence**, in strict preference
order:

1. **Official API documentation with example responses** — first and
   foremost, wherever available. Build parsers and test fixtures from the
   documented shapes; no account needed.
2. **Community-documented schemas** — local DBs and semi-documented export
   formats (Things 3 SQLite, Takeout JSON). Usable; the brief records the
   confidence level and source of the documentation.
3. **Real sample files** — required *only* for undocumented export formats
   (folklore schemas). Never write an import parser blind: build everything
   around it (DEF, card, import box, brief) and flag the parser
   **Needs-sample**. Samples may come from David or any future
   user/contributor.

## Capabilities & the optional-fields rule

Minimal-required-core + everything-else-optional is the vault-wide rule, not
a per-service special case. Contracts require only what identifies a record
(`ts`, `source`, `guid`, the thing's identity); every enrichment is optional
with omit-if-empty. Tiered services (e.g. Granola: summaries free,
transcripts on Business) are absorbed by default — a free-tier user's rows
simply carry fewer fields. The knowledge layer lives in each brief: a
**capabilities table** (what the service can yield, per plan/tier, from the
docs) and a **validation matrix** (which slices are confirmed on real data
vs. fixture-only).

## Status & validation axes

Built and validated are separate axes. Per-provider status:

| Status | Meaning |
|---|---|
| 🚫 unavailable | catalogued; cannot/should not be built — reason shown in-app |
| 📋 queued | brief exists, not started |
| 🚧 building | a loop iteration owns it |
| 🧪 built | code-complete, fixture-tested, **not** validated on real data |
| ✅ validated | confirmed against a real account/file — **only David promotes 🧪 → ✅** |

Validation is sliced by capability: an integration can be ✅ for summaries
and 🧪 for transcripts. Every brief carries concrete validation steps
(commands, what to look for, which login/permission). The hub's last-data
hooks are the live evidence trail.

## The four phases

### Phase 1 — Foundation gate (small; mostly spec-writing)

Lock the cross-domain write-time conventions — the only things expensive to
retrofit across 100+ integrations:

1. **Identity/handle convention** — every stream records people as raw,
   consistently-shaped handles (E.164 phones, lowercase emails, source-native
   handles), never prematurely resolved. Added to
   `docs/vault-spec/conventions.md`.
2. **Domain taxonomy** — the research doc's 17 domain catalogs mapped to
   vault folders, recorded in `docs/integrations/README.md`. Marks which domains
   are known-multi-source (contract needed) vs. single-source (raw-only).
3. **Conventions audit** — confirm guid/dedup, timestamps, partitioning rules
   cover the wave; write down the overlapping-sources rule (each source
   writes its own folder with stable guids; reconciliation is read-time).
4. **Registry availability state** — a way for an `IntegrationDef` to be
   catalogued-but-unavailable with a reason string, so the hub renders the
   card greyed with "why not available" copy (the disabled-controls-need-
   affordance rule). Plus a hub UX decision for ~100 cards (grouping /
   show-unavailable toggle).

Gate: David reviews all four.

### Phase 2 — Catalog pass (parallel fan-out; mechanical)

Distill `integrations-research.md` into the catalog:

- One **brief** per provider at `docs/integrations/<provider>.md`
  (template + worked example in that directory), combining that provider's
  mechanisms/sources into one entry.
- One **registry stub** per provider (unavailable ones get the reason;
  feasible ones get `NotWired` until built) so the full catalog is visible
  in-app immediately.
- The **queue**: `docs/integrations/INDEX.md`, every provider with status,
  domain, needs-flags, and build order (priority from the research doc's
  P0–P2, time-sensitivity weighted, first-in-domain entries sequenced
  early). Ordering the queue = editing this one file. **Privacy-sensitive
  is a mandatory needs-flag** wherever the research flags it (dating
  apps, genetics, clipboard, mic dB, voicemail transcripts, message
  bodies, financial detail, location trails) — those ship opt-in with
  explicit acknowledgement, and password-manager imports hard-strip
  secrets at parse time.

Gate: David reviews the index, the queue order, and spot-checks briefs.

### Phase 3 — Contract pass (one agent per multi-source domain)

For each known-multi-source domain without a contract (meetings, location,
reading, social-posts, photos-metadata, home/IoT, environment, …): one agent
reads the research entries **plus the official docs/examples for every
queued source in that domain**, then drafts the narrowest contract the
converging sources actually need — spec page in `docs/vault-spec/domains/` +
JSON Schema + example lines wired into `spec_validation.rs`. Granularity
matters: a domain may hold several record shapes (calendar = events +
changes); draft only the shapes the queued sources converge on (location
gets trails now; visits waits for a visits-shaped source).

Designing from all queued sources at once beats first-in-domain drafting
from one source — and it batches every contract review into David's daytime
instead of gating the overnight loop.

Gate: David ratifies the contract batch. Existing ratified contracts
(correspondence, tasks, media-plays, calendar) are not reopened;
cross-domain unification ("conversations = correspondence + meetings") is a
read-time view, never a migration.

### Phase 4 — Build loop (the overnight wave)

The autonomous loop works down `INDEX.md` one provider at a time per
`docs/collector-loop.md`: each integration is built on its own short-lived
worktree/branch and, once its gate is green, merged by the loop into the
long-lived `integration-staging` branch (branch + worktree then deleted).
Flags are raised instead of stopping. David reviews staging and merges it
into the `integrations` hub branch on his go-ahead, and `integrations` →
`main` at milestones — the loop touches neither, and no branch sprawl
accumulates (branch map: `docs/integrations-plan.md`). Contract drafting
mid-loop survives only as the escape hatch
for surprises (additive extension or flag — never an overnight redesign).

### After the wave — read-time organization

Designed against the full corpus, exactly as decided 2026-06-10: entity
resolution joins (exact-match ships early — with the contacts integration —
as the *proof* that the identity convention suffices), unified timeline,
vault-wide search, the SQLite index when its trigger features arrive, and
the post-wave punch list from validation findings.

Also post-wave, and a read-time-organization feature by the same logic: the
**generic data browser and the import-time normalizer** (the "no-code
collector"). One concept that splits in two, deliberately not conflated:

- A **raw viewer** — renders any un-contracted `domain/<source>/` folder
  (whatever `.trove/manifest.json` lists) as a table/timeline. Cheap; a thin
  version is worth pulling *forward into* the wave so the loop's raw-only
  escape-hatch parks (`Needs-David (contract)`) are visible as they land
  rather than blind until later. The full version is post-wave.
- The **detect-and-map normalizer** — a user drops arbitrary data, a parser
  detects the closest ratified domain and offers to bind the upload's fields
  onto the contract columns, persisting the binding as a declarative vault
  artifact (`.trove/mappings/<source>.json`) so future pulls of that type
  auto-conform. Raw is always written full-fidelity first, so the mapping is
  a read-time opinion — wrong → re-project, never data loss. It is a
  collector authored as data instead of code, and it is what fuses the
  supported-set goal and the import-anything goal into one axis: contracts
  cover known sources, the normalizer extends every contract to unknown
  sources of the same shape.

Sequenced after the wave for this section's standing reason — design it
against all ~290 of the wave's hand-written parse→map→write mappings (its
real spec) rather than guessing a mapping language up front. The hard,
uncertain part is *detect* (the AI-native bet — header heuristics escalating
to an at-import LLM classify-and-suggest), not map-and-persist.
`notion-airtable`'s planned connect-time field-mapping step is the prototype
the generic mechanism should subsume; the loop must **not** build a bespoke
per-provider mapping UI (flag/raw-only instead) so no throwaway version
accrues. **Phase-4 carry-forward that enables this:** every contract field a
collector binds or additively adds must ship `description` + unit +
`examples` in its JSON Schema — that field metadata is the normalizer's API
surface (see `docs/collector-loop.md` → Definition of done).

Also on the post-wave punch list (decided with David 2026-06-12): the
**grandfathered-path tidy-up** — one consolidated folder-move migration of
the pre-taxonomy streams (the closed set enumerated in
`docs/integrations/README.md`: `weather/`, `youtube/`, `books/`,
`books/google/`, `music/library/`, `podcasts/`) into their taxonomy
homes, carrying sync cursors and verifying guid/row parity. Done once,
after the wave proves the map stable — never mid-wave, and the
grandfathered set must not grow in the meantime.

## Decisions log (settled 2026-06-12 — do not relitigate)

- Catalog **everything**, including hard-blocked sources, with in-app
  unavailable reasons.
- Combine by **provider**; many defs may share one connection (Google
  model).
- **Build isolated, land on staging**: per-integration short-lived
  branches, loop self-merges green work into `integration-staging` and
  cleans up; David alone merges staging → `integrations` → `main`.
- **Catalog pass runs as its own phase** before any building.
- **Contracts are drafted upfront in Phase 3** from documented payload
  shapes across all of a domain's queued sources — not mid-loop.
- Evidence hierarchy: official docs → community schemas → real samples;
  blind import parsers are forbidden (Needs-sample instead).
- Built vs. validated are separate axes; only David promotes to ✅.
- Read-time organization (timeline/search/index/person-graph features)
  stays post-wave.

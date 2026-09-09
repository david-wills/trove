# Integrations — Execution Playbook

*2026-06-12. The step-by-step process for executing the integration
pipeline. Doctrine and phase definitions: `docs/integration-pipeline.md`.
Loop contract: `docs/collector-loop.md`. Briefs/queue: `docs/integrations/`.
This file is David's checklist — what to kick off, what each gate looks
like, and what only he can do.*

## Branch structure

```
main                    ← stable; David alone merges into it
└── integrations        ← the hub for all pipeline work (Phases 1–4 land here)
    └── integration-staging   ← the loop's landing branch (Phase 4), cut from integrations
        └── integration/<provider>  ← short-lived per-provider build branches,
                                      self-merged into staging when green, then deleted
```

Flow: loop work accumulates on `integration-staging` → David approves →
merge into `integrations` → when a milestone is finalized, merge
`integrations` → `main`. The loop never touches `main` or `integrations`.

## Phase 0 — setup (done / in progress)

- [x] Pipeline docs reviewed and committed (`integration-pipeline.md`,
  `collector-loop.md`, `docs/integrations/`, this file).
- [x] `integrations` branch created off `main`.
- [ ] **Start the slow exports now** (multi-day lead times): Google
  Takeout, Apple privacy export (privacy.apple.com), GDPR archives (Meta,
  X, Reddit, Discord…). Drop arrivals into `~/Documents/Trove-samples/` — this is
  what un-blocks Needs-sample parsers later.

## Phase 1 — Foundation gate (one supervised session)

1. On `integrations`, say: **"Execute Phase 1 of
   docs/integration-pipeline.md."** Deliverables: identity/handle
   convention (`conventions.md`), 24-domain taxonomy
   (`docs/integrations/README.md`), conventions audit, registry
   unavailable-state + hub UX proposal for ~100 cards.
2. **Gate (David):** review all four. Highest stakes: the **taxonomy**
   (pre-decides where every integration writes) and the **hub UX** (you
   look at it daily). Approve → commit on `integrations`.

## Phase 2 — Catalog pass (one day; fan-out, light review)

3. Say: **"Run the Phase 2 catalog pass — use a workflow."** Parallel
   distillation of `integrations-research.md` into ~80–100 provider briefs
   + registry stubs (unavailable ones carry their reason) +
   `docs/integrations/INDEX.md`.
4. **Gate (David, ~1–2 h — the highest-leverage review):**
   - `INDEX.md` queue order — impose your priorities; time-sensitive
     sources early; first-in-domain entries before their followers.
   - Spot-check ~5 briefs (one unavailable, one OAuth, one import, one
     local-DB).
   - Read every `unavailable_reason` — that copy ships in the app.
5. Approve → commit. The app now answers "is X integrated, and why not?"
   for everything, before any new collector exists.

## Phase 3 — Contract pass (one session; ratification is the real work)

6. Say: **"Run Phase 3 — draft contracts for the multi-source domains in
   the taxonomy."** One agent per domain (meetings, location, reading,
   social-posts, photos-metadata, home/IoT, environment, …) reads the
   research + official docs for *all* queued sources in its domain and
   drafts spec page + JSON Schema + `spec_validation` examples.
7. **Gate (David — ratification):** for each draft: is the required core
   truly minimal? is the granularity right (one row = one *what*)? is
   anything important relegated to `extra`? Do a few per sitting (~10
   contracts total). Once ratified, agents write these shapes
   unsupervised — last cheap moment to change them.

## Phase 4 — Build loop (overnight, repeated)

8. **Once, at launch:** create `integration-staging` off `integrations`.
9. **Pre-flight, each loop night (5 min):**
   - `pmset -g sched` shows no "Repeating power events"
     (`sudo pmset repeat cancel` if it does); optional `caffeinate -ims`.
   - Next stretch of `INDEX.md` has ratified contracts.
   - Newly-arrived exports dropped into `~/Documents/Trove-samples/`.
10. **Launch:** in a session on `integration-staging`, say: **"/loop —
    work the integration queue per docs/collector-loop.md"** (self-paced).
    The loop: picks providers top-down, builds each on its own short-lived
    branch, self-merges green work into staging (then deletes the
    branch/worktree), flags instead of stopping, journals everything.
11. **Morning ritual (~30–60 min per loop night):**
    - Read `docs/loop-journal.md` — the Log for what landed, the
      **Needs-David queue** for what only you can do (API keys, OAuth
      consents, TCC grants, escape-hatch contract proposals).
    - Review staging's new per-provider commits.
    - **Validate what you can** (each brief has exact steps); promote
      slices 🧪 → ✅ in the brief. Partial validation is fine — the matrix
      tracks the rest.
    - Clear Needs-David items so the next night has fewer blocks.
    - When satisfied with a batch: merge `integration-staging` →
      `integrations`.
12. **Deploy on your cadence, never the loop's:** after a merge batch,
    `scripts/build-troved.sh` + reinstall once, re-grant TCC once.
    (Batched deliberately — every reinstall costs the per-binary grants.)
13. Repeat 9–12 until the queue is done; blocked items accumulate flags
    until you clear them. Milestones: merge `integrations` → `main`.

## After the wave

14. **Punch list:** sweep validation findings + spec-interpretation drift
    across merged integrations — small fixes, not migrations (raw layer is
    lossless).
15. **Read-time organization begins** — entity resolution joins, unified
    timeline, vault-wide search, SQLite index when its trigger features
    arrive — designed against the full corpus, per the pipeline doctrine.

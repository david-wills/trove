# CLIP Image Embeddings (derived)

- **id:** `clip-embeddings`
- **domains:** `photos/` (derived enrichment over photo sources already in
  the vault; contract: **Phase 3 pending** — photos-metadata governs the
  source rows it enriches). No external data — purely local ML.
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (background batch pass over newly indexed photos;
  resumable, CPU-throttled)
- **connection:** none
- **evidence:** official-docs — crates.io/crates/fastembed (pure Rust, ONNX
  Runtime via pykeio/ort, `ImageEmbeddingModel::ClipVitB32`, actively
  maintained, v4+ shares one embedding space for text + images)
- **effort / priority:** L / P2
- **needs:** Needs-David (opt-in feature decision — ~300 MB model download
  on first run + CPU-heavy embedding pass; default-off). Sequenced
  **after** Apple Photos psi.sqlite labels + EXIF import exist (the free,
  already-computed tiers come first).

## What it is

Not a data source — a local ML enrichment layer. CLIP embeds images into a
512-d vector space shared with text, enabling natural-language photo search
("find photos with mountains") entirely on-device, with no cloud call. It
is the advanced tier above Apple's free psi.sqlite scene labels: those
answer "what did Apple's ML already tag", CLIP answers arbitrary queries.
Flagship differentiator for fully-local semantic search, at real compute
cost.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Image embeddings | none (local) | 512-d ClipVitB32 vector per photo, keyed to the source row's guid | fastembed docs, research L3138–L3145 |
| Text-query embedding | none (local) | same space — query-time, not stored | fastembed v4+ |

One capability, no tiers. The only gates are the opt-in and the model
download.

## Access & auth

- No endpoints, no auth, no TCC of its own (reads images via paths the
  photo collectors already index; their grants — e.g. FDA for the Photos
  library — cover it).
- **Standalone-rule note:** the ~300 MB ONNX model download on first enable
  is a network fetch — one-time, explicit, user-initiated by the opt-in
  toggle, clearly labeled (same posture as `browser-ads-identify`: the only
  networked action, by explicit choice). Model cached locally thereafter;
  inference is fully on-device CPU (Metal/CoreML acceleration possible via
  ort features).

## Vault mapping

- **Raw layer:** `photos/clip-embeddings/` — embedding rows keyed by the
  *source* photo row's guid (`{source, guid, model: "clip-vit-b32", dims,
  vector}`), partitioned to mirror the source stream.
- **Contract layer:** none — embeddings are not photos-metadata rows; they
  reference them. Open question for the build loop (flag to David):
  embeddings are deterministic derived data and arguably belong in a
  rebuildable `.trove/` index rather than vault files. Decide at build
  time; default per this catalog is the vault folder above with the model
  id recorded so regeneration is well-defined.
- **Dedupe:** (source, guid, model) — re-running the pass never duplicates.

## Build plan

1. **Prerequisites first:** Apple Photos (psi.sqlite labels) and EXIF
   import shipped — CLIP enriches what they index; pointless before.
2. Module `crates/trove-core/src/clip_embeddings.rs`: `DEF` (Periodic,
   default-off opt-in; enable flow states the 300 MB download + CPU cost
   per the disabled-affordance rule).
3. fastembed integration: model download/cache step, then a batched,
   resumable background pass (a 50k-photo library takes real time on CPU —
   checkpoint per batch in `.trove/clip-embeddings-sync.json`, never block
   the owner loop).
4. Settle the vault-vs-index storage question with David (step above).
5. Fixtures: tiny image set, assert stable vector dims + dedupe + resume;
   unique temp dirs. Gate heavy model tests behind an ignored/feature flag
   so `cargo test -p trove-core` stays fast.
6. Read-side (semantic search UI) is post-wave Phase-"after" work — this
   brief covers only producing the vectors.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Model download + cache | — | enable the opt-in toggle on a fresh vault; confirm one-time download, cached on re-enable |
| Embedding pass | — | run over a small real photo set; confirm rows keyed to source guids; re-run adds nothing |
| Resume after interrupt | — | kill mid-pass; restart; pass completes without duplicates |
| Semantic sanity | — | embed "mountains" as text, nearest-neighbor a known mountain photo above unrelated ones |

## Research notes

`integrations-research.md` → "Photos & Visual Media" §CLIP / fastembed-rs
(L3138–L3145), 🟡 medium, recommendation: icebox until the local-ML /
embedding pipeline exists (v0.2). psi.sqlite labels + EXIF GPS/face tags
deliver most of the value for free first. The `visual-search` crate is a
less-maintained alternative — fastembed is the pick. Cross-cutting note 6
(L3156): CLIP is the v0.2 enrichment layer above the Vision-OCR bridge.

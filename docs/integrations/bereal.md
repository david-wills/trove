# BeReal

- **id:** `bereal`
- **domains:** `photos/` (contract: **Phase 3 pending** — photos-metadata;
  metadata only, never image copies) + `social/` (account metadata, login
  history — per-source raw under `social/bereal/`; social-posts contract is
  **Phase 3 pending** if the daily-post rows fit it)
- **status:** 🧪 built (parser parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (GDPR export ZIP — the only path; no API exists)
- **connection:** none
- **evidence:** community-schema — bereal-gdpr-photo-toolkit,
  bereal-data-transform, BeReal GDPR Explorer (GitHub) document the export
  layout and the raw photo format; no official format spec. Confidence
  medium → **Needs-sample**.
- **effort / priority:** M / P2
- **needs:** Needs-sample (export layout community-understood only;
  parser-last)

## What it is

Dual-camera daily-photo social app: one front + one back photo per day at a
random prompt time. Declining platform (~40M MAU in 2026 vs 73M peak in
2022) and no API — but the export is a complete, timestamped daily photo
diary, a personally meaningful archive format no other service produces.
Worth holding in the vault even if the platform fades.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Daily post archive | all accounts | every BeReal taken, timestamps, front+back photos as separate files | research L4136–L4142 |
| Account metadata | all accounts | username, phone, registration date, privacy-settings history | research L4140 |
| Login history | all accounts | login events | research L4140 |

All optional in the (pending) contracts; sparse exports just carry fewer
fields.

## Access & auth

- **GDPR data request via in-app support chat only** — no self-serve export
  button. ZIP arrives within ~48 hours. UI copy must explain the chat-request
  dance honestly; this is the whole acquisition path.
- Dual-camera images are stored in a proprietary raw format (not standard
  JPEG); the open-source toolkits convert them. **Trove never copies image
  bytes**, so conversion matters only if we want EXIF-grade metadata out of
  the raw files — timestamps and per-post metadata come from the export's
  metadata files.
- No auth inside Trove, no TCC, no network. Standalone-clean (any parsing
  logic is reimplemented in Rust, not shelling out to the Python toolkits).

## Vault mapping

- **Raw layer:** `photos/bereal/raw/YYYY-MM.jsonl` — per-post metadata
  objects full-fidelity; `social/bereal/raw/` — account metadata + login
  history (distinct record types route by shape; each record routes whole).
- **Contract layer:** `photos/bereal/YYYY-MM.jsonl` per the pending
  photos-metadata contract — `ts` = post time, `guid` = post id (or
  timestamp-derived if the export lacks ids — confirm on sample), front/back
  file names, location if present, overflow in `extra`.
- **Dedupe:** post `guid` — re-importing a newer export extends the stream
  without duplication.

## Build plan

1. Module `crates/trove-core/src/bereal.rs`: `DEF` (Behavior::Import);
   generic import box hosts the ZIP parser. ✅ shipped.
2. **Parser-last / Needs-sample:** export layout comes from community
   toolkits, not an official spec — acquire one real GDPR ZIP (David or any
   user) before finalizing the parser; the scaffold is built against the
   best-effort community-documented structure.
3. Fixtures: synthetic ZIP with `posts.json` manifest (with/without location);
   folder-only ZIP (no manifest); account metadata file. Parser + store tests
   all green (8/8). ✅
4. Contract rows land via `photos::Photo` which is already ratified. The
   brief's "Phase 3 pending" is stale — `photos.rs` struct is bound.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Post archive import | needs real ZIP | request a GDPR export via in-app chat, drop ZIP into import box, confirm rows in `photos/bereal/` + hub last-data |
| Account/login data | needs real ZIP | same import; confirm `social/bereal/raw/` files written |
| Re-import dedupe | ✅ tested | synthetic ZIP imported twice → row count unchanged (test: `reimport_same_zip_dedupes_cleanly`) |
| Folder-only export (no manifest) | ✅ tested | folder-named image entries → Photo rows from folder-timestamp parser |

## Parser notes (Needs-sample items to verify with real ZIP)

- The posts JSON filename: assumed `posts.json`, `memories.json`, `bereal.json`,
  or `data.json` — exact name unknown.
- Post JSON field names **confirmed** from hatobi/bereal-gdpr-photo-toolkit
  (process-photos.py): `primary.path` (rear camera → `back_file`),
  `secondary.path` (selfie → `front_file`), `takenAt` (ISO UTC),
  `location.latitude/longitude`, `caption`. No per-post `id` field observed.
  The old assumption of `frontCamera`/`backCamera` was wrong.
- Image files are random-named flat `.webp` files under `Photos/`
  (e.g. `Photos/_OaBX9TnSgcfapL8.webp`); there are NO date-named post folders
  in real exports. The folder-fallback path in the parser is theoretical only.
- `guid` is now `bereal:moment:<takenAt>:<primaryFilename>` (stable across
  re-exports that add reactions/RealMojis).
- Account metadata and login history JSON filenames and field names still need
  a real sample to confirm.
- Whether GPS coordinates are present in the export JSON still needs a real sample.

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §BeReal
(L4136–L4142), 🟡 medium. No self-serve export and no API; support-chat
request is friction the app can't remove, only explain. Platform decline
makes this a get-the-archive-out play more than a live integration — no
ongoing-sync upgrade path exists or is likely.

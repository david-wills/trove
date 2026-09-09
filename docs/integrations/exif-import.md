# Image Files (EXIF)

- **id:** `exif-import`
- **domains:** `photos/` (contract: **Phase 3 pending** — photos-metadata,
  drafted from Apple Photos + Google Photos Takeout + EXIF import together)
- **status:** 🧪 built (fixture-tested; first-in-domain binding of `photos/`; nom-exif JPEG/HEIC/TIFF/PNG/MOV; needs a real folder drop to validate)
- **unavailable_reason:** none
- **behavior:** Import (user-dropped files/folders; camera-card import)
- **connection:** none — pure local parsing.
- **evidence:** community-schema, high confidence — maintained Rust crates
  on crates.io: nom-exif (JPEG/HEIF/HEIC/TIFF/MOV/MP4/WebM/MKV),
  kamadak-exif, little_exif, libheif-rs; exiftool-rs for pro RAW (93
  formats). All actively maintained 2024–25.
- **effort / priority:** S / P1
- **needs:** privacy (GPS geotags form a location trail — opt-in with
  explicit acknowledgement)

## What it is

EXIF metadata extraction for image and video files managed *outside* Apple
Photos — drag a folder of JPEGs/HEICs, import a camera SD card, and Trove
indexes capture timestamps, GPS, camera/lens model, and orientation. The
companion path to the Apple Photos collector for users (or subsets of a
library) that never enter Photos.app. Metadata only — the vault indexes,
it never copies the images.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Photo EXIF (JPEG/HEIC/TIFF/PNG) | none | capture ts, GPS lat/lon, camera + lens model, orientation, dimensions | nom-exif / kamadak-exif crate docs |
| Video metadata (MOV/MP4 atoms) | none | creation ts, GPS, device | nom-exif (QuickTime/MP4 atoms) |
| Pro RAW formats (CR2 etc.) | none (parser-last) | same fields where present | exiftool-rs (93 format readers) |

All optional in the contract — a GPS-stripped or scanned image still lands
with whatever fields it has. No gating, no special code paths.

## Access & auth

- No auth, no TCC for user-dropped files (drag/drop or file picker through
  the registry import box); FDA only if ever reading a camera mount
  directly — not needed for v1.
- Parsing is pure Rust compiled into the binary (nom-exif first; it handles
  HEIC — the iPhone-native format — including the big-endian JPEG-variant
  byteswap). Standalone-clean: no exiftool binary, no network.

## Vault mapping

- **Raw layer:** `photos/exif-import/YYYY-MM.jsonl` (partitioned by capture
  month) — one row per file: `guid` (file content hash), `ts`, original
  filename + dropped-from path, GPS, camera/lens, dimensions, orientation,
  mime, remaining tags in `extra`. **Never image copies.**
- **Contract layer:** photos-metadata contract is Phase 3 pending; this
  provider is one of its three drafting sources. Until ratified, raw layer
  only.
- **Dedupe:** content hash as `guid` — the same photo re-dropped (or
  reachable via two paths) yields one row.

## Build plan

1. Module `crates/trove-core/src/exif_import.rs`: `DEF`
   (Behavior::Import), nom-exif parse, recursive folder walk on directory
   drops.
2. Registration line in `INTEGRATIONS`; the generic import box gives the
   UI for free.
3. RAW formats are parser-last: ship JPEG/HEIC/TIFF/PNG/MOV/MP4 first;
   add exiftool-rs for CR2/RAW behind the same `DEF` when a sample set is
   in hand (**Needs-sample** for RAW only — mainstream formats are fully
   documented by crate test suites).
4. Fixtures: small JPEG/HEIC/MOV with known EXIF (crate test corpora are a
   ready source), plus a GPS-less file for the sparse-row case; hash-dedupe
   tests, unique temp dirs.
5. Privacy gate: opt-in acknowledgement that geotags form a location trail.
6. Coordinate with the Apple Photos collector (shared photos-metadata
   shape at contract time); skip files inside `.photoslibrary` bundles to
   avoid double-counting with `apple-photos`.

## Build status — 🧪 2026-06-15

Shipped (`exif_import.rs`, INDEX #23 — also the **first-in-domain binding** of the
`photos/` contract). `Behavior::Import` (the generic import box; recursive folder
walk), no connection, **🔒 default-off** (GPS geotags = a location trail). Built
**Opus** (first-in-domain bind, per the model policy); a Sonnet evidence spike fed it.

- **Binding (first in `photos/`):** new `photos.rs` `Photo` struct matching
  `photos.photo.schema.json` (required `ts`/`source`/`guid`; kind/filename/title/
  mime/width/height/lat/lon/camera_make/camera_model/favorite/duration_secs/albums/
  tags/people/people_name/text/extra omit-empty); `photos` `DOMAINS` (EventStream,
  month of `ts`); promoted the draft fixture to the ratified triad (5/5). **Binds
  the shape apple-photos/google-photos/flickr/… reuse.** `people[]`/`people_name[]`
  are opt-in-gated and **never emitted by exif-import** (no face data); lens/
  altitude/orientation ride in `extra`.
- **Parsing:** `nom-exif = 3.6.1` (**pure-Rust, no C deps** — verified Cargo.lock
  C-free; JPEG/HEIC/HEIF/AVIF/TIFF/PNG + MOV/MP4/3GP). `guid` = `sha256:<hex>` of
  the file bytes (read only to hash, **never copied into the vault**); `ts` =
  `DateTimeOriginal` (offset-aware kept; **naive → system-local offset**, never
  synthesized UTC); GPS via nom-exif `latitude_decimal`/`longitude_decimal` (signed
  WGS84 — S/W negative); video (`Metadata::Track`) → `kind:"video"` + duration,
  preserving the container's own offset. EXIF-less file → still a row (mtime-fallback
  `ts`). **Full fidelity:** every non-normalized tag → `extra` (deterministic,
  IFD0-first on collision; internal pointer pseudo-tags stripped).
- Recursive folder walk **skips symlinks** (can't escape the dropped tree) and
  **`.photoslibrary` bundles** (no double-count with the future apple-photos);
  per-file parse errors are logged + skipped, never fatal; re-import dedups by
  `guid` (0 new).

Evidence: nom-exif API + the EXIF tag set confirmed on docs.rs/the crate repo + the
photos contract. Fixtures are real images from nom-exif's canonical test corpus
(JPEG w/ GPS, TIFF, PNG, MOV) — real parse paths, not hand-rolled blobs.
Adversarial-verify (Opus): **0 BLOCKING + 3 minor, all fixed** — symlink-escape in
the walk → now lstat-skips symlinks; nondeterministic `extra` (thumbnail-IFD clobber
+ pointer-tag noise) → IFD0-first deterministic + pseudo-tags stripped; a wrong
doc-comment. GPS sign (S/W negative), the dimension tag, no-image-copy, and the
binding triad were independently confirmed correct.

Gate (my run, serial): trove-core 693/0, `cargo check` clean, Cargo.lock C-free,
spec_validation 5/5 (photos ratified), `schedule_doc` regenerated (exif-import
Import), `bindings.ts` up to date. **Deferred (Needs-sample):** mainstream RAW
beyond CR3/RAF/IIQ (CR2/NEF/ARW) — those drop to a mtime-fallback row until a sample
set validates the parser.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Photo EXIF | 🧪 (needs a real drop) | drop a real iPhone HEIC; confirm a row in `photos/exif-import/` with correct ts/GPS vs Preview's inspector; confirm NO image copy in the vault |
| Video metadata | 🧪 (needs a real drop) | drop a .MOV from an iPhone; confirm `kind:"video"` + creation ts + duration |
| Folder drop + dedupe | 🧪 (needs a real drop) | drop a folder twice; confirm row count unchanged (sha256 dedupe) |
| RAW | 🚫 Needs-sample | needs a CR2/NEF/ARW sample set; validate field parity vs an exiftool reference (CR3/RAF/IIQ already parse) |

## Research notes

`integrations-research.md` → "Photos & Visual Media" §Camera EXIF /
standalone image files (L3082–L3089). Feasibility 🟢 high — the research
doc recommends building now alongside the Photos.sqlite collector with a
standalone drop path; the catalog keeps it a separate provider (`apple-
photos` reads the library DB, `exif-import` reads loose files) sharing the
pending photos-metadata contract. Not time-sensitive — files keep their
EXIF forever. Alternative considered: shelling out to ExifTool (rejected:
standalone rule; pure-Rust crates suffice).

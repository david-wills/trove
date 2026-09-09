# Screenshots (macOS screenshot folder watch + OCR)

- **id:** `macos-screenshots`
- **domains:** `photos/` (screenshot rows; contract: **Phase 3 pending** —
  photos-metadata, though OCR text likely stays raw-only) · `files/`
  (screen-recording file events; raw-only per the taxonomy)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Live (filesystem watch on the screenshot folder; backfill
  pass via Spotlight on enable)
- **connection:** none — local files + on-device Vision framework.
- **evidence:** official-docs — Vision `VNRecognizeTextRequest` is public
  API (macOS 10.15+), `RecognizeDocumentsRequest` macOS 26+;
  `kMDItemIsScreenCapture` Spotlight attribute documented; shipping prior
  art proves the Rust/Tauri+Vision pattern (mirowl, TidyShot, ClariRec).
- **effort / priority:** M / P2
- **needs:** privacy (OCR'd screen content is whatever was on screen —
  message bodies, financial detail — opt-in with explicit acknowledgement)

## What it is

A passive watcher on the user's saved screenshots: every screenshot already
deliberately taken gets its text extracted on-device and made searchable —
code snippets, receipt totals, error messages. This is the narrow,
privacy-safe variant of screen capture: **only user-saved screenshots, text
only, never image bytes**, and explicitly *not* the iceboxed Rewind-style
periodic capture.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Screenshot metadata | none | ts (from filename/ctime), filename, dimensions, file size | research doc (plain files, mdfind) |
| OCR text | none (on-device) | extracted text per screenshot | official Vision docs + shipping prior art |
| Structured extraction (tables, QR, emails/URLs) | macOS 26+ only | structured document fields | official (WWDC25 RecognizeDocumentsRequest) |
| Screen-recording file events | none | ts, filename, size (no OCR) | research doc (~/Library/ScreenRecordings/) |

All optional; on macOS < 26 rows simply carry plain OCR text, no structured
fields — version-gated fallback, no special code paths beyond the guard.

## Access & auth

- Default save location `~/Desktop`, user-configurable
  (`defaults read com.apple.screencapture location`); screen recordings at
  `~/Library/ScreenRecordings/` (10.15+). Watch via the `notify` crate
  (FSEvents/kqueue).
- Backfill/custom-location safety net: `mdfind 'kMDItemIsScreenCapture == 1'`
  finds screenshots regardless of save location.
- OCR: Vision framework via `objc2-vision` or a thin Swift helper compiled
  into the bundle (the EventKit bridge proved the Swift-helper pattern).
  Fully on-device — standalone-clean, no network.
- TCC: none for ~/Desktop; FDA (already held) if the user's custom location
  is protected.

## Vault mapping

- **Raw layer:** `photos/macos-screenshots/YYYY-MM.jsonl` — one row per
  screenshot: `guid` (file content hash), `ts`, `filename`, `width`,
  `height`, `file_size_bytes`, `ocr_text`, structured fields in `extra`.
  Records route whole: metadata + text is one row, one folder. **Never
  image bytes.** Screen recordings (video, no OCR in v1) are a distinct
  record type → `files/macos-screenshots/YYYY-MM.jsonl` (raw-only), per
  the distinct-record-types rule.
- **Contract layer:** photos-metadata contract (Phase 3 pending) may take
  the metadata slice; OCR text is likely sidecar/raw. Decided at contract
  drafting — raw layer is unaffected either way.
- **Dedupe:** content hash as `guid` (filenames collide across renames).

## Build plan

1. **Spike first: the Vision OCR bridge** (objc2-vision vs. Swift helper —
   pick by bridge cost). Build it as shared infrastructure: the same
   bridge serves future imported-image OCR.
2. Module `crates/trove-core/src/macos_screenshots.rs`: `DEF`
   (Behavior::Live), watcher on the resolved screencapture location +
   ScreenRecordings dir; mdfind backfill pass on first enable.
3. Version guard: `RecognizeDocumentsRequest` on macOS 26+, fall back to
   `VNRecognizeTextRequest` below. Batch OCR off the watcher thread
   (vault-touching work async + spawn_blocking per house rule).
4. Registration line in `INTEGRATIONS`. Lives in troved long-term (always-
   on watcher).
5. Fixtures: tiny PNG with known text; tests for filename-timestamp parse,
   hash dedupe, custom-location resolution (unique temp dirs).
6. Privacy gate: ships opt-in with explicit acknowledgement ("extracts the
   text of every screenshot you save — screenshots often contain private
   content"). Copy must state image bytes are never stored.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Watch + OCR | — | enable; Cmd-Shift-4 a window with known text; confirm a row with that text in `photos/macos-screenshots/` and no image copy anywhere in the vault |
| Backfill | — | confirm pre-existing Desktop screenshots appear after first enable (mdfind path) |
| Custom location | — | move the save location via Cmd-Shift-5; take a screenshot; confirm it's still captured |
| Structured extraction | — | macOS 26 machine: screenshot a table/QR; confirm structured fields in `extra` |

## Build notes (2026-06-21)

- Behavior: `Live` (poll-based folder scan per tick; no FSEvents in v1 — tick cadence is sufficient given troved runs continuously).
- Photos contract reused: `crate::photos::Photo` written to `photos/macos-screenshots/YYYY-MM.jsonl`. The `text` field carries OCR output; `kind="screenshot"`.
- Screen recordings: raw-only to `files/macos-screenshots/YYYY-MM.jsonl` (path+mtime hash guid; no content read for video).
- OCR: `objc2-vision` 0.3.2 `VNRecognizeTextRequest` + `VNImageRequestHandler` via `initWithURL:options:`. macOS-only (`#[cfg(target_os = "macos")]`); returns empty string on non-macOS CI.
- PNG/JPEG header parser for dimensions (no full decode); HEIC/TIFF dims deferred to a future nom-exif integration.
- Seen set persisted to `.trove/macos-screenshots-seen.json` across restarts.
- Screenshot folder read from `defaults read com.apple.screencapture location` with `~/Desktop` fallback.
- Added `objc2-vision = { version = "0.3.2", features = ["VNRequestHandler","VNRecognizeTextRequest","VNRequest","VNObservation"] }` + `NSDictionary` feature to `objc2-foundation` in `crates/trove-core/Cargo.toml`.
- Tests: 26 passing; no env-var races (scan_folder called directly, not via tick).

### Defect fixes (2026-06-21)

- **seen-set cursor-advance-on-failure**: GUIDs are now collected into a local `Vec` and only merged into `self.seen` inside the `Ok` branch of each `stream.append()` call. A transient write error leaves `self.seen` untouched, so the next poll retries correctly.
- **Cmd-Shift-5 recordings collected**: `scan_folder` now also picks up `mov`/`mp4`/`m4v` files from the screencapture location (the real default sink for Cmd-Shift-5 recordings). `~/Library/ScreenRecordings/` is still scanned for QuickTime Player recordings.
- **Pre-Mojave "Screen Shot" prefix**: `parse_screenshot_ts` now accepts all three macOS filename prefixes: `"Screenshot "` (Mojave+), `"Screen Shot "` (pre-Mojave two-word form), and `"Screen Recording "` (recordings). The 12h AM/PM path is now reachable for real pre-Mojave files.
- **Single-digit hour in 12h filenames**: Parser now splits on the first space rather than hard-indexing at position 8, fixing `"3.51.22 PM"` (7 chars before the space) correctly.
- **Recording filename timestamp**: `parse_screenshot_ts` accepts the `"Screen Recording "` prefix, so `Screen Recording YYYY-MM-DD at HH.MM.SS.mov` captures the embedded timestamp instead of falling back to mtime.
- **mdfind backfill**: A one-shot `mdfind 'kMDItemIsScreenCapture == 1'` pass now runs on the first enabled tick, picking up screenshots at custom locations and historical screenshots. Silently skipped if mdfind is unavailable (Spotlight off, sandbox, CI).
- **Recording GUID tradeoff**: Documented explicitly in module-level doc — path+mtime is intentional to avoid reading large video files; moves/renames produce duplicates (accepted v1 limitation).

## Research notes

`integrations-research.md` → "Photos & Visual Media" §Screenshot folder —
watch and OCR (L3074–L3081) + §Apple Vision framework (L3090–L3097), and
"Computer & Developer Activity" §Screenshots Folder Metadata (L1388–L1394)
— three research entries, one provider. The metadata-only entry suggested
`developer/screenshots/`; the taxonomy wins and routes screenshot rows to
`photos/`. Feasibility 🟢 high on all three. Explicitly *not* the iceboxed
Rewind-style periodic capture — keep that distinction in the hub copy.
Not time-sensitive for backfill, but live capture only sees screenshots
saved while the watcher runs (users who auto-clean their Desktop lose
unwatched ones — another reason troved hosts this).

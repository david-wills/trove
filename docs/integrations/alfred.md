# Alfred Clipboard

- **id:** `alfred`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (copy-then-read the SQLite store; ts watermark)
- **connection:** none (local files)
- **evidence:** community-schema — clipboard.alfdb table schema confirmed by
  multiple independent sources incl.
  gist.github.com/pirate/6551e1c00a7c4b0c607762930e22804c (high confidence)
- **needs:** privacy (clipboard history is privacy-critical — opt-in with
  explicit acknowledgement; app-name exclusion list mandatory) ·
  time-sensitive (retention is user-configurable from 1 day — uncollected
  history ages out)
- **effort / priority:** S / P2

## What it is

Alfred's Powerpack clipboard history: everything the user copied, with
timestamp and source app, sitting in a plain SQLite database. A dense
"what was I actually working with" trail that no other source captures —
but also one of the most sensitive stores on the machine, since clipboards
routinely carry passwords, tokens, and private text. Powerpack is a ~$34
one-time purchase, so this only applies to existing Alfred power users.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Text clips | Powerpack only | ts, text, source app, app path | community schema (gist) |
| Image/file clips | Powerpack only | ts, source app, data type (content not copied) | community schema (gist) |

All optional; non-Powerpack users simply have no database and the def
reports no data.

## Access & auth

- SQLite at `~/Library/Application Support/Alfred/Databases/clipboard.alfdb`.
  Table `clipboard(item, ts, app, apppath, dataType, dataHash)`; dataType
  0 = text, 2 = image, 8 = file list.
- Home-dir path — no FDA prompt needed; troved's existing grant covers it
  regardless. Copy-then-read (M3 pattern from `browser.rs`) since Alfred
  may hold the file open.
- No network, no auth. Standalone-clean.

## Vault mapping

- **Raw layer:** `developer/alfred/YYYY-MM.jsonl` — one row per clip:
  `ts`, `text` (truncated at a configurable limit), `app`,
  `app_bundle_path`, `data_type`. Image/file clips store metadata only,
  never blob content. (Research doc's `developer/clipboard/` path predates
  the taxonomy; folder = provider id.)
- **Contract layer:** none — `developer/` is raw-only.
- **Dedupe:** `guid` = `dataHash` + `ts`; cursor = highest imported `ts` in
  `.trove/alfred-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/alfred.rs`: `DEF` (Periodic), copy-then-read
   pull, last-data hook off the newest row.
2. **Privacy gate first-class:** ships default-off, opt-in with explicit
   acknowledgement (clipboard ≈ message bodies and worse). A configurable
   app-name exclusion list (seeded with 1Password, Keychain Access, other
   password managers) drops clips originating from excluded apps at parse
   time — they never reach the vault.
3. Registration line in `INTEGRATIONS`.
4. Fixtures: a synthetic clipboard.alfdb with text/image/file rows +
   excluded-app rows; parser, exclusion, truncation, and cursor tests in
   unique temp dirs.
5. Time-sensitivity: surface "collect soon — Alfred may be pruning history"
   in the brief/UI copy; default retention can be as short as 1 day.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Text clips | ✅ unit-tested | enable the toggle (acknowledge opt-in), copy a few strings, Sync now; confirm rows in `developer/alfred/` + hub last-data |
| Exclusion list | ✅ unit-tested | copy from an excluded app (1Password, Bitwarden, etc.); confirm no row lands |
| Image/file clips | ✅ unit-tested | copy an image/file; confirm metadata-only row (no `text` field) |
| Incremental cursor | ✅ unit-tested | second sync with no new clips returns 0 new rows |
| Text truncation | ✅ unit-tested | clips longer than 4000 chars stored with `text_truncated: true` |

## Build notes (2026-06-16)

- Schema confirmed against a real `clipboard.alfdb` on the build machine:
  `CREATE TABLE clipboard(item, ts decimal, app, apppath, dataType INTEGER, dataHash)` with indexes on ts, app, dataHash, dataType.
- **ts is Core Foundation / Mac absolute time (seconds since 2001-01-01 UTC), NOT Unix epoch.**
  Must add 978_307_200 s before calling timestamp_opt. Primary source: the cited
  gist (gist.github.com/pirate/6551e1c00a7c4b0c607762930e22804c lines 115-116)
  states "clipboard timestamps are in Mac epoch format … add 978307200"; confirmed
  independently by rmoff.net/2020/05/18 where sample `ts=610489734` decodes to
  2020-05-06 only with the offset (without it: 1989). Real Alfred ts values are
  ~8.0e8 (CF era 2026); Unix-magnitude fixtures (~1.75e9) silently mask this bug.
  The cursor is kept in raw CF units; the offset is applied only at display/partition time.
- dataType: 0=text, 2=image, 8=file (confirmed via PRAGMA + community schema gist).
- Text clips truncated at 4000 chars; image/file clips store metadata only (never blob content).
- App-name exclusion list seeded with 1Password, Keychain Access, Bitwarden, Dashlane, LastPass, and other common password managers.
- copy-then-open via `browser::import_via_copy` (M3 pattern from imessage.rs).
- Contract mode: raw-only (developer/ domain).
- 16 tests green (incl. CF-epoch regression tests: rmoff.net sample + current-era rendering); cargo check clean.

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Alfred
Clipboard History (L1356–L1362). Feasibility 🟢 high. Maccy (free, open
source; `~/Library/Application Support/Maccy/Maccy.sqlite`, similar SQLite
shape) is the accessible alternative the research doc says to cover too —
catalogue it as its own provider in a later pass rather than merging it
into this brief (distinct service, same domain). Retention is configurable
1 day → unlimited, hence the time-sensitive flag.

# R2 — The drop-in normalizer (detect-and-map)

*Drafted 2026-07-20 with David; the R2 spec per `docs/post-wave-roadmap.md`.
Status: **RATIFIED 2026-07-20** — the core forks and all four open rulings
were settled by David the same day (see Decisions and Ratified rulings).
Build proceeds Step 0 → 5.*

## Context (verified against the repo, 2026-07-20)

The normalizer's foundation is already in place:

- **Contracts:** 21 domains compiled in `DOMAINS` (`contracts.rs:93`), 32
  shape schemas in `docs/vault-spec/schemas/`, and **all 421 fields carry
  `description` + `examples`** — the Phase-4 carry-forward landed during the
  wave, so the "normalizer's API surface" is complete.
- **Import machinery:** `ImportSpec` (`registry.rs:132`) + the one generic
  `run_import` command (`src-tauri/src/lib.rs:142`) already serve every
  compiled file import; `imdb.rs` (ratings + watchlist + custom lists) shows
  header-shape recognition inside a compiled module.
- **Store:** partitioned guid-merged writes, `write_json_atomic`, and (post-R1)
  the generic `ensure_index` — the write path the projection reuses unchanged.
- **Conventions:** raw-first, `.trove/` for machine artifacts, vault-relative
  paths via `Vault::resolve`, async + `spawn_blocking` for every vault-touching
  command, reads O(displayed).

## Decisions (settled with David 2026-07-20 — do not relitigate)

1. **The runtime is deterministic; the LLM is an advisor only.** Header/value
   heuristics always run first. If inconclusive, offer an **explicit per-drop
   opt-in** cloud LLM call whose consent dialog shows exactly what leaves the
   machine (headers + ≤5 sample rows). The model only *pre-fills the binding
   form*; the user confirms; the saved mapping is plain data. The call-site is
   designed so R4d's local inference can replace the cloud call without
   redesign. Key handling follows the ConnectSpec precedent (baked + BYO).
2. **Write-projection.** A confirmed mapping is applied at import: contract-
   conformant JSONL is written into the domain folders through the existing
   store helpers. Raw is kept full-fidelity, so a wrong mapping is a
   re-projection from raw, never data loss. The pipeline doc's "read-time
   opinion" means *rebuildable*, not applied-at-read — read paths, indexes,
   and every future R4 feature consume projected data unchanged.
3. **v1 transform ceiling: a fixed coercion toolkit,** derived from the wave's
   ~290 hand-written parse→map→write mappings (Step 0). No expression
   language. Anything the toolkit can't express rides `extra` (and raw has
   everything). Additive later if real drops demand it.
4. **Fixtures and gate are real files, not synthetics.** The gate is David's
   `pub_ops fb_pages` Looker export → `social.post`; his IMDb custom-list
   export drives the route-to-built-importer path; a junk file drives the
   honest-decline path. (Details in Appendix A.)
5. **v1 formats: CSV and JSONL.** JSON-array and xlsx are additive later.

## The decision tree (what a dropped file can become)

Every drop lands **raw first**, then detect resolves to exactly one of three
outcomes — all three are v1 scope, and each has a real fixture:

1. **A compiled importer claims the shape** → offer "run the built import"
   (e.g. the IMDb list export, which `imdb.rs` already parses). No mapping is
   created; the built module does what it always did.
2. **A ratified contract fits** → suggest domain + field bindings (heuristics,
   escalating to the opt-in LLM), user confirms/edits, binding persists as
   `.trove/mappings/<source>.json`, projection writes contract rows. Future
   drops matching the mapping's signature auto-conform with zero prompts.
3. **Nothing fits** → the file stays raw under a user-named source folder,
   listed in the manifest and browsable in a minimal generic table view (the
   punch-list "thin raw viewer", subsumed here). Declining honestly is a
   feature: forcing a curated list into `media.play` with invented `seconds`
   would violate "don't invent a fate you can't observe".

A doctrine note for the docs: contract semantics (e.g. `social.post`'s "only
the user's authored content") are advisory at mapping time — the user
confirming the binding owns the interpretation. Drop-in-anything means the
user decides what their data means.

## Steps

### Step 0 — Mine the corpus (design input; no product code)

Survey the wave's ~290 integration modules' parse→map→write code and produce
`docs/vault-spec/normalizer-toolkit.md`: the coercion vocabulary with
frequency evidence. Expected members (to be confirmed by the survey, not
assumed): date/time format parsing (named format table, not user regex),
number cleanup (thousands separators, currency symbols), unit conversion
(declared source unit → contract unit), value mapping (source enum → contract
enum), constants, guid recipes (source id column | hash of named columns |
extract-from-URL), split/join on a delimiter (e.g. `"Action, Adventure"` →
array). The v1 toolkit is the smallest set that covers the three fixtures
plus the top recurring coercions; the doc records what was deliberately left
out. Fan-out agent work; a day, not a week.

### Step 1 — Mapping artifact + projection engine (`crates/trove-core/src/normalizer.rs`)

The mapping file (location per the pipeline doc: `.trove/mappings/<source>.json`):

```jsonc
{
  "version": 1,
  "source": "acme-pubops",              // user-confirmed slug; folder + source field
  "domain": "social", "shape": "post",    // target contract
  "signature": { "headers": ["", "creative framing tags", "framed by", /*…*/ ],
                 "format": "csv" },        // normalized; exact match ⇒ auto-conform
  "bindings": [
    { "from": "Post Date",  "to": "ts",      "coerce": "date" },
    { "from": "Post URL",   "to": "url" },
    { "from": "List Name",  "to": "title" },
    { "from": "Page Name",  "to": "context" }
  ],
  "constants": { "kind": "post" },
  "guid": { "hash": ["Post URL"] },
  "unbound": "extra",                      // every unbound column → extra, verbatim
  "provenance": { "created": "2026-07-20", "suggested_by": "heuristic|llm|manual" }
}
```

The engine: stream-parse CSV/JSONL → apply bindings/coercions → validate each
row against the embedded schema (the 32 schema files become `include_str!`
assets so validation and field metadata are available at runtime) → write via
the store helpers with the domain's partitioning, guid-merged so re-drops of
overlapping exports dedupe naturally. Rows that fail validation are counted
and reported in the `ImportOutcome`, never silently dropped — and always
recoverable from raw. Headless-testable before any UI exists.

### Step 2 — Detect

- **2a — built-importer signatures.** Add an optional static header signature
  to `ImportSpec` (one line per import def with a CSV shape; start with the
  handful that have one, `imdb` first). The drop surface checks these before
  anything else. *(Default-in; David may veto for scope.)*
- **2b — heuristics.** Normalized header tokens scored against every shape's
  field names, descriptions, and examples; value-shape checks on sample rows
  (dates, URLs, numbers, enum members). Output: confident match / ranked
  ambiguous candidates / no match. Pure functions, fixture-tested.
- **2c — LLM escalation (opt-in per drop).** Payload: headers + ≤5 sample
  rows + the candidate shapes' field metadata. Response: a suggested mapping
  in exactly the Step-1 format (structured output, schema-validated, retry on
  mismatch). The UI treats an LLM suggestion identically to a heuristic one —
  it pre-fills the same form. No key configured / declined consent ⇒ the form
  is simply blank, path 2 still works manually.

### Step 3 — UI

A drop zone in the hub (plus file picker) → the three-outcome flow: route
offer (path 1), binding confirm/edit sheet showing suggested domain +
per-column bindings with contract field descriptions inline (path 2), or
name-and-keep-raw (path 3). A minimal generic table view renders any raw
source folder. **No hub cards for mappings in R2** — that is R3's explicit
gate; data visibility comes from the existing domain/Recent-data views.
New Tauri commands (`normalizer_detect`, `normalizer_confirm`,
`normalizer_reproject`) follow the hard rules: `async fn` + `spawn_blocking`,
vault-relative paths, progress via the existing import-progress event pattern.

### Step 4 — Lifecycle

Signature match on a future drop ⇒ auto-conform silently (append + dedupe).
Mapping edited ⇒ `normalizer_reproject(source)`: delete that source's
projected partitions, rewrite from raw. Mapping deleted ⇒ projection removed,
raw kept. Header drift (the BI tool renames a column) ⇒ signature mismatch ⇒
treated as a new shape; the old mapping and data stay intact.

### Step 5 — Tests & the gate

Fixture-driven (`crates/trove-core/tests/fixtures/normalizer/`): synthesized
mini-copies of both real files (same shape, fake values — the real exports
contain names/URLs and stay out of the repo) plus a nothing-fits file. Tests:
detect ranks the right contract for pub_ops; the IMDb header hits the
`imdb` signature; projection output validates against the schema; re-drop
dedupes; re-project is idempotent; unmapped columns land in `extra` verbatim.

**Gate (per the roadmap, anchored on the real files):** David drops the real
`pub_ops fb_pages` export → confirms the suggested `social.post` binding →
reads the rows in the social view with no code written; a following week's
export auto-conforms with zero prompts. Secondary: the IMDb list export gets
routed to the built importer; a junk CSV lands raw and is browsable.

## Ratified rulings (David, 2026-07-20 — do not relitigate)

1. **`social.post.ts` admits date-only.** Relaxed to `anyOf[date-time,
   date]` (schema + domain doc edited same day). The gate file carries `Post
   Date` only, and per the ratified date-only ruling a date is a lexical
   prefix of a timestamp while fake midnights are dishonest. **No standing
   meta-rule was adopted:** the offered pre-authorization ("any domain gets
   the relaxation on first day-granular demand") was *not* taken — each
   future domain's relaxation is asked individually, one explicit
   ratification at a time.
2. **Raw landing locations.** Mapped drops:
   `<domain>/<source>/raw/<original-filename>` (the existing wave idiom —
   `amazon.rs`, `23andme.rs`, et al.). Declined drops:
   `imports/<source>/<original-filename>` — a new top-level vault folder,
   manifest-listed, raw-viewer rendered. R3 may promote `imports/` folders
   into named user categories once mappings are first-class hub citizens.
3. **Path 1 (built-importer routing) stays in.** The drop zone is the one
   front door for any file; compiled `ImportSpec` header signatures are
   checked first, and a match produces an *offer* to run the built import,
   never an automatic run.
4. **LLM key handling: baked + BYO per the ConnectSpec precedent**
   (compiled-in audience), with a recorded **tripwire: public binary
   distribution flips this to BYO-only or the deferred broker** — a baked
   LLM key is a spending credential and must not ship in strangers' hands.
   Model pinned at build time, not in this spec. Consent dialog shows the
   literal payload (headers + ≤5 sample rows) every time — no "always
   allow". No key configured ⇒ the suggest control renders disabled with an
   inline hint pointing at Settings (the disabled-controls-need-affordance
   rule), and manual binding still works.

## Risks / edge cases

- **Detect false positives** are the failure mode that matters: a wrong
  binding writes plausible-looking wrong rows. Mitigations: user confirmation
  is mandatory the first time, validation rejects non-conforming rows, raw +
  re-projection make every mistake reversible.
- **`extra` bloat:** unbound-columns-verbatim is the honest default; raw
  remains the full record either way. Revisit only if real vaults hurt.
- **Large drops:** stream parsing + `spawn_blocking` + progress events;
  never load the whole file into memory.
- **Privacy:** the LLM path is the only networked path, per-drop opt-in,
  payload shown verbatim in the consent dialog, capped at headers + 5 rows.
- **Community mappings** (shareable integrations-as-data) need no extra work
  in R2: a mapping file dropped into `.trove/mappings/` simply works; making
  them first-class citizens is R3.

# Appendix A — The fixture files (real, examined 2026-07-20)

**`pub_ops fb_pages 2026-07-20T1403.csv`** — 500 rows, Looker-style BI
export from a social-publishing operations team. Header: *(unnamed index), Creative
Framing Tags, Framed By, Framed Date Date, Post Date, List Name, Page Name,
Scheduling Type, Post URL, Total Link Clicks*. Why it's the gate: no module
will ever exist for it; it maps cleanly to `social.post` (ts ← Post Date,
url ← Post URL, guid ← hash(Post URL), title ← List Name, context ← Page
Name, kind ← constant, five columns → `extra`); and it exercises the toolkit
on real mess — an unnamed leading column, date-only timestamps,
comma-formatted numbers (`"13,133"`), and constants.

**IMDb custom-list export** (`b3503b34….csv`, 44 rows) — header *(Position,
Const, Created, Modified, Description, Title, Original Title, URL, Title
Type, IMDb Rating, Runtime (mins), Year, Genres, Num Votes, Release Date,
Directors, Your Rating, Date Rated)*. Already parsed by the built `imdb.rs`
(stored at `media/imdb/lists/<stem>.jsonl`), and it fits **no** ratified
contract — a curated list is not a `media.play`. It is therefore the fixture
for path 1 (route to the built importer) and the reference case for honest
declining: had it come from an unknown service, raw landing is the *correct*
outcome, not a forced mapping.

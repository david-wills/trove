# Hypothesis

- **id:** `hypothesis`
- **domains:** `reading/` (contract: **Phase 3 pending** — drafted from
  Readwise + Instapaper + Raindrop + Pinboard + Kindle together; annotations
  are highlight-shaped and join the same draft)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the search API; watermark cursor on `updated`)
- **connection:** `hypothesis` — TokenPaste (personal developer token from
  hypothes.is/account/developer; no OAuth dance). Not shared with other defs.
- **evidence:** official-docs — REST API at hypothes.is/api/ (search endpoint,
  Bearer token); community reference implementation `hypexport`
  (karlicoss/hypexport) confirms shapes
- **effort / priority:** S / P2
- **needs:** none (reading contract not yet ratified — Needs-David at the
  contract-write step, like all `reading/` providers)

## What it is

Hypothesis is the open-source (AGPL) web-annotation layer: users highlight
and annotate any web page or PDF, publicly or in private groups. Niche but
high-value for power users — it captures *what the user thought about what
they read*, the densest signal in the reading domain. A strong supplement to
Readwise for people who annotate the open web directly.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Annotations (public + private) | free (all plans) | annotation text, quoted passage, target URI, tags, created/updated timestamps | official docs |
| Group annotations | free; group ID needed | same fields, scoped to group | official docs (`/api/profile/groups` discovers groups) |

All capability fields are optional in the contract (omit-if-empty); tiering
never needs special code paths.

## Access & auth

- REST: `GET https://api.hypothes.is/api/search?user=acct:USERNAME@hypothes.is`
  with `Authorization: Bearer TOKEN`. Token generated at
  hypothes.is/account/developer — pure TokenPaste, no app registration.
- Private annotations and private-group annotations are returned with the
  token; groups are enumerable via `GET /api/profile/groups`.
- Small response payloads; paginate the search endpoint, cursor on the
  `updated` field for incremental pulls.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `reading/hypothesis/raw/YYYY-MM.jsonl` — full API annotation
  objects, partitioned by month.
- **Contract layer:** `reading/hypothesis/YYYY-MM.jsonl` per the (pending)
  reading contract — expected shape: one row per annotation (`ts`, `source`,
  `guid` = annotation id, `url` = target URI, `title`, `highlight` = quoted
  passage, `note` = annotation body, `tags[]`), overflow (group id, document
  metadata) in `extra`.
- **Dedupe:** annotation id as `guid`; `updated` watermark in
  `.trove/hypothesis-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/hypothesis.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: label/help/placeholder per the SimpleFIN affordance rule —
   help text points at hypothes.is/account/developer; also needs the
   username, captured alongside the token for the `acct:` search param).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the documented search-response shape and the hypexport
   reference (annotation with/without note, with/without tags, group-scoped);
   parser + store + cursor tests, unique temp dirs.
4. Vault writes via `store` helpers once the reading contract is ratified;
   raw layer can land first (per-source raw is always allowed).

## Build notes (completed)

- **Contract fit:** `reading.Highlight` — Hypothesis is explicitly a highlight/annotator in the
  reading domain spec. `guid` = annotation `id`; `ts` = `created` (annotation make-time, NOT
  `updated` — edit-time would misplace annotations on the reading timeline); `text` = extracted
  from `target[0].selector[TextQuoteSelector].exact` only (non-TextQuote selectors with an
  `exact` field are not the highlighted passage and are not surfaced); `note` = annotation `text`
  body; `url` = `uri`; `title` = `document.title[0]`; `tags` = `tags[]`; `group`,
  `links.incontext`, and `updated` → `extra`.
- **Pagination:** `sort=id&order=asc`, cursor = last annotation `id` on the page (stored as
  `last_id` in the non-secret cursor). Using `id` guarantees forward progress even when many
  annotations share the same `updated` timestamp (bulk imports / group migrations). The old
  `sort=updated` cursor with strict-`>` API semantics would silently drop rows beyond the first
  200 when >200 annotations share a timestamp. `updated_after` is still stored for telemetry
  (max `updated` across the drain) but is no longer the pagination cursor. Stop on empty or
  short page. Cursors advance only after a full successful write (crash-safe).
- **Composite credential:** `username::token` — the username goes into the non-secret cursor
  (`.trove/hypothesis-sync.json`); the token is stored 0600 under `.trove/sync/hypothesis`.
- **document.title is `string[]`** — first non-empty element taken.
- **Raw layer:** full API objects verbatim under `reading/hypothesis/raw/YYYY-MM.jsonl`.
- **16 tests pass** (added: multi-page timestamp-tie pagination, created-fallback-to-updated,
  no-timestamps-skip), `cargo check` green.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Public + private annotations | ✅ built | paste `username::token` (from hypothes.is/account/developer) in the connect card; Sync now; confirm rows in `reading/hypothesis/highlights/` + hub last-data |
| Group annotations | ✅ built | annotate in a private group on a real account; Sync now; confirm the group-scoped row arrives with group id in `extra.group` |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption"
§Hypothesis (Web Annotations) (L1576–L1583). Feasibility 🟢 high. Open-source
and active — low shutdown risk. hypexport is the schema reference if the
docs leave a field ambiguous. Sequence with the other `reading/` providers
so the Phase 3 reading contract is drafted from real shapes.

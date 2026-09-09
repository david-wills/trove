# OneNote

- **id:** `onenote`
- **domains:** `notes/` (contract: **Phase 3 pending** — `notes/` shape drafted
  across the notes providers together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (Graph API sync once the Microsoft connection exists).
  Per-section DOCX/PDF export is the Import fallback.
- **connection:** `microsoft` — OAuth (delegated; shared with OneDrive and
  Outlook defs — one Microsoft login, many defs)
- **evidence:** official-docs — Microsoft Graph API; delegated OAuth required
  since 31 Mar 2025; pages return HTML, no bulk export endpoint
- **effort / priority:** M / P2
- **needs:** Needs-login (validation only — build proceeds from documented Graph
  shapes) · `notes/` contract not yet ratified (Needs-David)

## What it is

OneNote is Microsoft's free-form notebook app, popular in enterprise and
education. On the Mac it stores nothing useful locally (the app is essentially
a web wrapper), so the only real read path is the Microsoft Graph API. Worth
building for users in the Microsoft ecosystem; the auth and HTML-to-Markdown
conversion put it at M effort.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Notebooks/sections | all (Graph) | notebook + section names, hierarchy | official docs |
| Page content | all (Graph) | title, HTML body (→ convert to Markdown), created/edited times | official docs |
| Per-section export | all | DOCX/PDF per section (no workspace dump) | official docs |

All optional in the contract. No tier-specific code paths.

## Access & auth

- Graph: `GET /me/onenote/notebooks`, `/sections`, `/pages`,
  `/pages/{id}/content` (returns HTML). Listing requires pagination.
- Auth: OAuth 2.0 **delegated** (user signs in) — app-only auth was removed
  31 Mar 2025. Requires an Azure AD app registration; bundle the Microsoft
  OAuth connection with OneDrive/Outlook (one `microsoft` connection, many
  defs).
- No bulk-export endpoint; page content arrives as HTML and must be converted
  to Markdown with a library.
- Export fallback: File > Export → DOCX/PDF per section (`.one` is proprietary
  binary; skip). Section-by-section only — no workspace-wide dump.
- Standalone-clean: plain HTTPS via Graph; no running external app needed (the
  Mac client stores nothing useful locally).

## Vault mapping

- **Raw layer:** `notes/onenote/raw/…` — native Graph page/section objects
  (HTML bodies preserved), full fidelity.
- **Contract layer:** `notes/onenote/YYYY-MM.jsonl` per the (pending) `notes/`
  contract — expected shape: one row per page (`ts` = created/edited, `source`,
  `guid` = page id, `title`, `body` = Markdown converted from HTML, `path` =
  notebook/section), overflow in `extra`.
- **Dedupe:** OneNote page id as `guid`; `lastModifiedDateTime` watermark in
  `.trove/onenote-sync.json`, rebuildable by scanning output files.

## Build plan (completed)

1. Module `crates/trove-core/src/onenote.rs`: `DEF` (Periodic/hourly),
   `connection: Some("microsoft")` — reuses the shared Microsoft OAuth
   `CONNECTION` from `outlook.rs`. `Notes.Read` scope added to `MICROSOFT`
   provider; `"onenote"` added to `CONNECTION.auto_pull`.
2. Paginated `GET /me/onenote/pages?$expand=parentNotebook,parentSection`
   with `lastModifiedDateTime desc` ordering + watermark cursor in
   `.trove/onenote-sync.json`.
3. HTML body stored verbatim (HTML→Markdown is a read-time rendering opinion
   per the notes schema spec — `body` field comment says "full fidelity;
   trimming or rendering is a read-time opinion").
4. `notes/onenote/YYYY-MM.jsonl` (contract layer, `Note` type) + raw
   `notes/onenote/raw/YYYY-MM.jsonl`, both partitioned by `created` month,
   upserted by page `id`.
5. 13 unit tests cover parsing, upsert, watermark, backcompat, and DEF
   shape; all pass with `cargo test -p trove-core onenote::` + `cargo check`.
6. Import fallback (per-section DOCX/PDF) and HTML→Markdown conversion
   deferred (out of scope for this collector pass).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Page content | 🧪 built | sign in via the Microsoft OAuth connect card (Notes.Read scope must be added to the Azure app); Sync now; confirm rows in `notes/onenote/` + hub last-data; spot-check HTML body fidelity |
| Notebook/section hierarchy | 🧪 built | confirm a row's `folder` field reflects `Notebook/Section` hierarchy |
| Export fallback | — | export a section to DOCX; point the import box at it — out of scope for this collector |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§OneNote (L2931–L2937). Feasibility 🟡 medium — API works but needs an Azure
AD app registration for delegated OAuth (app-only auth removed 31 Mar 2025);
page content is HTML (needs conversion); no bulk export endpoint; M1 export is
section-by-section only. The Mac app stores nothing locally useful. Bundle the
Microsoft OAuth with OneDrive. Popular in enterprise/education — worthwhile for
a general-audience app.

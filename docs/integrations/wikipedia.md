# Wikipedia Contributions

- **id:** `wikipedia`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  edits are public contributions, the closest shape in the taxonomy)
- **status:** 🧪 built (fixture-tested; Periodic/daily; TokenPaste connection — Wikipedia username)
- **unavailable_reason:** none
- **behavior:** Periodic (poll usercontribs; watermark on newest edit
  timestamp)
- **connection:** none — keyless public API; the user supplies a username
  (and optionally extra MediaWiki domains) in the def's settings
- **evidence:** official MediaWiki Action API
  (`action=query&list=usercontribs`), public, paginated, complete edit
  history — official-docs level
- **effort / priority:** S / P2
- **needs:** none

## What it is

Edit history for Wikipedia (and any MediaWiki wiki, including Wikidata).
Editors-only audience, but for them it's a complete public record of
contribution activity going back years — and it is one of the cheapest
integrations in the whole catalog: keyless GET, official, stable.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Edit history | none (public API) | ts, page title, edit comment, size diff, revision ids | official MediaWiki API (research L1692) |
| Other wikis | none | same fields, any MediaWiki domain (wikidata.org, …) | same endpoint per domain (L1694) |

Full diff text is not fetched (metadata only by default) — a deliberate
scope cut; revision ids in `extra` let a future reader fetch diffs on
demand. All fields optional in the contract.

## Access & auth

- `GET https://{domain}/w/api.php?action=query&list=usercontribs&ucuser={username}&ucprop=ids|title|timestamp|comment|sizediff&format=json`,
  paginate with `uccontinue`. No auth, no key.
- Default domain `en.wikipedia.org`; settings allow adding domains (each
  polled independently). Username is per-domain in MediaWiki terms but
  usually identical — one username field, list of domains.
- No TCC, plain HTTPS, standalone-clean.

## Vault mapping

- **Raw layer:** `social/wikipedia/raw/YYYY-MM.jsonl` — usercontribs
  objects as returned, tagged with the source domain.
- **Contract layer:** `social/wikipedia/` rows per the pending
  social-posts contract (`ts`, `source`, `guid` = `{domain}:{revid}`,
  `title` = page title, `body` = edit comment, sizediff + revision ids in
  `extra`).
- **Dedupe:** revision id (domain-qualified) as `guid`; newest-timestamp
  cursor per domain in `.trove/wikipedia-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/wikipedia.rs`: `DEF` (Periodic, daily;
   username + domains settings — disabled-with-hint until username set).
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. Paginated client; fixtures from a real public API response (keyless —
   capture once, no fresh research needed).
4. Tests: pagination (`uccontinue`), multi-domain merge, cursor resume,
   unique temp dirs.
5. Store via `store` helpers; contract rows wait on social-posts
   ratification — raw layer can ship first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Edit history | 🧪 | enter a username with real edits; Sync now; confirm rows in `social/wikipedia/` + hub last-data; second sync pulls only new edits |
| Multi-domain | — | adding wikidata.org would require a second domain in the domains list; multi-domain plumbing is wired but the UI currently only exposes the default domain |

## Build notes (2026-06-17)

- **Status:** 🧪 built — Periodic/daily, TokenPaste connection (Wikipedia username, no API key).
- **Contract:** `social.Post` reused (`kind:"edit"`, `guid="{domain}:{revid}"`,
  `title`=page title, `text`=edit comment, `context`=domain, extras: `revid`, `parentid`, `sizediff`, `minor`, `tags`).
- **Raw layer:** `social/wikipedia/raw/YYYY-MM.jsonl` — full API objects tagged with `_domain`.
- **Cursor:** `.trove/wikipedia-sync.json`, watermark per domain (RFC3339 of newest edit seen).
  Incremental passes use `ucend=<watermark>` with default `older` direction (newest-first).
- **Dedupe:** guid set loaded from vault before each pull; only new revids written to contract layer.
  Raw layer is unconditionally appended (full fidelity) every sync.
- **Tests:** 8 unit tests green — contract/raw write, dedupe, pagination drain, cursor persistence,
  empty response, timestamp parsing, field mapping, back-compat deserialise.
- **Narrower than brief:** Multi-wiki (Wikidata) is wired but only the default domain
  (`en.wikipedia.org`) is polled; a future settings UI can add domains to the list.

## Research notes

`integrations-research.md` → Web Activity §Wikipedia Contributions
(L1688–L1695). Feasibility 🟢 high. Niche-but-trivial is the whole
justification: low cost buys completeness for editor users. No ToS risk —
this is the documented public API used exactly as intended.

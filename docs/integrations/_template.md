# <Provider display name>

- **id:** `<provider-id>` (lowercase-dash; = module name, source folder, def id prefix)
- **domains:** <vault domain folder(s) this writes>
- **status:** 🚫 unavailable | 📋 queued | 🚧 building | 🧪 built | ✅ validated
- **unavailable_reason:** <only for 🚫 — the honest one-liner the app shows>
- **behavior:** Periodic / Import / Live / NativeHost / CoveredBy / NotWired
- **connection:** none | `<connection-id>` (OAuth / TokenPaste; shared with which defs)
- **evidence:** official-docs <links> | community-schema <links + confidence> | sample-required
- **effort / priority:** S·M·L·XL / P0–P2
- **needs:** none | Needs-sample | Needs-login | Needs-David (<what>)

## What it is

One paragraph: the service, who uses it, why the data matters.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|

All capability fields are optional in the contract (omit-if-empty); tiering
never needs special code paths.

## Access & auth

Endpoints / file paths / export mechanism; auth shape; rate limits;
permissions (TCC); standalone-rule notes.

## Vault mapping

- **Raw layer:** `<domain>/<provider-id>/…` — what's stored, partitioning.
- **Contract layer:** which ratified contract(s), field mapping, what goes
  in `extra`, the dedupe `guid`.

## Build plan

Concrete steps for the loop iteration (module, connection, fixtures,
tests). Note anything the registry doesn't give for free.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|

## Research notes

Distilled from `integrations-research.md` (link the section): gotchas,
ToS/risk notes, time-sensitivity, alternatives considered.

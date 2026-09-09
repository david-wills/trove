# Read.ai

- **id:** `read-ai`
- **domains:** `meetings` (contract: **not yet ratified** — Phase 3 drafts it
  from Granola + Fathom + Zoom + Fireflies together; Read.ai conforms once
  ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll for completed meetings; watermark cursor —
  webhooks are Enterprise-only, so polling is the design, not a fallback)
- **connection:** `read-ai` — TokenPaste (API key; Bearer token)
- **evidence:** official-docs — open-beta REST docs (support.read.ai article
  49381161088659) + official MCP announcement (read.ai/post/read-ai-mcp)
- **effort / priority:** S / P2
- **needs:** Privacy-sensitive (opt-in) · Needs-David (contract: meetings)

## What it is

AI meeting notetaker popular in enterprise Zoom/Teams/Google Meet workflows:
joins meetings, produces transcripts, summaries, action items, and speaker
analytics. Meeting content is otherwise never captured locally, so this is
high-value for Read.ai users — but the API is **open beta**, so the module
must pin a version and expect breaking changes.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Meeting list | open beta (no plan gate stated) | title, time, participants | official open-beta docs |
| Meeting report | open beta | transcript, summary, action items | official open-beta docs |
| Speaker analytics | open beta | speaker stats per meeting | official open-beta docs |

All optional in the contract (omit-if-empty); no tier-specific code paths.
Webhooks (Enterprise+) are not used — polling covers everyone.

## Access & auth

- REST (open beta): list meetings + get meeting report endpoints; base URL
  per the support.read.ai article. Auth: Bearer token (API key).
- No rate limits documented in the research entry — default to the registry's
  gentle periodic cadence.
- No TCC, no local files. Standalone-clean (plain HTTPS). Official MCP exists
  but requires a running client — not a path for the compiled-in collector.

## Vault mapping

- **Raw layer:** `meetings/read-ai/raw/YYYY-MM.jsonl` — the API meeting/report
  objects, full fidelity.
- **Contract layer:** `meetings/read-ai/YYYY-MM.jsonl` per the (pending)
  meetings contract — one row per meeting (`ts`, `source`, `guid` = meeting
  id, `title`, `attendees[]`, `summary`); transcripts as sidecar documents;
  action items + speaker stats in `extra` unless the ratified contract claims
  them.
- **Dedupe:** API meeting id as `guid`; cursor in `.trove/read-ai-sync.json`,
  rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/read_ai.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste with setup copy pointing at where the key lives in Read.ai
   settings), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the open-beta docs' example responses; parser + store +
   cursor tests, unique temp dirs. Pin the API version string; treat unknown
   fields as `extra` so beta churn degrades gracefully.
4. Privacy: meeting transcripts are message-body-class content — ship the
   toggle default-off with explicit opt-in acknowledgement.
5. Parked behind Needs-David (meetings contract) like the rest of the domain;
   sequence after Granola/Fathom/Fireflies have exercised the contract.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Meeting list + summaries | 🧪 built | paste a real API key; Sync now; confirm rows in `meetings/read-ai/` + hub last-data |
| Transcript + sidecar | 🧪 built | same run; confirm `.md` sidecar and `transcript_ref` land for a meeting that has them |
| Speaker analytics | raw-only via extra | action_items/platform_id land in `extra`; no separate analytics endpoint documented |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Read.ai
(L546–L553). Feasibility 🟢 high. Open-beta API: pin a version, monitor for
breaking changes. Historical export: the API is the only flexible bulk path
(Zapier only captures new meetings).

API field names confirmed from `@hyperdrive.bot/read-ai` npm package
(v0.1.2, `@hyperdrive.bot/read-ai/-/read-ai-0.1.2.tgz`), which is an
independent open-source CLI wrapping the same open-beta REST API. Key
differences from Fathom: auth is `Authorization: Bearer` (not `X-Api-Key`),
timestamps are Unix-epoch milliseconds (not RFC3339), cursor is the last item's
`id` (not a `next_cursor` field), and `transcript` is a plain string (not an
utterance array). Speaker analytics are not surfaced as a separate field in the
list/get endpoint; `action_items` and `platform_id` route to `extra`.

# Sunsama

- **id:** `sunsama`
- **domains:** `tasks/` (contract: **tasks** — ✅ ratified; not written —
  this provider is unavailable)
- **status:** 🚫 unavailable
- **unavailable_reason:** Sunsama has no public API and no reliable export;
  API/MCP access is gated behind the $65/month Power Pro plan. Connect the
  upstream sources it pulls from (Todoist, Asana, Linear, Jira) instead.
- **behavior:** Unavailable (not toggleable, never default_on)
- **connection:** none
- **evidence:** community — no public API docs found; API/MCP listed only as
  Power Pro plan features; manual export reported near-impossible. Low
  confidence on any programmatic path.
- **effort / priority:** L / P2
- **needs:** none

## What it is

Sunsama is a daily-planning app that aggregates tasks from other tools
(Todoist, Asana, Linear, Jira, calendars) into a guided daily/weekly plan. It
is a *destination* for task data, not an authoritative store — almost
everything in it originates upstream.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| (none accessible) | API/MCP gated to Power Pro ($65/mo); no public docs; no CSV/JSON export | — | community |

## Access & auth

No documented public API endpoints. API and MCP access are advertised only as
Power Pro ($65/month) features with no public schema. There is no
CSV/JSON export — community reports describe manual copy-paste as the only
way out. A free-API feature request has been open for years. With no
documented endpoint, no export, and a price wall serving a tiny subset of
users, there is no standalone-compatible path worth building.

## Vault mapping

none — unavailable, nothing is written. Were it ever built, its
authoritative records originate upstream and would route by shape (tasks →
`tasks/` per the ratified tasks contract), so the correct move is to connect
those upstream providers directly.

## Build plan

none — catalogued as unavailable so the hub answers "why isn't Sunsama
available?" in-app. Revisit only if Sunsama ships a documented, non-gated
API or a real export.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| — | 🚫 | n/a — unavailable; card renders dim with the reason |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Sunsama (L2674–L2680). Feasibility 🟠 low / icebox. The research doc's
explicit recommendation is to capture the upstream sources Sunsama pulls
from rather than Sunsama itself — recorded here as the honest user-facing
alternative.

# Trello

- **id:** `trello`
- **domains:** `tasks/` (contract: **✅ ratified** — `tasks`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (full-snapshot diff every 15 min; no watermark needed)
- **connection:** `trello` — TokenPaste (API key + user token from the Trello
  Developer Portal; key is per-app and free, token via OAuth 1.0 flow but
  pasted in). Not shared with other defs.
- **evidence:** official-docs — api.trello.com/1 (stable REST, full
  card/board/list model)
- **effort / priority:** S / P2
- **needs:** none

## What it is

Kanban-style task/project tool: boards → lists → cards, with due dates,
checklists, labels, members, attachments, and comments. Large user base.
Cards map cleanly onto the `tasks` contract (a card is a unit of work with a
status given by its list), making this a straightforward second/third source
to exercise the ratified contract.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Boards / lists | all plans | names, ids, closed flag | official docs |
| Cards | all plans | name, desc, due, labels, members, dateLastActivity | official docs |
| Checklists | all plans | items, checked state | official docs |
| Comments / actions | all plans | text, member, date | official docs |

All optional in the `tasks` contract (omit-if-empty).

## Access & auth

- REST: `GET /1/members/me/boards?key={key}&token={token}`, then `GET
  /1/boards/{id}?fields=all&cards=all&lists=all` returns a **full board
  snapshot in one call** — efficient. Cards carry due dates, checklists,
  labels, attachments, comments.
- Rate limits: 100 req/10s per token, 300 req/10s per key — fine for periodic.
- No TCC, no local files. Standalone-clean (HTTPS). Board-menu JSON export is
  an Import fallback if API access is ever unavailable.

## Vault mapping

- **Raw layer:** `tasks/trello/raw/YYYY-MM.jsonl` — board snapshot objects,
  full fidelity (boards/lists/cards as returned).
- **Contract layer:** `tasks/trello/YYYY-MM.jsonl` per the ratified `tasks`
  contract — one row per card (`ts` = created (decoded from the card id's
  embedded timestamp) or `dateLastActivity`, `source`, `guid` = card id,
  `title` = name, `status` derived from the card's list (open list vs.
  archived/done), `due`, `labels[]`), overflow (board/list names, checklist
  items, members, comments) in `extra`.
- **Dedupe:** card id as `guid`; raw layer partitioned by stable creation time
  (ObjectId-decoded) so edits/moves never cause cross-month duplication; cursor
  file `.trove/trello-sync.json` reserved as a stub.

## Build plan

1. Module `crates/trove-core/src/trello.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste — key + token, per the SimpleFIN affordance rule), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the official docs example board snapshot (with cards, lists,
   checklists, labels); parser + store + cursor tests, unique temp dirs.
4. Status mapping: Trello has no native done flag — derive `status` from list
   membership / `closed`; document the heuristic and keep the raw list name in
   `extra` so readers can re-derive.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Boards + cards | 🧪 built | paste `key=K token=T` in the connect card; Sync now; confirm rows in `tasks/trello/tasks.jsonl` + hub last-data |
| Checklists | 🧪 built | open a card with checklist items; confirm they land in `subtasks` on the task row |
| Done-list detection | 🧪 built | move a card to a "Done" list; next sync logs a `completed` event and removes it from the snapshot |
| Raw firehose | 🧪 built | open `tasks/trello/raw/YYYY-MM.jsonl`; confirm full card objects including `badges`, `labels`, `idMembers` |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity" §Trello
(L2594–L2600). Feasibility 🟢 high. The single full-snapshot call keeps the
pull cheap. Power-Ups (calendar, timeline) are reachable via the same API but
out of scope for v1. Sequence alongside Todoist/Linear/Asana to validate the
ratified `tasks` contract across differing native models.

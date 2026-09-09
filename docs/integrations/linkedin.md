# LinkedIn

- **id:** `linkedin`
- **domains:** `contacts/` (contract: **Phase 3 pending** — contacts),
  `correspondence/` (messages — contract: **✅ ratified**), `social/`
  (posts/comments/reactions — contract: **Phase 3 pending** social-posts;
  profile snapshots stay per-source raw under `social/linkedin/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user-initiated archive export; re-runnable)
- **connection:** none (no API key, no OAuth — the official API never
  exposes personal connection data; export is the only path)
- **evidence:** official export, stable for years and still available in
  2026; `Connections.csv` schema community-documented; LinkedIn API
  explicitly blocked for personal connections (research: do not invest in
  an API connector)
- **effort / priority:** S / P1
- **needs:** privacy (archive messages are message bodies — the
  correspondence slice ships opt-in with explicit acknowledgement)

## What it is

The professional network — where most professional contacts originate.
The official data export yields the connection list (with `Connected On`
dates, enabling relationship-timeline analysis), full message history,
and the user's posts/comments/reactions. One provider, three vault
domains: data routes by shape, never split per record.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Connections.csv | free, ~10–24 min scoped export | First/Last Name, Email (sparse — present for only 10–20%), Company, Position, Connected On | official export |
| Messages (full archive) | free, archive up to 72 h | conversation history, CSV | official export |
| Posts/comments/reactions | free, full archive | own posts, comments, reactions; articles as HTML | official export |
| Profile / search history / ads data | free, full archive | profile info, searches, ad interactions | official export |

All optional in the contracts; emails absent by privacy default — never
assume present. Phone numbers are never included.

## Access & auth

- LinkedIn.com → Settings & Privacy → Data privacy → Get a copy of your
  data. Two grades: scoped Connections-only export (ready in ~10–24 min,
  link by email) or the full archive (up to 72 h). CSV, UTF-8; some files
  JSON/HTML.
- No auth in-app: the user drags the export in (generic registry import
  box). Re-export quarterly is sufficient cadence — the import card should
  say so.
- The LinkedIn REST API (v1 deprecated 2015; v2 partner-gated) does not
  expose personal connections — hard research conclusion, not revisited.
- No TCC, no network. Standalone-clean.

## Vault mapping

- **Raw layer:** `contacts/linkedin/connections.jsonl` (one row per
  connection), `social/linkedin/raw/` (posts/comments/reactions/profile
  as exported), messages raw alongside the correspondence rows.
- **Contract layer:**
  - Connections → Phase 3 contacts contract; `source: linkedin`;
    `Connected On` preserved as the relationship-start field.
  - Messages → `correspondence/linkedin/YYYY-MM.jsonl` per the ratified
    correspondence contract (one row per message; overflow in `extra`).
  - Posts/comments → `social/linkedin/` per the pending social-posts
    contract (archive DMs route to `correspondence/`, per the taxonomy
    routing rule).
- **Dedupe:** connections idempotent by profile URL when present, else
  name+company composite key; messages by conversation+timestamp+sender
  composite (the CSV has no message ids — derive a stable hash guid).

## Build plan

1. Module `crates/trove-core/src/linkedin.rs`: `DEF` (Import), archive
   detector (handles both the scoped `Connections.csv` and the full ZIP).
2. Registration line in `INTEGRATIONS`; the registry import box is free.
3. Parse order: Connections.csv first (the P1 payload, schema documented);
   messages CSV second; posts last.
4. Privacy gate: the messages slice is conversation content — opt-in with
   explicit acknowledgement at import time (offer connections-only import
   without it). Contacts rows alone are not gated.
5. Fixtures: hand-built CSVs matching the documented header set, including
   a missing-email row and a UTF-8 name; idempotent re-import test; unique
   temp dirs.
6. Contacts + social contract rows conform when Phase 3 ratifies; raw
   rows land now (full fidelity first).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connections.csv | ✅ unit-tested | request the scoped export on a real account; drag into the import box; confirm `contacts/linkedin/connections.jsonl` rows + `connected_on` dates; re-import is a no-op (snapshot overwrite) |
| Messages | ✅ unit-tested | full-archive export; confirm `correspondence/linkedin/YYYY-MM.jsonl` rows render in Recent data; re-import skips dupes via sha256 guid |
| Posts/comments/shares | ✅ raw only | same archive; `social/linkedin/raw/shares.jsonl`, `comments.jsonl`, `reactions.jsonl` written verbatim; social contract rows pending Phase 3 ratification |

## Build notes (follower fan-out 2026-06-15, fixed 2026-06-15)

- Behavior: `Import` — accepts `.zip` (full archive) or bare `Connections.csv`
- Contract: `reuse-bound` contacts (connections → `Contact` snapshot); correspondence (messages → `Message` stream); social raw (posts/comments/reactions → `social/linkedin/raw/*.jsonl`)
- **Connections.csv real header:** `First Name,Last Name,URL,Email Address,Company,Position,Connected On` — URL (profile link) is the 3rd column; present in all real exports
- Connections id: sha256 of profile URL when present (the stable, collision-free key per brief); falls back to sha256 of `given|family|company|connected_on` composite when URL is absent
- Profile URL stored in `extra.profile_url` (contacts.rs: "urls ride verbatim in extra")
- **Messages.csv real header includes TO and RECIPIENT PROFILE URLS** — both parsed; `TO` populates `Message.to`; profile URL fields are deserialized for fidelity (no extra field on Message, kept in struct)
- Privacy gate: messages import requires `import_messages=yes` param (brief § 4 "opt-in with explicit acknowledgement")
- `from_me` set via optional `owner_name` param (mirrors instagram.rs pattern)
- FOLDER column mapped to `Message.labels` (correspondence contract: "source-native labels/folders")
- Messages guid: sha256 of `conversation_id|date|from|content|index` — content + per-second index disambiguates same-second same-sender messages; `seen` set is mutated in-loop so intra-import collisions are deterministic (first row wins)
- `Connected On` date stored verbatim in `extra.connected_on` (format `DD MMM YYYY`)
- Messages UTC timestamps converted to local RFC3339 via chrono
- Preamble rows in Connections.csv handled: skip to first line starting with `First Name`
- Social CSVs (shares/comments/reactions) written as generic key-value JSONL with no schema binding
- 17 unit tests, all passing; `cargo check` clean (0 warnings in our code)

## Research notes

`integrations-research.md` → "People, Contacts & Relationship Graph"
§LinkedIn Connections Export (L696–L702) and "Social Media & Web Presence"
§LinkedIn (L4040–L4046). Feasibility 🟢 high; cross-cutting note 6:
"LINKEDIN API IS BLOCKED — do not invest engineering time in a LinkedIn
API connector." Research vault paths predate the taxonomy; the README
table's routing (contacts / correspondence / social) governs.

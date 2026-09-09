# Microsoft Teams

- **id:** `microsoft-teams`
- **domains:** `correspondence` (contract: ✅ ratified) + `meetings` (contract:
  **not yet ratified** — Phase 3 drafts it from Granola + Fathom + Zoom +
  Fireflies + Teams together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll chats + meeting transcripts; cursor per chat)
- **connection:** `microsoft` — OAuth (Microsoft Entra app registration;
  shared with future Outlook/OneDrive/Microsoft 365 defs, the Google model)
- **evidence:** official-docs — Microsoft Graph (`/me/chats`,
  `/me/chats/{id}/messages`, `/me/onlineMeetings/{id}/transcripts`);
  transcript APIs GA and unmetered since 2025-08-25
- **effort / priority:** M / P2
- **needs:** privacy-sensitive (message bodies — opt-in) · Needs-login
  (validation needs a work/school Microsoft 365 account) · Needs-David
  (contract: meetings, for the transcript slice)

## What it is

Microsoft's team-chat + meetings platform, dominant in enterprise. For
work-account users it holds years of DMs, group chats, channel messages, and
(where transcription was enabled) full meeting transcripts — work
correspondence that exists nowhere else on the machine.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| DMs + group chats | work/school account (Graph `Chat.Read`) | sender, body, ts, chat id, participants | official Graph docs |
| Channel messages | work/school account (`ChannelMessage.Read.All`; admin consent likely) | sender, body, ts, team/channel | official Graph docs |
| Meeting transcripts | work/school account; scheduled online meetings with transcription enabled (`OnlineMeetingTranscript.Read`) | VTT utterances w/ speakers | official Graph docs (GA, unmetered since 2025-08-25) |
| Privacy-download JSON import | personal Microsoft accounts (account.microsoft.com → Privacy → Download your data) | chat history JSON | official export mechanism |

All optional in the contract; a personal-account user simply gets the import
path and no live transcripts. No tier-specific code paths.

## Access & auth

- Graph REST: `GET /me/chats`, `GET /me/chats/{id}/messages`,
  `GET /teams/{id}/channels/{id}/messages`,
  `GET /me/onlineMeetings/{id}/transcripts(/{tid}/content)`. OAuth 2.0 via
  Entra app registration; baked + BYO client id per ConnectSpec.
- Personal consumer accounts (outlook.com/hotmail) cannot use the Teams
  Graph endpoints — their path is the privacy-download JSON Import.
- Transcripts exist only for scheduled online meetings where transcription
  was on; available only after the meeting ends. EWS dies October 2026 —
  Graph is the only API path.
- No TCC, no local files. Standalone-clean (plain HTTPS).
- **Privacy:** message bodies and transcripts — ships opt-in with explicit
  acknowledgement, per the privacy-sensitive needs-flag rule.

## Vault mapping

- **Raw layer:** `correspondence/microsoft-teams/raw/YYYY-MM.jsonl` (Graph
  message objects, full fidelity); transcript VTTs as sidecar artifacts under
  `meetings/microsoft-teams/`.
- **Contract layer:** chats/channel messages → ratified `correspondence`
  rows (`guid` = Graph message id, `thread` = chat/channel id, overflow like
  reactions/mentions in `extra`). Meeting transcripts → pending `meetings`
  contract; that slice is parked behind Needs-David (contract) and ships
  second.
- **Dedupe:** Graph message id / transcript id as `guid`; per-chat cursor in
  `.trove/microsoft-teams-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/microsoft_teams.rs`: `DEF` (Periodic),
   `pull` hook; `CONNECTION` `microsoft` declared here until an Outlook
   module exists to own it (record sharing intent in the def comment).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Import box accepts the personal-account privacy-download JSON
   (parser-last if no sample surfaces — flag Needs-sample for that variant).
4. Fixtures from Graph docs example responses (chat, channel, transcript
   VTT); parser + store + cursor tests, unique temp dirs.
5. Meetings slice lands only after the Phase 3 meetings contract ratifies.

## Build notes (fan-out)

- Reuses `crate::outlook::microsoft_fresh_token` / `microsoft_accounts` for auth.
- Reuses the "microsoft" connection (`connection: Some("microsoft")`); no new ConnectionDef.
- **Scope gap:** the existing `MICROSOFT` provider bundles `Mail.Read Calendars.Read User.Read offline_access`. Teams chat requires `Chat.Read`. That scope must be added to `crate::outlook::MICROSOFT.scopes` and the Entra app, flagged `Needs-David(scope-update)`.
- Raw layer: `correspondence/microsoft-teams/raw/YYYY-MM.jsonl` (full Graph chatMessage objects).
- Contract layer: `correspondence/microsoft-teams/YYYY-MM.jsonl` (source="microsoft-teams", guid=Graph message id).
- Cursor: `.trove/microsoft-teams-sync.json` (per-account, per-chat `since` watermark).
- Meeting transcript slice parked (Needs-David(meetings-contract) — ships after meetings contract ratifies).
- 17/17 tests pass; cargo check green.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Chats + channel messages | built (needs scope-update in Entra app) | OAuth a work/school account; add Chat.Read scope; Sync now; rows in `correspondence/microsoft-teams/` + hub last-data |
| Meeting transcripts | parked (Needs-David: meetings contract) | — |
| Privacy-download import | not built (personal accounts fallback) | — |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Microsoft Teams
(L361-L367) + "Calls, Voice & Meeting Transcripts" §Microsoft Teams
Transcripts (L578-L585). Feasibility 🟡 medium — the API is solid but the
work-account requirement limits the audience; Entra OAuth is heavier than
consumer OAuth. Sequence after Outlook email so the `microsoft` connection
is justified by more than one def.

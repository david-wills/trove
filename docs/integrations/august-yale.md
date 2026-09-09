# August / Yale Smart Lock

- **id:** `august-yale`
- **domains:** `home/` (contract: **deferred-sibling-draft** — lock events fit
  the `home.event` Phase-3 draft shape, not the bound `HomeReading`; raw +
  pre-shaped events written; bind pending David gate on `home.event`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the activity log; watermark cursor)
- **connection:** `august-yale` — TokenPaste-shaped: August/Yale account
  email + password + SMS 2FA at connect time (unofficial API). Not shared
  with other defs.
- **evidence:** community-schema — yalexs (github.com/Yale-Libs/yalexs),
  the reference implementation Home Assistant uses; actively maintained.
  Confidence medium (undocumented API can change).
- **effort / priority:** M / P2
- **needs:** Needs-login (real account + a lock required to validate; build
  proceeds from yalexs-documented shapes) · home contract not yet ratified
  (Needs-David)

## What it is

Entry/exit log from August and Yale smart locks: every lock/unlock event
with who did it and how (app, keypad code, auto-lock). Presence-rich data —
it timestamps comings and goings at the front door, including guests via
their codes. August/Yale has the largest consumer smart-lock installed
base, which is why it's the brand pick.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Lock/unlock log | all locks | ts, event type, user, method (app/keypad/auto-lock) | yalexs activity log |
| Guest-code events | all locks | code-holder identity on keypad events | yalexs (research doc) |
| Lock state | all locks | current locked/unlocked, battery | yalexs |

All optional in the contract; a lock without a keypad simply yields no
keypad-method rows.

## Access & auth

- Unofficial cloud API (developer.august.com endpoints, reverse-
  engineered); no local protocol exists for any major consumer lock brand
  — cloud-only is inherent to the category, not a design choice.
- Auth: account credentials + phone number with **SMS 2FA at connect
  time**; the connect card must handle the 2FA code prompt.
- Known instability from HA bug reports: the event listener breaks after
  1–2 days — use periodic polling with reconnect, not a long-lived
  listener.
- Unofficial API may change without notice — degrade with a clear hub
  error, never fail silently.
- No TCC. Network egress to Yale cloud only, labeled on the card.

## Vault mapping

- **Raw layer:** `home/august-yale/raw/YYYY-MM.jsonl` — activity-log
  records as returned.
- **Contract layer:** lock events are discrete events, not sensor
  readings — likely per-source raw alongside the (pending) home contract;
  decide at ratification. Rows: `ts`, `guid`, lock id, event type, user,
  method, `extra` overflow.
- **Dedupe:** `guid` from the API's event id (or lock id + ts + type if
  ids prove unstable); cursor = latest event ts, rebuildable.

## Build plan

1. Module `crates/trove-core/src/august_yale.rs`: `DEF` (Periodic), `pull`
   hook; reimplement the yalexs auth + activity endpoints in Rust (no
   Python sidecar — absorb as a library).
2. `CONNECTION`: credentials + SMS-2FA step in the connect flow (TokenPaste
   method with a verification-code stage; setup copy on the def).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from yalexs's documented response shapes (app-unlock, keypad
   w/ guest code, auto-lock variants); parser + store + cursor tests,
   unique temp dirs.
5. Poll-with-reconnect design from day one (listener instability is
   documented); surface auth-expiry as a reconnect prompt on the card.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connect + 2FA | Needs-login | real August/Yale account: complete the SMS code flow; connection shows connected (David has no lock; any real user's run validates) |
| Lock/unlock log | Needs-login | lock/unlock via app and keypad; Sync now; confirm both rows with correct method in `home/august-yale/events/` |
| Raw layer | Needs-login | confirm `home/august-yale/raw/YYYY-MM.jsonl` written with verbatim API objects |
| Multi-day stability | Needs-login | leave enabled 3+ days; confirm polling survives (periodic reconnect avoids listener-expiry, since we poll not listen) |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §August / Yale
Smart Lock (L1912–L1918). Feasibility 🟡 medium. Alternatives considered:
Schlage Encode (similar unofficial API), Kwikset Halo (SmartThings path) —
August/Yale chosen for installed base; all brands are cloud-backend-only.
Entry logs are presence-adjacent: while not on the mandatory
privacy-flag list, the brief recommends honest "comings and goings" copy
on the enable card.

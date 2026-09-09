# IRC Logs (ZNC / WeeChat / Irssi)

- **id:** `irc`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (re-scan known log directories for new/grown
  files; LocalSync, no network)
- **connection:** none
- **evidence:** community-schema — real plaintext log formats of the three
  major clients, trivially verifiable against any user's own logs
  (confidence high; formats are stable for decades)
- **effort / priority:** S / P2
- **needs:** privacy-sensitive (message bodies — ships opt-in with explicit
  acknowledgement)

## What it is

Plaintext chat logs left on disk by the three IRC setups still in use in
2026: a ZNC bouncer, WeeChat, and Irssi. A tiny user base — but those
users are exactly the open-source/developer crowd, often with a decade-plus
of logs, and the parse is near-zero effort.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Channel + query messages | none (free clients) | ts, nick, body, channel, network | real log files |
| Join/part/action lines | none | event lines per client format | real log files |

All optional in the contract; a user with only one client's logs yields
only that slice. No tier-specific code paths.

## Access & auth

- ZNC: `~/.znc/users/<user>/networks/<net>/moddata/log/<channel>/YYYY-MM-DD.log`
- WeeChat: `~/.weechat/logs/<network>.<channel>.weechatlog`
- Irssi: `~/.irssi/logs/`
- All plain text, consistent timestamps per client
  (`[HH:MM:SS] <nick> message` shape); one regex-based parser with three
  client dialects covers everything.
- Home-directory paths — no TCC, no FDA, no network. Standalone-clean.
- Let the user also point at a custom directory (bouncer logs synced from
  a server) via the generic import box.

## Vault mapping

- **Raw layer:** none needed — the on-disk logs *are* the raw layer; Trove
  never copies them (file paths + read offsets tracked instead).
- **Contract layer:** `correspondence/irc/YYYY-MM.jsonl` per the ratified
  correspondence contract — `chat` = `network/#channel` (or `network/nick`
  for queries), `sender` = nick, `from_me` when the nick matches the
  client's configured nick, `kind:"event"` for join/part lines, `service`
  = client name (`znc`/`weechat`/`irssi`). ISO dates come from filename +
  line time (logs carry no timezone — assume local, note in `extra`).
- **Dedupe:** `guid` = `file-relative-path:line-number` (logs are
  append-only); per-file byte-offset cursors in `.trove/irc-sync.json`,
  rebuildable by rescanning.

## Build plan

1. Module `crates/trove-core/src/irc.rs`: `DEF` (Periodic, slow tick),
   path probing for the three default locations, `pull` hook.
2. One registration line in `INTEGRATIONS` (no connection).
3. Fixtures: hand-built log files per client dialect (channel msg, action,
   join/part, query) — formats are simple enough to author confidently
   from the documented shapes; first real-user run confirms.
4. Privacy: message bodies — opt-in toggle with explicit acknowledgement.
5. Handle log rotation/growth: re-stat known files, resume at stored
   offset; new files picked up by directory scan.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| ZNC logs | ✅ | unit tests: `znc_collect_basic`, `incremental_no_double_write` |
| WeeChat logs | ✅ | unit test: `weechat_collect_basic` |
| Irssi logs | ✅ | unit test: `irssi_collect_basic` |

## Build notes (2026-06-16)

Behavior: `Periodic` (hourly, `every_on_run`). No connection, no login.

**Dialect detection** (from source code evidence):
- ZNC (`znc/modules/log.cpp`): `[HH:MM:SS] <nick> message`; actions `* nick text`; events `*** Joins/Parts/Quits/…`
- WeeChat (`logger-config.c`): ISO-8601 timestamp + tab separator: `YYYY-MM-DDTHH:MM:SS.μsZ\tnick\ttext`; event nicks are `-->` `<--` `--` etc.
- Irssi (`log.c`): `HH:MM <nick> message`; events `-!-`; day-change banner `--- Day changed %a %b %d %Y`

**WeeChat UTC timestamps**: WeeChat 4.7+ added `%@` (UTC marker) and 4.10+ made UTC the default logger format, appending a trailing `Z`. The parser detects the `Z` suffix and converts via `Utc.from_utc_datetime` → `.with_timezone(&Local)` to preserve the correct UTC instant; space-separated timestamps without `Z` (WeeChat < 4.7) continue to be treated as local wall-clock time.

**WeeChat ACTION lines**: `/me` actions write `" *"` (space-star) as the nick field with `"actnick body"` in the text field. The parser detects this prefix, extracts the actor nick from the text field, and emits `kind="message"` with the correct sender — matching ZNC/Irssi action handling.

**WeeChat event detection**: Prefix-based classification (`-->`, `<--`, `--`, `=`, `=!=`) covers default `weechat.look.prefix_*` settings. A text-pattern fallback (`has joined`/`has left`/`has quit`/`has been kicked`) ensures join/part/quit lines are classified as `kind="event"` even when the user has customised the prefix strings.

**Guid** = `<absolute-file-path>:<line-number>` (stable; logs are append-only). Cursor in `.trove/irc-sync.json` (file path → byte offset).

**from_me**: not set (no config param for own nick; deferred to a future enhancement).

**Narrower than brief**: The brief mentions privacy opt-in toggle. `default_on: false` + `toggleable: true` covers this; no additional UI needed (the registry toggle IS the opt-in).

36 unit tests, all green. cargo check clean.

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §IRC (ZNC / WeeChat
logs) (L425-L431; at-a-glance L204). Feasibility 🟢 high; effort S.
Very small 2026 user base (libera.chat and OFTC are the main networks) but
near-zero build cost and a good signal for developer users. ZNC also has
an IRC v3 self-message log extension — covers `from_me` rows when the
bouncer relays own messages.

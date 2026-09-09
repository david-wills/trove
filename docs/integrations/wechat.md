# WeChat

- **id:** `wechat`
- **domains:** `correspondence` (contract: ✅ ratified — would apply if ever
  buildable)
- **status:** 🚫 unavailable
- **unavailable_reason:** WeChat encrypts its local database with a key that
  exists only in the running app's memory; extracting it requires attaching
  to the process, which WeChat's terms prohibit. WeChat offers no export
  feature.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** hard block — research front matter L139; community tooling
  (Chatlog CLI, wechat-db-decrypt-macos approach) discontinued Oct 2025 for
  WeChat-policy compliance
- **effort / priority:** XL / P2
- **needs:** privacy-sensitive (message bodies — would be opt-in if ever
  buildable)

## What it is

Tencent's messaging super-app (~800M users, primarily East Asia). The macOS
desktop client stores full chat history locally — but inside
SQLCipher-encrypted SQLite databases whose keys never touch disk.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| none — all paths blocked | — | — | research front matter L139 |

## Access & auth

None viable. The local DBs live at
`~/Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files/<account>/db_storage/`,
SQLCipher-encrypted. The 32-byte key is derived at runtime (WeChat ID +
machine UUID) and cached only in process memory; extraction means attaching
lldb to the running WeChat process and scanning memory — a standalone-rule
violation, a ToS violation, and as of WeChat 4.x there are 24 separate
per-database keys. No official export or personal API exists. Unlike
Signal, the key is never stored on disk or in the Keychain.

## Vault mapping

Would be `correspondence/wechat/` under the ratified correspondence
contract. Not applicable while blocked.

## Build plan

None. Ships as a catalogued unavailable card (`Behavior::Unavailable`) with
the reason above, so the app answers "why isn't WeChat available?" honestly.
Revisit only if WeChat ships an official export (the GDPR-style trigger to
watch for).

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Unavailable card | — | hub shows WeChat greyed, sorted last in Correspondence, with the reason copy above |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §WeChat (macOS
desktop) (L385-L391) + Hard Blocks front matter (L139). Feasibility 🟠 low
→ catalogued as hard-blocked. Third-party tools (WechatExplorer, wx-cli)
work but all require the running-process memory scan; the primary tool was
discontinued October 2025. Effort XL recorded for the record — irrelevant
while every path violates the standalone rule and WeChat's terms.

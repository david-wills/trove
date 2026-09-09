# LINE

- **id:** `line`
- **domains:** `correspondence` (contract: ✅ ratified — would apply if ever
  buildable)
- **status:** 🚫 unavailable
- **unavailable_reason:** LINE offers no personal export or API, and its
  local database format is undocumented. Nothing can be read until LINE adds
  an export feature or community tooling documents the format.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** hard block — research front matter L140; local DB format
  undocumented, no community schema exists
- **effort / priority:** L / P2
- **needs:** privacy-sensitive (message bodies — would be opt-in if ever
  buildable)

## What it is

The dominant messenger of Japan, Thailand, and Taiwan (~200M users). Since
LINE for macOS 9.8.0 (April 2025) the desktop client keeps all chat history
received on the PC locally — but in an undocumented format with no way out.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| none — all paths blocked | — | — | research front matter L140 |

## Access & auth

None viable. The Mac App Store client is sandboxed (container likely
`~/Library/Containers/jp.naver.line.mac/`, FDA required to even look), and
the database format inside is undocumented — reading it would mean
reverse-engineering from scratch. The in-app "Back Up Chat History" backs
up to LINE's servers, not to local files. LINE's developer platform
(Messaging API) is for chatbots only — no personal-message access.

## Vault mapping

Would be `correspondence/line/` under the ratified correspondence contract.
Not applicable while blocked.

## Build plan

None. Ships as a catalogued unavailable card (`Behavior::Unavailable`) with
the reason above. Two revisit triggers: LINE adds a personal export, or
community tooling documents the local DB schema (which would lift this to
an FDA-gated local read, like other sandboxed-app integrations).

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Unavailable card | — | hub shows LINE greyed, sorted last in Correspondence, with the reason copy above |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §LINE (macOS desktop)
(L393-L399) + Hard Blocks front matter (L140). Feasibility 🟠 low →
catalogued as hard-blocked. Softer block than WeChat (no encryption-key
problem reported — purely an undocumented format), hence effort L not XL;
the icebox condition is "community tooling emerges," which is plausible.
Lower priority for the initial Western-audience launch regardless.

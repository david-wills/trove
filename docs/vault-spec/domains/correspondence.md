# Domain: correspondence

Every conversation medium — messages, email, chat, calls — as one unified
stream. iMessage, email imports, Gmail pulls, Slack, and call history all
write this shape; readers see one timeline regardless of medium.

- **Layout:** `correspondence/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/correspondence.message.schema.json`](../schemas/correspondence.message.schema.json)
- **Dedupe key:** `guid` (source-unique: iMessage guid, email Message-ID,
  Slack `channel/ts`). Imports must skip already-stored guids.

## Fields

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time sent/received |
| `source` | string | ✔ | collector id, = the folder name |
| `chat` | string | ✔ | conversation key within the source: handle / group id (iMessage), thread subject (email), channel (Slack) |
| `sender` | string | ✔ | canonical author address; empty when `from_me` |
| `kind` | string | ✔ | `"message"` (default) \| `"reaction"` \| `"event"` \| `"call"` |
| `text` | string | | the body (full fidelity — trimming is a read-time opinion) |
| `chat_name` | string | | display name for the chat (group names, channel topics) |
| `sender_name` | string | | display name for the sender |
| `from_me` | bool | | sent by the vault owner |
| `duration_secs` | int | | connected seconds for `kind:"call"` (0 = missed) |
| `to` | string[] | | explicit recipients (email to/cc) |
| `subject` | string | | subject where the medium has one |
| `service` | string | | transport detail: `"iMessage"`/`"SMS"`, the email account address, the Slack workspace |
| `attachments` | object[] | | attachment *metadata* only (name, mime, size) |
| `reaction` | string | | for `kind:"reaction"`: `"loved"`, `"liked"`, … |
| `reply_to` | string | | source-native id this reacts/replies to |
| `labels` | string[] | | source-native labels/folders (Gmail `INBOX`, `CATEGORY_*`, …) — a read-time filter |
| `guid` | string | | source-unique id, the dedupe key |
| `rowid` | int | | source-native monotonic id (drives cursor rebuilds) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-10T14:03:00-07:00","source":"imessage","chat":"+15551234567","sender":"+15551234567","from_me":false,"kind":"message","text":"running late, omw","service":"iMessage","guid":"ABC-123","rowid":88412}
{"ts":"2026-06-10T09:12:00-07:00","source":"email","chat":"trip planning","sender":"ana@example.com","sender_name":"Ana","from_me":false,"kind":"message","text":"Flights are booked!","to":["you@example.com"],"subject":"Trip planning","service":"you@example.com","labels":["INBOX"],"guid":"<msg-1@example.com>"}
```

## Read-time semantics (FYI for writers)

Calls sit in the same timeline as messages without counting as message
volume (`kind` separates them). Reactions and group events are stored at
full fidelity; views decide what counts as conversation. Sender handles are
raw until a contacts source maps them to people — write handles, not
guesses.

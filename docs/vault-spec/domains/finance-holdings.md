# Domain: finance-holdings

Investment **position snapshots** — what you hold, not what you traded.
Brokerages and crypto exchanges (Schwab and Interactive Brokers via API,
Coinbase and Kraken via API, Fidelity/Vanguard/Robinhood via CSV export)
write one row per position at a point in time; the read-time view sums them
into per-account and total net worth and charts it over time. Trades stay in
the canonical `finance/` ledger and never appear here — this domain answers
"what do I own right now, and what did I own then?", the ledger answers
"what moved?". Each source writes its own folder; the holdings reader merges
them at read time.

- **Layout:** `finance/holdings/<source>/YYYY-MM.jsonl` (month of `as_of`)
- **Kind:** snapshot (each line is one position at `as_of`), realized as a
  dated append-only stream so history accrues — see Read-time semantics
- **Schema:** [`schemas/finance-holdings.position.schema.json`](../schemas/finance-holdings.position.schema.json)
- **Dedupe key:** `as_of`-day + `account` + `symbol` — one position per
  instrument per account per snapshot day; a re-run on the same day replaces
  nothing new, a new day appends a fresh point.

Source folders are discovered by scanning — no registration, no code change;
rows appear in the Holdings view and the generic data browser. A per-source
raw layer (`finance/holdings/<source>/raw/…`, full API/CSV fidelity) may sit
alongside, as the briefs specify.

## Position

| Field | Type | Required | Meaning |
|---|---|---|---|
| `as_of` | string | ✔ | RFC3339 local time the snapshot was taken (the brokerage's "as of", else the sync time); date-only sources use local midnight |
| `source` | string | ✔ | collector id, = the folder name |
| `account` | string | ✔ | account this position sits in: a masked number (`"...7421"`) or a name (`"Roth IRA"`) — never the full account number |
| `symbol` | string | ✔ | ticker / asset code as the source gives it (`"AAPL"`, `"VTSAX"`, `"BTC"`, `"USD"` for a cash sweep) |
| `quantity` | number | ✔ | units held; fractional for crypto and fractional-share brokerages; for cash, the cash amount |
| `name` | string | | human name of the instrument (`"Apple Inc"`, `"Bitcoin"`) |
| `price` | number | | per-unit market price at `as_of`, in `currency` |
| `value` | number | | market value of the position at `as_of` (`quantity` × `price`), in `currency` |
| `cost_basis` | number | | total acquisition cost of the position, in `currency` (omit when the source doesn't report basis) |
| `currency` | string | | currency of `price`/`value`/`cost_basis` (`"USD"`, …); omit when unknown |
| `asset_class` | string | | instrument type, source vocabulary normalized where clear: `"equity"` \| `"etf"` \| `"fund"` \| `"crypto"` \| `"cash"` \| `"option"` \| `"bond"` \| `"future"` \| `"forex"` \| … (open set — readers tolerate unknowns) |
| `institution` | string | | display name of the custodian (`"Charles Schwab"`, `"Coinbase"`) |
| `extra` | object | | everything source-specific (account type, lot detail, unrealized gain, day change, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"as_of":"2026-06-12T16:05:00-07:00","source":"schwab","account":"...7421","symbol":"AAPL","name":"Apple Inc","quantity":120,"price":214.29,"value":25714.8,"cost_basis":18204,"currency":"USD","asset_class":"equity","institution":"Charles Schwab","extra":{"account_type":"Roth IRA","day_change":312.45}}
{"as_of":"2026-06-12T16:00:00-07:00","source":"coinbase","account":"BTC Wallet","symbol":"BTC","name":"Bitcoin","quantity":0.5123,"price":61240.11,"value":31373.31,"currency":"USD","asset_class":"crypto","institution":"Coinbase"}
{"as_of":"2026-06-11T00:00:00-07:00","source":"vanguard","account":"Roth IRA","symbol":"VTSAX","quantity":842.117,"value":108452.33,"asset_class":"fund","institution":"Vanguard"}
```

## Read-time semantics (FYI for writers)

A holding is "current state", but the value of holdings is the *series* — net
worth over time — and a position snapshot is irreproducible after the fact
(the raw layer keeps today's positions, not a daily history). So holdings are
**not** a whole-file rewrite: each sync **appends** that run's positions,
stamped with `as_of`, into the month partition. The reader takes, per
(`account`, `symbol`), the row with the latest `as_of` to render the current
portfolio, and reads the whole series for the net-worth chart. Dedupe on
`as_of`-day + `account` + `symbol` keeps a re-run within a day idempotent
while a new day adds a fresh point.

Many sources fill few fields: a Needs-sample CSV (Fidelity, Vanguard) or a
balance-only crypto wallet (Coinbase, Kraken `/accounts`) may carry only the
required core plus `value`; Robinhood's export omits positions entirely, so it
contributes no rows here at all. Write `value` from the source rather than
deriving it where you can — read-time math stays honest. Sum per-account
totals by latest-`as_of` `value`; never persist a derived total back into the
vault.

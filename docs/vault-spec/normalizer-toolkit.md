# The Normalizer Coercion Toolkit — v1

*Step 0 deliverable of R2 (`docs/normalizer.md`). Design input; machine spec
for the projection engine built in Step 1. Ratified constraints: the toolkit
is a **fixed vocabulary of coercions expressed as plain data** — **no
expression language** (Decision 3), and anything the toolkit cannot express
rides `extra` verbatim while raw keeps everything.*

A mapping file (`.trove/mappings/<source>.json`, shape in `normalizer.md`
Step 1) binds source columns to a contract shape. Each binding may name a
`coerce` from the vocabulary below; the `guid`, `constants`, and `unbound`
blocks draw from the same fixed set. This document is the closed list of what
those names may be in v1, and the exact machine behavior of each.

## How v1 was chosen

The wave's ~290 hand-written `parse → map → write` mappings were surveyed and
every transform bucketed by kind. The aggregate frequency evidence:

| Survey kind | Count | What it is | v1 |
|---|--:|---|---|
| date-parse | 258 | parse a source date/time/epoch into the vault's RFC3339-local (or date-only) form | **in** — `date` |
| other | 227 | grab-bag: HTML unescape, BOM strip, stubs, per-module one-offs | out (long tail / not a coercion) |
| value-map | 177 | source enum/string → contract enum/string via a small table | **in** — `value_map` |
| nested-extract | 156 | pull a field out of nested JSON (`content.parts[]` joins, fallback cascades) | out (JSONL-nested; both fixtures are flat CSV) |
| guid-id-column | 138 | guid = a source id column, often with an ownership prefix | **in** — `guid.column` |
| guid-hash | 103 | guid = hash of one or more named columns | **in** — `guid.hash` |
| row-filter | 89 | drop rows (comment lines, sentinels like `-1`/NaN, excluded apps) | out (validation + raw handle this) |
| split-join | 60 | split a delimited cell into an array (`"Action, Adventure"`) | **in** — `split` |
| unit-convert | 41 | declared source unit → contract unit | out (contracts keep units as source-labeled strings) |
| number-clean | 32 | strip thousands separators / quotes / symbols before numeric use | **in** — `number` |
| constant | 30 | a field with no source column set to a fixed literal | **in** — `constants` block |
| rename-only | 29 | a plain `from → to` binding, no value change | **in** — the base binding (no `coerce`) |
| unit-convert (currency) | 9 | money-string cleanup + sign convention | **in** — folded into `number` |
| guid-url-extract | 19 | extract an id from inside a URL (last path segment, substring before `&`) | out (hash-over-URL gives a stable key without parsing) |
| url-extract | 1 | build a URL from parts | out (single occurrence) |

The ratified rule (Step 0) is **the smallest set covering the Appendix A
fixtures plus the top recurring coercions**. The fixtures require at minimum:
named-format date parsing incl. date-only, comma-thousands number cleanup,
constants, guid via hash-of-named-columns, and unbound-columns-to-extra. To
that fixture floor v1 adds the three unambiguously dominant recurring
coercions that are pure, deterministic, single-cell (or single-guid)
transforms expressible as plain data — `value_map` (177), `guid.column`
(138), and `split` (60). Everything else is recorded under **Deliberately
left out**.

### What the fixtures exercise

Only the `pub_ops fb_pages → social.post` fixture (Appendix A, path 2)
exercises the toolkit. The IMDb export is path 1 (routed to the built
`imdb.rs`, no mapping created) and the junk file is path 3 (raw only, no
mapping) — neither needs a coercion. The pub_ops mapping uses: `date`
(date-only `Post Date`), `guid.hash` (of `Post URL`), `constants`
(`kind: "post"`), `unbound → extra` (five columns incl. the comma-formatted
`Total Link Clicks` and the unnamed leading column), and — where a user binds
the comma-formatted `"13,133"` to a numeric field — `number`.

---

## The v1 vocabulary

Three groups, mirroring the mapping artifact:

1. **Binding coercions** — a `coerce` on one `{from, to}` binding; transforms
   one source cell into one contract field value.
2. **The guid recipe** — the `guid` block; derives the required dedupe key.
3. **Field-population primitives** — the `constants` block and the `unbound`
   policy; populate fields that are not a 1:1 bound cell.

A source **cell** is read as its trimmed string. A missing/blank cell that a
coercion cannot turn into a value produces **no value**: per the omit-empty
convention the target field is omitted, and if the contract requires that
field the row fails validation and is **counted and reported** in the
`ImportOutcome` (never silently dropped; always recoverable from raw). No
coercion ever fabricates a value (no midnight for a missing date, no `0` for a
blank number).

---

### 1. `date` — named-format temporal parse

Parse a source date/time into the vault's timestamp convention (RFC3339 with
the machine's local offset), or a date-only `YYYY-MM-DD` where the source is
day-granular.

```jsonc
{ "from": "Post Date", "to": "ts", "coerce": "date",
  "with": {
    "formats": ["date"],        // ordered; first that parses wins
    "assume_tz": "local"        // optional; only "local" in v1
  }
}
```

**`formats`** — an ordered array. Each element is either a reserved token or a
[chrono `strftime`](https://docs.rs/chrono/latest/chrono/format/strftime/)
pattern string. Reserved tokens (a closed set; no user-authored regex):

| token | meaning | emits |
|---|---|---|
| `"rfc3339"` | RFC3339 with `Z` or offset, optional fractional seconds | RFC3339-local |
| `"date"` | `%Y-%m-%d`, a bare calendar date | **date-only** `YYYY-MM-DD` |
| `"epoch_s"` | integer/float Unix seconds | RFC3339-local |
| `"epoch_ms"` | integer Unix milliseconds | RFC3339-local |

A `strftime` pattern that carries no time component (e.g. `"%m/%d/%Y"`,
`"%d-%b-%Y"`) emits **date-only**; one that carries a time emits RFC3339-local.

**`assume_tz`** — optional, `"local"` only in v1. A naive parsed date-time
(no offset in the source) is stamped with the machine's local offset.
Arbitrary named zones are out (they invite guessing; "local time is
deliberate", per conventions).

**Semantics.** Try each `formats` entry in order against the trimmed cell;
first that parses wins. A date-only parse emits `YYYY-MM-DD` **verbatim** — no
fabricated midnight (ratified date-only ruling: a date is a lexical prefix of
a timestamp; a fake midnight is dishonest). A date-time or epoch parse emits
RFC3339 with the local offset.

**Failure.** No format matches → **no value** (field omitted; validation
governs required-ness). Never emit a partial or guessed timestamp.

---

### 2. `number` — numeric string cleanup

Strip formatting from a numeric cell and emit a **typed JSON number** (an
integer when the cleaned magnitude is integral, otherwise a decimal). Covers
number-clean and currency.

> **Why a number, not a string.** Every ratified contract types its numeric
> fields `number`/`integer` (`lat`/`lon`, environment/home `value`, media
> `seconds`, finance `amount`/`qty`/`unit_price`). Row validation requires an
> actual JSON number there — a string is rejected — so a decimal-string output
> would make every one of those fields *unprojectable*. The cleanup still runs
> through the canonical decimal string internally (so the currency/thousands/
> sign rules below are exact and precision-preserving) and only the final,
> validated literal is parsed into the number.

```jsonc
{ "from": "Total Link Clicks", "to": "…", "coerce": "number",
  "with": {
    "decimal_sep": ".",         // "." (default) or ","
    "parens_negative": true,    // "(1,234.50)" -> -1234.5
    "negate": false             // flip sign after parsing (source-convention fix)
  }
}
```

**Semantics.** From the trimmed cell, strip: a wrapping single- or
double-quote pair; ASCII currency symbols and letters used as symbols
(`$ € £ ¥`); spaces; and the thousands separator (the non-`decimal_sep`
grouping character). If `decimal_sep` is `","`, the decimal comma is
normalized to `"."`. Apply `parens_negative` (a parenthesized value becomes
negative). Apply `negate` (unconditional sign flip; true zero is left
unsigned, never `-0`). Validate the remainder is an optionally-signed decimal
(leading/trailing zero-normalized, no `+`), then emit it as a typed JSON
number — integer when integral, decimal otherwise.

**Failure.** Empty, or non-numeric after stripping (e.g. `"Not Available"`) →
**no value**. Never emit `0` for a blank.

*v1 emits a JSON number keyed off the cleaned magnitude (integer vs. decimal).
An explicitly-typed / fixed-scale money representation is additive later.*

---

### 3. `split` — delimited cell → array

Turn a delimited cell into a JSON array of strings for an array-typed contract
field (e.g. `tags`, `Genres`).

```jsonc
{ "from": "Genres", "to": "tags", "coerce": "split",
  "with": {
    "sep": ",",                 // delimiter (default ",")
    "trim": true,               // trim each part (default true)
    "drop_empty": true          // drop empty parts (default true)
  }
}
```

**Semantics.** Split the cell on `sep`; per part apply `trim` then
`drop_empty`; emit a JSON array of the surviving strings. An empty input (or
all parts dropped) yields an empty array, which the omit-empty convention then
omits.

**Failure.** None at the value level. If `to` is not an array-typed contract
field, the row fails schema validation and is reported.

---

### 4. `value_map` — enum/string lookup

Map a source value to a contract value through a small explicit table (e.g.
booking status `accepted → confirmed`, `cancelled → canceled`).

```jsonc
{ "from": "Scheduling Type", "to": "…", "coerce": "value_map",
  "with": {
    "table": { "manual": "post", "auto": "post" },
    "case_insensitive": true,   // default true; match on trimmed+lowercased key
    "fallback": "omit"          // "omit" | "passthrough" | "<literal default>"
  }
}
```

**Semantics.** Normalize the cell (trim; lowercase if `case_insensitive`),
look it up in `table`. Hit → the mapped value. Miss → `fallback`: `"omit"`
(no value), `"passthrough"` (emit the original trimmed cell unchanged), or any
other string (emit that literal default). Default `fallback` is `"omit"`.

**Failure.** No exceptions — `fallback` governs every miss.

---

### 5. `guid` — the dedupe key recipe

Every projected row needs the contract's required `guid`. Exactly **one** of
two modes is present. Both accept an optional `prefix` string prepended to the
result (the wave's ownership-scoping idiom, e.g. `"cly:"`, `"caldav:"`).

**`column`** — the guid is a source id column, verbatim.

```jsonc
"guid": { "column": "Const", "prefix": "" }
```

The guid is `prefix + trimmed(cell)`. A blank cell → **no guid** → the row
fails validation (guid is required) and is reported.

**`hash`** — the guid is a hash of one or more named columns.

```jsonc
"guid": { "hash": ["Post URL"], "algo": "sha256", "prefix": "" }
```

The guid is `prefix + hex(sha256( join(trimmed cells of the listed columns,
"|") ))`, columns taken **in listed order**, joined by the fixed separator
`"|"`, lowercase hex. `algo` is optional, `"sha256"` only in v1. This is
deterministic and stable across re-drops of overlapping exports, so it drives
the store's guid-merged dedupe. A hash over blank inputs still yields a
(useless-but-present) guid; prefer `hash` over columns known to be populated.

*Extracting an id from inside a URL (guid-url-extract, 19) is out of v1 — a
`hash` over the URL column gives a stable dedupe key without URL parsing.*

---

### 6. `constants` — fixed field literals

Set contract fields that have no source column to fixed values (e.g.
`kind: "post"`, `source` label).

```jsonc
"constants": { "kind": "post" }
```

**Semantics.** Each `field: literal` is applied to **every** projected row.
The literal must satisfy the field's schema type. A field may appear in
`constants` **or** in a binding, never both — the mapping loader rejects an
overlap (a constant and a bound cell for one field is a mapping error, not a
precedence puzzle).

---

### 7. `unbound` — the catch-all policy

What happens to source columns not named by any binding's `from`.

```jsonc
"unbound": "extra"              // v1: the only supported value
```

**`"extra"`** (v1 default and only value). Every source column **not
referenced as any binding's `from`** is copied into the row's `extra` object,
verbatim, lossless:

- **key** = the column's original header text. A blank/unnamed header (the
  pub_ops leading index column) gets a deterministic synthetic key
  `col<zero-based-index>` (e.g. `col0`).
- **value** = the cell as a string, **no coercion**. Blank cells are omitted
  (omit-empty).

Columns consumed only by the `guid` recipe (not bound to a field) still land
in `extra` — nothing the source gave is dropped. Raw retains the full record
regardless, so `extra` is the honest normalized-layer default, not the only
copy.

*`"drop"` (discard unbound columns) is additive later; v1 keeps everything.*

---

## Deliberately left out (with reasons)

- **An expression language.** Ratified out (Decision 3). The toolkit is a
  fixed set of named coercions parameterized by plain data. Anything it cannot
  express rides `extra`, and raw has everything — so a future need is
  additive, never a reason to smuggle in a DSL.
- **nested-extract / dotted-path `from` (156).** The single largest tail, but
  format-specific to JSONL and frequently needing fallback cascades and joins
  (`content.parts` → `content.text` → …) that approach an expression language.
  Both v1 fixtures are flat CSV. v1 `from` addresses a CSV column or a
  top-level JSONL key only; nested addressing is additive. Nested data still
  rides raw + `extra`.
- **row-filter (89).** Dropping comment lines, sentinel values (`-1`, NaN), or
  excluded rows is **not a value coercion**. The projection instead validates
  each row against the schema and **reports** (never silently drops) the
  failures, and raw keeps every line. Format-level skips (e.g. `#` comment
  lines) belong to parse/detect, not the mapping vocabulary.
- **unit-convert (41).** The contracts keep units as **source-labeled
  strings** (`environment.reading`, `home.energy`, health metrics carry a
  `unit` field); no numeric conversion is required at projection — the unit
  rides as data, and a `value_map` can relabel a unit string. A numeric
  conversion table invites wrong factors; it is additive if a real drop needs
  it.
- **guid-url-extract (19) and url-extract (1).** Low frequency, many bespoke
  URL shapes. `guid.hash` over the URL column yields a stable dedupe key
  without parsing.
- **Composite/join transforms** (join as the inverse of `split`;
  multi-column concatenation into one field, e.g. `given + family`). These are
  expression-shaped. Composite **guids** are already covered by `guid.hash`
  over multiple columns; composite **fields** are out of v1.
- **Typed numeric output.** `number` emits a canonical decimal *string* (the
  finance never-parse-to-float rule). Coercing to a typed `f64`/`i64` for a
  non-money numeric contract field is additive.
- **Per-key renames inside `extra`.** Unbound columns keep their original
  header text (or the deterministic `col<n>` for a blank header); v1 has no
  rename vocabulary for `extra` keys.
- **Arbitrary time zones in `date`.** Only `assume_tz: "local"`; naming a zone
  invites guessing against the "local time is deliberate" convention.
- **HTML entity unescape, BOM stripping, and other "other"-bucket one-offs
  (227).** Per-module cleanups with no shared shape. Where one matters for a
  real drop it becomes a named coercion additively; until then such text rides
  `extra` verbatim and raw is untouched.

## Extension rule

The toolkit is **additive**: a new coercion is a new reserved name with its
own JSON parameters and a machine spec in this document, added when a real
dropped file demands it — never a general escape hatch. The mapping file
carries a `version` (currently `1`); an unknown `coerce` name is a mapping
error, not silently ignored.

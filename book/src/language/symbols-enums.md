# Symbols, categoricals & enums

Outside a table expression, `` `foo `` is a **symbol** — a distinct value kind
that names a column. (Tables are named, not symboled: write `trades`,
not `` `trades ``.) A symbol literal is a bareword (letters, digits, `_` `-`
`.` `/`) — it can't contain a space. `` `$expr `` interns a *string* into a
symbol, so a value with spaces or other punctuation goes through a string
literal instead:

```qpl
role: `$"Analytics Engineer"   / a symbol with a space — quote it, then intern it
```

Inside a table expression, `` `$col `` casts a column to a Polars **Categorical**
(an interned string pool — fast joins, group-bys and filters), `u32` codes by
default. `u8!` / `u16!` / `u32!` before `` `$ `` picks the physical width:

```qpl
select country: `$country from t
select country: u8!`$country from t   / u8 codes (<=255 distinct values)
```

An **enum** is an *ordered* symbol vector — the order fixes sort order and each
value's code. More performant than categorical and more efficient sorting using the physical representation.
Define it, then cast with `` name::`$col ``:

```qpl
lvl: `low`mid`high
t: update level: lvl::`$?[price>400;`high;price>100;`mid;`low] from trades

/ as it's a polars enum under the hood, you can sort/compare them too
select from t where level >= `mid
```

The cast input may be a string column or an existing categorical/enum (Polars
re-keys it). Values absent from an enum become null.

>See https://docs.pola.rs/user-guide/expressions/categorical-data-and-enums for more on this subject.

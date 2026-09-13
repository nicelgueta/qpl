# Column expressions & lists

A **column expression** pulls one column out of a table *without* a surrounding
`select` statement. It takes one of two forms:

```qpl
trades`price                 / backtick a column off a table name
select price from trades     / a one-column select
trades`price where size > 100   / a `where` may be attached
```

Used as a value — assigned to a name, reduced, sliced or indexed — a column
expression **materialises to a list** (`IntVec` / `FloatVec` / `StrVec` /
`SymVec` / `BoolVec`, or a typed temporal list such as `TimestampVec` for a
datetime column; other column dtypes, and columns containing nulls, are an
error). A bare one-column `select` typed on its own still prints as a table; it
becomes a list only in a value position.

```qpl
px: trades`price             / a FloatVec global
px: select price from trades  / same
```

**Reductions** (`sum` `avg`/`mean` `min` `max` `first` `last` `count`
`std`/`dev` `var` `med`/`median` `mode`/`modal` `skew` `kurt` `any` `all`
`prod` `argmin` `argmax` `nnull` `distinct`/`n_unique`) collapse a column
expression to a scalar you can bind:

```qpl
top:  max trades`price
n:    count select price from trades where size > 100
select sym, price from trades where price >= top   / the scalar composes into later queries
```

Any non-reducing column verb (`cumsum`, `abs`, `2 shift`, `2 round`, …) yields
another list.

**Slicing** — `n#<expr>` takes the first `n` rows, `-n#<expr>` the last `n`:

```qpl
3#trades`price
-3#trades`price
3#select price from trades
```

`n#<whole table>` (`3#trades`, `3#select sym, price from t`) stays a table — use
`n limit …` or `n#…` interchangeably there.

**Indexing** — `list[i]` picks one element (an atom); `list[i j k]` gathers a
sub-list. Works on a list global, a column expression, or a parenthesised
expression, and chains:

```qpl
l: 10 20 30 40 50
l[0]                         / i64: 10   (an atom)
l[1 3 4]                     / i64[3]: 20 40 50
sub: trades`price[2 3]       / a 2-element FloatVec: rows 2 and 3
one: (trades`sym)[0]
```

A parenthesised expression may also be followed by a bare int run, q-style:
`(trades`price) 2 3`.

Once a column expression has been persisted as a list, `where` no longer applies
to it — filter before materialising.

Bare names in a value position resolve at run time: first a scalar global, then
a lazy binding, then a table. `t2: trades` copies the table under a new name.

**Vector arithmetic** — every list is backed by a Polars `Series`, so the usual
operators (arithmetic, comparison) apply elementwise between a list and a
scalar, or between two lists of the same length (or one of length 1, which
broadcasts):

```qpl
l: 12 34
l + 2                        / i64[2]: 14 36
2 * l                        / i64[2]: 24 68
l > 20                       / bool[2]: 01
trades`price - trades`price[0]  / list minus a broadcast scalar
```

Every atomic scalar type has a matching list type (`IntVec` `FloatVec`
`SymVec` `StrVec` `BoolVec` `DateVec` `MonthVec` `TimeVec` `MinuteVec`
`SecondVec` `TimestampVec` `TimespanVec`) — a temporal column materialises to
its typed list rather than a raw integer offset:

```qpl
ts: trades`ts                        / a TimestampVec
ts + 0D00:01:00.000000000            / shift every timestamp forward one minute
```

**Filtering a list** — `<list-expr> where <predicate>` filters a list
elementwise; `x` in the predicate refers to the current element (a comma joins
predicates with AND, same as a table's `where`):

```qpl
nums: 10 20 30 40 50
nums where x > 25              / i64[3]: 30 40 50
nums where x > 10, x < 50      / i64[3]: 20 30 40
trades`price where x > 400     / filter an already-materialised list by its own values
```

This is a different `where` from the one on a `` table`col `` column
expression (which filters table *rows* by any other column before
projecting) — the two share the keyword but not a grammar rule, so chaining
them needs parentheses: `` (trades`price where size>100) where x>400 ``.

**Building lists and tables** — `til` generates a range list; `zip` builds a
table from a dict of same-length named lists:

```qpl
til 5                           / i64[5]: 0 1 2 3 4
10 til 15                       / i64[5]: 10 11 12 13 14

a: til 20
b: 2 * til 20
tbl: zip `cola`colb!a b         / a 2-column table, 20 rows
```

A dict literal (`` `k1`k2!v1 v2 ``) pairs a symbol (vector) key with one value
noun per key — a compound value expression needs parens, e.g. `` `a`b!(x+1) y ``.

# qpl — Quick Polars Query Language

An agent-friendly, q/kdb+-inspired query language that compiles to Polars lazy
frames. Write concise select statements; Polars runs them fast. Single binary,
zero dependencies — no Python, no Polars install needed.

There's also a [VSCode extension](tools/vscode/) — syntax highlighting plus a
Ctrl+Enter REPL for sending lines straight from the editor.

## Table of Contents

- [Why?](#why)
  - [Polars](#polars)
  - [Example](#example)
- [Install](#install)
- [Quickstart](#quickstart)
- [Working incrementally](#working-incrementally)
- [Language](#language)
  - [Assignment](#assignment)
  - [select / update / delete](#select--update--delete)
  - [Column expressions & lists](#column-expressions--lists)
  - [Expressions](#expressions)
  - [Window functions](#window-functions)
  - [Functions](#functions)
  - [Casts](#casts)
  - [Temporal types](#temporal-types)
  - [Symbols, categoricals & enums](#symbols-categoricals--enums)
  - [Reading & writing files](#reading--writing-files)
  - [Table operators](#table-operators)
  - [lazy / collect](#lazy--collect)
  - [Comments](#comments)
  - [Multi-line statements](#multi-line-statements)
  - [Logging](#logging)
  - [Config](#config)
  - [Operator reference](#operator-reference)
- [REPL](#repl)
- [Architecture](#architecture)
- [Releases](#releases)
- [Roadmap](#roadmap)

## Why?

I often need to query large parquet files on cloud storage under time
pressure, or knock out a quick transform job. DuckDB is great for this, but I
always forget the syntax — and prompting an agent for it is often slower than
just writing the query myself.

So: a language fast enough to type without thinking, but with DuckDB-level
performance. As a bonus, one that LLM agents can drive easily too — and can't
do much damage with, sandboxed or not.

### Polars
Writing a DuckDB-grade engine from scratch solo isn't realistic, so I cheated:
Polars (a dataframe library written in Rust) is the backend. Its API is clean
enough that the "language" is really just a VM translating instructions into
Polars queries — which left me free to build whatever front-end I wanted.

That front-end borrows from kdb+/q, a language I'd been lightly exposed to at
work and wanted an excuse to actually learn, while still getting to write Rust.

Thus: `qpl`.

### Example

Read two parquet files, left-join them, derive a couple of columns, tag every
row with a conditional, dictionary-encode a key, aggregate, sort, and write the
result back out.

DuckDB:

```sql
CREATE TYPE sym_t AS ENUM (SELECT DISTINCT sym FROM read_parquet('trades.parquet'));
COPY (
    SELECT
        CAST(t.sym AS sym_t)        AS csym,
        t.side,
        CASE WHEN t.size >= 1000 THEN 'large'
             WHEN t.size >= 250  THEN 'mid'
             ELSE 'small' END       AS band,
        SUM(t.price * t.size)       AS tot,
        AVG(q.ask - q.bid)          AS avg_spread,
        COUNT(*)                    AS n
    FROM read_parquet('trades.parquet') t
    LEFT JOIN read_parquet('quotes.parquet') q USING (sym)
    WHERE t.price > 0
    GROUP BY csym, t.side, band
    ORDER BY tot DESC
) TO 'summary.parquet' (FORMAT PARQUET);
```

qpl — the whole thing is one statement:

```q
select tot: sum price * size, avg_spread: avg ask - bid, n: count price
    by csym: `$sym, side, band: ?[size >= 1000; `large; size >= 250; `mid; `small]
    from load "trades.parquet" `sym lj load "quotes.parquet" `sym where price > 0
    order tot desc
    sink "summary.parquet"
```

But the same pipeline is more naturally built up **one statement at a time** in
the REPL — see [Working incrementally](#working-incrementally):

```q
j: select sym, side, price, size, bid, ask from load "trades.parquet" `sym lj load "quotes.parquet" `sym where price > 0
j  / check the table so far
j: update spread: ask - bid, notional: price * size from j
/ try the next step without committing — don't assign, just look
update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from j
/ happy with it — now assign to persist
j: update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from j
cols j                                     / check the schema so far
select tot: sum notional, avg_spread: avg spread, n: count price by csym: `$sym, side, band from j order tot desc
j sink "summary.parquet"
```

## Install

```bash
cargo install --path .
```

Or grab a pre-built binary from [Releases](../../releases).

## Quickstart

```bash
qpl                 # REPL, with demo tables `trades` and `quotes` preloaded
qpl script.qpl      # run a script
qpl -i script.qpl   # run a script, then drop into the REPL
```

```q
qpl) select sym, price from trades where price > 200
qpl) select avg price by sym from trades
qpl) t: select from trades where size > 100     / bind a table
qpl) t sink "big.parquet"                       / write it out
```

Runnable scripts are in [`examples/`](examples/) — e.g.
`qpl examples/lazy_join_pipeline.qpl`.

Editor support (syntax highlighting + a Ctrl+Enter REPL) is in
[`tools/vscode/`](tools/vscode/).

## Working incrementally

This is the part qpl really leans on: **a transformation is a sequence of
statements, and you build it one statement at a time.**

Every step binds a name; the binding persists, so the next step starts from it.
There's no re-running a growing query, no stacking CTEs, no scrolling up to edit
and resubmit a 30-line block — the loop is *type a line, look, type the next*.

```q
qpl) t: load "trades.parquet"      / bind a table
qpl) t                              / look at it
qpl) cols t                         / ...or just its schema

qpl) t: select sym, side, price, size from t where price > 0
qpl) t: update notional: price * size from t

qpl) / not sure about the next step? run it WITHOUT assigning — the source is untouched
qpl) update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from t
qpl) t: update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from t   / keep it

qpl) select traded: sum notional by sym, side, band from t order traded desc
qpl) t sink "out.parquet"
```

Things that make the loop tight:

- **`t` / `cols t`** — peek at a table or its schema between steps.
- **Run a statement without assigning it** — you see the result, nothing changes.
  Assign only once you're happy.
- **`\d <stmt>`** — show the compiled plan without executing anything.
- **`lazy`** binds a *plan* rather than a table: nothing touches disk until a
  `collect` or `sink`, so building a large pipeline is instant and you pay for it
  once, at the end.
- In the [VSCode extension](tools/vscode/), **Ctrl+Enter** sends the current line
  or selection to this same session.

The SQL equivalent is: edit the query, re-run the whole thing, eyeball the
result, comment a block out to isolate a step, uncomment it, repeat.

## Language

### Assignment

`:` binds a name. The right-hand side decides what kind of binding it is:

```q
threshold: 150                                 / scalar
t: select from trades where size > threshold   / table
lvl: `low`mid`high                             / symbol vector
px: trades`price                               / column expression → a list
top: max trades`price                          / reduction → a scalar
```

Scalar variables are evaluated in Rust and substituted into later queries as
Polars literals, so they compose transparently with column expressions. See
[Column expressions & lists](#column-expressions--lists) for `` table`col ``,
reductions, slicing (`n#`) and indexing.

### select / update / delete

```
select <cols> from <table-expr> [by <keys>] [where <preds>] [order <col> <asc|desc>, ...]
```

```q
select from trades
select sym, price from trades
select px: price, qty: size from trades          / aliasing
select avg price by sym from trades              / group-by aggregation
select total: sum size by sym from trades where side = "buy"
select from trades where size > 100
select from trades where size > 100, price < 400  / comma-separated preds = AND
select from trades where (size > 400) | (side = "buy")   / & / | for and / or
select from trades where 10100000b               / boolean-vector mask
select from trades order sym asc, price desc
```

`from` takes any table expression, not just a table name — a nested `select`, a
`load`, `distinct`, `lazy`, and so on all work, and this composes with the
table operators below too:

```q
select from select price from trades where price > 100  / from a nested select
cols select from trades where size > 100                / cols on a nested select
select sym, price from load "data/trades.parquet"        / from a load directly
```

A join's right side (`` `sym lj/ij/rj <table> `sym ``) is a bare table name or
`load "path"` by default; wrap it in parens to join against any other table
expression — the parens give the parser an explicit end point, otherwise a
nested select's own join would swallow the outer `right_on` symbols:

```q
select price, bid from trades `sym lj quotes `sym                            / bare name
select price, bid from trades `sym lj (distinct quotes) `sym                 / any table expr, parenthesised
select price, bid from trades `sym lj (select sym, bid from quotes where bid > 0) `sym
```

`update` returns the whole table with the named columns replaced or added:

```q
update price: price * 2 from trades
update price: price * 2 by sym from trades where size > 100
update notional: price * size from trades where price > 0   / new column; null where the filter misses
```

With a `where`, rows that don't match keep the column's old value — or `null` if
it's a brand-new column.

`delete` removes rows (with `where`) or columns (with a symbol list):

```q
delete from trades where size < 100
delete `price`size from trades
```

### Column expressions & lists

A **column expression** pulls one column out of a table *without* a surrounding
`select` statement. It takes one of two forms:

```q
trades`price                 / backtick a column off a table name
select price from trades     / a one-column select
trades`price where size > 100   / a `where` may be attached (column expressions only)
```

Used as a value — assigned to a name, reduced, sliced or indexed — a column
expression **materialises to a list** (`IntVec` / `FloatVec` / `StrVec` /
`SymVec` / `BoolVec`; other column dtypes, and columns containing nulls, are an
error). A bare one-column `select` typed on its own still prints as a table; it
becomes a list only in a value position.

```q
px: trades`price             / a FloatVec global
px: select price from trades  / same
```

**Reductions** (`sum` `avg`/`mean` `min` `max` `first` `last` `count`
`std`/`dev` `var` `med`/`median` `mode`/`modal` `skew` `kurt` `any` `all`
`prod` `argmin` `argmax` `nnull` `distinct`/`n_unique`) collapse a column
expression to a scalar you can bind:

```q
top:  max trades`price
n:    count select price from trades where size > 100
select sym, price from trades where price >= top   / the scalar composes into later queries
```

Any non-reducing column verb (`cumsum`, `abs`, `2 shift`, `2 round`, …) yields
another list.

**Slicing** — `n#<expr>` takes the first `n` rows, `-n#<expr>` the last `n`:

```q
3#trades`price
-3#trades`price
3#select price from trades
```

`n#<whole table>` (`3#trades`, `3#select sym, price from t`) stays a table — use
`n limit …` or `n#…` interchangeably there.

**Indexing** — `list[i]` picks one element (an atom); `list[i j k]` gathers a
sub-list. Works on a list global, a column expression, or a parenthesised
expression, and chains:

```q
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

### Expressions

| Kind | |
|---|---|
| arithmetic | `+` `-` `*` `%` (`%` is division, q convention) |
| comparison | `=` `<>` `!=` `<` `<=` `>` `>=` |
| pattern match | `<str/sym> like <pattern>` — q-style glob, see below |
| logical | `&` `\|` |
| conditional | `?[cond; then; cond2; then2; ...; else]` — vectorised, nests for else-if |
| cast | `type$expr` — see [Casts](#casts) |
| round | `<precision> round <col>` — round a float column to N places |
| dyadic verbs | `<param> verb <col>` — `quantile`/`pctl`, `shift`/`lag`, `lead`, `diff`, `pctchange` (and `round`) |
| window | `<expr> over `p1`p2 [order `k1 asc `k2 desc] [rolling n]` — see [Window functions](#window-functions) |

A leading `-` negates: `-45.3` is a negative literal, `-col` / `-x` folds to
`0 - …` (works in scalars, column expressions and filters).

```q
l: int$-45.3                                         / scalar: -45
select price_bin: ?[price>400;`high;price>200;`mid;`low] from trades
select neg_mv: -market_value from trades
select mv: 2 round market_value from trades          / round to 2 dp
select p95: 0.95 quantile price by sym from trades   / 95th percentile
select sym, price, ret: 1 diff price by sym from trades   / row-over-row change
```

`like` tests a string or symbol against a glob pattern, [same as q](https://code.kx.com/q/ref/like/):
`*` matches any sequence (including empty), `?` matches exactly one character,
and `[abc]` / `[a-z]` / `[^abc]` are character classes (case-sensitive; no
pattern characters means an exact match). Escape a pattern character by
putting it in its own one-character class — `[*]`, `[?]`, `[[]`, `[]]`:

```q
select sym from trades where sym like "AA*"          / starts with AA
select sym from trades where sym like "[AM]*"        / starts with A or M
select sym from trades where sym like "?A?L"         / exactly 4 chars, A then L
select from trades where not sym like "AAPL"         / negate with `not`
```

`round` and the other **dyadic verbs** are q-style: the left operand is a
parameter (a literal), the right is the column. `<p> quantile <col>` takes a
fraction in `[0,1]`; `<n> shift <col>` (alias `lag`) moves values `n` rows later,
`lead` `n` rows earlier; `<n> diff <col>` is the difference from `n` rows back;
`<n> pctchange <col>` the fractional change. `round`'s rounding mode is the
session config `round_type` (`HALF_TO_EVEN` by default; see [Config](#config)).

**Aggregates:** `sum`, `avg`/`mean`, `min`, `max`, `count`, `first`, `last`,
`std`/`dev`, `var`, `med`/`median`, `mode`/`modal` (modal average — most
frequent value; ties resolve to the smallest), `skew`, `kurt`/`kurtosis`,
`any`, `all`, `prod`/`product`, `argmin`, `argmax`, `nnull`/`null_count`,
`abs`, `neg`, `not`, `string`, `distinct`/`n_unique`.

**Ordered / cumulative** (most useful with `over` + an `order` sub-clause, which
sorts each partition before applying): `cumsum`, `cummax`, `cummin`, `cumprod`,
`cumcount`, `ffill` (forward-fill nulls), `bfill` (backward-fill).

### Window functions

`<expr> over `key` computes `<expr>` per partition and broadcasts the result
back to every row (SQL `<expr> OVER (PARTITION BY key)`). Any column expression
or aggregate works:

```q
select sym, price, top: max price over `sym from trades
select sym, gap: (max price over `sym) - price from trades   / `over` binds tighter than `-`
```

Partition keys are backtick symbols — one (`` `sym ``) or several (`` `sym`day ``).

An `order` sub-clause (space-separated `` `col asc|desc `` pairs, per-column
direction) enables the ranking verbs, which stand alone in place of `<expr>`:

| Verb | SQL | Ties |
|---|---|---|
| `rn`    | `row_number()` | broken by row order — strict `1..n` |
| `rank`  | `rank()`       | share the lowest rank, then a gap (`1,1,3`) |
| `drank` | `dense_rank()` | share a rank, no gap (`1,1,2`) |

```q
select emp, country, role,
    seat: rank over `country`role order `desk asc `date desc,
    seniority: rn over `country order `hired asc
    from staff
```

The ranking verbs **require** `order`. Any other aggregate **may** take it: with
an `order` sub-clause the partition is sorted before the aggregate runs, so
`cumsum` / `diff` / `ffill` and friends compose in a defined order (a single
direction applies to every key — mixed asc/desc is ranking-verb only):

```q
select sym, ts, px,
    run:  cumsum px over `sym order `ts asc,       / running total in time order
    prev: lag px    over `sym order `ts asc         / previous row's price
    from trades
```

**Rolling windows** — a trailing `rolling <n>` sub-clause turns the aggregate
into a fixed `n`-row rolling reduction over the ordered partition
(`sum`/`avg`/`min`/`max`/`std`/`var`/`median`):

```q
select sym, ts, px, ma5: avg px over `sym order `ts asc rolling 5 from trades
```

The first `n-1` rows of each partition are `null` (the window isn't full yet).

**Virtual column `i`** is the row index (aliased to `x` in output, per q):

```q
select i, sym from trades
select from trades where i < 5
```

### Functions

q-style lambdas, bound to a name:

```q
add:   {[x,y] x+y}            / parameter list in [ ], body is ;-separated
add[2;3]                      / 5   — call with bracketed args
inc:   {[x] x+1}
inc 41                        / 42  — a one-arg function also takes `f x`
inc[41]                      / 42

hypot2: {[a,b] s: (a*a)+(b*b); s}   / earlier statements bind call-local vars;
hypot2[3;4]                          / the last statement is the return value

fac: {[n] ?[n<2; 1; n*fac[n-1]]}   / `?[..]` works in value context, so
fac[5]                              / recursion terminates → 120
```

A function can return a table, and its result composes like any value
expression:

```q
bysym: {[s] select sym, price from trades where sym = s}
bysym[`AAPL]                  / a table
avgpx: {[s] avg select price from trades where sym = s}
avgpx[`MSFT]                  / a scalar
```

The final statement in the body must be an expression (a trailing assignment is
an error). Parameters and any locals the body assigns are scoped to the call —
a function cannot mutate outer bindings. Niladic functions are written `{[] ..}`
or `{ ..}` and called `f[]`. Functions are a named binding kind, not first-class
values: they cannot be passed as arguments, returned, or used inside a `select`
projection. See [examples/functions.qpl](examples/functions.qpl).

### Casts

`type$expr` casts a column or scalar:

```q
select f: f64$size from trades
select f64$size, str$sym from trades

l: int$45.3                    / scalar: 45
ok: bool$"true"                / scalar: 1b
n: 1 + int$"42"                / string parses, then composes: 43
n:  (int$"42") - 1             / remember right to left evaluation, so parens for subtraction
```

Types: `f64`/`float`, `f32`, `i64`/`int`, `i32`, `i16`, `i8`, `u64`, `u32`,
`u16`, `u8`, `bool`, `str`/`string`. In a scalar context every integer width
folds to a single 64-bit integer and `f32`/`f64` to a single float — the width
only takes effect once the value lands in a column. A string (or symbol) scalar
is parsed: `int$"42"`, `f64$"3.5"`, `bool$"false"`.

### Temporal types

kdb+/q-style date & time literals. Each has an underlying integer offset that
`` `int$ `` / `` `long$ `` exposes.

| type | literal | offset |
|---|---|---|
| date | `2024.03.15` | days since `2000.01.01` |
| month | `2024.03m` | months since `2000.01` |
| time | `12:30:00.000` | ms of day (stored as ns) |
| minute | `12:30` | minutes of day |
| second | `12:30:00` | seconds of day |
| timestamp | `2024.03.15D12:30:00.000000000` | ns since `2000.01.01` |
| timespan | `0D12:30:00.000000000` | ns duration |

```q
d: 2024.03.15
d + 10                              / 2024.03.25   (days)
2024.03.20 - 2024.03.15             / 5
p: 2024.03.15D09:30:00.000000000
p - 0D00:05:00.000000000            / 2024.03.15D09:25:00.000000000
p < 2024.03.15D16:00:00.0           / 1b
```

Adding an integer shifts by one unit of the operand's own resolution — `date`+n
days, `month`+n months, `time`+n ms, `minute`+n minutes, `second`+n seconds,
`timestamp`/`timespan`+n ns. Comparison works across variants of the same
family (`date`↔`timestamp`, `time`↔`minute`↔`second`).

Casts use the backtick form `` `date$x ``, `` `month$x ``, `` `timestamp$x ``,
or a kdb single-char type code on a string — `"p"$` timestamp, `"d"$` date,
`"t"$` time, `"m"$` month, `"u"$` minute, `"v"$` second, `"n"$` timespan:

```q
`date$2024.03.15D12:30:00.0         / 2024.03.15
`month$2024.03.15                    / 2024.03m
`timestamp$2024.03.15               / 2024.03.15D00:00:00.000000000
"p"$"2024.03.15D12:30:00"           / parse a string
```

In **column** context, a string→temporal cast parses through Polars' string
parser (`expr.cast(<temporal>)` on a string is deprecated). The format is
inferred per value — ISO *and* kdb's dotted `2024.03.15` both work:

| cast | parser | result column |
|---|---|---|
| `` `date$s `` / `` `month$s `` | `str.to_datetime` → date | `Date` |
| `` `timestamp$s `` (`"p"$s`) | `str.to_datetime` | `Datetime` (keeps the time part) |
| `` `time$s `` (`"t"$s`) | `str.to_time` | `Time` |

```q
select d: `date$date_str from t        / "2024.03.15"          -> 2024-03-15
select ts: `timestamp$ts_str from t    / "2024-03-15T09:30:00" -> 2024-03-15 09:30:00
```

A value the inferred format cannot read aborts the query (strict by default).

Now-functions (**UTC** — there is no timezone database): `.qpl.d` today's date,
`.qpl.t` time, `.qpl.p` timestamp (ns), `.qpl.n` timespan since midnight. They
are ordinary expressions:

```q
log .qpl.d
```

Temporal literals project as Polars-native columns (`Date`, `Datetime[ns]`,
`Time`, `Duration[ns]`); month maps to `Date` at the 1st. Column output is
Polars' ISO form, not the kdb form. Not yet in the language: `xbar` bucketing,
the `within` window operator, `.minute` / `.date` unit accessors, and
temporal arithmetic on a **column** (`date_col + n` — scalar arithmetic is
fully supported) — those are planned.

### Symbols, categoricals & enums

Outside a table expression, `` `foo `` is a **symbol** — a distinct value kind
that names a column. (Tables are named, not symboled: write `trades`,
not `` `trades ``.) A symbol literal is a bareword (letters, digits, `_` `-`
`.` `/`) — it can't contain a space. `` `$expr `` interns a *string* into a
symbol, so a value with spaces or other punctuation goes through a string
literal instead:

```q
role: `$"Analytics Engineer"   / a symbol with a space — quote it, then intern it
```

Inside a table expression, `` `$col `` casts a column to a Polars **Categorical**
(an interned string pool — fast joins, group-bys and filters), `u32` codes by
default. `u8!` / `u16!` / `u32!` before `` `$ `` picks the physical width:

```q
select country: `$country from t
select country: u8!`$country from t   / u8 codes (<=255 distinct values)
```

An **enum** is an *ordered* symbol vector — the order fixes sort order and each
value's code. More performant than categorical and more efficient sorting using the physical representation.
Define it, then cast with `` name::`$col ``:

```q
lvl: `low`mid`high
t: update level: lvl::`$?[price>400;`high;price>100;`mid;`low] from trades

/ as it's a polars enum under the hood, you can sort/compare them too
select from t where level >= `mid
```

The cast input may be a string column or an existing categorical/enum (Polars
re-keys it). Values absent from an enum become null.

>See https://docs.pola.rs/user-guide/expressions/categorical-data-and-enums for more on this subject.

### Reading & writing files

`load` reads a parquet or CSV file, taking a string path. On its own it is
**eager** — `t: load ...` materialises a table straight away. Prefix it with
[`lazy`](#lazy--collect) to keep it as a deferred scan instead.

```q
select avg price by sym from load "data/trades.parquet"
t: load "data/trades.parquet"        / eager — reads the file now, binds a table
t: lazy load "data/trades.parquet"   / deferred — binds a plan, no IO yet
select from load "data/quotes.csv"
```

`sink` streams a **table expression** to a file, also taking a string path — a
table name, a `select ...`, an `update ...`:

```q
trades sink "summary.parquet"
select sym, price from trades where size > 100 sink "big_trades.parquet"
```

`cols` shows a table's schema (works on lazy bindings too):

```q
cols trades
```

### Table operators

```q
distinct select sym from trades

10 limit select from trades      / first N rows
10#select from trades            / `#` is the same
10#trades

`price`size drop select from trades   / drop columns
`price`size _ trades                  / `_` is the same

`sym`price!01b trades            / sort by a `col!bool` map (0 asc, 1 desc)
sorted: `sym`price!01b select from trades where size > 100
```

### lazy / collect

`lazy` as the first token of a table expression stores the **query plan** under a
name instead of running it. Nothing touches disk until you `collect` (materialise
to a DataFrame) or `sink` (stream to a file) — so a whole pipeline can process
**larger-than-RAM** data in a single pass.

```q
t: lazy load "trades.parquet"
q: lazy select sym, bid, ask from load "quotes.parquet"
```

Extend a plan by **re-assigning the binding** — each step just adds plan nodes,
the file is never touched:

```q
t: select sym, side, price, size from t where size > 100
t: update notional: price * size from t
t: update band: ?[notional > 50000; `big; `small] from t
```

Reading a lazy binding is contagious — you get another plan, and the REPL prints
it instead of a table:

```q
select from t
/ SELECT [col("sym"), col("side"), col("price"), col("size"), ...]
/   Parquet SCAN [trades.parquet]
/   SELECTION: col("size") > 100
```

Joins, `by` aggregation, `order`, `distinct` and `limit` all compose lazily:

```q
j: select sym, side, price, size, bid, ask from t `sym lj q `sym
j: select traded: sum notional, n: count price by sym, side from j
```

`collect` runs the plan once and binds a normal table; or skip the table and
`sink` the plan straight to disk:

```q
tm: collect j
j sink "summary.parquet"
```

[`examples/lazy_join_pipeline.qpl`](examples/lazy_join_pipeline.qpl) is a
two-input join + aggregate pipeline sunk to parquet without ever being collected.

### Comments

`/` starts a comment that runs to end of line:

```q
/ full-line comment
select from trades  / inline comment
```

### Multi-line statements

In a script, a statement may span several lines: any line indented by a tab or
4+ spaces continues the one above it; a line starting in column 0 (or a blank
line) ends it. No continuation character needed.

```q
t: select
    tot: sum size,
    apx: mean price
    by sym
    from trades
    where size > 50
```

In the REPL the prompt keeps reading while brackets are open, after a trailing
`,`, or when input was cut off mid-statement; a blank line submits.

### Logging

`log <expr>` (or `1 <expr>`, kdb-style) evaluates a scalar and prints it raw;
bare `log` / `1` prints a blank line. Space-separated expressions are rendered
and concatenated:

```q
log "starting run"
log "test" str$2*3 " that"       / test6 that
log "rows > " thr ": " n         / rows > 150: 42
```

Top-level juxtaposition separates items rather than forming a call — wrap a call
in parens: `log (f x) " done"`.

`\1 <path>` tees all stdout (log lines *and* query output) to a file as well as
the terminal; bare `\1` detaches it. Works in scripts and the REPL.

```q
\1 run.log
select from trades where size > 100
\1
```

### Config

`.qpl.cfg key=value ...` sets session-wide knobs. A bare `.qpl.cfg` prints the
current settings. Works in scripts and the REPL.

| Key | Meaning | Default |
|---|---|---|
| `maxcol` | max columns physically printed when rendering a table | `8` |
| `maxrow` | max rows physically printed when rendering a table | `10` |
| `round_type` | rounding mode for `round`: `HALF_UP` or `HALF_TO_EVEN` | `HALF_TO_EVEN` |

```q
.qpl.cfg maxrow=50 maxcol=20
.qpl.cfg round_type=HALF_UP
select mv: 2 round market_value from trades
```

### Operator reference

| Operator | Meaning |
|---|---|
| `+` `-` `*` | arithmetic |
| `%` | division (q convention) |
| `=` `<>` `!=` | equality |
| `<` `<=` `>` `>=` | comparison |
| `&` `\|` | logical and / or |
| `?[...]` | vectorised conditional |
| `round` | `<precision> round <col>` — round a float column (mode: `.qpl.cfg round_type`) |
| `over` | window: `<expr> over `p [order `k asc]`; verbs `rn` / `rank` / `drank` |
| `$` | cast (`f64$x`, `` `date$x ``, `"p"$s`); `` `$x `` -> categorical |
| `.qpl.d` `.qpl.t` `.qpl.p` `.qpl.n` | now: date / time / timestamp / timespan (UTC) |
| `!` | `col!bool` sort map; `` u8!`$x `` -> categorical physical width |
| `::` | enum cast (`` lvl::`$x ``) |
| `#` | limit (`10#t`); take / slice a list (`3#l`, `-3#l`) |
| `[...]` | positional index into a list (`l[0]`, `l[1 2 3]`) |
| `_` | drop columns (`` `a`b _ t ``) |

## REPL

| Command | Action |
|---|---|
| `\d <stmt>` | disassemble — show bytecode without executing |
| `\l <path>` | run a `.qpl` script in the current session |
| `\1 <path>` | tee all stdout to `<path>` (bare `\1` detaches) |
| `log <expr>` / `1 <expr>` | print a scalar |
| `cols <name>` | show a table's schema |
| Ctrl-C | abandon a partial statement (or exit at an empty prompt) |
| Ctrl-D | exit |

```
qpl) \d select avg price by sym from trades where size > 100
0000: FROM_SRC InMem("trades")
0001: PUSH_COL_REF size
0002: PUSH_CONST Int(100)
0003: BIN_OP >
0004: FILTER 1
0005: PUSH_COL_REF sym
0006: ALIAS Some("sym")
0007: BUILD_KEYS 1
0008: PUSH_COL_REF price
0009: CALL avg 1
0010: ALIAS Some("price")
0011: BUILD_PROJ 1
0012: SELECT_BY
0013: RESULT
```

## Architecture

```
source -> lexer -> tokens -> parser -> AST -> compiler -> instructions -> VM (Polars LazyFrame) -> DataFrame
```

| Module | Role |
|---|---|
| `lexer` | tokenise source text |
| `parser` | build the typed AST |
| `compiler` | emit stack-machine instructions |
| `vm` | execute instructions, build & collect a `LazyFrame` |
| `repl` | interactive loop + script runner |

## Releases

Binaries build automatically on every version bump (GitHub Actions) for Linux
(gnu + musl), Linux ARM64, and macOS (x86 + ARM).

## Roadmap

- WASM build, so qpl can run in the browser.
- More of the language.

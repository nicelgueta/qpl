# qpl — Quick Polars Query Language

An agent-friendly qsql/kdb+-inspired query language that compiles to Polars lazy frames.
Write concise q-style select statements; Polars executes them efficiently. Great for use without having python or polars installed.

## Why?
I often need to quickly query large data in parquet format on cloud storage under high time-pressure as well as write quick transformation jobs. DuckDB is brilliant for that kind of thing but I always forget the syntax and can't really knock something up more quickly than typing a prompt into Claude, which sometimes takes longer than I want to get the result I need or goes off on a tangent and provides fluff I wasn't looking for.

So I wanted to see if I could create a language/interface that is faster to write than writing a prompt into Claude, but just as efficient as something like DuckDB.

Of course the added benefit is that, inevitably using AI agents a lot to query data and debug issues, we can have a language that can actually be easily used by LLM agents too (that can't do that much damage whether in a sandbox, webUI or running free on your machine) but is efficient as polars or DuckDB - especially as it's zero dependency without even needing a python runtime.

### Polars
I'm also actually kinda cheating here.

Although the goal was to write something of similar efficiency to DuckDB (which is obviously not gonna happen by myself from scratch as that is an incredible piece of software crafted over many years), I can cheat if I use something well established as the backend for my language. I had thought about just writing some kind of dialect translator as an abstraction over DuckDB but that didn't excite me. Since I love Rust however, this meant Polars (an also brilliant DataFrame library mostly used in the python world but actually written in Rust) was an option.
Given it has such a neat API - all my lazy self had to do was write a VM that implements instructions as Polars queries and voilá - I can then go crazy with my front-end in what ever I want.

Given I have been lightly introduced to kdb+/q at work - and I don't know that much about the language, but am very impressed with its syntactic sugar as lack of verbosity. I thought this could be a good way to try to learn some of the language by writing the front-end interpreter for this language in this style, but still get to satisfy my Rust cravings.

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
    from << `trades.parquet `sym lj << `quotes.parquet `sym where price > 0
    order tot desc
    >> `summary.parquet
```

But the same pipeline is more naturally built up **one statement at a time** in
the REPL — see [Working incrementally](#working-incrementally):

```q
j: select sym, side, price, size, bid, ask from << `trades.parquet `sym lj << `quotes.parquet `sym where price > 0
`j  / check the table so far
j: update spread: ask - bid, notional: price * size from j
/ try the next step without committing — don't assign, just look
update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from j
/ happy with it — now assign to persist
j: update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from j
cols `j                                    / check the schema so far
select tot: sum notional, avg_spread: avg spread, n: count price by csym: `$sym, side, band from j order tot desc
`j >> `summary.parquet
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
qpl) `t >> `big.parquet                          / write it out
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
qpl) t: << `trades.parquet          / bind a table
qpl) `t                             / look at it
qpl) cols `t                        / ...or just its schema

qpl) t: select sym, side, price, size from t where price > 0
qpl) t: update notional: price * size from t

qpl) / not sure about the next step? run it WITHOUT assigning — the source is untouched
qpl) update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from t
qpl) t: update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from t   / keep it

qpl) select traded: sum notional by sym, side, band from t order traded desc
qpl) `t >> `out.parquet
```

Things that make the loop tight:

- **`` `t `` / `cols `t``** — peek at a table or its schema between steps.
- **Run a statement without assigning it** — you see the result, nothing changes.
  Assign only once you're happy.
- **`\d <stmt>`** — show the compiled plan without executing anything.
- **`lazy`** binds a *plan* rather than a table: nothing touches disk until a
  `collect` or `>>`, so building a large pipeline is instant and you pay for it
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
```

Scalar variables are evaluated in Rust and substituted into later queries as
Polars literals, so they compose transparently with column expressions.

### select / update / delete

```
select <cols> from <table> [by <keys>] [where <preds>] [order <col> <asc|desc>, ...]
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

### Expressions

| Kind | |
|---|---|
| arithmetic | `+` `-` `*` `%` (`%` is division, q convention) |
| comparison | `=` `<>` `!=` `<` `<=` `>` `>=` |
| logical | `&` `\|` |
| conditional | `?[cond; then; cond2; then2; ...; else]` — vectorised, nests for else-if |
| cast | `type$expr` — see [Casts](#casts) |
| round | `<precision> round <col>` — round a float column to N places |
| dyadic verbs | `<param> verb <col>` — `quantile`/`pctl`, `shift`/`lag`, `lead`, `diff`, `pctchange` (and `round`) |
| window | `<expr> over `p1`p2 [order `k1 asc `k2 desc] [rolling n]` — see [Window functions](#window-functions) |

```q
select price_bin: ?[price>400;`high;price>200;`mid;`low] from trades
select mv: 2 round market_value from trades          / round to 2 dp
select p95: 0.95 quantile price by sym from trades   / 95th percentile
select sym, price, ret: 1 diff price by sym from trades   / row-over-row change
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

### Casts

`type$expr` casts a column or scalar:

```q
select f: f64$size from trades
select f64$size, str$sym from trades
```

Types: `f64`/`float`, `f32`, `i64`/`int`, `i32`, `i16`, `i8`, `u64`, `u32`,
`u16`, `u8`, `bool`, `str`/`string`.

### Symbols, categoricals & enums

Outside a table expression, `` `foo `` is a **symbol** — a distinct value kind
that names a column, table or path. `` `$expr `` interns a string into a symbol:

```q
o: "out/summary.parquet"
`t >> `$o                      / use a string variable as a path
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

`load` (or the `<<` operator) reads a parquet or CSV file. On its own it is
**eager** — `t: load ...` materialises a table straight away. Prefix it with
[`lazy`](#lazy--collect) to keep it as a deferred scan instead.

```q
select avg price by sym from load `data/trades.parquet
t: load `data/trades.parquet     / eager — reads the file now, binds a table
t: << `data/trades.parquet       / same, operator form
t: lazy load `data/trades.parquet   / deferred — binds a plan, no IO yet
select from << `data/quotes.csv
```

`sink` (or `>>`) streams a **table expression** to a file — `` `tbl ``, a
`select ...`, an `update ...`; never a bare identifier:

```q
`t >> `summary.parquet
`t sink `summary.parquet
select sym, price from trades where size > 100 >> `big_trades.parquet
```

`cols` shows a table's schema (works on lazy bindings too):

```q
cols `trades
```

### Table operators

```q
distinct select sym from trades

10 limit select from trades      / first N rows
10#select from trades            / `#` is the same
10#`trades

`price`size drop select from trades   / drop columns
`price`size _ `trades                 / `_` is the same

`sym`price!01b `trades           / sort by a `col!bool` map (0 asc, 1 desc)
sorted: `sym`price!01b select from trades where size > 100
```

### lazy / collect

`lazy` as the first token of a table expression stores the **query plan** under a
name instead of running it. Nothing touches disk until you `collect` (materialise
to a DataFrame) or `sink` (stream to a file) — so a whole pipeline can process
**larger-than-RAM** data in a single pass.

```q
t: lazy load `trades.parquet
q: lazy select sym, bid, ask from load `quotes.parquet
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
tm: collect `j
`j >> `summary.parquet
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
| `$` | cast (`f64$x`); `` `$x `` -> categorical |
| `!` | `col!bool` sort map; `` u8!`$x `` -> categorical physical width |
| `::` | enum cast (`` lvl::`$x ``) |
| `<<` `>>` | load / sink |
| `#` | limit (`10#t`) |
| `_` | drop columns (`` `a`b _ `t ``) |

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

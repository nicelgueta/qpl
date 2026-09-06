# qpl — Quick Polars Query Language

An agent-friendly qsql/kdb+-inspired query language that compiles to Polars lazy frames.
Write concise q-style select statements; Polars executes them efficiently. Great for use without having python or polars installed.

## Why?
I often need to quickly query large data in parquet format on cloud storage under high time-pressure as well as write quick transformation jobs. DuckDB is brilliant for that kind of thing but I always forget the syntax and can't really knock something up more quickly than typing a prompt into Claude, which sometimes takes longer than I want to get the result I need or goes off on a tangent and provdies fluff I wasn't looking for.

So I wanted to see if I could create a language/interface that is faster to write than writing a prompt into Claude, but just as efficient as something like DuckDB.

Of course the added benefit is that, inevitably using AI agents a lot to query data and debug issues, we can have a language that can actualyl be easily used by LLM agents too (that can't do that much damage whether in a sandbox, webUI or running free on your machine) but is efficient as polars or DuckDB - especially as it's zero dependency without even needing a python runtime.

### Polars
I'm also actually kinda cheating here.

Although the goal was to write something of similar efficiency to DuckDB (which is obviously not gonna happen by myself from scratch as that is an incredible piece of software crafted over many years), I can cheat if I use something well established as the backend for my language. I had thought about just writing some kind of dialect translator as an abstraction over DuckDB but that didn't excite me. Since I love Rust however, this meant Polars (an also brilliant DataFrame library mostly used in the python world but actually written in Rust) was an option.
Given it has such a neat API - all my lazy self had to do was write a VM that implements instructions as Polars queries and voilá - I can then go crazy with my front-end in what ever I want.

Given I have been lightly introduced to kdb+/q at work - and I don't know that much about the language, but am very impressed with its syntactic sugar as lack of verbosity. I thought this could be a good way to try to learn some of the language by writing the front-end interpreter for this language in this style, but still get to satisfy my Rust cravings.

Thus: `qpl`.


### Example:
DuckDB - loading table from parquet, transforming into another table and then saving to another parquet
```sql
SET VARIABLE thr = 2 * 45;
CREATE TEMP TABLE t AS
SELECT
    sym,
    side,
    CAST(-AVG(size) AS BIGINT) AS r,
    SUM(size * price) AS total_market_value
FROM read_parquet('my_trades.parq')
WHERE price < thr
GROUP BY sym, side;

COPY t TO 'output.parquet' (FORMAT PARQUET);
```
qpl equivalent:
```q
thr: 3 * 45
t: select r: i64$neg mean size, total_market_value: sum size * price by sym, side from << `my_trades.parq where price > thr
t >> `output.parquet
```

## Install

```bash
cargo install --path .
```

Or grab a pre-built binary from [Releases](../../releases).

## Usage

```
# Interactive REPL (loads demo tables: trades, quotes)
qpl

# Run a script
qpl script.qpl

# Run a script then drop into the REPL
qpl -i script.qpl
```

Runnable scripts live in [`examples/`](examples/) — `qpl examples/lazy_join_pipeline.qpl`.

## Language

### Select

```
select <cols> from <table> [by <keys>] [where <preds>] [order <column> <asc|desc>, ...]
```

(the interactive REPL comes with some demo tables `quotes` and `trades` for you to play around with)
```q
select from trades
select sym, price from trades
select px: price, qty: size from trades
select avg price by sym from trades
select from trades where size > 100
select total: sum size by sym from trades where side = "buy"
select from trades order sym asc, price desc
select price_bin: $[price>400;`high;price>200;`mid;`low] from trades

### Update

```q
update price: price * 2 from trades
update price: price * 2 by sym from trades where size > 100
```

Updates return the complete table, retaining columns that are not updated.

### Delete

```q
delete from trades where size < 100
delete `price`size from trades
```

### Column dropping

```q
`price`size drop select from trades
`price`size _ `trades
```

### Table operators

```q
/ distinct
distinct select sym from trades

/ limit
10 limit select from trades
10#select from trades
10#`trades

/ also use str vars a symbols (maybe change this but easy to write for now)
select total: sum size by sym from trades where side = `buy

/ using bool vecs
select from trades where 10100000b
```

### Assignments

```q
/ table variable
t: select from trades where size > 100

/ scalar variable (usable in subsequent queries)
threshold: 150
select from trades where size > threshold
```

### load — load files lazily

```q
/ parquet
select avg price by sym from load `data/trades.parquet
t: load `data/trades.parquet
/ also use special operator <<
t: << `data/trades.parquet

/ csv
select from << `data/quotes.csv
```

### sink — write to file

```q
t: select total_size: sum size, apx: mean price, total_value: sum price * size by sym, side from trades
t sink `summary.parquet

/ also with operator >>
t >> `summary.parquet
```

### lazy / collect — defer materialisation

`lazy` as the first token of a table expression stores the **query plan** under a
name instead of a materialised table. Nothing runs until you `collect` it
(materialise to a DataFrame) or `sink` it to a file.

```q
/ build a plan, don't run it — no IO happens here
t: lazy load `trades.parquet
/ `lazy` works on any table expression, not just load
q: lazy select sym, bid, ask from load `quotes.parquet
```

Extend a plan by **re-assigning the binding**. Each step is still just plan
nodes; the file is never touched:

```q
t: select sym, side, price, size from t where size > 100
t: update notional: price * size from t
t: update band: $[notional > 50000; `big; `small] from t
```

Reading a lazy binding without collecting is contagious — the result is another
lazy plan, and the REPL prints it rather than a table:

```q
select from t
/ SELECT [col("sym"), col("side"), col("price"), col("size"), ...]
/   Parquet SCAN [trades.parquet]
/   SELECTION: col("size") > 100
```

Joins, `by` aggregation, `order`, `distinct` and `limit` all compose lazily too:

```q
j: select sym, side, price, size, bid, ask from t `sym lj q `sym
j: select traded: sum notional, n: count price by sym, side from j
```

`collect` runs the plan once and binds the result as a normal table:

```q
tm: collect j
select from tm where side = `buy
```

...or skip the table entirely and stream the plan straight to a file:

```q
j >> `summary.parquet
```

`cols` always resolves to a table, even on a lazy binding. Assignment uses `:`
(`tm: collect t`), same as everywhere else in qpl.

See [`examples/`](examples/) for runnable scripts, including
[`lazy_join_pipeline.qpl`](examples/lazy_join_pipeline.qpl) — a two-input,
join + aggregate pipeline that is sunk to parquet without ever being collected.

### cols — inspect schema

```q
cols `trades
cols `t
```

### sorting — pass a map of column names to bools (false = ascending, true = descending) to sort by

```q
`sym`price!01b `trades
sorted: `sym`price!01b select from trades where size > 100
```

### Type casts

Uses the `$` operator: `type$expr`

```
select f: f64$size from trades
select f64$size, str$sym from trades
```

Supported types: `f64`/`float`, `f32`, `i64`/`int`, `i32`, `i16`, `i8`,
`u64`, `u32`, `u16`, `u8`, `bool`, `str`/`string`.

### Operators

| Operator | Meaning |
|----------|---------|
| `+` `-` `*` | arithmetic |
| `%` | division (q convention) |
| `=` `<>` `!=` | equality |
| `<` `<=` `>` `>=` | comparison |
| `&` `\|` | logical and / or |
| `$` | cast (`f64$x`) |

### Aggregates

`sum`, `avg`/`mean`, `min`, `max`, `count`, `first`, `last`,
`std`/`dev`, `var`, `med`/`median`, `abs`, `neg`, `not`,
`string`, `distinct`/`n_unique`

### Virtual column `i`

`i` is the row index. It is aliased to `x` in the result (q convention).

```q
select i, sym from trades
select from trades where i < 5
```

### Comments

Lines beginning with `/` are comments (in scripts and in subexpressions).

```q
/ this is a comment
select from trades  / inline comment
```

## REPL commands

| Command | Action |
|---------|--------|
| `\d <stmt>` | disassemble — show bytecode without executing |
| `cols <name>` | show column names and types for a table |
| Ctrl-C / Ctrl-D | exit |

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
source → Lexer → Tokens → Parser → AST → Compiler → Instructions → VM (Polars LazyFrame) → DataFrame
```

| Module | Role |
|--------|------|
| `lexer` | tokenise source text |
| `parser` | build typed AST |
| `compiler` | emit stack-based instructions |
| `vm` | execute instructions, build and collect a `LazyFrame` |
| `repl` | interactive loop + script runner |

Scalar variables (`x: 1+2`) are evaluated in Rust; their values are
substituted as Polars `lit(...)` literals at query time so they compose
transparently with column expressions.

## Releases

Binaries are built automatically on every version bump via GitHub Actions
for Linux (gnu + musl), Linux ARM64, macOS (x86 + ARM).


## TODO
>aside from obviously expanding the language further...
- WASM (so this can be used directly in a web browser)
- binary size is non-trivial (100MB). Likely because it has the whole polars lib + other deps bundled in. should find a way to reduce this.

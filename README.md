# qpl — Quick Polars Query Language

An agent-friendly qsql/kdb+-inspired query language that compiles to Polars lazy frames.
Write concise q-style select statements; Polars executes them efficiently. Great for use without having python or polars installed by humans or AI agents.

## Why?
Because using AI agents a lot to query data I thought it would be great to give them unfettered access
to query data using something as a efficient as polars but have the liberty to write code without approvals
or even needing a specific python environment.

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

## Language

### Select

```
select <cols> from <table> [by <keys>] [where <preds>]
```

```
select from trades
select sym, price from trades
select px: price, qty: size from trades
select avg price by sym from trades
select from trades where size > 100
select total: sum size by sym from trades where side = "buy"
```

### Assignments

```
/ table variable
t: select from trades where size > 100

/ scalar variable (usable in subsequent queries)
threshold: 150
select from trades where size > threshold
```

### scan — load files lazily

```
/ parquet
select avg price by sym from scan "data/trades.parquet"
t: scan "data/trades.parquet"

/ csv
select from scan "data/quotes.csv"
```

### cols — inspect schema

```
cols trades
cols t
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

```
select i, sym from trades
select from trades where i < 5
```

### Comments

Lines beginning with `/` are comments (in scripts and in subexpressions).

```
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
0000: FROM_TABLE trades
0001: PUSH_COL_REF size
0002: PUSH_CONST Int(100)
0003: BIN_OP >
0004: FILTER 1
...
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

# qpl — Quick Polars Language

qpl is a language with two rather different influences. The syntax is
inspired by kdb+/q, so it's terse to the point of looking cryptic until it
suddenly doesn't. The engine underneath is [Polars](https://pola.rs), so the queries
run at a speed competitive with DuckDB. Nothing else is involved: qpl is a
single static binary, with no Python, no Polars install, and no runtime to
set up.

The result is a language you can type a real query into faster than you could
describe that query to someone else, and which will then chew through a
parquet file considerably larger than the machine's memory.

It's also a language you can safely hand to an AI agent. Every qpl session is
read-only unless it's started with `-w`, and a read-only session can't write
to disk or change anything outside itself, whatever it's asked to run. That
guarantee comes from the language itself, not from a sandbox around it.
[More below](#built-for-agents).

qpl borrows a handful of ideas from q (terse right-to-left expressions,
`select … by … from` queries, dotted temporal literals) but it isn't q, a q
dialect, or an attempt at compatibility with kdb+. It's its own language, and
in most respects a very different one: a different type system, a different
execution model built around lazy Polars plans, and missing plenty of syntax and features that q actually has. Don't expect q code to run unchanged.

📖 **[Read the book][book]** for the full guided tour. This README is the
short version, and each section links to the chapter covering it properly.


## What it looks like

Read two parquet files, left-join them, derive columns, tag every row with a
conditional, encode a key column, aggregate, sort, and write the result out.

In DuckDB:

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

In qpl, where the whole thing is one statement:

```q
select tot: sum price * size, avg_spread: avg ask - bid, n: count price
    by csym: `$sym, side, band: ?[size >= 1000; `large; size >= 250; `mid; `small]
    from load "trades.parquet" `sym lj load "quotes.parquet" `sym where price > 0
    order tot desc
    sink "summary.parquet"
```

No `CREATE TYPE` preamble, no `COPY (...) TO` wrapper to get the output onto
disk, and the grouping keys named once rather than repeated in a `GROUP BY`.

## Built for agents

Giving an agent Python or a shell to explore data means giving it everything
else too: it can delete files, overwrite the data it was meant to read, or
install packages. The usual answer is a container or a permissions layer
wrapped around the interpreter. That works, but the safety then lives in
infrastructure the agent's language knows nothing about, and it has to be
rebuilt for every deployment.

qpl puts the boundary inside the language:

- **Any qpl, limited reach.** An agent can write and run any qpl it likes:
  queries, bindings, functions, loops, lazy plans. But everything it does
  goes through qpl's own actions. There's no `exec`, no shell-out and no
  package import to step outside them, so in a read-only session the worst a
  confused agent can do is run a bad query.
- **Everything has a permission.** Every action in the language is either a
  **read** or a **write**. Read covers almost everything: querying, loading
  files, binding names, defining functions, changing settings, logging,
  connecting to other servers. Write is anything that writes to disk or
  changes state outside the session: `sink`, `\1 <path>`, and a write handle
  to another server. The VM checks the permission before the action runs,
  every time.
- **Read-only by default.** A session only gets write permission when it's
  started with `-w` (`--write`). Without it, every write fails with
  `Cannot perform write action in read-only session: <action>`, and
  everything else works as normal, so the agent still gets a full language
  to think in.
- **Fixed for the life of the session.** No statement can grant write
  permission. It holds through functions, `\l`-loaded scripts, compiled
  `.qplc` files and IPC requests alike.
- **Extensions keep the guarantee.** A [Rust extension][extensions] function
  has to declare `read` or `write`, and a read-only session refuses its
  writes like any built-in's.

In practice a qpl tool for an agent is a one-liner:

```bash
qpl -c "$QUERY"            # one query per tool call
qpl -i setup.qpl           # or a long-lived session: load the data once,
                           # open a \port, and let agents query it over IPC
```

Because writing needs an explicit flag, it's also easy to forbid. In
[Claude Code](https://code.claude.com/docs/en/permissions), for example, a
project's `.claude/settings.json` can let the agent run qpl freely while
denying it write permission:

```json
{
  "permissions": {
    "allow": ["Bash(qpl *)"],
    "deny": ["Bash(qpl *-w*)", "Bash(qpl *-iw*)", "Bash(qpl *--write*)"]
  }
}
```

[The chapter][read-only] explains how that works and where its limits are.

## Install

```bash
cargo install --path .
```

Polars is a large crate, so a cold build takes a few minutes. Prebuilt
binaries for Linux (gnu and musl), Linux ARM64, and macOS on Intel and Apple
silicon are on the [releases page](../../releases).

The [client/server][ipc] is built in by default (`ipc` feature); build with
`--no-default-features` to drop it and its two extra dependencies.

### Editor support

The [VSCode extension](tools/vscode/) adds syntax highlighting, autocomplete,
and a Ctrl+Enter REPL — install it straight from the Marketplace as
[`nicelgueta.qpl`](https://marketplace.visualstudio.com/items?itemName=nicelgueta.qpl).

### Try it in a browser, no install

qpl also compiles to WebAssembly, running free in your browser at
[FastBoard](https://nicelgueta.github.io/fastboard/board/). Add a **Code
Editor** widget there and set its language to `qpl` — it's a Monaco editor,
the same one VSCode is built on, so it gets the same syntax highlighting and
autocomplete as the extension. 
You can also add a **Data Table** widget too, upload a CSV
or Parquet file, and link the editor to it to route query results into the
table instead of the CLI output. It all runs client-side as compiled wasm in a Web Worker — there's no backend, so nothing you upload ever leaves the browser tab. Check the network tab under dev tools if you don't believe me. 

## Quickstart

```bash
qpl --load-demo     # REPL with demo `trades` and `quotes` tables
qpl script.qpl      # run a script
qpl -i script.qpl   # run a script, then stay in the REPL with its state
qpl -c 'select avg price by sym from trades' --load-demo   # run one command and exit
qpl -C script.qpl   # compile to script.qplc; `qpl script.qplc` then runs it with no parsing
qpl -w script.qpl   # allow writes (`sink`, `\1 <path>`): sessions are read-only by default
```

```q
qpl) trades                                     / a bare table name prints it
qpl) select sym, price from trades where price > 200
qpl) select avg price by sym from trades        / group-by, key named once
qpl) t: select from trades where size > 100     / bind a table
qpl) t sink "big.parquet"                       / stream it to a file (needs qpl -w)
```

Runnable scripts are in [`examples/`](examples/).

## Why?

I often need to query large parquet files on cloud storage under time
pressure, or knock out a quick transform job. DuckDB is great at this but
stops short as soon as I want to do anything scripty around the query. Python
and Polars handle that part, but turn every query into a verbose block of
method chaining, and Polars' SQL support means embedding a large string with
no editor support inside it.

Handing it to an agent is sometimes the right move. But for the eighty
percent of queries that are genuinely simple, describing what I want in
English takes longer than writing the query, assuming the language lets me
write it quickly.

So: something concise enough to type without thinking, that behaves like a
scripting language, treats queries as first-class syntax rather than strings,
and runs in the same league as DuckDB. A language like that also turns out to
be easy for an agent to drive and hard for one to do much damage with, and
read-only sessions turn "hard" into "can't" (see [Built for agents](#built-for-agents)).

Writing a DuckDB-grade engine solo isn't realistic, so Polars does that part.
Its API is clean enough that the language is really a small VM translating
syntax into Polars operations, which left me free to design the front end.
That front end takes some inspiration from q, which I'd been lightly exposed
to at work, but it goes its own way wherever that suited qpl better. [More on the reasoning][why].

## Working incrementally

This is the part qpl leans on hardest: **a transformation is a sequence of
statements, built one at a time.** Each step binds a name, the binding
persists, and the next step starts from it. There's no growing query to
re-run and no stack of CTEs.

```q
qpl) t: select sym, side, price, size from trades where price > 0
qpl) t: update notional: price * size from t

qpl) / unsure about the next step? run it WITHOUT assigning — nothing changes
qpl) update band: ?[size >= 250; `large; size >= 100; `mid; `small] from t
qpl) t: update band: ?[size >= 250; `large; size >= 100; `mid; `small] from t

qpl) select traded: sum notional by sym, band from t order traded desc
qpl) t sink "out.parquet"
```

What keeps the loop tight: printing a name or `cols t` to look between steps,
running a statement unassigned to try it for free, `\d` to see what a
statement compiles to, and `lazy` to defer the whole pipeline until the end.
[Full chapter][incremental].

## Language

### Assignment · [chapter][assignment]

`:` binds a name, and the right-hand side decides what kind of binding it is.
Scalars are evaluated in Rust and compiled into later queries as literals, so
they compose transparently.

```q
threshold: 150                                 / scalar
t: select from trades where size > threshold   / table
lvl: `low`mid`high                             / symbol list
top: max trades`price                          / reduction to a scalar
```

### Symbols · [chapter][symbols]

A backtick makes a **symbol**, an atomic name used for columns and short
labels. Data in a text column is a string; the *name* of a column is a
symbol. Symbols written in a run need no separator, and `` `$ `` interns a
string into one.

```q
`low`mid`high                  / three symbols, not one
`$"Analytics Engineer"         / a symbol with a space
```

### select / update / delete · [chapter][select]

```
<select|update|delete> <cols> from <table-expr> [by <keys>] [where <preds>] [order <col> <asc|desc>, ...]
```

`select` projects, `update` returns the whole table with columns added or
replaced, and `delete` removes rows or columns. The clauses work the same way
in all three.

```q
select from trades                               / no column list = all of them
select px: price, qty: size from trades          / aliasing
select avg price by sym from trades              / group-by
select total: sum size by sym from trades where side = "buy"
select from trades where size > 100, price < 400 / commas are AND
select from trades where (size > 400) | (side = "buy")
select from trades where 10100000b               / boolean-vector mask
select from trades order sym asc, price desc

update price: price * 2 by sym from trades where size > 100
delete from trades where size < 100
delete `price`size from trades
```

`update`'s `where` never drops rows: non-matching rows keep their old value,
or `null` for a brand-new column.

`from` takes any table expression rather than just a name, so queries nest
without special subquery syntax:

```q
select from select price from trades where price > 100
select sym, price from load "data/trades.parquet"
```

Joins are `lj` / `ij` / `rj`, naming the key on each side. The right side is a
bare table or a `load`; parenthesise anything richer, which tells the parser
where the inner expression ends.

```q
select price, bid from trades `sym lj quotes `sym
select price, bid from trades `sym lj (select sym, bid from quotes where bid > 0) `sym
```

### Reading & writing files · [chapter][files]

`load` reads parquet or CSV and is eager on its own; `sink` streams a table
expression out to a file (in a session started with `-w`); `cols` shows a
schema without reading data.

```q
t: load "data/trades.parquet"
select sym, price from trades where size > 100 sink "big_trades.parquet"
cols trades
```

### Expressions · [chapter][expressions]

| Kind | |
|---|---|
| arithmetic | `+` `-` `*` `%` (`%` is division, since `/` starts a comment) |
| comparison | `=` `<>` `!=` `<` `<=` `>` `>=` |
| logical | `&` `\|` |
| conditional | `?[cond; then; cond2; then2; ...; else]`, vectorised, nests for else-if |
| pattern match | `<str/sym> like <pattern>`, q-style glob |
| cast | `type$expr` |
| dyadic verbs | `<param> verb <col>`: `quantile`/`pctl`, `shift`/`lag`, `lead`, `diff`, `pctchange`, `round`, `fill`; the operand may be a table expression (`"" fill select b from t`) |
| window | `` <expr> over `p [order `k asc] [rolling n] `` |

```q
select price_bin: ?[price>400;`high;price>200;`mid;`low] from trades
select sym from trades where sym like "[AM]*"
select mv: 2 round market_value from trades          / mode: .qpl.cfg round_type
select p95: 0.95 quantile price by sym from trades
```

`%` is always true division, even for two ints (`10 % 4` is `2.5`, not `2`) —
as in q, unlike `+`/`-`/`*` which stay integer when both sides are.
Evaluation is right to left, so parenthesise when an operator doesn't
commute: `(int$"42") - 1`.

**Aggregates:** `sum`, `avg`/`mean`, `min`, `max`, `count`, `first`, `last`,
`std`/`dev`, `var`, `med`/`median`, `mode`/`modal`, `skew`, `kurt`, `any`,
`all`, `prod`, `argmin`, `argmax`, `nnull`, `distinct`/`n_unique`, plus `abs`, `neg`, `not`. (In a select
list `distinct` counts a column's unique values; in front of a table it
deduplicates rows.)

**Null tests:** `isnull` / `notnull` give a boolean per row, for filtering:
`select from t where isnull price`, `select count i from t where notnull sym`.
`nnull` counts the nulls in a column.

**Removing nulls:** `<value> fill <col>` replaces nulls in a column with a value
(`select 0 fill price from t`, `update qty: 0 fill qty from t`), and `` `a`b dropnull <table> ``
drops every row that has a null in any of the named columns
(`` `price dropnull t ``). Lists can't hold nulls, so use these before pulling a
nullable column into a list.

**Cumulative:** `cumsum`, `cummax`, `cummin`, `cumprod`, `cumcount`, `ffill`,
`bfill`. Most useful with `over` and an `order`.

### Column expressions & lists · [chapter][columns]

A column expression pulls one column out without a surrounding statement.
Used as a value it materialises to a **list**; reduce it and you get a scalar
you can feed straight back into a query.

```q
px: trades`price                / f64[8]
top: max trades`price           / f64: 416
select sym, price from trades where price >= top

l: 10 20 30 40 50
l[1 3 4]                        / i64[3]: 20 40 50 — index, or gather
3#trades`price                  / first 3 (-3# for the last 3)
l + 2                           / elementwise; scalars broadcast
nums where x > 25               / filter a list by its own values, `x` is the element
asc 3 1 2                       / i64[3]: 1 2 3 — a keyword that works in a query also works on a list: asc/desc/distinct/limit/drop/_/dropnull/where
til 5                           / i64[5]: 0 1 2 3 4
("ab" "cd" "ef")                / str[3] — space-separated strings; parens keep it apart from other values
enlist 23                       / i64[1]: 23 — one-element list of any atom (`enlist "a"` is a str[1])
3?6                             / i64[3]: 3 random ints in 0..5, drawn with replacement
2 ? 10 20 30 40                 / 2 random picks from the list (any list: ints, syms, strings, a column)
5?2.5                           / f64[5]: uniform in [0, 2.5)
zip `cola`colb!a b              / build a table from named lists
```

### Casts · [chapter][casts]

`f64`/`float`, `f32`, `i64`/`int`, `i32`, `i16`, `i8`, `u64`, `u32`, `u16`,
`u8`, `bool`, `str`/`string`. Strings are parsed rather than reinterpreted.
Integer widths collapse to one 64-bit value in scalar context and only take
effect once in a column.

```q
int$45.3                       / 45
int$"42"                       / 42, parsed
select s: f64$size, y: str$sym from trades   / name them: unaliased casts both want `x`
`date$select ts from trades where sym = "AAPL"   / casts a whole query's one-column result
```

### Temporal types · [chapter][temporal]

Dotted literals (a notation borrowed from kdb+/q), each an integer offset underneath. Adding an integer
shifts by one unit of that type's own resolution.

| type | literal | counts |
|---|---|---|
| date | `2024.03.15` | days since `2000.01.01` |
| month | `2024.03m` | months since `2000.01` |
| time | `12:30:00.000` | ms into the day |
| minute | `12:30` | minutes into the day |
| second | `12:30:00` | seconds into the day |
| timestamp | `2024.03.15D12:30:00.000000000` | ns since `2000.01.01` |
| timespan | `0D12:30:00.000000000` | ns of duration |

```q
2024.03.15 + 10                     / 2024.03.25
2024.03.15D09:30:00.0 - 0D00:05:00.000000000
`date$2024.03.15D12:30:00.0         / 2024.03.15
"p"$"2024.03.15D12:30:00"           / parse a string
```

Now-functions, all UTC: `.qpl.dt` date, `.qpl.tm` time, `.qpl.ts` timestamp,
`.qpl.dlta` timespan since midnight. Not yet implemented: `xbar`, `within`, unit
accessors, and column-plus-integer temporal arithmetic.

A raw integer crossing the `int`/`long` ↔ `timestamp` boundary (`` `timestamp$n ``
to build one, `` `long$ts `` to unwrap one) is read and written as **ns since
the Unix epoch (`1970.01.01`)** by default — the same convention a whole-column
`` `timestamp$col `` cast already uses under Polars, and the one most people
reach for. Set `.qpl.cfg useqepoch=true` to switch that boundary to
ns-since-`2000.01.01` (kdb's convention), matching the type's internal
representation exactly (only the raw-integer casts move; date literals,
arithmetic, and display are unaffected either way):

```q
`timestamp$1700000000000000000     / a Unix-epoch nanosecond timestamp
.qpl.cfg useqepoch=true
`timestamp$0                       / now reads as 2000.01.01D00:00:00.0
```

### Categoricals & enums · [chapter][enums]

`` `$col `` casts a column to a Polars **categorical** (interned strings, fast
joins and group-bys), with `u8!`/`u16!`/`u32!` choosing the code width. An
**enum** is the ordered version: declare the order, and it fixes both sorting
and each value's code.

```q
select country: u8!`$country from t

lvl: `low`mid`high
t: update band: lvl::`$?[size >= 250; `high; size >= 100; `mid; `low] from trades
select from t where band >= `mid     / ordered comparison, not alphabetical
```

Values absent from an enum become null.

### Window functions · [chapter][window]

`over` computes per partition and broadcasts back to every row. An `order`
sub-clause sorts the partition first, which is what makes the cumulative and
row-relative verbs meaningful, and is required by the ranking verbs `rn`
(`row_number`), `rank` (ties share the lower rank, then a gap) and `drank`
(no gap).

```q
select sym, price, top: max price over `sym from trades
select sym, gap: (max price over `sym) - price from trades   / `over` binds tighter than `-`
select sym, run: cumsum price over `sym order `ts asc from trades
select sym, r: rank over `sym order `price desc from trades
select sym, ma5: avg price over `sym order `ts asc rolling 5 from trades
```

Rolling leaves the first `n-1` rows of each partition null. The virtual
column `i` is the row index, printed as `x`.

### Table operators · [chapter][tableops]

```q
distinct select sym from trades
10 limit select from trades      / 10#select from trades is the same
-3 limit trades                  / last 3 rows; -3#trades is the same
`price`size drop trades          / `price`size _ trades is the same
`price dropnull trades           / drop rows with a null in the named columns
`sym`price!01b trades            / sort map: 0 asc, 1 desc

n: 10
n limit trades                   / the count can be any scalar expression, not just a literal
n#trades
collect n#(lazy load "trades.parquet")
```

### lazy / collect · [chapter][lazy]

`lazy` stores the **plan** instead of running it, so nothing touches disk
until `collect` or `sink`. Extend a plan by re-assigning the binding; the
whole pipeline then runs as one streaming pass, which is how larger-than-RAM
data gets processed.

```q
t: lazy load "trades.parquet"
t: select sym, side, price, size from t where size > 100
t: update notional: price * size from t
j: select traded: sum notional by sym, side from t `sym lj q `sym
j sink "summary.parquet"          / or: tm: collect j
```

Reading a lazy binding gives another plan, and the REPL prints it, which is a
good way to see what Polars intends to do before paying for it.

### Functions · [chapter][functions]

q-style lambdas. The last statement is the return value, and locals are
call-scoped.

```q
add: {[x,y] x+y}              / add[2;3] -> 5
inc: {[x] x+1}                / inc 41 or inc[41] -> 42
fac: {[n] ?[n<2; 1; n*fac[n-1]]}
bysym: {[s] select sym, price from trades where sym = s}
now: {[] .qpl.ts}               / niladic (no params) -> callable bare, `now`, or bracketed, `now[]`
```

A function taking one or more parameters must be called with `f[..]`; a
niladic one (no params) can also be called bare, like a variable — that's how
the `.qpl.dt`/`.qpl.tm`/`.qpl.ts`/`.qpl.dlta` [now-functions](#temporal-types--chaptertemporal)
work: they're ordinary built-ins, not special syntax.

Functions are first-class, so they can be passed, returned and stored, and a
`{[..] ..}` literal works anywhere an expression does:

```q
apply: {[f,x] f[x]}
apply[{[y] y*2}; 5]           / -> 10
apply[inc; 41]                / -> 42
{[y] y*2}[21]                 / applied on the spot -> 42
adder: {[n] {[y] y+1}}
plus1: adder[0]               / returned, stored, called later -> plus1[41] = 42
```

There is no lexical capture: a body sees its own params plus the session
globals, never the caller's locals — the same rule named functions have always
followed. A function isn't a *column* value either, so `select px: inc from
trades` is an error.

### Control flow · [chapter][controlflow]

`?[..]` is the conditional — with an atom condition it is lazy in its branches
(so recursion works), with a vector condition it is elementwise and every branch
must match its length — and functions stay pure. `while[test; s1; s2; ..]` runs statements in order in the
current scope while a boolean test holds; `noop` is "nothing" — it prints
nothing and can't be assigned or used as a value. `while` and `noop` are
reserved words. Ctrl-C stops a running statement (`'interrupted`).

```q
n: 3
while[n>0; log[n]; n: n-1]              / 3 2 1
sumto: {[m] s: 0; while[m>0; s: s+m; m: m-1]; s}
sumto[100]                              / 5050 — the loop binds function locals
?[n>3; noop; log["small"]]              / a branch that does nothing
?[1011b; 1; 0]                          / vector condition: i64[4]: 1 0 1 1
```

### Comments, multi-line, logging, config

`/` comments to end of line. In a script, a line indented by a tab or 4+
spaces continues the one above it; in the REPL, input keeps being read while
brackets are open or after a trailing comma. [multi-line][multiline]

`log <expr>` prints a scalar raw, concatenating space-separated expressions.
`\1 <path>` tees all stdout to a file. [logging][logging]

```q
log "rows > " thr ": " n         / rows > 150: 42
log (f x) " done"                / juxtaposition separates items, so parenthesise
log f[x] " done"                 / bracket application isn't ambiguous, no parens needed
info: {[s] log[str$.qpl.ts " - INFO " s]; noop}   / log[..] works in a body; noop stops the result echoing
```

`.qpl.cfg key=value` sets session knobs: `maxcol`, `maxrow`, `tblwidth` (max
characters wide a printed table may be), and `strlen` (max characters shown
per cell before truncating with an ellipsis) are display only, `-1` means
unlimited for `tblwidth`/`strlen`; `round_type` (`HALF_UP` or `HALF_TO_EVEN`)
and `useqepoch` (`true`/`false`, see Temporal types above) do change answers.
A bare `.qpl.cfg` prints the current settings. [config][config]

### Namespaces & imports · [chapter][imports]

A namespaced name is any dotted identifier, `.ns.name`, nesting allowed. It's
an ordinary variable, table or function reference that lives under a prefix
instead of in the flat session scope. `.qpl` is the one built in, holding the
now-functions and `.qpl.cfg`.

`\l <path>` loads a script flat into the shared scope. `\i "<path>"` instead
*imports* it: every table, global and function it binds at its top level
lands under `.<file-stem>.*` (`lib/my-lib.qpl` -> `.my_lib`), so an import
never overwrites a name already in your session. The path is a quoted string because
the namespace derives from it, which keeps it visually distinct from a
namespaced identifier on the same line. `\i` behaves the same typed at the
prompt or nested inside another script.

```q
\i "lib/utils.qpl"       / defines helper: {[x] x*2}
.utils.helper 21         / -> 42
```

A binding the imported script already namespaced itself is left alone rather
than double-prefixed. A function's *body* can still call another top-level
helper from the same script by its bare, unqualified name (`info` calling
`_log`, say) — that unqualified call resolves against the called function's
own namespace first, so a session name can't hijack it. This qualification
happens when the imported script is *compiled*, not while it runs: a bare
name the script binds at its own top level always means its own namespaced
binding, everywhere in the script, even in a statement that runs before that
binding's own statement does. So a library can't write `t: select from t
where ...` to read a session-level `t` and rebind it under its own name —
inside the script, a bare `t` never sees the session's.

An import is all-or-nothing: if the script fails, the session is restored as
it was, and a re-import is a clean reload of the namespace. Inside a script,
`\l`/`\i` paths are relative to that script's own directory.

### IPC · [chapter][ipc]

On by default (`ipc` feature; drop with `--no-default-features`). Lets one qpl process query another over a
REQ/REP socket pair ([zmq.rs](https://github.com/zeromq/zmq.rs), pure Rust,
no system libzmq). The point is to load slow tables once in a long-lived
session and let short-lived clients query it, to let non-qpl callers fetch
real tables over plain TCP, to fan out across several servers concurrently,
or to reshape a running session without restarting it.

`\port <n>` opens a listener and a bare `\port` closes it — in a script exactly
like the REPL (`qpl script.qpl` keeps serving after the script's last statement
if it left a port open, just as `-i` drops into the REPL). `<n>` is an ordinary
expression, not just a literal — `\port ?[env like "prod"; 5001; 5002]` picks
the port at runtime from a variable, a condition, whatever — see
[`examples/ipc_server.qpl`](examples/ipc_server.qpl). Each request is
evaluated exactly like a typed line, except for the `\`-prefixed system commands.

```q
conn: hopen 5001                       / or hopen "db.internal:5001"
resp: conn dispatch select from trades where price > 100

pending: conn async dispatch select avg price by sym from trades
result: await pending
```

A bare `hopen` is **read-only**, and the server rejects assignments, `sink`
and changing `.qpl.cfg` settings from it. `` `w!hopen `` opens a write handle. The permission is
chosen by the client, enforced per request, and never applies to the server
operator's own input.

### Read-only sessions · [chapter][read-only]

The safety guarantee that makes qpl fit for agents (see
[Built for agents](#built-for-agents)). Every action is a read or a write,
and a session is read-only unless it's started with `qpl -w`. In a
read-only session `sink`, `\1 <path>` and `` `w!hopen `` fail with
`Cannot perform write action in read-only session: <action>`, while queries,
assignments, functions, `log` and `.qpl.cfg` work as normal. No statement
can change a session's permission.

### Operator reference · [chapter][operators]

| Operator | Meaning |
|---|---|
| `+` `-` `*` | arithmetic |
| `%` | division (q convention) |
| `=` `<>` `!=` `<` `<=` `>` `>=` | comparison |
| `&` `\|` | logical and / or |
| `/` | comment to end of line |
| `:` | bind a name; alias a column |
| `` `x `` | symbol |
| `?[...]` | conditional: vectorised in a select; outside one an atom picks a branch, a vector gives an elementwise result of the same length |
| `while[test; s1; ...]` | loop: run statements in the current scope while `test` holds |
| `noop` | nothing: prints nothing, can't be assigned or used as a value |
| `like` | glob pattern match |
| `$` | cast (`f64$x`, `` `date$x ``, `"p"$s`); `` `$x `` -> categorical |
| `::` | enum cast (`` lvl::`$x ``) |
| `!` | dict literal; `` `col!01b `` sort map; `` u8!`$x `` code width |
| `#` | first n rows of a table; take from a list (`3#l`, `-3#l`) |
| `[...]` | index a list (`l[0]`, `l[1 2 3]`); call a function |
| `_` | drop columns (`` `a`b _ t ``) |
| `where` | filter rows; filter a list elementwise, `x` is the element |
| `over` | window, with verbs `rn` / `rank` / `drank` |
| `i` | virtual row-index column, printed as `x` |
| `til` | `til n` -> `0..n-1`; `lo til hi` -> `lo..hi-1` |
| `enlist` | `enlist x` -> the one-element list of the atom `x` (a string is one atom) |
| `?` | roll: `n?6` -> `n` random ints below 6; `n?list` -> `n` random elements, with replacement. (Prefix `?[c;a;b]` is the conditional) |
| `zip` | build a table from a dict of named lists |
| `lj` `ij` `rj` | left / inner / right join |
| `{...}` | lambda |
| `.qpl.dt` `.qpl.tm` `.qpl.ts` `.qpl.dlta` | now: date / time / timestamp / timespan (UTC) |
| `.qpl.cfg` | session config |
| `hopen` `` `w!hopen `` `dispatch` `async dispatch` `await` | IPC client |

## Rust extensions · [chapter][extensions]

Add your own native functions in Rust. Each one declares its permission, and
the VM enforces it like any built-in, so a read-only session refuses a
`write` extension exactly as it refuses `sink`. Extensions stay safe to hand
to an agent.

```rust
#[qpl::native(read)]
fn km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 { /* ... */ }

#[qpl::native(write)]
fn note(path: String, text: String) -> std::io::Result<()> { /* ... */ }

fn main() {
    let geo = qpl::ext::Extension::new("geo").with::<km>().with::<note>();
    qpl::cli::run(vec![geo]);   // the full `qpl` CLI, plus `.geo.km` and `.geo.note`
}
```

```bash
cargo run --example extension -- -c '.geo.km[51.51;-0.13;40.71;-74.01]'
cargo run --example extension_toolkit -- --load-demo -c '.stats.top[trades; `price; 3]'
```

[`examples/extension_toolkit.rs`](examples/extension_toolkit.rs) registers two
extensions with several functions each, covering lists, tables, lazy plans and
errors.

## REPL

| Command | Action |
|---|---|
| `\d <stmt>` | disassemble — show the compiled instructions without executing |
| `\l <path>` | run a `.qpl` script in the current session |
| `\i "<path>"` | import a script, namespacing its bindings under `.<file-stem>.*` |
| `\1 <path>` | tee all stdout to `<path>` (bare `\1` detaches) |
| `\port <n>` | start the IPC listener (bare `\port` stops) |
| `log <expr>` | print a scalar |
| `cols <name>` | show a table's schema |
| Ctrl-C | stop a running statement; otherwise abandon a partial statement (or exit at an empty prompt) |
| Ctrl-D | exit |

```
qpl) \d select avg price by sym from trades where size > 100
0000  PUSH       Name(trades)
0001  SOURCE
0002  PUSH       Name(size)
0003  LOAD_COL
0004  PUSH       Value(Int(100))
0005  PUSH       BinOp(Gt)
0006  BINOP
0007  PUSH       Count(1)
0008  FILTER
0009  PUSH       Name(sym)
0010  LOAD_COL
0011  PUSH       Name(sym)
0012  ALIAS
0013  PUSH       Count(1)
0014  LIST
0015  PUSH       Name(price)
0016  LOAD_COL
0017  PUSH       Count(1)
0018  PUSH       Verb(avg)
0019  VERB
0020  PUSH       Name(price)
0021  ALIAS
0022  PUSH       Count(1)
0023  LIST
0024  SELECT_BY
```

## Architecture

```
source -> lexer -> tokens -> parser -> AST -> compiler -> bytecode program -> VM (Polars LazyFrame) -> DataFrame
```

A script compiles to one bytecode program and runs in a single pass — a
syntax error anywhere in the file is caught before the first statement runs.
A REPL line or an IPC request is the same thing on a smaller scale.

| Module | Role |
|---|---|
| `lexer` | tokenise source text |
| `parser` | build the typed AST — a whole file at once for a script |
| `compiler` | emit a one-byte-per-instruction bytecode program |
| `vm` | execute it, building and collecting a `LazyFrame` |
| `repl` | interactive loop and script runner |

The interpreter is a library (`src/lib.rs`); the `qpl` binary (`cli` feature,
on by default) and the browser bindings (`wasm` feature) are both thin
front-ends over it.

There's no query optimiser, because there doesn't need to be one: the VM's
job ends at producing a Polars `LazyFrame`, and predicate pushdown and the
rest happen on the other side of that boundary. [More][architecture].

## Releases

Automated and driven by the version in `Cargo.toml`. A bump landing on `main`
tags the commit and cross-compiles binaries for Linux (gnu + musl), Linux
ARM64, and macOS (x86 + ARM).

## In the browser

A `wasm` feature exposes the same interpreter to JavaScript — a `Repl` object
with an `eval(line)` method that behaves like a CLI line, plus a
`qplLangConfig()` that hands an online editor (Monaco and friends) the
tokenizer, language configuration and completions the [VS Code
extension](tools/vscode/) uses.

It builds: `wasm-pack` produces a working `pkg/`. The catch is that stock
Polars doesn't compile for `wasm32-unknown-unknown` (its only supported wasm
target is `wasm32-unknown-emscripten`, for Pyodide), so the build needs a
four-change patch to Polars, kept in
[`tools/wasm/patches/`](tools/wasm/patches/) and applied for you by `make
wasm`. [The full story](tools/wasm/README.md).

## Roadmap

- Drop the Polars patch from the WASM build, once upstream builds for
  `wasm32-unknown-unknown` unaided.
- A basic standard library of commonly needed functions, shipped with qpl.
- Python extensions, with the same read/write permissions as
  [Rust extensions][extensions].
- Extension functions inside queries, built on Polars' own framework for
  vectorised Rust functions.
- Decide whether to support null as a first-class value in the language.
- Float list literals: `1.5 2.5` should be a list, as `1 2 3` is for ints.
- More of the language. Gaps are noted in the [book][book] beside the feature
  they belong to.

[book]: https://nicelgueta.github.io/qpl
[why]: https://nicelgueta.github.io/qpl/why.html
[incremental]: https://nicelgueta.github.io/qpl/working-incrementally.html
[assignment]: https://nicelgueta.github.io/qpl/language/assignment.html
[symbols]: https://nicelgueta.github.io/qpl/language/symbols.html
[select]: https://nicelgueta.github.io/qpl/language/select-update-delete.html
[files]: https://nicelgueta.github.io/qpl/language/files.html
[expressions]: https://nicelgueta.github.io/qpl/language/expressions.html
[columns]: https://nicelgueta.github.io/qpl/language/column-expressions.html
[casts]: https://nicelgueta.github.io/qpl/language/casts.html
[temporal]: https://nicelgueta.github.io/qpl/language/temporal-types.html
[enums]: https://nicelgueta.github.io/qpl/language/categoricals-enums.html
[tableops]: https://nicelgueta.github.io/qpl/language/table-operators.html
[window]: https://nicelgueta.github.io/qpl/language/window-functions.html
[functions]: https://nicelgueta.github.io/qpl/language/functions.html
[controlflow]: https://nicelgueta.github.io/qpl/language/control-flow.html
[lazy]: https://nicelgueta.github.io/qpl/language/lazy-collect.html
[multiline]: https://nicelgueta.github.io/qpl/language/multiline.html
[logging]: https://nicelgueta.github.io/qpl/language/logging.html
[config]: https://nicelgueta.github.io/qpl/language/config.html
[imports]: https://nicelgueta.github.io/qpl/language/imports.html
[ipc]: https://nicelgueta.github.io/qpl/language/ipc.html
[operators]: https://nicelgueta.github.io/qpl/language/operator-reference.html
[read-only]: https://nicelgueta.github.io/qpl/language/read-only.html
[extensions]: https://nicelgueta.github.io/qpl/extensions.html
[architecture]: https://nicelgueta.github.io/qpl/architecture.html

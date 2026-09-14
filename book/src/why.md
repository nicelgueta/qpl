# Why?

The honest answer is that this started as a personal itch.

A lot of my work involves querying large parquet files sitting on cloud
storage, usually under time pressure, or knocking out a quick transform job
that needs to exist by the end of the afternoon. DuckDB is genuinely great at
this, but it stops short as soon as I want to do anything scripty around the
query. So I'd reach for Python and Polars instead, which handles the scripty
part fine but turns every query into a verbose block of method chaining.
Polars' own SQL support isn't much better in practice: you end up with a
large SQL string embedded in a Python file, with no editor support inside it
and no feedback until it runs.

Handing the whole thing to an agent has become the obvious move, and
sometimes it is the right one. But for the eighty percent of queries that are
genuinely simple, describing what I want in English and waiting takes longer
than just writing the query, assuming the language lets me write it quickly.

That "assuming" is the whole project. I wanted something concise enough to
type without really thinking about it, which behaves like a scripting
language when I need it to, treats queries as first-class syntax rather than
strings, and still runs at a speed in the same league as DuckDB. A pleasant
side effect is that a language like that is also easy for an LLM agent to
drive, and difficult for one to do much damage with, sandboxed or not.

Ambitious? Well. Quite.

## Standing on Polars

Writing a DuckDB-grade query engine solo isn't a realistic weekend project,
so I didn't. Polars does that part.

Polars is a dataframe library written in Rust, and its API turned out to be
clean enough that the "language" I needed to build was really just a small
virtual machine translating my syntax into Polars operations. That left me
free to design whatever front end I wanted without also having to invent
columnar execution, predicate pushdown, or a parquet reader. Published
benchmarks put Polars a little behind DuckDB and comfortably ahead of
everything else in the space, which is more than good enough for what I need.

For the front end I borrowed heavily from kdb+/q. I'd been lightly exposed to
q at work, wanted a proper excuse to learn it, and wanted to write some Rust.
qpl is what came out of those three things colliding.

## What it looks like

Here is the kind of job this is for. Read two parquet files, left-join them,
derive a couple of columns, tag every row with a conditional, encode a key
column compactly, aggregate, sort, and write the result back out.

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

And in qpl, where the whole thing is a single statement:

```qpl
select tot: sum price * size, avg_spread: avg ask - bid, n: count price
    by csym: `$sym, side, band: ?[size >= 1000; `large; size >= 250; `mid; `small]
    from load "trades.parquet" `sym lj load "quotes.parquet" `sym where price > 0
    order tot desc
    sink "summary.parquet"
```

Don't try to read that yet. Every piece of it is introduced properly over the
next few chapters, and the point of showing it here isn't the syntax but the
shape: no `CREATE TYPE` preamble, no `COPY (...) TO`, no repeating the
grouping keys in a `GROUP BY` clause after you've already named them, and no
wrapper around the whole thing just to get the output onto disk.

## The way you'd actually write it

Although that statement fits on four lines, it isn't how the query would
really come into existence. In practice you'd build it up one statement at a
time, looking at the data between each step:

```qpl
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

That loop, rather than any individual piece of syntax, is what the language
is really designed around. [Working incrementally](working-incrementally.md)
comes back to it in detail once you've seen enough of the basics for it to
land.

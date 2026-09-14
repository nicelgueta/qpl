# Reading & writing files

Everything so far has queried the demo tables, which qpl handed you already
loaded. Real work starts and ends on disk, and qpl keeps that deliberately
small: one verb to read, one verb to write, and one to peek at a schema.

## load

`load` reads a parquet or CSV file, choosing between them by extension, and
takes the path as a string:

```qpl
t: load "data/trades.parquet"
```

Because `load` produces a table, it is a table expression, which means it can
go anywhere a table expression goes. In particular it can go straight into
the `from` clause you met in the [previous chapter](select-update-delete.md),
with no intermediate binding at all:

```qpl
select avg price by sym from load "data/trades.parquet"
select from load "data/quotes.csv"
```

That composability is the reason there's no separate "open a file" step in
the language. A file is just another source of rows.

On its own, `load` is **eager**: the statement above reads the file there and
then, and `t` holds a real table in memory. That's the right behaviour for a
file you're about to poke at interactively, and the wrong one for a file
larger than the machine's memory. [lazy / collect](lazy-collect.md) covers
the alternative, where `load` is deferred until something actually needs the
rows; the syntax is a single extra word and nothing else about this chapter
changes.

## sink

`sink` goes the other way, streaming a table out to a file:

```qpl
trades sink "summary.parquet"
```

Its left-hand side is a table expression too, so you don't have to store a
result before writing it. A whole query can be piped to disk in one
statement:

```qpl
select sym, price from trades where size > 100 sink "big_trades.parquet"
```

There's no "materialise, then save" ceremony because `sink` is itself the end
of the pipeline. This matters more than it might appear: it's what allows a
query over a very large input to write its output without ever holding the
whole result in memory, which is the subject of
[lazy / collect](lazy-collect.md).

## cols

The last of the three is `cols`, which shows a table's schema rather than its
data:

```qpl
qpl) cols trades
```

```
shape: (5, 2)
┌────────┬──────────────┐
│ column ┆ dtype        │
│ ---    ┆ ---          │
│ str    ┆ str          │
╞════════╪══════════════╡
│ sym    ┆ str          │
│ price  ┆ f64          │
│ size   ┆ i64          │
│ side   ┆ str          │
│ ts     ┆ datetime[ns] │
└────────┴──────────────┘
```

This is the first thing to run against a file you've never seen, and the
thing to keep running as you build a pipeline up, since it answers "what have
I actually got here" without printing a screenful of rows. It takes a table
expression like everything else, so it works on a query as readily as on a
name:

```qpl
cols select from trades where size > 100
```

Because `cols` reads only the schema, it stays cheap no matter how large the
underlying data is, and it works on deferred pipelines that haven't read a
single row yet.

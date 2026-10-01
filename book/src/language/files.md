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

`sink` goes the other way, streaming a table out to a file. It's a write
action, so it needs a session started with `qpl -w`; see
[Read-only sessions](read-only.md).

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

## read0 and read1: text and bytes

`load`/`sink` are for tabular data. `read0`, `read1`, `write0` and `write1`
are the line- and byte-level equivalents, for anything else on disk.

`read0` reads a file as text and returns its lines as a string list, split
on `\n` with a trailing `\r` stripped from each line (so CRLF files read the
same as LF ones). A final trailing newline doesn't produce a trailing empty
element:

```qpl
read0 "notes.txt"
```

`read1` reads the same file as raw bytes, returned as a `ByteVec` — a byte
list with no scalar counterpart. It prints as `0x` followed by the lowercase
hex of every byte (`byte[5]: 0x68656c6c6f`), and indexing one gives a plain
int, since there's no byte scalar to index into:

```qpl
read1 "notes.txt"
(read1 "notes.txt")[0]
```

Both take an optional byte range, kdb-style: `read0[p; off; len]` reads
`len` bytes starting at byte `off`, then splits the result into lines (or
bytes, for `read1`); `read0[p; off]` reads from `off` to the end of the
file. An offset or length past the end of the file is clamped rather than
erroring. `read0` requires the bytes it reads to be valid UTF-8.

`write0` and `write1` go the other way, and need a session started with
`qpl -w` (see [Read-only sessions](read-only.md)):

```qpl
("line one" "line two") write0 "notes.txt"
read1 "notes.txt" write1 "copy.bin"
```

`write0` takes a string list (or a single string, written as one line) and
writes each element followed by `\n`, truncating the file first. `write1`
takes a `ByteVec`, or an int list whose values are all 0-255, and writes the
raw bytes, also truncating first. Neither prints anything.

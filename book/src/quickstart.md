# Quickstart

Start the REPL with the demo tables loaded:

```bash
qpl --load-demo
```

```
Loading demo tables `trades` and `quotes`
qpl)
```

You now have two tables to play with. Everything in this chapter happens at
that `qpl)` prompt.

## Looking at a table

The simplest possible statement is the name of a table:

```qpl
qpl) trades
```

```
shape: (8, 5)
┌──────┬───────┬──────┬──────┬─────────────────────┐
│ sym  ┆ price ┆ size ┆ side ┆ ts                  │
│ ---  ┆ ---   ┆ ---  ┆ ---  ┆ ---                 │
│ str  ┆ f64   ┆ i64  ┆ str  ┆ datetime[ns]        │
╞══════╪═══════╪══════╪══════╪═════════════════════╡
│ AAPL ┆ 182.3 ┆ 100  ┆ buy  ┆ 2024-03-15 09:30:00 │
│ AAPL ┆ 183.1 ┆ 250  ┆ sell ┆ 2024-03-15 09:31:15 │
│ MSFT ┆ 415.2 ┆ 80   ┆ buy  ┆ 2024-03-15 09:32:40 │
│ MSFT ┆ 416.0 ┆ 300  ┆ buy  ┆ 2024-03-15 09:45:05 │
│ GOOG ┆ 140.5 ┆ 150  ┆ sell ┆ 2024-03-15 10:01:30 │
│ GOOG ┆ 141.2 ┆ 90   ┆ buy  ┆ 2024-03-15 10:02:50 │
│ AAPL ┆ 184.0 ┆ 500  ┆ sell ┆ 2024-03-15 10:15:00 │
│ MSFT ┆ 414.8 ┆ 200  ┆ sell ┆ 2024-03-15 10:20:35 │
└──────┴───────┴──────┴──────┴─────────────────────┘
```

Eight trades across three symbols, one March morning. There was no `select *`
to write, because a bare table name is already a complete statement in qpl.
The `shape:` line and the type row under each column header come from Polars,
which does the printing.

That's the whole dataset for this chapter, so it's worth a few seconds
getting familiar with it: `sym` is the ticker, `price` and `size` describe
the trade, `side` is buy or sell, and `ts` is when it happened.

## Narrowing it down

Now ask something of it:

```qpl
qpl) select sym, price from trades where price > 200
```

```
shape: (3, 2)
┌──────┬───────┐
│ sym  ┆ price │
│ ---  ┆ ---   │
│ str  ┆ f64   │
╞══════╪═══════╡
│ MSFT ┆ 415.2 │
│ MSFT ┆ 416.0 │
│ MSFT ┆ 414.8 │
└──────┴───────┘
```

If you know SQL, there is nothing to learn here beyond the missing commas
around the clause names: you name the columns you want, say where they come
from, and filter. Only the three MSFT trades clear \$200.

## Grouping

The differences start to show once you aggregate:

```qpl
qpl) select avg price by sym from trades
```

```
shape: (3, 2)
┌──────┬────────────┐
│ sym  ┆ price      │
│ ---  ┆ ---        │
│ str  ┆ f64        │
╞══════╪════════════╡
│ MSFT ┆ 415.333333 │
│ GOOG ┆ 140.85     │
│ AAPL ┆ 183.133333 │
└──────┴────────────┘
```

`by sym` groups, and `avg price` is what gets computed per group. Compare
that to SQL, where you would name `sym` in the select list, then name it
again in a `GROUP BY` clause at the bottom. Here you say it once. The groups
come back in whatever order the grouping produced them, which is why MSFT
leads; sorting is a separate thing you ask for when you want it.

## Keeping a result

Queries become much more useful once you can keep one. `:` binds a name:

```qpl
qpl) t: select from trades where size > 100
```

Nothing prints, because you asked to store a result rather than see one. The
name `t` now holds that table for the rest of the session, so you can look at
it whenever you like by typing it on its own:

```qpl
qpl) t
```

```
shape: (5, 5)
┌──────┬───────┬──────┬──────┬─────────────────────┐
│ sym  ┆ price ┆ size ┆ side ┆ ts                  │
│ ---  ┆ ---   ┆ ---  ┆ ---  ┆ ---                 │
│ str  ┆ f64   ┆ i64  ┆ str  ┆ datetime[ns]        │
╞══════╪═══════╪══════╪══════╪═════════════════════╡
│ AAPL ┆ 183.1 ┆ 250  ┆ sell ┆ 2024-03-15 09:31:15 │
│ MSFT ┆ 416.0 ┆ 300  ┆ buy  ┆ 2024-03-15 09:45:05 │
│ GOOG ┆ 140.5 ┆ 150  ┆ sell ┆ 2024-03-15 10:01:30 │
│ AAPL ┆ 184.0 ┆ 500  ┆ sell ┆ 2024-03-15 10:15:00 │
│ MSFT ┆ 414.8 ┆ 200  ┆ sell ┆ 2024-03-15 10:20:35 │
└──────┴───────┴──────┴──────┴─────────────────────┘
```

Notice that `select from trades` has no column list at all, which means every
column. That's the form you'll reach for constantly when filtering without
wanting to project.

## Getting it out again

Finally, write it somewhere:

```qpl
qpl) t sink "big.parquet"          / write it out
```

`sink` streams a table to a file, choosing the format from the extension.
There's no separate "materialise, then save" step to perform.

That trailing `/ write it out` is a comment. `/` comments out everything to
the end of the line, and it's used throughout this book to annotate examples.
It is also why division in qpl is written `%` instead of `/`, which is
covered when arithmetic comes up properly.

## That's the core loop

Filter, aggregate, name the result, write it out. Those four things account
for most of what anyone does with qpl, and you now have all of them.

What makes the language feel different in practice is what happens when you
chain that loop together across many small steps rather than writing one
large query, which is what the [next chapter](working-incrementally.md) is
about.

## Other ways to run it

The REPL isn't the only entry point:

```bash
qpl                 # REPL, with no tables loaded
qpl --load-demo     # REPL, with `trades` and `quotes` as above
qpl script.qpl      # run a script, print its output, exit
qpl -i script.qpl   # run a script, then stay in the REPL with its state
```

That last one is worth remembering. It's how you'd load your real tables from
a setup script and then explore interactively, rather than retyping the same
`load` lines at the start of every session.

Longer worked examples live in
[`examples/`](https://github.com/nicelgueta/qpl/tree/main/examples) in the
repository. Once you've read a few more chapters,
`qpl examples/lazy_join_pipeline.qpl` is a good one to come back to.

# Working incrementally

You now know enough to write a query, keep its result, and write that result
to a file. This chapter is about the habit those three things add up to,
because it's the one that shapes the rest of the language.

The idea is simply this: **a transformation is a sequence of statements, and
you build it one statement at a time.**

Every step binds a name, that binding sticks around for the rest of the
session, and so the next step starts from wherever the last one finished.
There's no growing query to re-run, no stack of CTEs, and no scrolling back
up to edit a thirty-line block and resubmit the whole thing. The loop is:
type a line, look at what came back, type the next line.

## Watching it happen

Start from the demo `trades` table and narrow it down:

```qpl
qpl) t: select sym, side, price, size from trades where price > 0
```

Derive a column from what's left. `update` is the sibling of `select` that
adds or replaces columns while keeping the rest of the table intact; the
[next chapter](language/select-update-delete.md) covers it properly:

```qpl
qpl) t: update notional: price * size from t
```

Now for a step you're less sure about. Suppose you want to tag each trade as
large, mid or small. You could write it, assign it, look at the result, and
undo it if it's wrong. Much easier is to run it **without assigning**, which
prints the result and changes nothing:

```qpl
qpl) update band: ?[size >= 250; `large; size >= 100; `mid; `small] from t
```

```
shape: (8, 6)
┌──────┬──────┬───────┬──────┬──────────┬───────┐
│ sym  ┆ side ┆ price ┆ size ┆ notional ┆ band  │
│ ---  ┆ ---  ┆ ---   ┆ ---  ┆ ---      ┆ ---   │
│ str  ┆ str  ┆ f64   ┆ i64  ┆ f64      ┆ str   │
╞══════╪══════╪═══════╪══════╪══════════╪═══════╡
│ AAPL ┆ buy  ┆ 182.3 ┆ 100  ┆ 18230.0  ┆ mid   │
│ AAPL ┆ sell ┆ 183.1 ┆ 250  ┆ 45775.0  ┆ large │
│ MSFT ┆ buy  ┆ 415.2 ┆ 80   ┆ 33216.0  ┆ small │
│ MSFT ┆ buy  ┆ 416.0 ┆ 300  ┆ 124800.0 ┆ large │
│ GOOG ┆ sell ┆ 140.5 ┆ 150  ┆ 21075.0  ┆ mid   │
│ GOOG ┆ buy  ┆ 141.2 ┆ 90   ┆ 12708.0  ┆ small │
│ AAPL ┆ sell ┆ 184.0 ┆ 500  ┆ 92000.0  ┆ large │
│ MSFT ┆ sell ┆ 414.8 ┆ 200  ┆ 82960.0  ┆ mid   │
└──────┴──────┴───────┴──────┴──────────┴───────┘
```

(Two pieces of that line are getting ahead of the book. The `?[...]` is a
vectorised conditional, qpl's answer to `CASE WHEN`; read it as a list of
condition-then-value pairs with a fallback on the end, and see
[Expressions](language/expressions.md) for the real treatment. The
backticked words are *symbols*, a kind of value used for short labels like
these, covered in [Symbols](language/symbols.md).)

`t` is untouched, because you never assigned to it. The boundaries look
right, so commit to it by running the same line with a binding on the front:

```qpl
qpl) t: update band: ?[size >= 250; `large; size >= 100; `mid; `small] from t
```

And then aggregate the finished thing, sorting the result with `order` as you
go:

```qpl
qpl) select traded: sum notional by sym, band from t order traded desc
```

```
shape: (7, 3)
┌──────┬───────┬──────────┐
│ sym  ┆ band  ┆ traded   │
│ ---  ┆ ---   ┆ ---      │
│ str  ┆ str   ┆ f64      │
╞══════╪═══════╪══════════╡
│ AAPL ┆ large ┆ 137775.0 │
│ MSFT ┆ large ┆ 124800.0 │
│ MSFT ┆ mid   ┆ 82960.0  │
│ MSFT ┆ small ┆ 33216.0  │
│ GOOG ┆ mid   ┆ 21075.0  │
│ AAPL ┆ mid   ┆ 18230.0  │
│ GOOG ┆ small ┆ 12708.0  │
└──────┴───────┴──────────┘
```

Five statements, each one short enough to read at a glance, each one checked
before the next was written.

## Why bother

Consider what the same exercise looks like in SQL. The tagging step is buried
in a `CASE` expression inside a select list, inside a subquery, inside the
query you actually wanted. To check that one step you re-run everything. To
isolate it you comment out the parts around it, run it, then uncomment them
again. The query only ever exists in one enormous piece, and every inspection
costs a full round trip.

Here the pipeline exists as its history. Each intermediate is a name you can
go back and look at.

## What makes the loop tight

A handful of small things, several of which are covered later but are worth
knowing exist:

- **Type a name to see it.** `t` prints the table. No ceremony.
- **`cols t` shows just the schema**, which is much more useful than the data
  when a table is wide or long. See
  [Reading & writing files](language/files.md).
- **Run a statement without assigning it.** You see the result and nothing
  changes, which makes trying a step essentially free. Assign only once
  you're happy. This is the single most useful habit on the list.
- **`\d <stmt>` shows the compiled instructions** without running anything,
  for when a statement isn't doing what you expected. See [REPL](repl.md).
- **`lazy` binds a plan instead of a table**, so that none of the
  intermediate steps actually execute until you ask for the result at the
  end. On a large input this is the difference between paying for eight
  passes over the data and paying for one. See
  [lazy / collect](language/lazy-collect.md), and note that the loop you just
  learned is exactly the same either way.
- **Ctrl+Enter in the [VSCode extension](https://github.com/nicelgueta/qpl/tree/main/tools/vscode)**
  sends the current line or selection to a live session, which turns a `.qpl`
  file into something close to a notebook.

Keep this chapter in mind as you read on. Almost everything that follows is a
single statement in isolation, but they're all designed to be strung together
like the five above.

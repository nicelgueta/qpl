# lazy / collect

Everything so far has run the moment you pressed return. For interactive work
on a table that fits in memory, that's exactly right. It stops being right
when the input is enormous.

Consider the incremental workflow from
[Working incrementally](../working-incrementally.md): five statements, each
reading the result of the last. Run eagerly against a hundred-gigabyte
parquet file, that's five full passes over the data, four intermediate
results held in memory, and a long wait between each line you type.

`lazy` fixes this. Put it at the front of a table expression and qpl stores
the **plan** for producing that table instead of the table itself.

```qpl
t: lazy load "trades.parquet"
q: lazy select sym, bid, ask from load "quotes.parquet"
```

Neither statement has read a single byte. `t` and `q` are recipes.

## Building up a plan

The way you extend a plan is the way you'd extend anything else: assign the
name again. Each statement adds to the recipe rather than executing it.

```qpl
t: select sym, side, price, size from t where size > 100
t: update notional: price * size from t
t: update band: ?[notional > 50000; `big; `small] from t
```

Three more statements, still no file access, all effectively instantaneous.
The workflow you learned earlier is completely unchanged; the only difference
is the word `lazy` several lines further up.

Laziness is contagious, which is what makes that work. Querying a lazy
binding gives you another lazy binding, so the REPL shows the plan rather
than a table:

```qpl
qpl) l: lazy select sym, price from trades where size > 100
qpl) l
```

```
simple π 2/2 ["sym", "price"]
  FILTER col("size") > 100
  FROM
    DF ["sym", "price", "size", "side", ...]; PROJECT["sym", "price", "size"] 3/5 COLUMNS
```

That's Polars' own description of what it intends to do, read from the bottom
up: start from the frame, take only the three columns needed, filter, then
project the two you asked for. Notice that it plans to read three columns
rather than all five, and that it pushed the filter down next to the source.
Nobody asked for either optimisation. This is the real benefit of deferring
work: by the time anything executes, Polars can see the whole pipeline at
once and rearrange it.

Everything composes lazily. Joins, `by` aggregation, `order`, `distinct` and
`limit` all simply add to the plan:

```qpl
j: select sym, side, price, size, bid, ask from t `sym lj q `sym
j: select traded: sum notional, n: count price by sym, side from j
```

## Making it happen

Two things force the plan to run, and which you want depends on where the
result is going.

`collect` executes it and binds an ordinary in-memory table, for when you
want to carry on working with the result interactively:

```qpl
tm: collect j
```

`sink` executes it and streams the output straight to a file, without ever
assembling the whole result in memory:

```qpl
j sink "summary.parquet"
```

`sink` is the one that makes larger-than-memory work possible. The entire
pipeline — read, filter, derive, join, aggregate, write — becomes a single
streaming pass, and the peak memory is whatever the individual stages need
rather than the size of the data.

## When to use which

Use eager evaluation while exploring something small, because seeing results
immediately is the whole point of a REPL. Reach for `lazy` when the input is
large, when the pipeline has several steps, or when the output is headed for
a file rather than your screen. The syntax cost is one word, and you can
develop a pipeline eagerly against a sample and then add `lazy` to run it
over the full dataset.

[examples/lazy_join_pipeline.qpl](https://github.com/nicelgueta/qpl/blob/main/examples/lazy_join_pipeline.qpl)
is a complete example: two inputs, a join, an aggregation, and a sink to
parquet, with nothing ever collected along the way.

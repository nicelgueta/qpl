# lazy / collect

`lazy` as the first token of a table expression stores the **query plan** under a
name instead of running it. Nothing touches disk until you `collect` (materialise
to a DataFrame) or `sink` (stream to a file) — so a whole pipeline can process
**larger-than-RAM** data in a single pass.

```qpl
t: lazy load "trades.parquet"
q: lazy select sym, bid, ask from load "quotes.parquet"
```

Extend a plan by **re-assigning the binding** — each step just adds plan nodes,
the file is never touched:

```qpl
t: select sym, side, price, size from t where size > 100
t: update notional: price * size from t
t: update band: ?[notional > 50000; `big; `small] from t
```

Reading a lazy binding is contagious — you get another plan, and the REPL prints
it instead of a table:

```qpl
select from t
/ SELECT [col("sym"), col("side"), col("price"), col("size"), ...]
/   Parquet SCAN [trades.parquet]
/   SELECTION: col("size") > 100
```

Joins, `by` aggregation, `order`, `distinct` and `limit` all compose lazily:

```qpl
j: select sym, side, price, size, bid, ask from t `sym lj q `sym
j: select traded: sum notional, n: count price by sym, side from j
```

`collect` runs the plan once and binds a normal table; or skip the table and
`sink` the plan straight to disk:

```qpl
tm: collect j
j sink "summary.parquet"
```

[`examples/lazy_join_pipeline.qpl`](https://github.com/nicelgueta/qpl/blob/main/examples/lazy_join_pipeline.qpl) is a
two-input join + aggregate pipeline sunk to parquet without ever being collected.

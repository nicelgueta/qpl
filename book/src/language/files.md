# Reading & writing files

`load` reads a parquet or CSV file, taking a string path. On its own it is
**eager** — `t: load ...` materialises a table straight away. Prefix it with
[`lazy`](lazy-collect.md) to keep it as a deferred scan instead.

```qpl
select avg price by sym from load "data/trades.parquet"
t: load "data/trades.parquet"        / eager — reads the file now, binds a table
t: lazy load "data/trades.parquet"   / deferred — binds a plan, no IO yet
select from load "data/quotes.csv"
```

`sink` streams a **table expression** to a file, also taking a string path — a
table name, a `select ...`, an `update ...`:

```qpl
trades sink "summary.parquet"
select sym, price from trades where size > 100 sink "big_trades.parquet"
```

`cols` shows a table's schema (works on lazy bindings too):

```qpl
cols trades
```

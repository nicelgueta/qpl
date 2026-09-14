# select / update / delete

This is the heart of the language. Three statements share one grammar:

```
<select|update|delete> <cols> from <table-expr> [by <keys>] [where <preds>] [order <col> <asc|desc>, ...]
```

and they differ only in what they do with the result. `select` projects, so
you get back the columns you asked for and nothing else. `update` keeps the
whole table and adds or replaces columns in it. `delete` takes things away,
either rows or columns.

Learn the clauses once and they work the same way in all three.

## select

The minimal form asks for everything:

```qpl
qpl) select from trades
```

That's the same eight rows you saw in the Quickstart. There's no column list,
which means every column. You'll use this constantly, because it's the form
you want whenever you're filtering rather than projecting.

Name columns to narrow it:

```qpl
qpl) select sym, price from trades
```

And rename them as you go, using the same `:` that binds names elsewhere:

```qpl
qpl) select px: price, qty: size from trades
```

```
shape: (8, 2)
┌───────┬─────┐
│ px    ┆ qty │
│ ---   ┆ --- │
│ f64   ┆ i64 │
╞═══════╪═════╡
│ 182.3 ┆ 100 │
│ 183.1 ┆ 250 │
│ 415.2 ┆ 80  │
│ 416.0 ┆ 300 │
│ 140.5 ┆ 150 │
│ 141.2 ┆ 90  │
│ 184.0 ┆ 500 │
│ 414.8 ┆ 200 │
└───────┴─────┘
```

`px: price` names the output column. It's the same operator as an assignment,
doing the same conceptual job, one level down.

### Filtering

`where` takes one or more predicates. Separate them with commas and they're
combined with AND:

```qpl
qpl) select from trades where size > 100, price < 400
```

```
shape: (3, 5)
┌──────┬───────┬──────┬──────┬─────────────────────┐
│ sym  ┆ price ┆ size ┆ side ┆ ts                  │
│ ---  ┆ ---   ┆ ---  ┆ ---  ┆ ---                 │
│ str  ┆ f64   ┆ i64  ┆ str  ┆ datetime[ns]        │
╞══════╪═══════╪══════╪══════╪═════════════════════╡
│ AAPL ┆ 183.1 ┆ 250  ┆ sell ┆ 2024-03-15 09:31:15 │
│ GOOG ┆ 140.5 ┆ 150  ┆ sell ┆ 2024-03-15 10:01:30 │
│ AAPL ┆ 184.0 ┆ 500  ┆ sell ┆ 2024-03-15 10:15:00 │
└──────┴───────┴──────┴──────┴─────────────────────┘
```

Three trades are both larger than 100 and cheaper than \$400.

When you need OR, or want to be explicit about grouping, `&` and `|` are the
logical operators:

```qpl
qpl) select from trades where (size > 400) | (side = "buy")
```

A `where` clause doesn't have to be built from comparisons at all. Anything
producing a boolean per row will do, including a literal bit vector, which is
occasionally useful for hand-picking rows:

```qpl
qpl) select from trades where 10100000b
```

That's q's notation for a run of booleans, and it selects the first and third
rows. [Expressions](expressions.md) covers the full predicate vocabulary,
including pattern matching on text.

### Grouping

`by` groups, and the aggregate goes where a plain column name would:

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

Read it out loud and it's the statement: select the average price, by sym,
from trades. The grouping key appears once, not twice as SQL requires.

Aggregates combine with everything else, and naming the output is usually
worth it:

```qpl
qpl) select total: sum size by sym from trades where side = "sell"
```

```
shape: (3, 2)
┌──────┬───────┐
│ sym  ┆ total │
│ ---  ┆ ---   │
│ str  ┆ i64   │
╞══════╪═══════╡
│ GOOG ┆ 150   │
│ AAPL ┆ 750   │
│ MSFT ┆ 200   │
└──────┴───────┘
```

`avg` and `sum` are two of about twenty-five aggregates; the full list is in
[Expressions](expressions.md).

### Sorting

`order` takes sort keys with a direction each, comma separated:

```qpl
qpl) select sym, price from trades order sym asc, price desc
```

```
shape: (8, 2)
┌──────┬───────┐
│ sym  ┆ price │
│ ---  ┆ ---   │
│ str  ┆ f64   │
╞══════╪═══════╡
│ AAPL ┆ 184.0 │
│ AAPL ┆ 183.1 │
│ AAPL ┆ 182.3 │
│ GOOG ┆ 141.2 │
│ GOOG ┆ 140.5 │
│ MSFT ┆ 416.0 │
│ MSFT ┆ 415.2 │
│ MSFT ┆ 414.8 │
└──────┴───────┘
```

## `from` takes any table expression

Here's the part that makes the language compose rather than merely query.
`from` doesn't require a table name. It accepts anything that evaluates to a
table, and a `select` evaluates to a table, so queries nest with no special
subquery syntax:

```qpl
qpl) select from select price from trades where price > 100
```

The same is true in the other direction: anything that takes a table takes a
query. You'll see this repeatedly in later chapters, where operators like
`distinct` and `cols` are introduced and simply work on whatever you hand
them.

This is also how you query a file directly, using `load` — the
[next chapter](files.md) is about reading and writing files, but the shape is
worth seeing here, since it's just another table expression in the `from`
position:

```qpl
select sym, price from load "data/trades.parquet"
```

## Joining

A join names the column to match on either side, with the join type in the
middle. `lj` is a left join, `ij` an inner join, `rj` a right join. The column
names are symbols, from the [previous chapter](symbols.md):

```qpl
qpl) select sym, price, bid, ask from trades `sym lj quotes `sym
```

```
shape: (14, 4)
┌──────┬───────┬───────┬───────┐
│ sym  ┆ price ┆ bid   ┆ ask   │
│ ---  ┆ ---   ┆ ---   ┆ ---   │
│ str  ┆ f64   ┆ f64   ┆ f64   │
╞══════╪═══════╪═══════╪═══════╡
│ AAPL ┆ 182.3 ┆ 182.0 ┆ 182.5 │
│ AAPL ┆ 182.3 ┆ 183.8 ┆ 184.2 │
│ AAPL ┆ 183.1 ┆ 182.0 ┆ 182.5 │
│ AAPL ┆ 183.1 ┆ 183.8 ┆ 184.2 │
│ MSFT ┆ 415.2 ┆ 415.0 ┆ 415.5 │
│ …    ┆ …     ┆ …     ┆ …     │
│ GOOG ┆ 141.2 ┆ 140.3 ┆ 140.8 │
│ AAPL ┆ 184.0 ┆ 182.0 ┆ 182.5 │
│ AAPL ┆ 184.0 ┆ 183.8 ┆ 184.2 │
│ MSFT ┆ 414.8 ┆ 415.0 ┆ 415.5 │
│ MSFT ┆ 414.8 ┆ 414.5 ┆ 415.0 │
└──────┴───────┴───────┴───────┘
```

Eight trades went in and fourteen rows came out, which is worth pausing on
because it's a property of joins rather than anything qpl is doing. `quotes`
holds two AAPL rows and two MSFT rows, so every AAPL trade matches both AAPL
quotes and appears twice. This is ordinary join fan-out, and the reason the
`shape:` line is the first thing to look at after any join.

By default the right-hand side is a bare table name or a `load`. To join
against anything more elaborate, wrap it in parentheses:

```qpl
select price, bid from trades `sym lj (select sym, bid from quotes where bid > 0) `sym
```

The parentheses aren't decoration. Without them the parser has no way to tell
where the inner query stops, and the inner query's own join would swallow the
`` `sym `` that belongs to the outer one. You hit this once, add the
parentheses, and never think about it again.

## update

`update` returns the whole table with columns added or replaced. Everything
you don't mention is left alone:

```qpl
qpl) update notional: price * size from trades
```

```
shape: (8, 6)
┌──────┬───────┬──────┬──────┬─────────────────────┬──────────┐
│ sym  ┆ price ┆ size ┆ side ┆ ts                  ┆ notional │
│ ---  ┆ ---   ┆ ---  ┆ ---  ┆ ---                 ┆ ---      │
│ str  ┆ f64   ┆ i64  ┆ str  ┆ datetime[ns]        ┆ f64      │
╞══════╪═══════╪══════╪══════╪═════════════════════╪══════════╡
│ AAPL ┆ 182.3 ┆ 100  ┆ buy  ┆ 2024-03-15 09:30:00 ┆ 18230.0  │
│ AAPL ┆ 183.1 ┆ 250  ┆ sell ┆ 2024-03-15 09:31:15 ┆ 45775.0  │
│ MSFT ┆ 415.2 ┆ 80   ┆ buy  ┆ 2024-03-15 09:32:40 ┆ 33216.0  │
│ MSFT ┆ 416.0 ┆ 300  ┆ buy  ┆ 2024-03-15 09:45:05 ┆ 124800.0 │
│ GOOG ┆ 140.5 ┆ 150  ┆ sell ┆ 2024-03-15 10:01:30 ┆ 21075.0  │
│ GOOG ┆ 141.2 ┆ 90   ┆ buy  ┆ 2024-03-15 10:02:50 ┆ 12708.0  │
│ AAPL ┆ 184.0 ┆ 500  ┆ sell ┆ 2024-03-15 10:15:00 ┆ 92000.0  │
│ MSFT ┆ 414.8 ┆ 200  ┆ sell ┆ 2024-03-15 10:20:35 ┆ 82960.0  │
└──────┴───────┴──────┴──────┴─────────────────────┴──────────┘
```

Use an existing column name and you replace it instead of adding one:

```qpl
update price: price * 2 from trades
```

`by` works here too, which gives you "compute this per group and apply it to
the rows of that group":

```qpl
update price: price * 2 by sym from trades where size > 100
```

That example also shows `where` on an `update`, which behaves differently
from `where` on a `select` and catches people out. A `select` drops rows that
don't match. An `update` never drops anything: rows that don't match simply
keep the value they had. If the column is brand new, unmatched rows have no
previous value to keep, so they come out `null`.

## delete

`delete` removes rows when given a `where`. Note that the predicate describes
what goes *away*, so the two small trades under 100 shares are the ones
missing from the result:

```qpl
qpl) delete from trades where size < 100
```

```
shape: (6, 5)
┌──────┬───────┬──────┬──────┬─────────────────────┐
│ sym  ┆ price ┆ size ┆ side ┆ ts                  │
│ ---  ┆ ---   ┆ ---  ┆ ---  ┆ ---                 │
│ str  ┆ f64   ┆ i64  ┆ str  ┆ datetime[ns]        │
╞══════╪═══════╪══════╪══════╪═════════════════════╡
│ AAPL ┆ 182.3 ┆ 100  ┆ buy  ┆ 2024-03-15 09:30:00 │
│ AAPL ┆ 183.1 ┆ 250  ┆ sell ┆ 2024-03-15 09:31:15 │
│ MSFT ┆ 416.0 ┆ 300  ┆ buy  ┆ 2024-03-15 09:45:05 │
│ GOOG ┆ 140.5 ┆ 150  ┆ sell ┆ 2024-03-15 10:01:30 │
│ AAPL ┆ 184.0 ┆ 500  ┆ sell ┆ 2024-03-15 10:15:00 │
│ MSFT ┆ 414.8 ┆ 200  ┆ sell ┆ 2024-03-15 10:20:35 │
└──────┴───────┴──────┴──────┴─────────────────────┘
```

And it removes columns when given a list of symbols instead:

```qpl
qpl) delete `ts`side from trades
```

```
shape: (8, 3)
┌──────┬───────┬──────┐
│ sym  ┆ price ┆ size │
│ ---  ┆ ---   ┆ ---  │
│ str  ┆ f64   ┆ i64  │
╞══════╪═══════╪══════╡
│ AAPL ┆ 182.3 ┆ 100  │
│ AAPL ┆ 183.1 ┆ 250  │
│ MSFT ┆ 415.2 ┆ 80   │
│ MSFT ┆ 416.0 ┆ 300  │
│ GOOG ┆ 140.5 ┆ 150  │
│ GOOG ┆ 141.2 ┆ 90   │
│ AAPL ┆ 184.0 ┆ 500  │
│ MSFT ┆ 414.8 ┆ 200  │
└──────┴───────┴──────┘
```

One statement does one or the other, never both, since removing rows and
removing columns are different enough operations that combining them would
only be confusing.

## Where this is heading

You can now express most of what a day's work needs. The chapters that follow
fill in the pieces these statements are built from: how to get data in and
out of files, what can go inside a `where` or a projection, and how to pull
individual values out of a table rather than always getting a table back.

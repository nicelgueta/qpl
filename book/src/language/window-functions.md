# Window functions

There are three things you might want from an aggregate, and so far you've
seen two of them. `max trades`price` reduces the whole column to one value.
`select max price by sym` reduces each group to one row. The third is to
compute something per group and then give the answer back to *every row* in
that group, leaving the table its original height.

That's what `over` does, and it corresponds to SQL's
`<expr> OVER (PARTITION BY ...)`.

```qpl
qpl) select sym, price, top: max price over `sym from trades
```

```
shape: (8, 3)
┌──────┬───────┬───────┐
│ sym  ┆ price ┆ top   │
│ ---  ┆ ---   ┆ ---   │
│ str  ┆ f64   ┆ f64   │
╞══════╪═══════╪═══════╡
│ AAPL ┆ 182.3 ┆ 184.0 │
│ AAPL ┆ 183.1 ┆ 184.0 │
│ MSFT ┆ 415.2 ┆ 416.0 │
│ MSFT ┆ 416.0 ┆ 416.0 │
│ GOOG ┆ 140.5 ┆ 141.2 │
│ GOOG ┆ 141.2 ┆ 141.2 │
│ AAPL ┆ 184.0 ┆ 184.0 │
│ MSFT ┆ 414.8 ┆ 416.0 │
└──────┴───────┴───────┘
```

Still eight rows, but each now knows the highest price its symbol reached.
Compare that to `select max price by sym`, which would have given three rows
and thrown away everything else.

The partition keys are symbols, one or several: `` over `sym `` or
`` over `sym`day ``.

Having the group's answer on every row is what makes comparisons against it
possible:

```qpl
select sym, gap: (max price over `sym) - price from trades
```

The parentheses there are load-bearing. `over` binds more tightly than `-`,
so without them the expression would be read as partitioning by
`` (`sym) - price ``, which is nonsense.

## Ordering within a partition

Some questions need the rows of a partition to be in a defined order before
they can be answered at all. "The previous price" and "the running total"
both presuppose a sequence.

An `order` sub-clause supplies one. It takes space-separated pairs of a
column and a direction:

```qpl
qpl) select sym, ts, price, run: cumsum price over `sym order `ts asc from trades
```

```
shape: (8, 4)
┌──────┬─────────────────────┬───────┬────────┐
│ sym  ┆ ts                  ┆ price ┆ run    │
│ ---  ┆ ---                 ┆ ---   ┆ ---    │
│ str  ┆ datetime[ns]        ┆ f64   ┆ f64    │
╞══════╪═════════════════════╪═══════╪════════╡
│ AAPL ┆ 2024-03-15 09:30:00 ┆ 182.3 ┆ 182.3  │
│ AAPL ┆ 2024-03-15 09:31:15 ┆ 183.1 ┆ 365.4  │
│ MSFT ┆ 2024-03-15 09:32:40 ┆ 415.2 ┆ 415.2  │
│ MSFT ┆ 2024-03-15 09:45:05 ┆ 416.0 ┆ 831.2  │
│ GOOG ┆ 2024-03-15 10:01:30 ┆ 140.5 ┆ 140.5  │
│ GOOG ┆ 2024-03-15 10:02:50 ┆ 141.2 ┆ 281.7  │
│ AAPL ┆ 2024-03-15 10:15:00 ┆ 184.0 ┆ 549.4  │
│ MSFT ┆ 2024-03-15 10:20:35 ┆ 414.8 ┆ 1246.0 │
└──────┴─────────────────────┴───────┴────────┘
```

Each symbol accumulates its own running total in time order. AAPL's third
trade continues from where its second left off, at 549.4, rather than from
anything MSFT did in between.

This is where the cumulative and row-relative verbs from
[Expressions](expressions.md) earn their place. `cumsum`, `lag`, `diff`,
`ffill` and the rest are all far more useful with an explicit ordering than
without one:

```qpl
select sym, ts, price,
    prev: 1 lag price over `sym order `ts asc
    from trades
```

For a plain aggregate like these, one direction applies to all the ordering
keys.

## Ranking

Ordering also unlocks three verbs that answer "where does this row come in
its group". They stand on their own in place of an expression, and they
*require* an `order`, since a ranking without a sequence is meaningless.

```qpl
qpl) select sym, price, r: rank over `sym order `price desc from trades
```

```
shape: (8, 3)
┌──────┬───────┬─────┐
│ sym  ┆ price ┆ r   │
│ ---  ┆ ---   ┆ --- │
│ str  ┆ f64   ┆ i64 │
╞══════╪═══════╪═════╡
│ AAPL ┆ 182.3 ┆ 3   │
│ AAPL ┆ 183.1 ┆ 2   │
│ MSFT ┆ 415.2 ┆ 2   │
│ MSFT ┆ 416.0 ┆ 1   │
│ GOOG ┆ 140.5 ┆ 2   │
│ GOOG ┆ 141.2 ┆ 1   │
│ AAPL ┆ 184.0 ┆ 1   │
│ MSFT ┆ 414.8 ┆ 3   │
└──────┴───────┴─────┘
```

Each symbol's most expensive trade is ranked 1. The three verbs differ only
in how they handle ties:

| Verb | SQL equivalent | On a tie |
|---|---|---|
| `rn` | `row_number()` | never ties; row order breaks it, giving a strict `1..n` |
| `rank` | `rank()` | tied rows share the lower rank, then a gap: `1, 1, 3` |
| `drank` | `dense_rank()` | tied rows share a rank, no gap: `1, 1, 2` |

Unlike the plain aggregates, ranking verbs accept a different direction per
key, since ranking by one column ascending and another descending is a
perfectly sensible request:

```qpl
select emp, country, role,
    seat: rank over `country`role order `desk asc `date desc,
    seniority: rn over `country order `hired asc
    from staff
```

## Rolling windows

Adding `rolling <n>` turns an aggregate into a fixed-width window over the
ordered partition, which is how you'd compute a moving average:

```qpl
qpl) select sym, price, ma2: avg price over `sym order `ts asc rolling 2 from trades
```

```
shape: (8, 3)
┌──────┬───────┬────────┐
│ sym  ┆ price ┆ ma2    │
│ ---  ┆ ---   ┆ ---    │
│ str  ┆ f64   ┆ f64    │
╞══════╪═══════╪════════╡
│ AAPL ┆ 182.3 ┆ null   │
│ AAPL ┆ 183.1 ┆ 182.7  │
│ MSFT ┆ 415.2 ┆ null   │
│ MSFT ┆ 416.0 ┆ 415.6  │
│ GOOG ┆ 140.5 ┆ null   │
│ GOOG ┆ 141.2 ┆ 140.85 │
│ AAPL ┆ 184.0 ┆ 183.55 │
│ MSFT ┆ 414.8 ┆ 415.4  │
└──────┴───────┴────────┘
```

The first row of each symbol is `null`, because a two-row window needs two
rows and only one has been seen. In general the first `n-1` rows of every
partition are null. That's correct rather than a problem, but it is something
to plan for downstream, either by filtering those rows out or by accepting
nulls in the result.

Rolling works with `sum`, `avg`, `min`, `max`, `std`, `var` and `median`.

## The row number column

One last piece fits naturally here. `i` is a virtual column holding the row
index, available in any query without existing in the table. Following q, it
comes out named `x`:

```qpl
qpl) select i, sym from trades
```

```
shape: (8, 2)
┌─────┬──────┐
│ x   ┆ sym  │
│ --- ┆ ---  │
│ u32 ┆ str  │
╞═════╪══════╡
│ 0   ┆ AAPL │
│ 1   ┆ AAPL │
│ 2   ┆ MSFT │
│ 3   ┆ MSFT │
│ 4   ┆ GOOG │
│ 5   ┆ GOOG │
│ 6   ┆ AAPL │
│ 7   ┆ MSFT │
└─────┴──────┘
```

It also works in a predicate, where `where i < 5` is a crude "first five
rows". For that specific job `5#trades` from
[Table operators](table-operators.md) is clearer; `i` comes into its own when
you need the index itself as a value.

# Table operators

Alongside the three query statements there are a few operators that act on a
whole table directly, with no `from` clause. They're terse because they're
used constantly, and each one takes a table expression, so they compose with
queries and with each other.

## distinct

Unique rows:

```qpl
qpl) distinct select sym from trades
```

```
shape: (3, 1)
┌──────┐
│ sym  │
│ ---  │
│ str  │
╞══════╡
│ AAPL │
│ MSFT │
│ GOOG │
└──────┘
```

Eight rows in, three out. Note that `distinct` applies to whole rows, so what
it does depends on which columns you selected first.

Inside a select list, `distinct` is something else: the column verb that counts
distinct values, an alias for `n_unique`. See [Expressions](expressions.md).

## Limiting

Taking the first few rows has two spellings that do the same thing. `limit`
reads as prose; `#` is the q-style shorthand:

```qpl
qpl) 3#trades
```

```
shape: (3, 5)
┌──────┬───────┬──────┬──────┬─────────────────────┐
│ sym  ┆ price ┆ size ┆ side ┆ ts                  │
│ ---  ┆ ---   ┆ ---  ┆ ---  ┆ ---                 │
│ str  ┆ f64   ┆ i64  ┆ str  ┆ datetime[ns]        │
╞══════╪═══════╪══════╪══════╪═════════════════════╡
│ AAPL ┆ 182.3 ┆ 100  ┆ buy  ┆ 2024-03-15 09:30:00 │
│ AAPL ┆ 183.1 ┆ 250  ┆ sell ┆ 2024-03-15 09:31:15 │
│ MSFT ┆ 415.2 ┆ 80   ┆ buy  ┆ 2024-03-15 09:32:40 │
└──────┴───────┴──────┴──────┴─────────────────────┘
```

```qpl
10 limit select from trades      / identical to 10#select from trades
```

This is the same `#` that sliced a list in
[Column expressions & lists](column-expressions.md). Whether you get a slice
or a limit depends only on whether the thing on its right is a list or a
table, and in both cases it means "the first n".

A negative count takes from the end instead — the last `n` rows — same as `#`
on a list:

```qpl
qpl) -3 limit trades      / identical to -3#trades
```

## Dropping columns

Also two spellings, `drop` and `_`, taking a list of column names as symbols:

```qpl
`price`size drop select from trades
`price`size _ trades                  / the same thing
```

This overlaps with `delete `price`size from trades` from the
[select chapter](select-update-delete.md). Use whichever reads better in
context; `_` is handy mid-pipeline, `delete` when the statement is already a
query.

## Dropping rows with nulls

`dropnull` is the row-wise counterpart of `drop`. It takes a list of column
names as symbols on the left and removes every row that has a null in any of
them:

```qpl
`price dropnull trades           / one column
`price`size dropnull trades      / a null in either drops the row
clean: `price dropnull trades
```

Nulls in columns you didn't name are ignored. To keep rows and replace the
nulls instead, use [`fill`](expressions.md#nulls).

## Sorting

Sorting uses the dict-literal form you met at the end of
[Column expressions & lists](column-expressions.md): a list of column names,
`!`, then a boolean per column giving its direction. `0` is ascending and `1`
is descending.

```qpl
qpl) `sym`price!01b select sym, price from trades
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

Read `` `sym`price!01b `` as: by `sym` ascending, then by `price` descending.
The bit vector lines up positionally with the symbol list.

That result is identical to `select sym, price from trades order sym asc,
price desc` from the select chapter, which raises the obvious question of why
both exist. `order` is part of a query and reads better inside one; the `!`
form is an operator that applies to any table, which makes it convenient when
you're sorting something you already have:

```qpl
sorted: `sym`price!01b select from trades where size > 100
```

## They compose

Since each of these takes a table expression and produces one, they stack:

```qpl
`price`size drop distinct select from trades
`price dropnull `size drop select from trades
```

Because qpl evaluates right to left, the reading order is the order things
happen: select from trades, take the distinct rows, then drop two columns.

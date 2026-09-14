# Categoricals & enums

[Symbols](symbols.md) introduced the backtick as a way of naming things, and
ended by promising that symbols do one more job. This is that job.

The problem is a familiar one. A column like `sym` in the demo data holds
text, and the same handful of values repeat over and over: three distinct
tickers across eight rows here, and perhaps a few hundred distinct values
across a hundred million rows in real data. Stored as plain strings, every
row carries its own copy of the text, and every comparison during a group-by,
a join or a sort is a string comparison.

Both problems go away if you store each distinct value once and give each row
a small integer pointing at it. Polars offers two flavours of that idea, and
qpl exposes both.

## Categoricals

Inside a table expression, `` `$ `` casts a column to a **categorical**: an
interned pool of strings, with the column itself holding integer codes.

```qpl
select country: `$country from t
```

Notice that this is the same `` `$ `` that interned a string into a symbol in
the earlier chapter. It's the same idea applied to a whole column rather than
a single value, which is why the spelling is shared.

The codes are `u32` by default. If you know the column has few distinct
values you can ask for a narrower code type, which costs less memory:

```qpl
select country: u8!`$country from t   / u8 codes, up to 256 distinct values
```

A categorical fixes the storage and comparison cost, but it doesn't give the
values any *order*. Sorting or comparing categorical values falls back to
comparing the underlying text, because as far as Polars knows, the pool is
just a bag of strings.

## Enums

An **enum** is the ordered version. You declare the values up front, in the
order you want them to have, and that order fixes both how they sort and
which code each one gets. The declaration is an ordinary symbol list:

```qpl
qpl) lvl: `low`mid`high
```

Then cast a column against it with `::`, naming the enum on the left:

```qpl
qpl) t: update band: lvl::`$?[size >= 250; `high; size >= 100; `mid; `low] from trades
qpl) select sym, size, band from t
```

```
shape: (8, 3)
┌──────┬──────┬──────┐
│ sym  ┆ size ┆ band │
│ ---  ┆ ---  ┆ ---  │
│ str  ┆ i64  ┆ enum │
╞══════╪══════╪══════╡
│ AAPL ┆ 100  ┆ mid  │
│ AAPL ┆ 250  ┆ high │
│ MSFT ┆ 80   ┆ low  │
│ MSFT ┆ 300  ┆ high │
│ GOOG ┆ 150  ┆ mid  │
│ GOOG ┆ 90   ┆ low  │
│ AAPL ┆ 500  ┆ high │
│ MSFT ┆ 200  ┆ mid  │
└──────┴──────┴──────┘
```

The `dtype` row reports `enum` rather than `str`. The `?[...]` in the middle
is just the conditional from [Expressions](expressions.md) producing symbols,
and `lvl::` converts its output into the declared enum.

Now the payoff. Because the enum knows that `low` comes before `mid` comes
before `high`, you can compare against a value and get the meaning you'd
expect:

```qpl
qpl) select sym, size, band from t where band >= `mid
```

```
shape: (6, 3)
┌──────┬──────┬──────┐
│ sym  ┆ size ┆ band │
│ ---  ┆ ---  ┆ ---  │
│ str  ┆ i64  ┆ enum │
╞══════╪══════╪══════╡
│ AAPL ┆ 100  ┆ mid  │
│ AAPL ┆ 250  ┆ high │
│ MSFT ┆ 300  ┆ high │
│ GOOG ┆ 150  ┆ mid  │
│ AAPL ┆ 500  ┆ high │
│ MSFT ┆ 200  ┆ mid  │
└──────┴──────┴──────┘
```

`band >= `mid`` means mid or high. Ask the same question of a plain string
column and you'd get alphabetical order, in which "high" sorts before "low"
and the query means nothing useful. Sorting behaves the same way, and because
the comparison happens on the integer codes rather than the text, it's also
faster than the string version it replaces.

## Practical notes

What you cast can be a string column, or an existing categorical or enum,
which Polars re-keys against the new ordering.

Any value that appears in the column but not in the enum declaration becomes
`null` rather than raising an error. This is the one thing to watch: it's
worth checking that your declared list genuinely covers every value the data
can contain, since a typo in the declaration shows up as a column of nulls
rather than as a complaint.

Choosing between the two is straightforward. If the values have a natural
order that you want to sort or compare by, declare an enum. If they're just
labels and you only care about grouping, joining and memory, a categorical is
less setup.

The [Polars user guide on categorical data and enums](https://docs.pola.rs/user-guide/expressions/categorical-data-and-enums)
goes deeper on the representation; qpl's `` `$ `` and `::` are thin syntax
over exactly that machinery.

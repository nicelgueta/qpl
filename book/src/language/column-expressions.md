# Column expressions & lists

Every query so far has returned a table, even when that table had a single
column and a single row. Often what you actually want is the value itself:
the highest price, the number of matching rows, the list of prices. This
chapter is about getting those out.

## Pulling a column out of a table

A **column expression** names one column of one table, without a surrounding
statement. There are two ways to write it, and they mean the same thing:

```qpl
trades`price                 / backtick the column off the table name
select price from trades     / a one-column select
```

The backtick form is the one you'll type day to day. What makes either of
them interesting is not the syntax but what happens when you *use* the
result. Typed on its own, a one-column select is still a query, so it prints
as a table. Put it somewhere a value belongs — bind it to a name, reduce it,
slice it — and it becomes a **list**:

```qpl
qpl) px: trades`price
qpl) px
```

```
f64[8]: 182.3 183.1 415.2 416 140.5 141.2 184 414.8
```

That's a list of eight floats, displayed with its type and length, rather
than a table with one column. Lists are a first-class kind of value in qpl,
and the rest of this chapter is about what you can do with them.

Under the hood a list is a Polars `Series`, and each atomic type has a
matching list type: `IntVec`, `FloatVec`, `StrVec`, `SymVec`, `BoolVec`, and
one per temporal type. A column whose type has no list equivalent, or which
contains nulls, will refuse to materialise rather than quietly losing
information. Use `fill` or `dropnull` first; see [Nulls](expressions.md#nulls).

A column expression can carry a `where` of its own, which filters the table's
rows before the column is extracted:

```qpl
trades`price where size > 100
```

## Reductions turn a list into a scalar

Apply an aggregate to a column expression and you get back a single value:

```qpl
qpl) top: max trades`price
qpl) top
```

```
f64: 416
```

The aggregates listed in [Expressions](expressions.md) all work here.
`count`, in particular, is how you answer "how many rows match":

```qpl
qpl) n: count select price from trades where size > 100
qpl) n
```

```
i64: 5
```

Now recall from [Assignment](assignment.md) that a scalar gets compiled into
later queries as a literal. That's what makes this genuinely useful, because
it means a value computed from the data can be fed straight back into a query
over that data:

```qpl
qpl) top: max trades`price
qpl) select sym, price from trades where price >= top
```

```
shape: (1, 2)
┌──────┬───────┐
│ sym  ┆ price │
│ ---  ┆ ---   │
│ str  ┆ f64   │
╞══════╪═══════╡
│ MSFT ┆ 416.0 │
└──────┴───────┘
```

In SQL that needs a correlated subquery. Here it's two statements, and the
first one is reusable — handy when several later queries need the same
`top`. When you don't need to reuse it, skip the intermediate name and write
the reduction straight into the `where`, wrapped in parentheses so qpl can
tell it's a single value and not a column reference:

```qpl
select sym, price from trades where price >= (max price)
```

Verbs that don't reduce — `cumsum`, `abs`, and the parameterised ones like
`2 round` — return another list of the same length instead.

## Taking part of a list

`n#` takes the first `n` elements, and a negative count takes from the end:

```qpl
qpl) 3#trades`price
```

```
f64[3]: 182.3 183.1 415.2
```

```qpl
-3#trades`price              / the last three
```

The same `#` applied to a whole table means "the first n rows" and gives back
a table, which is covered in [Table operators](table-operators.md). The rule
is just that a list in gives a list out, and a table in gives a table out.

## Indexing

Square brackets pick elements by position. One index gives a single value; a
run of indices gives a shorter list:

```qpl
qpl) l: 10 20 30 40 50
qpl) l[1 3 4]
```

```
i64[3]: 20 40 50
```

Notice that `10 20 30 40 50` is a list literal: numbers separated by spaces,
no commas or brackets, in the same spirit as the run-of-symbols form from
[Symbols](symbols.md).

Indexing works on anything that produces a list, not just a bound name:

```qpl
sub: trades`price[2 3]       / rows 2 and 3 of the price column
one: (trades`sym)[0]         / a single value: str "AAPL"
```

Following q, a parenthesised expression can also be indexed by just putting
the indices after it: `` (trades`price) 2 3 `` means the same as
`` (trades`price)[2 3] ``.

One limitation to note: the `where` in `trades\`price where size > 100` filters
the *table's* rows by any of its columns, before the one column is extracted.
Once a list is bound to a name, that particular form is gone — a bound list
has no other columns left to filter by, so `size` is no longer in scope. (A
list still has a `where` of its own after that point, just a different one —
filtering by the list's *own* values, covered later in this chapter — so
don't read "no longer available" as "lists can't be filtered.")

## Arithmetic on whole lists

Like in **array programming** languages: operators work on whole lists at once instead
of looping over elements, the same style kdb+/q and NumPy use. Because a list
is a Series, operators apply elementwise. A list against a scalar broadcasts
the scalar across every element:

```qpl
qpl) l: 10 20 30 40 50
qpl) l + 2
```

```
i64[5]: 12 22 32 42 52
```

Two lists of the same length combine pairwise, and a list of length one
broadcasts against a longer one, which is the same rule NumPy and Polars use.
Comparison works the same way and gives back a list of booleans.

A neat consequence is that rebasing a series to its first value needs no
special support:

```qpl
trades`price - trades`price[0]
```

The left side is a list of eight, the right side is a single value, so the
subtraction broadcasts.

## Filtering a list by its own values

Lists have a `where` of their own, in which `x` refers to the element being
tested:

```qpl
qpl) nums: 10 20 30 40 50
qpl) nums where x > 25
```

```
i64[3]: 30 40 50
```

Commas combine predicates with AND, as they do in a query:

```qpl
nums where x > 10, x < 50
```

This is a genuinely different operation from the `where` on a column
expression earlier in the chapter, even though it's the same word. That one
filters a *table's rows* using any column, before a single column is
extracted. This one filters a *list* using its own values. They're separate
rules in the grammar, so combining them needs parentheses to say which is
which:

```qpl
(trades`price where size>100) where x>400
```

Read that as: take the prices of trades bigger than 100 shares, then keep
only those above 400.

## Building lists and tables from nothing

A few constructors round the chapter off. `til` generates a range, either from
zero or between two bounds:

```qpl
qpl) til 5
```

```
i64[5]: 0 1 2 3 4
```

```qpl
10 til 15                       / i64[5]: 10 11 12 13 14
```

Strings can be written in a run too, separated by spaces. Wrap the run in
parentheses so it stays apart from whatever sits next to it:

```qpl
qpl) ("ab" "cd" "ef")
```

```
str[3]: "ab" "cd" "ef"
```

`enlist` makes a one-element list from any atom. A string counts as one atom,
so `enlist "a"` is a `str[1]`, not a list of characters:

```qpl
qpl) enlist 23
```

```
i64[1]: 23
```

`?` with a count on the left draws random values, **with replacement**. With
an integer on the right it gives ints from zero up to (not including) that
number. With a float it gives uniform floats in `[0, f)`. With a list it
picks elements from that list, which can be any list at all, including a
column:

```qpl
3?6                             / i64[3]: e.g. 2 5 4
5?2.5                           / f64[5], uniform in [0, 2.5)
2 ? 10 20 30 40                 / two picks from the list
4?trades`sym                    / four random tickers
```

Don't confuse the infix `n?x` with the prefix `?[..]` conditional from
[Expressions](expressions.md): the bracket straight after `?` is what makes
it a conditional.

And `zip` assembles a table from named lists of equal length:

```qpl
a: til 20
b: 2 * til 20
tbl: zip `cola`colb!a b         / a 2-column table, 20 rows
```

The `` `cola`colb!a b `` part is a **dict literal**: a run of symbols, then
`!`, then one value per symbol. It pairs the first symbol with the first
value and so on. If a value is itself a compound expression, wrap it in
parentheses so qpl can tell where one value ends and the next begins, as in
`` `a`b!(x+1) y ``.

This `!` pairing shows up again as a way of specifying sort order in
[Table operators](table-operators.md).

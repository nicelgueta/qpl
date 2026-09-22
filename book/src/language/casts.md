# Casts

`$` converts between types, with the target type on the left and the thing
being converted on the right. It reads as "as":

```qpl
qpl) int$45.3
```

```
i64: 45
```

The same operator works on a column inside a query:

```qpl
qpl) select f: f64$size from trades
```

```
shape: (8, 1)
┌───────┐
│ f     │
│ ---   │
│ f64   │
╞═══════╡
│ 100.0 │
│ 250.0 │
│ 80.0  │
│ 300.0 │
│ 150.0 │
│ 90.0  │
│ 500.0 │
│ 200.0 │
└───────┘
```

Note the alias. A cast produces a *new* column rather than modifying the one
it read, and an unnamed derived column is called `x` by default. That's
harmless with one of them, but two in the same query would both want the name
`x` and the query fails rather than guessing. So when casting more than one
column at a time, name them:

```qpl
select s: f64$size, y: str$sym from trades
```

The available types are `f64` (or `float`), `f32`, `i64` (or `int`), `i32`,
`i16`, `i8`, `u64`, `u32`, `u16`, `u8`, `bool`, and `str` (or `string`).

## Casting a whole query

The thing on the right of `$` doesn't have to be a bare column reference —
a whole `select` (or `update`/`collect`/…) works too:

```qpl
qpl) `date$select ts from trades where sym = "AAPL"
```

```
date[3]: 2024.03.15 2024.03.15 2024.03.15
```

This only makes sense for a **single-column** query — the cast applies to
one column and returns a list, not a table. If the query returns more than
one column, the cast doesn't error; it silently casts the *first* column
and drops the rest, so `` `date$select ts, sym from trades `` gives back the
dates only, with `sym` discarded. Keep the query to the one column you
actually want cast.

## Width only matters in a column

The narrow integer types are worth a note. In a **scalar** context, every
integer width collapses to a single 64-bit integer, and both float widths
collapse to one float. So `i8$5` and `i64$5` give the identical scalar, and
the width you asked for takes effect only once the value lands in a table
column, where Polars stores it in that representation.

In other words a scalar cast is about the *kind* of value — integer, float,
boolean, text — while a column cast is also about how many bits it occupies.
Narrowing a large column from `i64` to `i16` is a real memory saving;
narrowing a scalar is not.

## Strings are parsed, not reinterpreted

Casting a string (or a symbol) to a number reads the text and parses it,
which is what you'd want:

```qpl
qpl) int$"42"
```

```
i64: 42
```

```qpl
qpl) bool$"true"
```

```
bool: true
```

A parsed value composes with everything else immediately:

```qpl
qpl) 1 + int$"42"
```

```
i64: 43
```

## Evaluation order bites here first

[Expressions](expressions.md) already introduced qpl's right-to-left
evaluation order, but a cast is usually where it's first felt in practice,
because a cast tends to be the innermost thing in an expression.

Writing `int$"42" - 1` doesn't subtract one from forty-two. Reading right to
left, the `-` is applied first, so qpl tries to cast the result of
`"42" - 1`. What you want is:

```qpl
qpl) (int$"42") - 1
```

```
i64: 41
```

Parentheses settle it. This is the single most common source of surprise for
people arriving from left-to-right languages, and casts are usually where it
bites first, because a cast is often the innermost thing in an expression.

# Expressions

The last few chapters covered the shape of a statement: which clauses exist
and what each one does with a table. This chapter is about what you can write
*inside* those clauses.

Everything here works in the same places. A projection, a `where` predicate,
an `update` assignment, the right-hand side of a scalar binding: they all
accept the same expression language, so anything you learn in one context
carries to the others.

## Arithmetic and comparison

The arithmetic operators are `+`, `-`, `*` and `%`. As
[Comments](comments.md) explained, `%` is division, because `/` starts a
comment.

One wrinkle worth knowing: `%` is always true division, even when both sides
are integers — unlike `+`, `-` and `*`, which stay integer when both operands
are.

```qpl
qpl) 10 % 4
```

```
f64: 2.5
```

Comparison is `=`, `<>` or `!=` for inequality, and `<`, `<=`, `>`, `>=` as
usual. Note that equality is a single `=`, not `==`, since there's no
assignment operator competing for it.

`&` and `|` are logical and and or, which you saw combining predicates in a
`where` clause.

A leading `-` negates. On a literal it simply makes a negative number, and in
front of a column or variable it expands to a subtraction from zero, so it
works the same way in every context:

```qpl
select neg_mv: -price from trades
```

## A word about evaluation order

This is worth internalising early, because it applies to every expression in
the language, not just the operators above: qpl evaluates right to left,
following q, rather than the left-to-right order most languages use. With `+`
or `*` that's invisible, but with anything that doesn't commute it changes the
answer.

`10 - 3 - 2` doesn't give `5`; reading right to left, `3 - 2` happens first,
then `10 - 1`, giving `9`. Parentheses make the order explicit whenever it
matters: `(10 - 3) - 2` is the `5` a left-to-right reader would expect.
[Casts](casts.md) is where this most often catches people out, because a cast
tends to sit innermost in an expression — but the rule is general, so keep it
in mind anywhere you chain non-commutative operators.

## Conditionals

`?[...]` is the vectorised conditional, and it's the piece of syntax most
worth getting comfortable with, since it does the work of SQL's `CASE WHEN`
in a fraction of the space. The contents are a list of condition-then-value
pairs, with a final fallback:

```qpl
qpl) select sym, size, band: ?[size >= 250; `large; size >= 100; `mid; `small] from trades
```

```
shape: (8, 3)
┌──────┬──────┬───────┐
│ sym  ┆ size ┆ band  │
│ ---  ┆ ---  ┆ ---   │
│ str  ┆ i64  ┆ str   │
╞══════╪══════╪═══════╡
│ AAPL ┆ 100  ┆ mid   │
│ AAPL ┆ 250  ┆ large │
│ MSFT ┆ 80   ┆ small │
│ MSFT ┆ 300  ┆ large │
│ GOOG ┆ 150  ┆ mid   │
│ GOOG ┆ 90   ┆ small │
│ AAPL ┆ 500  ┆ large │
│ MSFT ┆ 200  ┆ mid   │
└──────┴──────┴───────┘
```

Read it as: if `size >= 250` then `` `large ``, otherwise if `size >= 100`
then `` `mid ``, otherwise `` `small ``. Conditions are tested in order and
the first match wins, so the ordering of the pairs matters. Add as many pairs
as you need; the final item with no condition in front of it is the else.

"Vectorised" means this is evaluated once across the whole column rather than
row by row, which is why it stays fast on large inputs. It's an ordinary
expression, so it also works outside a query entirely, including in a scalar
binding or a function body.

### Outside a query

Outside a `select` the condition can be a boolean **atom** or a boolean
**vector**.

With an atom, only the branch that is taken is evaluated, and the result is
whatever that branch is. That is what makes conditional recursion work
(`fac: {[n] ?[n<2; 1; n*fac[n-1]]}`) and lets a branch be a function call.

With a vector, the result is elementwise and as long as the condition. Atoms
broadcast; a vector branch must be exactly as long as the condition, and it is
a runtime error if not:

```qpl
qpl) ?[1011b; 1; 0]
```

```
i64[4]: 1 0 1 1
```

```qpl
qpl) x: 5 6 7 8
qpl) ?[x>6; x; 0]
```

```
i64[4]: 0 0 7 8
```

```qpl
qpl) ?[1011b; 1 2 3; 0]
'`?[..]` branch has length 3, expected 4 (the length of the condition)
```

With several pairs the first true condition wins per element, and every
condition after the first must be boolean and either an atom or the same
length. A vector conditional has no short-circuit: every branch is evaluated,
since each element picks its own. Its branches must all be text or all be
non-text (`?[m; 1; "a"]` is an error rather than turning the numbers into
strings), and an all-symbol result stays a symbol vector. `while` is different:
its test is always a single boolean atom.

## Pattern matching

`like` tests text against a glob pattern, following
[q's rules](https://code.kx.com/q/ref/like/):

```qpl
qpl) select sym, price from trades where sym like "A*"
```

```
shape: (3, 2)
┌──────┬───────┐
│ sym  ┆ price │
│ ---  ┆ ---   │
│ str  ┆ f64   │
╞══════╪═══════╡
│ AAPL ┆ 182.3 │
│ AAPL ┆ 183.1 │
│ AAPL ┆ 184.0 │
└──────┴───────┘
```

`*` matches any run of characters including none, `?` matches exactly one,
and `[abc]`, `[a-z]` and `[^abc]` are character classes. A pattern with none
of those characters in it is simply an exact match. Matching is
case-sensitive.

```qpl
select sym from trades where sym like "[AM]*"        / starts with A or M
select sym from trades where sym like "?A?L"         / four characters, A then L
select from trades where not sym like "AAPL"         / negated with `not`
```

To match a literal `*`, `?`, `[` or `]`, put it in a one-character class of
its own: `[*]`, `[?]`, `[[]`, `[]]`. Glob syntax has no backslash escape.

## Verbs that take a parameter

A family of operations needs a parameter as well as a column. In q these are
written with the parameter on the *left*, which reads oddly for about five
minutes and then starts to feel natural:

```qpl
qpl) select p95: 0.95 quantile price by sym from trades
```

```
shape: (3, 2)
┌──────┬─────────┐
│ sym  ┆ p95     │
│ ---  ┆ ---     │
│ str  ┆ f64     │
╞══════╪═════════╡
│ MSFT ┆ 415.92  │
│ AAPL ┆ 183.91  │
│ GOOG ┆ 141.165 │
└──────┴─────────┘
```

The full set:

| Written | Does |
|---|---|
| `<p> quantile <col>` | the `p`th quantile, `p` between 0 and 1 (`pctl` is an alias) |
| `<n> shift <col>` | move values `n` rows later (`lag` is an alias) |
| `<n> lead <col>` | move values `n` rows earlier |
| `<n> diff <col>` | the change from `n` rows back |
| `<n> pctchange <col>` | the fractional change from `n` rows back |
| `<n> round <col>` | round to `n` decimal places |

`round` takes its rounding mode from a session setting rather than from the
expression, since it's the sort of thing you'd want to fix once for a whole
script. [Config](config.md) covers it.

The row-relative verbs in that table — `shift`, `lead`, `diff`, `pctchange` —
raise an obvious question: relative to which ordering? On their own they use
the table's existing row order. To define the ordering explicitly, and to
compute these things per group, they combine with `over`, which is
[Window functions](window-functions.md).

## Aggregates

These collapse many values into one. You've already used `avg` and `sum`
with `by`; they work the same way anywhere an aggregate makes sense.

`sum`, `avg` (or `mean`), `min`, `max`, `count`, `first`, `last`, `std` (or
`dev`), `var`, `med` (or `median`), `mode` (or `modal`, the most frequent
value, resolving ties to the smallest), `skew`, `kurt` (or `kurtosis`),
`any`, `all`, `prod` (or `product`), `argmin`, `argmax`, `nnull` (or
`null_count`), and `distinct` (or `n_unique`).

A few more verbs transform a column without collapsing it, and are listed
here for completeness since they appear in the same position: `abs`, `neg`,
`not`, `string`.

## Cumulative and fill verbs

The last group produces a running result down a column, which means their
output depends on row order:

`cumsum`, `cummax`, `cummin`, `cumprod`, `cumcount`, plus `ffill` and `bfill`
for carrying values forward or backward over nulls.

Like the row-relative verbs above, these are at their most useful with `over`
and an explicit ordering, which the [next chapter but one](window-functions.md)
covers. On their own they run down the table in its current order.

## Casting

One operator is missing from this chapter deliberately: `$`, which converts
between types. It comes up often enough to deserve its own short chapter, and
it's [two chapters ahead](casts.md).

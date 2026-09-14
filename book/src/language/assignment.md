# Assignment

You met `:` in the [Quickstart](../quickstart.md), where it kept the result of
a query under the name `t`. It's worth spending a moment on properly, because
the same operator handles every kind of binding in the language and it's the
glue that makes the incremental style possible.

The rule is that `:` binds a name, and whatever sits on the right decides what
kind of binding you get. There's no type to declare and no keyword to choose
between:

```qpl
qpl) threshold: 150                                 / a scalar
qpl) t: select from trades where size > threshold   / a table
```

Nothing prints for either statement. An assignment stores something; if you
want to see it, ask for it by name on a line of its own.

## Scalars are real values, not placeholders

The second line above quietly did something worth understanding, because it
explains a lot about how qpl behaves later.

When you write `threshold: 150`, qpl evaluates the right-hand side
immediately, in Rust, and stores the resulting value. It doesn't keep the
expression around to evaluate later. So by the time `threshold` is used
inside the next query, it is simply the number 150, and it gets compiled into
that query as a literal.

The practical effect is that scalars compose into queries transparently, with
no binding syntax, no parameter list, and no quoting rules to worry about:

```qpl
qpl) threshold: 150
qpl) select from trades where size > threshold
```

```
shape: (4, 5)
┌──────┬───────┬──────┬──────┬─────────────────────┐
│ sym  ┆ price ┆ size ┆ side ┆ ts                  │
│ ---  ┆ ---   ┆ ---  ┆ ---  ┆ ---                 │
│ str  ┆ f64   ┆ i64  ┆ str  ┆ datetime[ns]        │
╞══════╪═══════╪══════╪══════╪═════════════════════╡
│ AAPL ┆ 183.1 ┆ 250  ┆ sell ┆ 2024-03-15 09:31:15 │
│ MSFT ┆ 416.0 ┆ 300  ┆ buy  ┆ 2024-03-15 09:45:05 │
│ AAPL ┆ 184.0 ┆ 500  ┆ sell ┆ 2024-03-15 10:15:00 │
│ MSFT ┆ 414.8 ┆ 200  ┆ sell ┆ 2024-03-15 10:20:35 │
└──────┴───────┴──────┴──────┴─────────────────────┘
```

That query is indistinguishable from one with `150` typed directly into it,
which is exactly the intent. A threshold you'll use in six places becomes a
name you can change in one.

Ask for a scalar by name and qpl tells you its type along with its value:

```qpl
qpl) threshold
```

```
i64: 150
```

## Rebinding is the normal thing to do

Names aren't precious, and reassigning one is the ordinary way to move a
pipeline forward rather than something to avoid:

```qpl
qpl) t: select sym, side, price, size from trades where price > 0
qpl) t: update notional: price * size from t
```

The second statement reads `t`, produces a new table, and binds it back to
`t`. This is the loop from [Working incrementally](../working-incrementally.md),
and it's why the language doesn't need a pipeline operator: the name *is* the
pipeline.

## The other kinds

Two more sorts of value can sit on the right of a `:`, and both get their own
chapter shortly. They're mentioned here only so the shape is familiar when
they appear:

```qpl
qpl) lvl: `low`mid`high        / a list of symbols — see the next chapter
qpl) top: max trades`price     / a value pulled out of a table
```

Lists and the `` table`column `` form belong to
[Column expressions & lists](column-expressions.md), and the backticks in the
first line are the subject of the [very next chapter](symbols.md). For now the
only thing to take from them is that `:` doesn't care: it binds whatever you
give it.

Finally, a bare name on the right just copies the binding, which is
occasionally handy for keeping a snapshot before you start modifying
something:

```qpl
qpl) orig: trades
```

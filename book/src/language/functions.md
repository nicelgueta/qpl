# Functions

Once you've typed the same query three times with only one value changing,
it's time to give it a name. qpl has lambdas in the q style, bound with the
same `:` as everything else.

```qpl
qpl) add: {[x,y] x+y}
qpl) add[2;3]
```

```
i64: 5
```

The body goes in braces, the parameters in brackets at the front, and
arguments are passed in brackets separated by semicolons. Semicolons rather
than commas, because a comma already means something inside a query.

A function of one argument can also be called by just putting the argument
after it, which reads better for small helpers:

```qpl
qpl) inc: {[x] x+1}
qpl) inc 41
```

```
i64: 42
```

`inc[41]` works too; they're the same call.

## Several statements

A body can hold more than one statement, separated by semicolons. Earlier
statements bind local names, and the value of the last one is what comes
back. There's no `return` keyword:

```qpl
qpl) hypot2: {[a,b] s: (a*a)+(b*b); s}
qpl) hypot2[3;4]
```

```
i64: 25
```

Because `?[...]` from [Expressions](expressions.md) is an ordinary
expression rather than statement syntax, it works as a function body, and
recursion follows without any special support:

```qpl
qpl) fac: {[n] ?[n<2; 1; n*fac[n-1]]}
qpl) fac[5]
```

```
i64: 120
```

## Writing a body across multiple lines

Semicolons are only needed to fit more than one statement on the same
physical line. Written across several lines, a body reads like a script —
one statement per line, indented under the opening `{[..]`:

```qpl
summarise: {[min_price]
    joined: select sym, side, price, size, bid, ask from trades `sym lj quotes `sym
    priced: select from joined where price > min_price
    banded: update band: `$?[size >= 300; `large; size >= 150; `mid; `small] from priced
    select tot: sum price * size, avg_spread: avg ask - bid, n: count price
        by sym, side, band
        from banded
        order tot desc
    }
```

The rule is exactly the one from [Multi-line statements](multiline.md), just
shifted one indent level in: a new line
indented no further than the body's first statement starts a new one; a line
indented *more* than that continues the statement above (like the `by`/
`from`/`order` lines above, which all belong to the final `select`). An
explicit `;` still works if you'd rather keep two statements on one line.

## Returning tables

Nothing restricts a body to arithmetic. A function can end in a query, in
which case calling it gives you a table:

```qpl
qpl) bysym: {[s] select sym, price from trades where sym = s}
qpl) bysym["AAPL"]
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

Or, using a reduction from
[Column expressions & lists](column-expressions.md), a scalar:

```qpl
qpl) avgpx: {[s] avg select price from trades where sym = s}
qpl) avgpx["MSFT"]
```

```
f64: 415.3333333333333
```

Either result composes like any other value of that kind, so a function
returning a table can be fed straight into another query.

## The rules

A few constraints define what a function is in qpl, and they're worth reading
once rather than discovering individually:

**The last statement must be an expression.** A trailing assignment is an
error, since there would be nothing to return.

**Locals are local.** Parameters, and anything the body binds, exist only for
the duration of the call. A function cannot modify a name outside itself, so
a call's effect is entirely described by what it returns.

**Niladic functions** take no arguments and are written `{[] ...}` or simply
`{ ... }`, then called as `f[]`. The empty brackets are what distinguish
calling it from naming it.

**Functions are not values.** You can bind one and call it, but you cannot
pass one as an argument, return one from another function, or use one inside
a `select` projection. If you're reaching for a higher-order function, the
language will not meet you there; that's a deliberate limit on how much
machinery the interpreter carries rather than an oversight.

[examples/functions.qpl](https://github.com/nicelgueta/qpl/blob/main/examples/functions.qpl)
in the repository is a runnable script covering all of the above.

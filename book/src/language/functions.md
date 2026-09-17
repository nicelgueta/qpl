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
`{ ... }`. They can be called either with empty brackets, `f[]`, or bare,
`f`, which reads like a variable that recomputes every time you mention it:

```qpl
qpl) now: {[] .qpl.ts}
qpl) now
qpl) now[]
```

That's exactly how the `.qpl.dt`/`.qpl.tm`/`.qpl.ts`/`.qpl.dlta` now-functions from
[Temporal types](temporal-types.md) work — they're built-ins resolved by the
same name lookup user functions go through, not special syntax. A function
that *does* take parameters has no bare form: naming it gives you the function
itself rather than calling it. Built-in names are reserved: binding over one
is an error rather than a silent shadow.

**A function is not a column value.** `select px: inc from trades` is an
error. Everywhere else, a function is an ordinary value — see below.

## Functions as values

A function is a value like an int or a list. It can be passed to another
function, returned from one, and stored in a variable, and a `{[..] ..}`
literal is legal anywhere an expression is:

```qpl
qpl) apply: {[f,x] f[x]}
qpl) apply[{[y] y*2}; 5]
```

```
i64: 10
```

The parameter `f` holds a function, so `f[x]` inside the body calls it. A
function bound by name works the same way, since the name is just where the
value happens to live:

```qpl
qpl) apply[inc; 41]
```

```
i64: 42
```

A literal can also be applied on the spot, without being bound first, and a
body can end in one — which is how a function returns a function:

```qpl
qpl) {[y] y*2}[21]
qpl) adder: {[n] {[y] y+1}}
qpl) plus1: adder[0]
qpl) plus1[41]
```

```
i64: 42
i64: 42
```

**There is no lexical capture.** A body sees its own parameters plus the
session globals, and nothing else — not the locals of whoever called it, and
not the locals of wherever the literal was written. So in `adder` above, the
inner `{[y] y+1}` cannot refer to `n`; writing `{[y] y+n}` there gives you an
undefined-name error when it runs. That's the same rule named functions have
always followed, and it's why a function value needs to carry nothing but its
parameters and its body.

[examples/functions.qpl](https://github.com/nicelgueta/qpl/blob/main/examples/functions.qpl)
in the repository is a runnable script covering all of the above.

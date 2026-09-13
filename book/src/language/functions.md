# Functions

q-style lambdas, bound to a name:

```qpl
add:   {[x,y] x+y}            / parameter list in [ ], body is ;-separated
add[2;3]                      / 5   — call with bracketed args
inc:   {[x] x+1}
inc 41                        / 42  — a one-arg function also takes `f x`
inc[41]                      / 42

hypot2: {[a,b] s: (a*a)+(b*b); s}   / earlier statements bind call-local vars;
hypot2[3;4]                          / the last statement is the return value

fac: {[n] ?[n<2; 1; n*fac[n-1]]}   / `?[..]` works in value context, so
fac[5]                              / recursion terminates → 120
```

A function can return a table, and its result composes like any value
expression:

```qpl
bysym: {[s] select sym, price from trades where sym = s}
bysym[`AAPL]                  / a table
avgpx: {[s] avg select price from trades where sym = s}
avgpx[`MSFT]                  / a scalar
```

The final statement in the body must be an expression (a trailing assignment is
an error). Parameters and any locals the body assigns are scoped to the call —
a function cannot mutate outer bindings. Niladic functions are written `{[] ..}`
or `{ ..}` and called `f[]`. Functions are a named binding kind, not first-class
values: they cannot be passed as arguments, returned, or used inside a `select`
projection. See [examples/functions.qpl](https://github.com/nicelgueta/qpl/blob/main/examples/functions.qpl).

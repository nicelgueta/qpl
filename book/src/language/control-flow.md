# Control flow

qpl has one conditional, one loop and one "no-op" value:

| | |
|---|---|
| `?[c1; v1; c2; v2; else]` | the conditional — see [Expressions](expressions.md) |
| `while[test; s1; s2; ...]` | run statements while a test holds |
| `noop` | nothing |

## The conditional is `?[..]`

There is no `if`. `?[..]` already covers it: outside a `select` an atom
condition evaluates only the branch that is taken. (A boolean vector gives an
elementwise result instead — see [Expressions](expressions.md).) That is
what makes recursion work:

```qpl
qpl) fac: {[n] ?[n<2; 1; n*fac[n-1]]}
qpl) fac[5]
```

```
i64: 120
```

A branch can also be a function call, so it can do things as well as compute
values:

```qpl
qpl) shout: {[msg] log[msg]; noop}   / noop: see Logging for why
qpl) ?[n>3; shout["big"]; shout["small"]]
```

Functions stay pure, though: a call never assigns outer state. Only a plain
assignment does — at the top level, or in the body of a `while`.

With a boolean vector condition the result is elementwise instead, and every
branch is evaluated. Each vector operand must be exactly as long as the
condition (a runtime error otherwise), and atoms broadcast:

```qpl
qpl) ?[1011b; 10 20 30 40; 0]
```

```
i64[4]: 10 0 30 40
```

## while

`while[test; s1; s2; ...]` evaluates `test` (a boolean atom) before every
iteration, and while it is true runs the statements in order:

```qpl
qpl) n: 3
qpl) while[n>0; log[n]; n: n-1]
3
2
1
```

The statements are ordinary statements, so they can assign, `log`, or run a
table query. They run in the **current** scope: at the top level they bind
globals, inside a function they bind that function's locals, which are gone
when it returns:

```qpl
qpl) sumto: {[m] s: 0; while[m>0; s: s+m; m: m-1]; s}
qpl) sumto[100]
```

```
i64: 5050
```

`while` can be spread over several lines, like any statement with an open
bracket (in a script, indent the continuation lines as described in
[Multi-line statements](multiline.md)):

```qpl
while[k<=10;
    total: total+k;
    k: k+1]
```

A test that isn't a boolean atom is an error, as it is for `?[..]`. There is
no iteration limit.

### Stopping a loop

Ctrl-C stops the statement that is running:

```
qpl) while[1b; noop]
^C interrupting... (Ctrl-C again to force quit)
'interrupted
qpl)
```

Assignments made by earlier iterations stay made — there is no rollback. In a
script the interrupt ends the run (exit code 130); with `-i` you land in the
REPL. A second Ctrl-C while a statement is still running exits the process,
which is the way out of a single Polars query that is too slow to wait for.

The browser playground has no signals, so an infinite `while` there hangs the
tab.

## noop

`noop` is nothing. It prints nothing, and it is what `while` evaluates to. A
`?[..]` branch or a function can end in it:

```qpl
qpl) ?[n>3; noop; shout["small"]]
```

Because it is nothing, it can't be used as a value:

```qpl
qpl) x: noop
'cannot assign a no-op expression.
qpl) x: while[0b; 1]
'cannot assign a no-op expression.
qpl) 1 + noop
'cannot use a no-op expression as a value
```

`while` and `noop` are only valid outside a `select` projection.

## Reserved words

`while` and `noop` are reserved: `while: 3` and `{[noop] ..}` are parse
errors.

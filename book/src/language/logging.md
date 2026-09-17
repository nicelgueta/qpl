# Logging

Tables print themselves. Everything else — a progress note halfway through a
script, a computed value you want to see, a label on a result — goes through
`log`.

```qpl
qpl) log "starting run"
```

```
starting run
```

`log` prints its argument raw, with none of the type prefix or table framing
that a result gets. Given several space-separated expressions it renders each
one and joins them, which serves as string interpolation without the language
needing any:

```qpl
qpl) apx: avg select price from trades where sym = "AAPL"
qpl) log "avg AAPL px: " apx
```

```
avg AAPL px: 183.13333333333333
```

Numbers are rendered for you, so the cast isn't required, though
[casting](casts.md) to `str` explicitly is available when you want to control
the form:

```qpl
qpl) log "test" str$2*3 " that"
```

```
test6 that
```

A bare `log` prints an empty line.

`log` is the only thing that writes to stdout. kdb spells this `1 x`, after
the Unix stdout file descriptor, and qpl used to accept that too — but a
statement beginning with a digit is genuinely ambiguous, since `1 + 1` is
also a perfectly good expression, and the shorthand caused more confusion
than it saved keystrokes. It's gone.

## A parsing note

At the top level, putting two things next to each other separates them into
two items to be printed, rather than applying one to the other. That matters
if you want to log the result of calling a function, since the call needs
parentheses to stay a call:

```qpl
log (f x) " done"
```

## Inside a function body

The bareword form above only works as a whole top-level line — it runs to the
end of the line, so it has no way to stop early if it's embedded in something
bigger. `log[...]` is the same thing scoped with brackets instead, which is
what makes it usable inside a function body or nested in a larger expression:

```qpl
info: {[s] log[str$.qpl.ts " - INFO " s]}
info["service started"]
```

```
2024.03.15D09:30:00.000000000 - INFO service started
```

Arguments inside the brackets are still space-separated and concatenated
exactly like the bareword form (a `;` between them is accepted too, but never
required) — `[...]` only marks where the argument list ends, since a function
body can't rely on "the rest of the line" the way a REPL line can.

## Teeing output to a file

`\1 <path>` mirrors everything printed — log lines and query results alike —
into a file as well as the terminal. A bare `\1` stops it. This works in
scripts and in the REPL:

```qpl
\1 run.log
select from trades where size > 100
\1
```

It's the simplest way to keep a record of what a script actually produced,
without changing any of the statements in it.

# Multi-line statements

Nearly every example in this book has been a single line, which is honest
about how qpl is normally used. But a query with several aggregates, a
grouping, a filter and a sort does get long, and a script benefits from room
to breathe in a way that a prompt doesn't.

In a **script**, indentation continues a statement. Any line indented by a
tab or four or more spaces belongs to the statement above it, and a line
starting in column zero, or a blank line, ends it. There's no continuation
character to remember:

```qpl
t: select
    tot: sum size,
    apx: mean price
    by sym
    from trades
    where size > 50
```

That's one statement, laid out a clause per line purely for readability. It
behaves identically to the same thing typed on one line.

The rule has a pleasant consequence for scripts in general: since a statement
can only be continued by indenting, a file of unindented lines is
unambiguously a file of separate statements, and you never need to hunt for a
missing semicolon.

In the **REPL** the rule has to be different, because there's no column-zero
boundary to look at when input arrives a line at a time. Instead the prompt
keeps reading when it can tell you aren't finished: while a bracket is still
open, straight after a trailing comma, or when the statement was clearly cut
off mid-expression. A blank line submits what it has.

In practice this means you can type or paste a multi-line statement at the
prompt much as you'd write it in a file, and press return on an empty line
when you're done.

Inside a `{[..] ..}` [function body](functions.md#writing-a-body-across-multiple-lines),
the same rule applies one indent level in: each line at the body's own
baseline indent is a separate statement (no `;` needed), and a line indented
*further* continues the one above, exactly like the script rule above.

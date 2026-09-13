# Multi-line statements

In a script, a statement may span several lines: any line indented by a tab or
4+ spaces continues the one above it; a line starting in column 0 (or a blank
line) ends it. No continuation character needed.

```qpl
t: select
    tot: sum size,
    apx: mean price
    by sym
    from trades
    where size > 50
```

In the REPL the prompt keeps reading while brackets are open, after a trailing
`,`, or when input was cut off mid-statement; a blank line submits.

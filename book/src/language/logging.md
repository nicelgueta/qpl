# Logging

`log <expr>` (or `1 <expr>`, kdb-style) evaluates a scalar and prints it raw;
bare `log` / `1` prints a blank line. Space-separated expressions are rendered
and concatenated:

```qpl
log "starting run"
log "test" str$2*3 " that"       / test6 that
log "rows > " thr ": " n         / rows > 150: 42
```

Top-level juxtaposition separates items rather than forming a call — wrap a call
in parens: `log (f x) " done"`.

`\1 <path>` tees all stdout (log lines *and* query output) to a file as well as
the terminal; bare `\1` detaches it. Works in scripts and the REPL.

```qpl
\1 run.log
select from trades where size > 100
\1
```

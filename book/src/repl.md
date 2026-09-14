# REPL

You've been using the REPL since the Quickstart. This chapter covers the
parts of it that aren't the language itself: a handful of commands, all
beginning with `\`, that administer the session rather than querying data.

| Command | Action |
|---|---|
| `\d <stmt>` | disassemble: show the compiled instructions without running them |
| `\l <path>` | run a `.qpl` script inside the current session |
| `\1 <path>` | mirror all output to a file (bare `\1` stops) |
| `\port <n>` | start the IPC listener, if built with that feature |
| Ctrl-C | abandon a half-typed statement, or exit at an empty prompt |
| Ctrl-D | exit |

Two of those are covered elsewhere: `\1` in [Logging](language/logging.md)
and `\port` in [IPC](language/ipc.md).

## Loading a script into a live session

`\l` runs a script *in the session you already have*, rather than starting a
new process. Anything the script binds is yours afterwards, and anything you
had already stays put.

```qpl
qpl) \l setup.qpl
```

This is the natural companion to the incremental style. Keep the settled part
of your work in a file, load it, and carry on exploring from there. It also
pairs with `qpl -i script.qpl`, which does the same thing at startup.

## Seeing what a statement compiles to

`\d` shows the instructions a statement produces, without executing any of
them. Every query you write is compiled to a short sequence of stack-machine
operations before the VM runs it, and this prints that sequence:

```
qpl) \d select avg price by sym from trades where size > 100
0000: FROM_SRC InMem("trades")
0001: PUSH_COL_REF size
0002: PUSH_CONST Int(100)
0003: BIN_OP >
0004: FRAME_EXPR Filter(1)
0005: PUSH_COL_REF sym
0006: ALIAS Some("sym")
0007: BUILD_KEYS 1
0008: PUSH_COL_REF price
0009: CALL avg 1
0010: ALIAS Some("price")
0011: BUILD_PROJ 1
0012: SELECT_BY
0013: RESULT
```

Read top to bottom it follows the query closely: start from `trades`, push
`size` and `100` and compare them, filter on the result, build `sym` into the
grouping keys, build `avg price` into the projection, then run the grouped
select.

Most of the time you'll never need this. It earns its place when a statement
does something you didn't expect, because the instruction list usually makes
it obvious which clause was parsed differently from how you read it. The
right-to-left evaluation described in [Casts](language/casts.md) is a common
culprit, and `\d` is the fastest way to confirm it.

[Architecture](architecture.md) says a little more about where those
instructions come from and what runs them.

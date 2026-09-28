# REPL

You've been using the REPL since the Quickstart. This chapter covers the
parts of it that aren't the language itself: a handful of commands, all
beginning with `\`, that administer the session rather than querying data.

| Command | Action |
|---|---|
| `\d <stmt>` | disassemble: show the compiled instructions without running them |
| `\l <path>` | run a `.qpl` script inside the current session |
| `\i "<path>"` | import a script, namespacing its new bindings under `.<file-stem>.*` |
| `\1 <path>` | mirror all output to a file (bare `\1` stops) |
| `\port <n>` | start the IPC listener, if built with that feature |
| Ctrl-C | stop the running statement; otherwise abandon a half-typed statement, or exit at an empty prompt |
| Ctrl-D | exit |

Three of those are covered elsewhere: `\i` in
[Scripts, imports & namespaces](language/imports.md), `\1` in
[Logging](language/logging.md) and `\port` in [IPC](language/ipc.md).
Stopping a running statement is described under
[Control flow](language/control-flow.md#stopping-a-loop).

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

`\l` loads the script *flat*, so its names become your names. For a library
of helpers you'd rather keep out of the way, `\i` imports it under a
namespace instead; see [Scripts, imports & namespaces](language/imports.md).

## Seeing what a statement compiles to

`\d` shows the instructions a statement produces, without executing any of
them. Every query you write is compiled to a short sequence of stack-machine
operations before the VM runs it, and this prints that sequence:

```
qpl) \d select avg price by sym from trades where size > 100
0000  PUSH       Name(trades)
0001  SOURCE
0002  PUSH       Name(size)
0003  LOAD_COL
0004  PUSH       Value(Int(100))
0005  PUSH       BinOp(Gt)
0006  BINOP
0007  PUSH       Count(1)
0008  FILTER
0009  PUSH       Name(sym)
0010  LOAD_COL
0011  PUSH       Name(sym)
0012  ALIAS
0013  PUSH       Count(1)
0014  LIST
0015  PUSH       Name(price)
0016  LOAD_COL
0017  PUSH       Count(1)
0018  PUSH       Verb(avg)
0019  VERB
0020  PUSH       Name(price)
0021  ALIAS
0022  PUSH       Count(1)
0023  LIST
0024  SELECT_BY
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

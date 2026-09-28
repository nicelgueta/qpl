# Architecture

A whole script, or a single line typed at the prompt, takes the same path:

```
source -> lexer -> tokens -> parser -> AST -> compiler -> bytecode program -> VM (Polars LazyFrame) -> DataFrame
```

| Stage | Role |
|---|---|
| `lexer` | turn source text into tokens |
| `parser` | build a typed syntax tree from those tokens; for a script, the whole file at once |
| `compiler` | emit a one-byte-per-instruction bytecode program from the tree |
| `vm` | execute the program, building and collecting a Polars `LazyFrame` |
| `repl` | the interactive loop and the script runner |

A script compiles to one program and runs in a single pass: a syntax error
anywhere in the file — even on the last line — is caught before the first
statement runs. A line typed at the REPL, or a request arriving over
[IPC](language/ipc.md), is the same thing on a smaller scale: a small program
compiled and run against the same session.

The `\d` command from the [previous chapter](repl.md) prints the output of
the compiler stage, which is the last point at which the pipeline is still
qpl's own.

What's notable is what *isn't* in that list. There's no query optimiser,
because qpl doesn't need one. The VM's job is to translate instructions into
calls against a Polars `LazyFrame`, and the plan that results is Polars'
to optimise: predicate pushdown, projection pruning and the rest all happen
on the other side of that boundary. You can watch it happen in the plan
output shown in [lazy / collect](language/lazy-collect.md).

This division is why the whole interpreter is a fairly small amount of code
for a language that queries at the speed it does, and it's also why the
language stays deliberately compact: the project prefers to express a new
feature in terms of the existing instruction set over growing it, and to
grow the instruction set over adding special-case machinery elsewhere. That's
a maintainer's concern more than a user's, but it explains why the
[operator reference](language/operator-reference.md) fits on a single page.

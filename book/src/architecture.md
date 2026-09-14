# Architecture

Every line you type takes the same path, one statement at a time:

```
source -> lexer -> tokens -> parser -> AST -> compiler -> instructions -> VM (Polars LazyFrame) -> DataFrame
```

| Stage | Role |
|---|---|
| `lexer` | turn source text into tokens |
| `parser` | build a typed syntax tree from those tokens |
| `compiler` | emit stack-machine instructions from the tree |
| `vm` | execute the instructions, building and collecting a Polars `LazyFrame` |
| `repl` | the interactive loop and the script runner |

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
language stays deliberately compact. New syntax generally means new
instructions for the VM to execute, and the project treats that as a cost to
justify rather than a default, preferring to reuse the existing instruction
set wherever a new feature can be expressed in terms of it. That's a
maintainer's concern more than a user's, but it explains why the
[operator reference](language/operator-reference.md) fits on a single page.

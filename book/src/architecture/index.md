# Architecture

Most of this book is about what qpl does. This part is about how it does it,
and the short version is that everything takes the same road, whether it's a
whole script, one line typed at the prompt, or a request that came in from
another process:

```
source -> lexer -> tokens -> parser -> AST -> compiler -> bytecode -> VM -> Polars LazyFrame -> DataFrame
```

| Stage | What it does |
|---|---|
| `lexer` | chops source text into tokens |
| `parser` | turns those tokens into a typed syntax tree, a whole file at a time |
| `compiler` | walks the tree and emits a bytecode program, one byte per instruction |
| `vm` | runs the program, building up a Polars `LazyFrame` and collecting it when a result is needed |
| `repl` | drives all of the above: the interactive loop, the script runner, `qpl -c` and friends |

A script is compiled in full before any of it runs. That means a typo on the
last line of a long script, or somewhere inside a file it pulls in with `\l`
or `\i`, gets reported before the first statement has had a chance to do
anything. A REPL line, or a request arriving over [IPC](../language/ipc.md),
is the same thing in miniature: a small program, compiled and run against
the session that's already there.

If you want to see where the pipeline stops being qpl's own business, `\d`
from the [REPL chapter](../repl.md#seeing-what-a-statement-compiles-to)
shows you the compiler's output. That bytecode is the last thing qpl
produces on its own. From there on the VM is mostly translating instructions
into calls against a Polars `LazyFrame`.

The interesting thing about that picture is what's missing from it: there's
no query optimiser. qpl doesn't need one, because the plan it builds belongs
to Polars, and Polars already does predicate pushdown, projection pruning and
the rest on its side of the fence. You can watch it happen in the plan output
in [lazy / collect](../language/lazy-collect.md).

That split is how the interpreter stays small for a language that runs as
fast as it does. Most features turn out to be a new arrangement of
instructions that already exist, rather than new machinery, which is also
why the [operator reference](../language/operator-reference.md) fits on one
page.

The chapters that follow take each stage a level deeper:

- [The pipeline](pipeline.md): how source text becomes something runnable,
  and why a REPL line, a script and an IPC request all end up the same shape.
- [The modules](modules.md): a guided tour of the source tree, file by file.
- [The VM](vm.md): where the work actually happens. One binding map, one
  stack, and function calls that are just more stack rather than Rust
  recursion.
- [Invariants](invariants.md): the handful of properties that keep the
  bytecode simple, and where a new feature slots in.
- [Compiled artifacts](compiled-artifacts.md): the `.qplc` format, what
  `qpl -C` writes, and why it never needs the original source again.
- [IPC internals](ipc-internals.md): how a single-threaded `Vm` serves other
  processes without ever being shared between threads.

# The pipeline

Every scrap of qpl source goes through the same four steps: lex, parse,
compile, run. The only things that vary are how much text goes in at once and
what becomes of the result.

## A whole file, in one pass

When you run a script (`qpl script.qpl`), qpl reads the entire file, lexes
and parses it into a list of statements, and compiles *all* of them into a
single `Program` before a single instruction executes:

```
script.qpl -> parse_program -> compile_program -> Program { code, operands, lines } -> run_compiled
```

A `Program` comes in three pieces that travel together:

- `code` holds the instructions, one byte each.
- `operands` is a separate stream of values (names, numbers, jump targets,
  function bodies and so on) that only the `Push` instruction ever reads.
- `lines` maps each instruction back to the `(file, line)` it came from,
  so an error can point at the right place, including a line inside a file
  that was pulled in with `\l` or `\i`.

Compiling everything up front is what makes qpl's errors predictable. A
syntax mistake on line 500 is caught before line 1 runs, because parsing and
compiling are finished before execution starts. Runtime errors are the
opposite. A runtime error stops the script where it happens and leaves
everything the earlier statements did in place. There's no rollback, with
one exception: a failed `\i` import undoes itself, as
[The VM](vm.md#namespaces) explains.
[Scripts, imports & namespaces](../language/imports.md) tells the same story
from the user's side.

## A single line

The REPL takes exactly the same path, just with less text. Each submitted
line (or multi-line statement, once the brackets balance) goes through the
same `parse_program` and `compile_program` a script does, becomes its own
small `Program`, and runs straight away against the session already in
memory. As far as the VM is concerned there's no difference at all: a
script's `Program` and a REPL line's `Program` go through the very same
`run_compiled`. `qpl -c '...'` is the same again, one command and then exit.

A request arriving over [IPC](../language/ipc.md) differs in one small way.
It's compiled in *result* mode rather than *script* mode. A script prints
each statement's result as it goes, while result mode prints nothing and
leaves only the last statement's value behind, which is what gets sent back
to the caller. It also turns away `\`-prefixed commands at compile time.

## Where includes fit in

`\l` and `\i` are resolved when the *including* script is compiled, not when
it runs. The target file is read, parsed and compiled right then, and the
resulting program is tucked inside the including program as a sub-program
operand. When execution reaches the include, it runs that embedded program
in place. Nothing is read from disk at that point and nothing is compiled.
The compiler also keeps track of which files it's in the middle of
including, so a file that ends up including itself, directly or in a
roundabout way, is a compile error rather than an infinite loop.

This is also why a `.qplc` [compiled artifact](compiled-artifacts.md) never
needs its original source files. Every include it could ever reach is
already baked in.

## Entry points

- `main.rs` parses the command line, builds one long-lived `Vm`, installs
  the Ctrl-C handler, and hands over to `repl`. It also handles the flags
  that never need a `Vm`: `-C` (compile to `.qplc`) and `-d` (print a
  `.qplc`'s or a script's bytecode without running it).
- `repl::run_script` pushes a file through the whole pipeline once. If the
  file starts with the `.qplc` magic bytes, it skips straight to the run.
- `repl::start` is the interactive loop. It takes one statement per trip
  round the loop and reuses the same `Vm`, so a name bound on one line is
  still there on the next.
- `repl::eval_capture` is the same thing with the output captured instead of
  printed, which is what the browser build uses.
- `vm::run_vm` is the single-statement shortcut most of the test suite uses.
  It skips the whole-program machinery and just parses, compiles and runs
  one statement.

State survives across all of these because they all share one `Vm`. Its
bindings, its settings and its registers aren't reset between runs, which
is also how `qpl -i script.qpl` can run a script and then leave you at a
prompt with everything the script defined still in scope.

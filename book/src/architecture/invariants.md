# Invariants

A handful of properties hold across the whole interpreter. They're what keep
the bytecode easy to reason about and safe to extend, and they're worth
knowing even if you never touch the code, because they explain why the code
looks the way it does.

- **One evaluator.** Everything runs through the VM's single dispatch loop.
  That includes arithmetic, function calls, closures, `while`, `?[..]`,
  building lists and IPC, all compiled down to bytecode. There's no
  tree-walking fallback lurking anywhere for the awkward cases.
- **Every instruction is exactly one byte.** No opcode carries data inline.
  Anything an instruction needs comes either off the stack or, via `Push`,
  off the side operand stream.
- **`Push` is the only thing that reads the operand stream.** Every other
  instruction takes its inputs from the stack. The compiler always lays out
  operands in the order their `Push`es will run along a straight-line path,
  which is what makes jumping (moving `ip` and `cp` together) safe.
- **Instruction numbers never change.** A new instruction is always added
  at the end. Retiring one leaves a gap and a comment explaining what used
  to be there, and the number is never reused. A test pins a few of the
  byte values specifically to catch an accidental renumbering. It matters
  because a `.qplc` file (see [Compiled artifacts](compiled-artifacts.md))
  is those numbers on disk, and a silent renumbering would turn an old
  artifact into the wrong program instead of a clean error.
- **All execution state is on the stack.** Where the program *is* and what
  it's working on lives in the stack, `globals` and the registers, and
  nowhere else. Every kind of binding shares the one map, and the call
  frames on the stack are the only scopes there are.

## Where a new feature goes

Those properties give every kind of addition an obvious home.

**A new instruction** is a new `Op` variant at the end of the list, a case
in the disassembler and the dispatch loop (with any real logic in `ops`),
and code in the compiler to emit it. Because it changes what a `.qplc` file
can contain, it also comes with a bump to the bytecode format version. New
instructions are rarer than you might expect, though. Often an existing
instruction can take a new `Operand` variant, and more often still a
feature can be built entirely from instructions that already exist. The
instruction set sits well short of the 256 a byte allows, and a small set
is easier to keep correct.

**A new native function** is one entry in the builtin table in `native`.
The lexer, parser and compiler don't need to know about it, because a
name-resolved native is looked up the same way a user function is. The
exceptions are the handful of primitives the compiler calls *by id* so that
nothing can ever shadow them. Those get a `NativeId`, and since those ids
are stored in `.qplc` files, a new one also bumps the format version.

**A new statement form** (a `\` command or top-level directive) is an AST
variant, a case in the top-level parser, and a lowering in the compiler,
usually to a call to a native that does the work. `\port` works this way,
which is why it behaves the same in a script as at the prompt, with no
special cases anywhere.

Tests live next to the code they cover, in each source file. Language
features also show up in three places outside the interpreter: runnable
scripts under `examples/` (checked against saved output by the test suite),
this book, and the editor tooling in `tools/vscode/`, whose vocabulary file
drives highlighting and completion in both VS Code and the browser build.

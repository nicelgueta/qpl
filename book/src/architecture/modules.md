# The modules

The interpreter is a single crate. The whole of it lives in `src/lib.rs` and
the files it pulls in, so the `qpl` binary (`main.rs`) and the browser build
(`wasm.rs`) are just two thin front doors onto the same library. What follows
is the tour you'd give someone opening the source tree for the first time,
roughly in the order a line of qpl passes through it.

## The front end

**`lexer`** turns source text into a flat list of tokens: keywords, names,
numbers, symbols, temporal literals, punctuation. **`tokens`** does nothing
but define what a `Token` is.

**`parser`** turns those tokens into a tree, using types from **`ast`**:
`Stmt` for a statement, `TableExpr` for a query, `Expr` for everything
inside one. It's the biggest file on the front end because it's where most
of the language's surface lives: `select`/`update`/`delete`, casts, window
functions, control flow, function literals. It has two ways in. `parse`
reads a single statement, which is what `run_vm` and the tests use.
`parse_program` reads a whole chunk of source (a file, a REPL line, an IPC
request) into a list of statements, and it's also where statement splitting
and multi-line function bodies get sorted out. The `\`-prefixed commands
(`\d`, `\l`, `\i`, `\1`, `\port`), `.qpl.cfg` and `log` are proper statement
variants here. Nobody downstream has to pattern-match raw text to recognise
them.

**`ast`** is just the types: `Stmt`, `TableExpr`, `SelectStmt` (one type
covers `select`, `update` and `delete`, told apart by a couple of flags),
`Expr`, and `Value`, the runtime value type. `Value` has an eager `Table`
and a lazy `Lazy` variant alongside the scalars, vectors and `Closure`, so
there's never a question of which map a table lives in, because there's
only one.

**`builtins`** is a small enum of the table operations that aren't a
`select`: `cols`, `sink`, `sort`, `distinct`, `limit`, `drop`, `lazy`,
`collect`.

## The middle: compiling to bytecode

**`compiler`** turns the tree into a `Program`. `compile`/`compile_stmt`
handle a single statement. `compile_program` handles a whole list of them in
one of two modes. *Script* mode prints each statement's result as it goes
and is used for scripts, REPL lines and embedded includes. *Result* mode
prints nothing and leaves only the last value on the stack, and is used for
IPC requests. Both modes share the same code generation for queries and
expressions. Two pieces of compile-time-only logic live here too: rewriting
names for `\i` namespaces (see [The VM](vm.md#namespaces)) and catching
`\l`/`\i` cycles.

**`program`** defines the bytecode itself. That's `Op`, the one-byte
instruction enum, and `Operand`, everything `Push` can read off the side
stream: names, counts, jump targets, literal values, function prototypes,
embedded sub-programs and so on. It also holds `Program` itself and the
disassembler behind `\d` and `qpl -d`. On top of that it owns
`Program::to_bytes`/`from_bytes`, the `.qplc` file format, which gets its
own chapter in [Compiled artifacts](compiled-artifacts.md).

**`codec`** is a small, self-contained binary encoder/decoder for `Value`.
Two very different consumers share it because they both need to serialise
a value without losing anything: `.qplc` operand encoding and the IPC wire
format.

## The back end: running it

**`vm`** is the evaluator: the `Vm` struct, the stack and its `Slot`
variants, and the loop that decodes and dispatches instructions. It's by
far the largest file in the crate, and big enough to get its own chapter,
[The VM](vm.md).

**`ops`** is what keeps `vm` from becoming one enormous function. It holds
the type-dispatched semantics the dispatch loop hands off to: what `+`
means between two vectors, how a cast or a temporal operation works, how a
verb applies, `like` matching, and a grab-bag of natives that don't need a
slot of their own (`til`, `enlist`, roll, `zip`, `log`, and IPC's
`hopen`/`whopen`/`await`/`dispatch`). Its `call_by_name` is the last stop
for a function call nobody else claimed. That's why a user function called
`til` quietly wins over the built-in one, while a name in the `native`
table can't be taken at all.

**`native`** is that table of name-resolved built-in functions, built once
when a `Vm` starts. It also defines `NativeId`, the short list of
primitives (`\l`, `\i`, `\port`, `.qpl.cfg` and a few others) that the
compiler calls *by id* rather than by name, so no binding can ever get in
their way.

**`temporal`** handles everything calendar-related: parsing q-style date
and time literals, formatting them back, the `.qpl.dt`/`.qpl.tm`-style "now"
functions, and the arithmetic between qpl's 2000-based epoch and the
1970-based one Polars uses.

**`vm_config`** holds the session settings (`maxcol`, `maxrow`,
`tblwidth`, `strlen`, and so on) that `.qpl.cfg key=value` changes; see
[Config](../language/config.md).

**`errors`** defines `QplError`, the one error type used everywhere. It has
a variant for each stage (`Lex`, `Parse`, `Compile`, `Runtime`) plus
`Interrupted`, which Ctrl-C produces so the REPL and script runner can tell
"you stopped it" apart from "it broke".

**`helpers`** is odds and ends: snake-casing column names on load and a
small random-number source for roll.

## Driving it

**`repl`** is the REPL loop, the script runner, the `.qplc` runner,
`qpl -c`, `qpl -C` and `qpl -d`, the demo tables, and result formatting.
Everything that prints goes through `Vm::emit`, which is also how `\1` can
mirror all output to a file. The interactive-only concerns live here too:
deciding whether a half-typed statement needs another line, and servicing
an open `\port` by juggling keyboard input and incoming requests.

**`interrupt`** is a tiny flag, an `Arc` of two atomics, that the Ctrl-C
handler sets and the VM checks at safe moments: loop back-edges, function
calls, around Polars `collect`s, and while waiting on IPC.

**`ipc`**, behind the (default) `ipc` feature, holds the client and server
halves of `hopen`/`dispatch`/`await`/`\port`. See
[IPC internals](ipc-internals.md) for how it gets along with a
single-threaded VM, and [IPC](../language/ipc.md) for how to use it.

**`wasm`** and **`arrow_io`**, behind the `wasm` feature, are the browser
build. `wasm` wraps a `Vm` behind a JavaScript-friendly `eval` that runs
the same path as the terminal REPL but captures output instead of printing
it. It also serves Monaco editor highlighting built from the same
vocabulary file the VS Code extension uses. `arrow_io` passes tables in
and out as Arrow IPC bytes, since a browser has no filesystem to `load`
from.

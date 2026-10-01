# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`qpl` (Quick Polars Language) is a CLI interpreter for a kdb+/q-inspired
language that compiles to Polars **lazy** frames. It is a single Rust
binary with no runtime dependencies (no Python). The language surface, examples,
and rationale are documented in [README.md](README.md) and [examples/](examples/) —
read those for language semantics; this file covers build/dev workflow and
internal architecture.

## Commands

```bash
cargo build                 # debug build -> target/debug/qpl (qpl-cli's binary)
cargo build --release       # release build (large: bundles all of Polars)
cargo test                  # run all tests (unit tests live in #[cfg(test)] modules per source file)
cargo test <name>           # run tests matching a substring, e.g. `cargo test lazy`
cargo test --package qpl parser::   # run one module's tests
cargo test --workspace      # also `qpl-macros` and `qpl-wasm` (`cargo test` alone skips them)
cargo run                   # start the REPL
cargo qpl --load-demo    # REPL preloaded with demo `trades` / `quotes` tables (`cargo qpl` is a `cargo run -p qpl-cli --` alias, see .cargo/config.toml)
cargo qpl script.qpl     # execute a script (read-only: `sink`/`\1 <path>` refused)
cargo qpl -w script.qpl  # execute a script with write permission
cargo qpl -i script.qpl  # execute a script, then drop into the REPL
cargo qpl examples/lazy_join_pipeline.qpl   # run an example
cargo qpl -c 'select avg price by sym from trades' --load-demo   # run an ad hoc command and exit
cargo qpl -C script.qpl                      # compile to script.qplc (add -o to redirect)
cargo qpl script.qplc                        # run a compiled artifact — no lexing/parsing/compiling
cargo build -p qpl --no-default-features        # the library alone, drop `ipc` (hopen/dispatch/await, \port — see Architecture)
cargo test -p qpl-std                           # `.std`: string/list/filesystem/env functions
cargo test -p qpl-std --no-default-features     # drop `os` (fs/env) and the ipc-only test feature
cargo test -p qpl-wasm                          # browser bindings, host-side tests
cargo run -p qpl-cli --example extension -- -c '.geo.km[51.51;-0.13;40.71;-74.01]'   # a qpl binary with a Rust extension
cargo run -p qpl-cli --example extension_toolkit -- --load-demo -c '.stats.top[trades; `price; 3]'   # two extensions, every arg/result kind
make wasm                   # browser bundle -> tools/wasm/pkg (patches polars first,
                            # see scripts/build-wasm.sh and tools/wasm/README.md)
```

`ipc` is a default feature of the `qpl` library, so a plain `cargo build`/`cargo
test`/`cargo run` already includes it; `-p qpl --no-default-features` is only
needed to build/test without it. The library (`qpl`, this crate's root) has no
front-end of its own and no terminal/browser dependencies — `qpl-cli` (the
`qpl` binary: clap/rustyline/mimalloc/ctrlc) and `qpl-wasm` (browser bindings:
wasm-bindgen/js-sys/serde_json) are thin front-ends over it in their own
crates, so neither pulls the other's dependencies in and neither needs to be
a dependency of the library itself. A binary with Rust extensions
(`qpl-cli/examples/extension.rs`) gets the identical front-end by depending on
`qpl-cli` and calling `qpl_cli::run(extensions)`. `qpl-std` (`.std`: string,
list, filesystem and environment functions — see the `ext` row below) is
itself an ordinary Rust extension crate, built only on the public extension
API; `qpl-cli` registers it before any user extension, and `qpl-wasm`
registers it with its `os` feature (filesystem/env) off, since a browser has
neither.

The repo is a Cargo workspace of five crates: `qpl` (the root package, the
pure interpreter library), `qpl-macros/` (the `#[qpl::native]` proc macro,
re-exported as `qpl::native`), `qpl-std/` (`.std`), `qpl-cli/` (the `qpl`
binary) and `qpl-wasm/` (browser bindings). `[workspace.package].version` is
the one version number, shared by every crate via `version.workspace = true`.
Plain `cargo build`/`test`/`run` at the root operate on `qpl`, `qpl-std` and
`qpl-cli` (`[workspace] default-members` in the root `Cargo.toml`) — the
obvious "build/test/run the interpreter and its CLI" default; `qpl-macros`
and `qpl-wasm` need an explicit `-p` or `--workspace`. `cargo qpl` is a
`.cargo/config.toml` alias for `cargo run -p qpl-cli --`. Workspace-wide
checks need `--workspace` (`cargo clippy --workspace --all-targets`).

There is no separate lint step configured; use `cargo clippy --all-targets` and
`cargo fmt` as normal. `cargo clippy --workspace --all-targets` is expected to
be **warning-free**, as is `cargo clippy -p qpl --all-targets` for
`--no-default-features` and `--no-default-features --features wasm`, and
`cargo clippy -p qpl-std --all-targets --no-default-features` — check all
four before calling a change done.

Tests are colocated with the code they cover (`#[cfg(test)] mod tests` at the
bottom of each source file). There is no `tests/` directory. `vm.rs`,
`parser.rs`, `compiler.rs`, and `lexer.rs` carry the bulk of them. `repl.rs`
also has a `golden` test module that runs every `examples/*.qpl` and diffs
its captured output against `examples/golden/<name>.out`
(`UPDATE_GOLDEN=1 cargo test golden` regenerates them — only do this for a
deliberate, reviewed output change, never to make a red test green).

## Release process

Releases are fully automated by GitHub Actions and driven by the workspace
version in `Cargo.toml`. On a push to `main` that touches `Cargo.toml`,
`Cargo.lock`, `src/**`, `qpl-macros/**`, `qpl-std/**`, `qpl-cli/**` or
`qpl-wasm/**`,
[`.github/workflows/tag.yml`](.github/workflows/tag.yml) reads
`workspace.package.version` and pushes a `v<version>` tag. That tag triggers
[`release.yml`](.github/workflows/release.yml), which creates the GitHub release
and cross-compiles the `qpl-cli` package's `qpl` binary for Linux (gnu/musl),
Linux ARM64, and macOS (x86/ARM). **To cut a release, bump `version` in
`[workspace.package]` and merge to main** — nothing else.

## Architecture

A whole source file compiles and runs in one pass:

```
source file → parse_program (lexer + parser, whole file) → compile_program → Program { code: Vec<u8>, operands, lines } → Vm::run_compiled → EvalResult / printed output
```

`Program.code` is one byte per instruction (`Op`, `#[repr(u8)]`); `operands`
is a side stream that only `Op::Push` reads; `lines` maps an instruction
pointer back to `(path, line)` for error messages. A REPL line, a `\port`/IPC
request, or a `\l`/`\i` target is exactly the same thing on a smaller scale —
each compiles to its own small `Program` and either runs against the
session's `Vm` directly (REPL, IPC) or is embedded inside the including
script's `Program` as `Operand::Program(Arc<Program>)` and executed in place
by the `\l`/`\i` native (`\l`/`\i` targets are read, parsed and compiled
*when the including script is compiled*, not at run time — see "Namespaces"
below).

Entry points: `qpl_cli::run` (called by `qpl-cli/src/main.rs`) parses CLI args
(clap), constructs one long-lived `vm::Vm` (read-only unless `-w`) and
registers any Rust extensions, then calls into `qpl::repl`. `repl::run_script`
(a file) and `qpl-cli`'s interactive REPL loop both funnel source through
`parser::parse_program` → `compiler::compile_program` → `Vm::run_compiled`.
`vm::run_vm(source, vm)` — the entry point most tests use — compiles a
*single* statement (`parser::parse` + `compiler::compile`) in `Result` mode
and reduces it to an `EvalResult` via `Vm::eval`. State
carries across runs because the same `Vm` (its `globals` map and registers)
is reused.

### Key modules

Every module below lives in the `qpl` library (`src/`) unless said otherwise.
`qpl-cli/src/` holds the command line (`lib.rs`, `main.rs`, `interactive.rs`
— the terminal REPL loop), `qpl-wasm/src/` the browser bindings (`wasm.rs`,
`arrow_io.rs`), and `qpl-std/src/` the standard library (`str.rs`, `arr.rs`,
`fs.rs`, `env.rs`, one module per `.std` namespace, plus `lib.rs`'s
`extensions()`/`register()`). The library's own `wasm` feature enables
nothing but `vm::Vm`'s `capture_table`/`last_table` fields — the plumbing a
non-terminal front-end needs to get a `DataFrame` back instead of printed
text — and has no dependencies of its own; `qpl-wasm` enables it on its `qpl`
dependency.

| Module | Role |
|--------|------|
| `lexer` | source text → `Vec<Token>` (`tokens.rs` defines `Token`) |
| `parser` | tokens → `Stmt` / `TableExpr` / `Expr` AST (`ast.rs`); largest front-end file. `parse` compiles one statement (used by `run_vm`); `parse_program(src, path)` parses a whole file into `Vec<(line, Stmt)>` — statement splitting (`logical_statements`, `normalize_function_body_newlines`) lives here too. Owns the string-level `\` commands and `.qpl.cfg`/`log` as real `Stmt` variants (`Stmt::System`, `Stmt::Cfg`, `Stmt::Log`) rather than leaving them to `repl.rs` string-matching |
| `ast` | AST types. `Stmt` (assignment / bare expr / `Log`/`Cfg`/`System`), `TableExpr` (`Select` vs. `BuiltIn`), `SelectStmt` (unified select/update/delete via `update`/`delete` flags), `Expr`. `Value` includes `Table(DataFrame)` and `Lazy(LazyFrame)` (tables share the one binding map) and `Closure(Arc<program::Closure>)` |
| `builtins` | `BuiltIn` enum — non-select table operations: `cols`, `sink`, `sort`, `distinct`, `limit`, `drop`, `lazy`, `collect` |
| `compiler` | AST → `Program`. `compile`/`compile_stmt` (single statement) and `compile_program`/`CompileCtx` (whole file: `Script` mode prints each statement, `Result` mode leaves only the last statement's value on the stack) share `compile_tbl_expr`/`compile_value_expr` for the actual codegen. Also does compile-time `\i` namespace qualification (`qualify_top_level`/`collect_ns_names`) and `\l`/`\i` cycle detection (`CompileCtx::including`) |
| `program` | `Program` (`code`/`operands`/`lines`), `Op` (the one-byte opcode enum, stable discriminants — see "Invariants" below), `Operand` (everything `Op::Push` can push: `Name`, `Count`, `Target{ip,cp}`, `BinOp`, `Verb`, `Native`, `Sort`, `Cast`, `Window`, `Func`, `Text`, `Program`, …), `Closure`/`FuncProto`, `WindowFn`/`WindowSpec`, the disassembler (`\d`'s output), and `Program::to_bytes`/`from_bytes` — the `.qplc` bytecode-file format (`qpl -C`/`qpl script.qplc`, see the "Compiled artifacts" subsection below) |
| `codec` | Ungated (not `ipc`-gated): the lossless `ast::Value` binary codec (`encode_value`/`decode_value`, plus the shared `Reader` cursor) used by both `.qplc` operand encoding (`program.rs`) and the IPC wire format (`ipc.rs`) |
| `ops` | Type-dispatched semantics the opcode dispatch loop delegates to, so `vm.rs`'s `match` arms stay thin: scalar/vector/temporal binops and casts, verb application, `like`, natives that don't need a native-function slot to themselves (`til`, `enlist`, roll, `zip`, `log`, `hopen`/`whopen`/`await`, `dispatch` — `call_by_name` is the shadowable-by-name dispatch point) |
| `vm` | `Vm` (the single evaluator), `Slot` (stack entries), `CallFrame`-equivalent (`Slot::Call`), the `Op` decode/dispatch loop (`Vm::run_compiled`) — each arm small, delegating to `ops.rs`. `run_vm` (single-statement helper) and `EvalResult` also live here |
| `native` | Built-in (native) functions — a `name → Builtin` map built once in `Vm::new`, resolved through `Vm::lookup` exactly like a user function, except a builtin name can never be bound over. `NativeId` (`Enlist`/`Roll`/`Cfg`/…) is for the handful of primitives the compiler references *by id* instead of by name, so they can't be shadowed at all. Adding a name-resolved native needs no lexer/parser/compiler change |
| `ext` | Rust extensions: `Native` (implemented by `#[qpl::native(read\|iread\|write)]` from `qpl-macros`, which generates a same-named braced struct in the type namespace so the function stays callable), `Extension` (`Extension::new(ns).with::<f>()`), `Vm::register` (namespaced `.<ns>.<name>` entries in the builtin table as `NativeCall::Extension`, atomic, `qpl` reserved), and the `FromValue`/`IntoValue`/`IntoReturn` conversions. `Vm::call_builtin` converts slots ↔ values at the boundary; an `Err` becomes `<name>: <msg>` |
| `permission` | `Effect` (`Read`/`IRead`/`Write`) — what an action may change. `Read` reads session data only; `IRead` reads outside the session or changes it (`load`, assignment, `.qpl.cfg`, `\1`); `Write` changes state outside the session. `Vm::authorize(effect, what)` is the single permission check: a read-only session (`Vm::new`/`Vm::default` — the default; `Vm::new_writable` for `qpl -w`; fixed at construction, never changed) refuses `Write`; a request over a read-only IPC handle refuses `IRead` and `Write`. `Builtin` entries carry an `effect` that `Vm::call_builtin` checks before every call (`read0`/`read1` go through this); `load`, `sink`, assignment, `.qpl.cfg`, `\1`, `whopen`, `write0` and `write1` call `authorize` inline |
| `vm_config` | `VmConfig` — session knobs set by `.qpl.cfg key=value` (`maxcol`, `maxrow`, `tblwidth`, `strlen`, `round_type`, `useqepoch`); a new knob is a field + a `VmConfig::set` arm and nothing else |
| `errors` | `QplError` (Lex/Parse/Compile/Runtime variants) — the single error type threaded everywhere |
| `repl` | Source-running plumbing shared by every front-end: `run_script` (parse_program → compile_program → run_compiled for a whole file, aborting before any statement runs on a parse/compile error anywhere in it — including inside a `\l`/`\i` target; also runs a `.qplc` file straight from bytes, sniffed by magic number), `compile_script` (source → `Program`, no run — `qpl -C`), `run_command` (`qpl -c`), `eval_capture`/`eval_capture_table` (output captured instead of printed — the wasm REPL), `eval_for_dispatch` (`ipc` feature: evaluate one `\port`-dispatched command), demo tables, result formatting. `wants_more` (interactive-loop-only: brackets/trailing-comma/parse-cut-off) decides whether to keep reading a half-typed statement. All printing goes through `Vm::emit`, which mirrors to the stdout log. `\port` is an ordinary statement (`Vm::native_port` sets `Vm::port`); driving the listener — polling stdin and the request channel once a port is open — is `qpl-cli`'s job, not the library's |
| `interrupt` | Ctrl-C flag (`Interrupt`, an `Arc` of two atomics on `Vm`). `qpl-cli`'s handler sets it; the interpreter polls `vm.interrupt.check()` (each backward `JUMP`, `CALL`, around Polars `collect`s, IPC waits) and returns `QplError::Interrupted`. One `interrupt.statement()` guard covers a whole `run_source` call (a full script or one REPL line), however many statements it contains |
| `ipc` | `ipc` feature only (`#[cfg(feature = "ipc")]`, `pub mod ipc;` in `lib.rs` is itself gated). Client (`hopen`/`dispatch`/`async dispatch`/`await`) and server (`\port`) over a plain `zeromq` REQ/REP pair — see the IPC subsection below |

### VM state and evaluation model

`Vm` (in `vm.rs`) holds exactly one binding map plus a set of registers;
tables, lazy plans and functions share the map, and call frames on the stack
are the only scopes:

- `globals: HashMap<String, ast::Value>` — every session-level binding:
  scalars, vectors, `Value::Table` (an eager binding), `Value::Lazy` (a
  `lazy select ...` plan), `Value::Closure` (a user function). A namespaced
  name (`.lib.x`) is just a `globals` key with dots in it — there's no
  separate namespace map.
- `builtins: HashMap<String, Builtin>` — native functions from `native.rs`,
  built once in `Vm::new` and never mutated; a builtin name cannot be bound
  over.
- Registers: `prog: Arc<Program>` (the program currently executing), `ip`
  (instruction pointer into `prog.code`), `cp` (operand-stream cursor into
  `prog.operands`, advanced only by `Op::Push`), `fp: Option<usize>` (index
  into `stack` of the innermost `Slot::Call` frame), `call_depth` (count of
  live `Slot::Call` frames, checked against `MAX_CALL_DEPTH = 128`).
- `stack: Vec<Slot>` — everything else. `Slot` variants: `Expr` (a Polars
  column expression, query context), `Frame { lf, lazy }` (a table being
  built — laziness travels with the frame value itself: reading a
  `Value::Lazy` binding or `Op::Lazy` sets `lazy: true`, `Op::Collect`/`cols`
  clears it), `Scalar(ast::Value)`, `List(Vec<Expr>)` (a projection/key/
  predicate list under construction), `Operand(Operand)` (a value just
  pushed by `Op::Push` for the next opcode to consume), and `Call` (a
  function activation: return `(prog, ip, cp, fp)`, locals, return mode).

`CALL`/`RET` push/pop `Slot::Call` frames on the same stack instead of
recursing in Rust: `CALL` on a closure does an arity/depth/interrupt check,
pushes a `Slot::Call` with the args bound into its `locals`, and jumps to the
closure's entry point (switching `prog` if the closure belongs to a
different `Program`); `RET` pops the result, truncates the stack back to
`fp`, restores the caller's registers from the frame, and pushes the result.
Scoping stays lexical: a bare-name lookup (`Vm::lookup`) searches only the
innermost call frame's `locals`, then `globals` — never an enclosing caller's
frame. A jump target (`while`, `?[..]`, a closure's entry point, a call's
return address) is itself an operand, `Operand::Target { ip, cp }` — `JUMP`
sets both registers, so a loop body re-reads its own operands each time
around and a skipped branch skips its operands too.

`Vm::run_compiled(Arc<Program>)` is the single decode/dispatch loop (backing
both `Vm::eval`, used by `run_vm`/the REPL/scripts, and the `\l`/`\i`
natives' nested sub-program runs); `prog`/`ip`/`cp` are saved and restored
around a nested run so it composes freely with an already-active call. A
top-level statement's result reduces to `EvalResult`: `Table(DataFrame)`,
`Scalar(Value)`, `Lazy(String)` (an explained plan), or `Stored` (an
assignment — nothing to print).

Namespaces (`\i`) are resolved at **compile time**, not by any runtime
lookup order: `compiler::compile_program` pre-scans a `\i`-imported file's
top-level assignment names and rewrites every bare `STORE` target and every
bare reference to one of those names (anywhere in the file, including inside
function bodies, but not a param/local shadowing it) to its namespaced form
before compiling it. This means a bare name that the file binds at top level
*always* refers to the import's own binding, even in a statement that runs
before that binding's own statement does — a library can't read a
session-level `t` and rebind it with `t: select from t where ...`; it reads
its own (as-yet-unbound) `.lib.t` and fails. The runtime `\i` native does the
transaction: snapshot
`globals`, drop any existing `.lib.*` keys, run the embedded program, restore
the snapshot on error.

The virtual column `i` (row index) is compiled by inserting `Op::RowIndex`
right after any table source a query references `i` from — a compile-time
scan of the query, not a runtime flag.

### Invariants

Don't add surface area you don't need. These constraints keep the bytecode
simple to reason about and safe to extend:

- **One evaluator.** Everything — scalar maths, calls, closures, `while`,
  `?[..]`, lists, IPC — compiles to bytecode and runs through
  `Vm::run_compiled`'s single dispatch loop. There is no tree-walking
  fallback anywhere in the codebase.
- **Every instruction is exactly one byte.** `Op` is `#[repr(u8)]`; no
  opcode carries an inline operand.
- **`Op::Push` is the only reader of the operand stream.** Every other
  opcode takes all of its inputs from `stack`. The compiler emits operands
  in exactly the order the corresponding `Push`es execute on a straight-line
  path; a jump changes both `ip` and `cp` together so a re-executed region
  re-reads its own operands.
- **Opcode discriminants are stable.** Never renumber an existing `Op`
  variant — retire it (leave a past-tense comment noting what used to be
  there) instead of reusing its number; `program.rs` has a test pinning a
  handful of byte values specifically to catch accidental renumbering.
- **No execution state outside the stack, `globals`, and the registers.**
  No per-statement locals living directly on `Vm`, no separate table/lazy
  binding maps, no call-stack `Vec` of scopes.

**Adding a new opcode**: add an `Op` variant (append — don't renumber),
teach the disassembler (`Op`'s `Display`/short-form, `program.rs`) and the
dispatch loop (`vm.rs`'s big `match`, delegating any nontrivial semantics to
`ops.rs`) about it, and emit it from the compiler. Prefer parameterising an
existing opcode via a new `Operand` variant over adding a new opcode, and
prefer expressing a new language feature in terms of existing opcodes over
either. Opcode count is deliberately kept well under the 256 a single byte
allows.

**Adding a Rust extension function** (outside qpl itself): see `src/ext.rs`
and `book/src/extensions.md`. Extensions reach the VM only through `Native`
and `Vm::register`; don't give them `Slot`/`Vm` access, which would let one
bypass `authorize`.

**Adding a new native function**: add an entry to `native::builtins()` with
the `Effect` it has — `Write` for anything that changes state outside the
session, so a read-only session refuses it; `IRead` for anything that reads
outside the session or changes the session itself, so a read-only IPC handle
refuses it (or, if it must never be shadowable and the compiler can
reference it directly without a name lookup, a `NativeId` variant) — no
lexer/parser/compiler change needed for a name-resolved native.

**Adding a new statement form** (a new `\` command, a new top-level
directive): add an `ast::Stmt` variant, parse it in `parser::parse_program`'s
top-level loop, lower it in `compiler::compile_program_stmt` (typically to a
native call), and implement the native in `vm.rs`/`ops.rs`.

Add `#[cfg(test)]` cases in each file you touch and, where it's a
user-visible feature, a runnable snippet under `examples/` and a note in
`README.md`.

**Every user-visible language change (new keyword, operator, or builtin) must
also update [`tools/vscode/`](tools/vscode/)** — this is not optional cleanup,
do it in the same change:
- `src/vocabulary.json` — the single source of truth for the vocabulary: add the
  keyword/operator to the relevant list (`statementKeywords`, `builtinKeywords`,
  `joinOperators`, `wordOperators`, `aggregates`) and give it an entry in
  `keywordDetail`/`aggregateDetail`. `src/vocabulary.ts` is a typed re-export of
  this file and needs no edit; `qpl-wasm` `include_str!`s the same file for
  `qplLangConfig()`, so both editors stay in sync automatically.
- `syntaxes/qpl.tmLanguage.json` — add it to the matching grammar rule so it
  highlights (validate with `python3 -c "import json; json.load(open(...))"`).
- `src/extension.ts` — only if the new vocabulary list isn't already wired
  into the completion provider's loops.
- `snippets/qpl.json` — add a snippet if the feature has a common invocation
  shape worth autocompleting.
- `README.md` — mention it in the feature list.
- Verify with `npx tsc -p ./ --noEmit` from `tools/vscode/`.

Don't touch `CHANGELOG.md`/version bumps for this — those are a separate,
maintainer-driven release step, not tied to individual language changes.

**Pre-0.2: document the current design, not its history.** While the version
is `0.1.x`, a breaking change is a revision of the original design, not a
change users need telling about. User-facing docs (`book/`, `README.md`,
`examples/`) describe how qpl works *now*, as if it had always worked that
way. No "used to", "no longer", "was removed", "previously", or migration
notes. Explaining *why* the design is the way it is is fine (e.g. "qpl
doesn't accept kdb's `1 x` because…"). This changes from `0.2` onwards,
when breaking changes need documenting.

The same goes for code comments at any version: describe what the code does
and why, never how it used to work ("regression: X used to…", "now an
ordinary statement", "replaces the old…"). Keep them as short as possible;
don't restate the code or cross-reference every caller. The one exception is
a retired `Op` number, which keeps a one-line note of what it was (see
"Invariants").

### IPC (`ipc` feature)

On by default (drop it with `--no-default-features`); `zeromq`/`tokio` are
`optional` deps in `Cargo.toml`, pulled in only by `ipc = ["dep:tokio", "dep:zeromq"]`
and enabled by default via `default = ["ipc"]`. This is the one place the codebase
is not fully synchronous, and it's deliberately confined: `Vm` itself is never
shared across threads (the one exception is `interrupt`, an `Arc` of atomics that only the Ctrl-C handler touches; no `Mutex` anywhere) — every connection's
worker thread (client) and the listener thread (server, `\port`) only ever
exchange owned `String`/`Vec<u8>` values over `std::sync::mpsc`, and the *only*
thread that ever calls into `Vm::run_compiled`/`vm::run_vm` is the main
REPL thread, exactly as if the request had been typed locally. See `ipc.rs`'s
module doc for the full design.

`hopen`/`await`/`dispatch` are ordinary natives and an opcode, not special
parser productions: `hopen`/`whopen`/`await` are resolved by name through
`ops::call_by_name` (the same fallback `til`/`log` go through once neither a
user closure nor a builtin-table entry matches), and `<conn> [async] dispatch
<cmd>` compiles to `Op::Dispatch`, which pops the connection, the payload
text and the async flag off the stack. `ast::Value` has two variants,
`Handle`/`Future`, for connection/pending-response handles — not
`#[cfg]`-gated themselves (that would force every exhaustive `match` over
`Value` elsewhere to grow a `#[cfg]` arm too), only the code that produces
them is.

The wire response mirrors `vm::EvalResult` (`ipc::encode_result`/`decode_response`):
a table serialises via the existing Parquet writer/reader (already linked for
`load`/`sink`, no new Polars feature), a scalar via the shared `codec::encode_value`/
`decode_value` tag+payload encoding for every `ast::Value` variant (no serde
dependency) — see "Compiled artifacts" below for the other consumer of that
codec. The server side (`\port`, `repl::eval_for_dispatch`) compiles each
incoming request as a `Result`-mode whole program and rejects `Stmt::System`
(a `\` command) outright — those are local-only; `Cfg` and `Log`
statements are allowed remotely (subject to the connection's read/write
mode).

### Compiled artifacts (`qpl -C` / `qpl -c`, `.qplc`)

`qpl -C script.qpl [-o out.qplc]` compiles a script to a `.qplc` file and
exits without running it (`repl::compile_script` + `Program::to_bytes`,
written via a temp file + rename so a failed compile never leaves a partial
artifact); `qpl script.qplc` (or any file whose first bytes are the `"QPLC"`
magic — `repl::run_script` sniffs this, not the extension) runs it directly
via `Program::from_bytes` + `Vm::run_compiled`, with no lexing, parsing or
compiling at all. `\l`/`\i` targets are already embedded sub-`Program`s,
so a `.qplc` never needs its original source files — it's a fully
self-contained artifact, not a security boundary, exactly equivalent to
running the source. `qpl -c '<command>'` runs a short ad hoc command and
exits (`repl::run_command`, source text through the same
parse→compile→run path as a script, reported unprefixed like `<main>` REPL
input); `-C`, `-c`, `-d`, `-i`, and a `file` argument all conflict with each other
in `qpl-cli`'s clap `Cli` (`qpl-cli/src/lib.rs`).

`Program::to_bytes`/`from_bytes` (`program.rs`) serialise `code`/`operands`/
`lines` (recursing into an embedded `Operand::Program`), gated by a `u16
format_version` (`FORMAT_VERSION`). **Any change to `Op`'s discriminants, to
`Operand`'s on-disk tags, to `NativeId`'s tags, or to `codec::ValueTag`'s
discriminants must bump `FORMAT_VERSION`** — a stale `.qplc` then fails with a clean "compiled with
an incompatible qpl" error instead of misdecoding. `from_bytes` never panics:
a truncated file, an unknown opcode/operand/value tag, an out-of-range jump
target, or trailing garbage all come back as `Err` (see `program.rs`'s
`.qplc` serialisation tests). The value codec itself (`codec::encode_value`/
`decode_value`) is lossless for everything that can appear in an
`Operand::Value` — every scalar and vector `Value` variant, nulls included
(a per-element validity flag); `Table`/`Lazy`/
`Closure`/`Handle`/`Future` have no operand encoding (a closure literal is
always `Operand::Func`, never a baked-in runtime closure) and `to_bytes`
errors rather than writing garbage if one somehow reached an operand.

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`qpl` (Quick Polars Query Language) is a CLI interpreter for a kdb+/q-inspired
query language that compiles to Polars **lazy** frames. It is a single Rust
binary with no runtime dependencies (no Python). The language surface, examples,
and rationale are documented in [README.md](README.md) and [examples/](examples/) —
read those for language semantics; this file covers build/dev workflow and
internal architecture.

## Commands

```bash
cargo build                 # debug build
cargo build --release       # release build (large: bundles all of Polars)
cargo test                  # run all tests (unit tests live in #[cfg(test)] modules per source file)
cargo test <name>           # run tests matching a substring, e.g. `cargo test lazy`
cargo test --package qpl parser::   # run one module's tests
cargo run                   # start the REPL
cargo run -- --load-demo    # REPL preloaded with demo `trades` / `quotes` tables
cargo run -- script.qpl     # execute a script
cargo run -- -i script.qpl  # execute a script, then drop into the REPL
cargo run -- examples/lazy_join_pipeline.qpl   # run an example
```

There is no separate lint step configured; use `cargo clippy` and `cargo fmt` as normal.

Tests are colocated with the code they cover (`#[cfg(test)] mod tests` at the
bottom of each `src/*.rs`). There is no `tests/` directory. `vm.rs`,
`parser.rs`, `compiler.rs`, and `lexer.rs` carry the bulk of them.

## Release process

Releases are fully automated by GitHub Actions and driven by the version in
`Cargo.toml`. On a push to `main` that touches `Cargo.toml`, `Cargo.lock`, or
`src/**`, [`.github/workflows/tag.yml`](.github/workflows/tag.yml) reads
`package.version` and pushes a `v<version>` tag. That tag triggers
[`release.yml`](.github/workflows/release.yml), which creates the GitHub release
and cross-compiles binaries for Linux (gnu/musl), Linux ARM64, and macOS
(x86/ARM). **To cut a release, bump `version` in `Cargo.toml` and merge to main** —
nothing else.

## Architecture

The pipeline is a classic interpreter, one line of source at a time:

```
source line → tokenise (lexer) → parse (parser) → AST (ast) → compile (compiler) → Vec<Instruction> → run_vm (vm) → EvalResult
```

Entry points: `main.rs` parses CLI args (clap) and constructs one long-lived
`vm::Vm`, then calls into `repl.rs`. `repl::run_script` and `repl::start` both
funnel every non-comment line through `repl::match_run_vm` → `vm::run_vm`, which
re-runs the whole tokenise→parse→compile→execute chain for that line. State
carries across lines because the same `Vm` is reused.

### Key modules

| Module | Role |
|--------|------|
| `lexer` | source text → `Vec<Token>` (`tokens.rs` defines `Token`) |
| `parser` | tokens → `Stmt` / `TableExpr` / `Expr` AST (`ast.rs`); largest front-end file |
| `ast` | AST types. `Stmt` (assignment vs. bare expr), `TableExpr` (`Select` vs. `BuiltIn`), `SelectStmt` (unified select/update/delete via `update`/`delete` flags), `Expr` |
| `builtins` | `BuiltIn` enum — non-select table operations: `cols`, `sink`, `sort`, `distinct`, `limit`, `drop`, `lazy`, `collect` |
| `compiler` | AST → `Vec<Instruction>`. Stack-machine codegen; `compile_select` is the core |
| `opcodes` | `Instruction` enum + `disassemble_instructions` (the `\d` REPL command). `Display` impls are the disassembly format |
| `enums` | `PolarsFrameExpr` / `PolarsStackArg` — thin wrappers over Polars ops (join type, filter, sort, distinct, limit, drop) referenced from instructions |
| `vm` | executes instructions against a `StackObj` stack, building a Polars `LazyFrame`; holds all interpreter state |
| `repl` | REPL loop, script runner (`logical_statements` folds indented continuation lines into one statement; the interactive loop instead uses `wants_more` — brackets/trailing-comma/parse-cut-off — to decide whether to keep reading), demo tables, result formatting. Also home to the string-level features that never reach the VM: `\` system commands (`\d` disassemble, `\l <path>` run a script, `\1 <path>` stdout log) and the `log` / `1` stdout-write (`parser::parse_expr_seq` parses its space-separated args, each rendered via `eval_scalar` and concatenated). All printing goes through `Vm::emit`, which mirrors to the stdout log |
| `errors` | `QplError` (Lex/Parse/Compile/Runtime variants) — the single error type threaded everywhere |

### VM state and evaluation model

`Vm` (in `vm.rs`) holds three maps that persist for the session:

- `tables: HashMap<String, DataFrame>` — materialised named tables
- `lazy_frames: HashMap<String, LazyFrame>` — stored **query plans** from `lazy` bindings; nothing runs until `collect` or `sink`
- `globals: HashMap<String, Value>` — scalar variables

(plus `stdout_log: Option<File>` — the `\1` stdout mirror; not query state.)

The VM executes instructions by pushing/popping a `StackObj` stack (`Expr`,
`Frame`, `Scalar`, `PolarsArg`). Everything table-shaped is assembled as a
Polars `LazyFrame` and only `.collect()`-ed at the end unless the statement is
lazy. `run_vm` returns an `EvalResult`: `Table(DataFrame)`, `Scalar(Value)`,
`Lazy(String)` (an explained plan, printed instead of a table), or `Stored`
(an assignment — nothing to print).

**Scalars are evaluated in Rust, not Polars.** `Vm::eval_scalar` folds
literal/global-only expressions to a `Value`; at query time those values are
injected as Polars `lit(...)` so `threshold: 150` composes with column
expressions in later queries.

The virtual column `i` (row index) is `PushIColRef` / `Expr::IColRef`, aliased
to `x` in output per q convention.

## Making language changes

**Keep the VM small.** The single most important constraint on this codebase is
avoiding bloat in `vm.rs` (and the instruction set it executes). Before adding a
new `Instruction` or a new match arm in the VM, exhaust the alternatives: can an
existing instruction be parameterised, can the work be done in the compiler or
parser instead, can it reuse an existing `PolarsFrameExpr` / `BuiltIn` /
`StackObj` path? A change should touch **only what is absolutely necessary** and
reuse as much of the existing machinery as possible. New VM surface area is a
last resort, not a default.

A new operator or keyword usually touches the chain end to end: `lexer` (token),
`parser` (grammar → AST), `ast`/`builtins` (new node if needed), `compiler` (emit
instructions), `opcodes` (new `Instruction` + `Display`), `vm` (execute it) —
but prefer to stop as early in that chain as you can.
Add `#[cfg(test)]` cases in each file you touch and, where it's a user-visible
feature, a runnable snippet under `examples/` and a note in `README.md`.

**Every user-visible language change (new keyword, operator, or builtin) must
also update [`tools/vscode/`](tools/vscode/)** — this is not optional cleanup,
do it in the same change:
- `src/vocabulary.ts` — add the keyword/operator to the relevant list
  (`STATEMENT_KEYWORDS`, `BUILTIN_KEYWORDS`, `JOIN_OPERATORS`, `WORD_OPERATORS`,
  `AGGREGATES`) and give it an entry in `KEYWORD_DETAIL`/`AGGREGATE_DETAIL`.
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

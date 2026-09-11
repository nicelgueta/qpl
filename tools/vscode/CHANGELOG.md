# Changelog

## 0.3.0

- Completion: `src/vocabulary.ts` (keywords, aggregates, cast types, join
  operators — single source of truth, kept in sync by hand with the TextMate
  grammar) and `src/docScan.ts` (regex scan of the open document for
  assignment targets and table references).
- Context-gated completions: cast type names right after `$`; table names
  after `from`/`by`/`drop`/`collect`/`sink`/`load` (document-defined tables,
  `<<`-loaded paths, and the configurable `qpl.demoTables`); statement and
  builtin keywords plus `\d`/`\1`/`\l`/`log` at the start of a line;
  aggregates and document-defined names everywhere else.
- Snippets (`snippets/qpl.json`): `sel`, `selby`, `upd`, `del`, `delcols`,
  `join`, `lazyp`, `cond`, `over`.
- New setting `qpl.demoTables` (default `["trades", "quotes"]`).
- Grammar: `.qpl.*` namespaced builtin functions (e.g. `.qpl.cfg`) share the
  `keyword.other.qpl` scope, so they colour the same as `load` / `sink` / `cols`.
  Dropped the removed `show` keyword; added `round` to the aggregate/function
  group. Window functions: `over` is a control keyword; `rn` / `rank` /
  `drank` colour with the aggregates. All bare identifiers share one scope
  (`variable.other.qpl`) so a declared variable, its later references, and
  column names/references all render in the same colour. Symbols (`` `hello ``)
  now use `entity.name.type.symbol.qpl` — themed like a class name. `<<` / `>>`
  share the `keyword.other.qpl` scope with the builtin keywords instead of an
  operator scope. q-style operator functions `?` `$` `::` `!` `#` use
  `support.function.operator.qpl` so they colour like `sum` / `avg`; comparison
  and arithmetic operators keep `keyword.operator.qpl`. `$` in a `type$expr`
  cast is coloured as an operator function too (the type name stays
  `support.type.cast.qpl`).

## 0.2.0

- Interactive REPL. **Ctrl+Enter** (Cmd+Enter on macOS) runs the current
  selection — or the whole file when nothing is selected — in a persistent
  `qpl REPL` terminal; state carries across runs. Multi-line input is sent via
  the interpreter's new `\l` run-script command. Commands: *qpl: Start REPL*,
  *qpl: Restart REPL*, *qpl: Run File or Selection in REPL*.
- Binary discovery: `qpl.path` setting → `qpl` on `PATH` →
  `target/release|debug/qpl` in the workspace → `cargo run`. New settings
  `qpl.path`, `qpl.loadDemo`.
- The extension now has a TypeScript build (`npm install && npm run compile`).

## 0.1.0

- M1: TextMate grammar and language configuration for `.qpl` files.
  Highlighting for comments, strings, symbols/symbol-vectors, numeric/bool
  literals, statement and builtin keywords, aggregates, cast types, join
  operators, `<<`/`>>` channels, assignments, `\d`/`\1` REPL lines, and the
  virtual `i` column. No JavaScript yet.

# Changelog

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

## Unreleased

- All bare identifiers share one scope (`variable.other.qpl`) so a declared
  variable, its later references, and column names/references all render in the
  same colour.
- Symbols (`` `hello ``) now use `entity.name.type.symbol.qpl` — the scope themes
  colour like Python class names.
- `<<` / `>>` share the `keyword.other.qpl` scope with the builtin keywords
  (`load`, `sink`, `lazy`, ...) instead of an operator scope.
- q-style operator functions `?` `$` `::` `!` `#` use `support.function.operator.qpl`
  so they colour the same as `sum` / `avg`. Comparison (`!=`, `<=`, ...) and
  arithmetic operators keep `keyword.operator.qpl`.
- `$` in a `type$expr` cast is coloured as an operator function too (the type name
  stays `support.type.cast.qpl`).

## 0.1.0

- M1: TextMate grammar and language configuration for `.qpl` files.
  Highlighting for comments, strings, symbols/symbol-vectors, numeric/bool
  literals, statement and builtin keywords, aggregates, cast types, join
  operators, `<<`/`>>` channels, assignments, `\d`/`\1` REPL lines, and the
  virtual `i` column. No JavaScript yet.

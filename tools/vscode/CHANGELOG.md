# Changelog

## Unreleased

- All bare identifiers share one scope (`variable.other.qpl`) so a declared
  variable, its later references, and column names/references all render in the
  same colour.
- Symbols (`` `hello ``) now use `entity.name.type.symbol.qpl` — the scope themes
  colour like Python class names.
- `<<` / `>>` share the `keyword.other.qpl` scope with the builtin keywords
  (`load`, `sink`, `lazy`, ...) instead of an operator scope.

## 0.1.0

- M1: TextMate grammar and language configuration for `.qpl` files.
  Highlighting for comments, strings, symbols/symbol-vectors, numeric/bool
  literals, statement and builtin keywords, aggregates, cast types, join
  operators, `<<`/`>>` channels, assignments, `\d`/`\1` REPL lines, and the
  virtual `i` column. No JavaScript yet.

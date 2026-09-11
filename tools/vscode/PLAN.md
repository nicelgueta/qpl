# qpl VSCode Extension — Plan

Syntax highlighting + autocomplete for the `qpl` query language.

## Approach

Two capabilities in one extension, no language server to start:

1. **Syntax highlighting** — a TextMate grammar (`syntaxes/qpl.tmLanguage.json`) +
   `language-configuration.json`. Declarative, zero runtime.
2. **Autocomplete** — a `CompletionItemProvider` in `src/extension.ts` combining a
   static vocabulary (keywords, aggregates, cast types) with a live regex scan of
   the open document for user-defined table / scalar names, plus snippets.

Structured so an optional **LSP / WASM phase** (real diagnostics + schema-aware
column completion, backed by the repo's actual `lexer`/`parser` compiled to WASM)
can be added later without reworking phase 1.

## Language surface (from `src/lexer.rs`, `src/tokens.rs`, `README.md`)

| Category | Tokens |
|---|---|
| Statement keywords | `select by from where over order asc desc distinct limit drop update delete` |
| Builtin keywords | `load sink cols lazy collect` |
| Aggregates (lexed as `Name`) | `sum avg mean min max count first last std dev var med median abs neg not string distinct n_unique round rn rank drank` |
| Cast types (after `$`) | `f64 float f32 i64 int i32 i16 i8 u64 u32 u16 u8 bool str string` |
| Join operators (infix `Name`) | `lj ij rj` |
| Operators | `+ - * % = <> != < <= >= > & \| $ ? :: : ! #`; special `<<` (load), `>>` (sink); `?[c;t;e]` conditional |
| Literals | int `42`, float `3.14`, bool `1b`/`0b`, bool-vec `1010b`, string `"..."` (escapes `\n \t \r \" \\`), symbol `` `AAPL ``, symbol-vec `` `a`b`c `` |
| Comments | `/` to end of line — **single slash only**, full-line and inline |
| Virtual column | `i` (row index, aliased `x` in output) |
| Assignment | `name: expr` |
| System / REPL | `\d <stmt>`, `\1 <path>`, `log <expr>`, `1 <expr>` |
| Continuation | lines indented >=1 tab or >=4 spaces continue the previous statement |

## File tree

```
tools/vscode/
├── package.json
├── language-configuration.json
├── syntaxes/qpl.tmLanguage.json
├── snippets/qpl.json
├── src/
│   ├── extension.ts        activate(): register completion providers
│   ├── vocabulary.ts       static keyword/aggregate/casttype tables (single source of truth)
│   └── docScan.ts          regex extraction of table & scalar names from a TextDocument
├── test/
│   ├── grammar.test.ts     vscode-tmgrammar-test cases
│   └── completion.test.ts  @vscode/test-electron integration tests
├── .vscodeignore
├── tsconfig.json
├── README.md
└── CHANGELOG.md
```

## TextMate grammar — scopes

- `comment.line.slash.qpl` — `/.*$`
- `string.quoted.double.qpl` + `constant.character.escape.qpl` for `\\[ntr"\\]`
- `entity.name.type.symbol.qpl` — `` `[A-Za-z0-9_./-]* `` (repeats for `` `a`b`c ``); themed like a class name
- `constant.numeric.bool.qpl` — `\b[01]+b\b`
- `constant.numeric.float.qpl` — `\b\d+\.\d*`
- `constant.numeric.integer.qpl` — `\b\d+\b`
- `keyword.control.qpl` — statement keywords
- `keyword.other.qpl` — `load sink cols lazy collect` and `.qpl.*` builtin functions (`.qpl.cfg`)
- `support.function.aggregate.qpl` — aggregate names
- `support.function.operator.qpl` — q-style operator functions `? $ :: ! #` (same colour as aggregates)
- `support.type.cast.qpl` — cast type name in `type$expr`
- `keyword.operator.join.qpl` — `\b(lj|ij|rj)\b`
- `keyword.other.qpl` — `<<` `>>` (with the builtin keywords)
- `keyword.operator.qpl` — arithmetic / comparison / logical (`+ - * % = < > & | <> != <= >=`)
- `variable.language.index.qpl` — `\bi\b`
- `variable.other.qpl` — assignment targets **and** every other bare identifier
  (column names/refs, table refs), so declaration and use share a colour
- `keyword.other.repl.qpl` — `^\s*(\\[d1]|log)\b`

Line-oriented language: no multi-line begin/end rules except strings. The bare
`#identifier` catch-all is last in the pattern list so keywords, aggregates,
casts, joins and `i` win first.

## language-configuration.json

- `comments.lineComment`: `/`
- `brackets` / `autoClosingPairs` / `surroundingPairs`: `[]` `()` `""`
- `` ` `` in `surroundingPairs` only (not auto-closing — breaks symbol vectors)
- `wordPattern` includes `` ` `` and `_`
- `onEnterRules`: continue indentation after a line ending in `,` or an already-indented line

## Autocomplete (`src/extension.ts`)

One `CompletionItemProvider` for language id `qpl`:

1. **Static items** (`vocabulary.ts`): keywords; aggregates (`Function`, with detail);
   cast types (`TypeParameter`) — only when `linePrefix.endsWith('$')`; join ops after
   a `` `col `` phrase.
2. **Document-derived** (`docScan.ts`, `Variable`/`Struct`): assignment targets
   `/^\s*([A-Za-z_]\w*)\s*:/m`; `load` / `<<` / `from` operands as table names; demo
   tables `trades`, `quotes` (configurable `qpl.demoTables`).
3. **Context gating** on `linePrefix`: after `from`/`by`/`drop`/`collect`/`sink` -> table
   names; after `$` -> cast types; start of line -> keywords + `\d` `\1` `log`; else
   identifiers seen in doc + aggregates.
4. **Trigger characters**: space, `$`, `` ` ``, `:`.

## Snippets

`sel`, `selby`, `upd`, `del`, `join`, `lazyp` (README lazy pipeline), `cond` (`?[...]`).

## package.json contributions

- `languages`: id `qpl`, ext `.qpl`, `configuration`
- `grammars`: scopeName `source.qpl`
- `snippets`
- `configuration`: `qpl.demoTables` (string[]), `qpl.enableAggregateCompletions` (bool),
  reserved `qpl.serverPath` (future LSP)
- `activationEvents`: `onLanguage:qpl`

## Milestones

1. **M1 — Highlighting (no JS): shipped `0.1.0`.** grammar + language-configuration.
2. **REPL — shipped `0.2.0`.** `src/extension.ts` (TypeScript build). Ctrl+Enter
   runs the file/selection in a persistent `qpl REPL` terminal; multi-line input
   goes through the interpreter's `\l` run-script command. Binary discovery:
   `qpl.path` → `PATH` → `target/release|debug/qpl` → `cargo run`.
3. **M2 — Static completion + snippets: shipped `0.3.0`.** `vocabulary.ts`, `snippets/qpl.json`,
   `$`-gated cast completion, start-of-line keyword completion.
4. **M3 — Document-aware completion: shipped `0.3.0`.** `docScan.ts`, table-name extraction
   (assignments, `from`/`load`/`sink`/`<<` operands, configurable `qpl.demoTables`), context
   gating (`from`/`by`/`drop`/`collect`/`sink`/`load` → tables).
5. **M4 — Tests + packaging:** `vscode-tmgrammar-test` snapshots, `@vscode/test-electron`
   tests, README screenshots. `1.0.0`.
6. **M5 (optional) — Semantic layer:** compile real `lexer`/`parser` to WASM for a
   `DocumentSemanticTokensProvider` + `DiagnosticCollection` from `QplError`; optionally
   shell out to the `qpl` binary (`cols <table>`) for column completions. (WASM is
   only needed for browser VSCode / a web playground — the desktop extension shells
   out to the native binary.)

## Testing

- **Grammar:** `vscode-tmgrammar-test` assertions covering comments (full + inline),
  strings + escapes, symbol/bool vectors, `$cast`, `<<`/`>>`, `lj/ij/rj`, assignments,
  `\d`/`\1`. Snapshot every `examples/*.qpl`.
- **Completion:** integration tests — cast types only after `$`, keywords at line start,
  demo tables after `from`, user tables after an earlier assignment.
- **CI:** separate GitHub Actions job (`xvfb-run` for electron tests); not coupled to the
  release-tag workflow.

## Open questions

- In-repo (`tools/vscode/`) vs separate repo — going in-repo for now.
- Invest in WASM semantic layer vs grammar-only long-term — grammar-only through `1.0.0`.

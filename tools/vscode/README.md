# qpl for VSCode

Editor support for [`qpl`](../../README.md) — the Quick Polars Query Language.

## Features

### Syntax highlighting

`.qpl` files get highlighting for comments (`/`), double-quoted strings with
escapes, symbols and symbol vectors (`` `a`b`c ``), integer / float / bool /
bool-vector literals, statement keywords (`select`, `by`, `from`, `where`,
`over`, `order`, `asc`, `desc`, `distinct`, `limit`, `drop`, `update`, `delete`),
builtin keywords (`load`, `sink`, `cols`, `lazy`, `collect`) and `.qpl.*`
builtin functions (`.qpl.cfg`), aggregates (`sum`, `avg`, `count`, `round`, ...),
cast types (`f64$x`), the operator functions `?` `$` `::` `!` `#`, join
operators (`lj`, `ij`, `rj`), assignments, `\d` / `\l` / `\1` REPL lines, and
the virtual `i` column. Plus line comments,
bracket matching and auto-closing pairs.

### Completion & snippets

- Statement/builtin keywords and REPL commands (`\d`, `\1`, `\l`, `log`) at the
  start of a line; aggregates (`sum`, `avg`, `round`, `rn`, `rank`, ...)
  everywhere.
- Cast type names (`f64`, `int`, `str`, ...) right after `$`.
- Table names after `from` / `by` / `drop` / `collect` / `sink` / `load`:
  tables assigned or referenced in the open document, plus the configurable
  `qpl.demoTables` (default `trades`, `quotes`).
- Variables assigned in the open document (`name: ...`).
- Snippets: `sel`, `selby`, `upd`, `del`, `delcols`, `join`, `lazyp`, `cond`,
  `over`.

### Interactive REPL

- **Ctrl+Enter** (Cmd+Enter on macOS) in a `.qpl` file runs the **selection**,
  or the **whole file** when nothing is selected, in a persistent `qpl REPL`
  terminal. State (tables, variables) carries across runs, like a Python REPL.
- Multi-line / multi-statement input is handed to the interpreter via `\l`
  (run-script), so its own continuation-folding and comment handling apply.
- Commands (Command Palette): **qpl: Start REPL**, **qpl: Restart REPL**,
  **qpl: Run File or Selection in REPL**.

#### Finding the `qpl` binary

The extension resolves, in order:

1. the `qpl.path` setting, if set;
2. `qpl` on your `PATH`;
3. `target/release/qpl` or `target/debug/qpl` in the workspace;
4. `cargo run --quiet --manifest-path <workspace>/Cargo.toml --` when a qpl
   `Cargo.toml` is present (handy while hacking on qpl itself).

Set `qpl.loadDemo` to start the REPL with the demo `trades` / `quotes` tables.
Set `qpl.demoTables` to change which table names completion offers by default
(alongside whatever it finds in the open document).

## Developing

```bash
cd tools/vscode
npm install
npm run compile          # or: npm run watch
# then press F5 in VSCode to launch an Extension Development Host with the
# ../../examples folder open. `npm: compile` runs automatically before launch.
```

## Packaging

`vsce` (Visual Studio Code Extensions) turns the folder into a distributable
`.vsix`:

```bash
npm install -g @vscode/vsce   # one-time
vsce package                  # runs `npm run compile`, then writes qpl-<version>.vsix
```

`vsce package` reads [package.json](package.json), runs the `vscode:prepublish`
script (`tsc`), bundles every file not excluded by [.vscodeignore](.vscodeignore),
and validates the manifest. Install the result with **Extensions view → ··· →
Install from VSIX…** or `code --install-extension qpl-<version>.vsix` — no
Marketplace account needed.

To publish to the Marketplace you need a `publisher` matching an Azure DevOps
organisation and a Personal Access Token: `vsce login <publisher>` then
`vsce publish`.

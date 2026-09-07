# qpl for VSCode

Editor support for [`qpl`](../../README.md) — the Quick Polars Query Language.

## Status

**M1 — syntax highlighting only.** Autocomplete, snippets, and diagnostics are
planned; see [PLAN.md](PLAN.md).

## Features (0.1.0)

- Highlighting for `.qpl` files: comments (`/`), double-quoted strings with
  escapes, symbols and symbol vectors (`` `a`b`c ``), integer / float / bool /
  bool-vector literals, statement keywords (`select`, `by`, `from`, `where`,
  `order`, `asc`, `desc`, `distinct`, `limit`, `drop`, `update`, `delete`),
  builtin keywords (`load`, `sink`, `cols`, `show`, `lazy`, `collect`),
  aggregates (`sum`, `avg`, `count`, ...), cast types after `$` (`f64$x`),
  join operators (`lj`, `ij`, `rj`), the `<<` / `>>` channel operators,
  assignments (`name: expr`), `\d` / `\1` REPL lines, and the virtual `i`
  column.
- Line comments, bracket matching, and auto-closing pairs.

## Developing

```bash
cd tools/vscode
# open this folder in VSCode and press F5 to launch an Extension Development Host,
# then open any file under ../../examples/ to see the grammar in action.
```

No build step for M1 — the grammar and language configuration are pure JSON.

## Packaging

`vsce` (Visual Studio Code Extensions) is Microsoft's CLI for turning an
extension folder into a distributable `.vsix` archive and, optionally,
publishing it to the Marketplace.

```bash
npm install -g @vscode/vsce   # one-time: install the packaging CLI globally
vsce package                  # produces qpl-<version>.vsix in this folder
```

`vsce package` reads [package.json](package.json) (name, version, publisher,
`engines.vscode`, `contributes`), bundles every file not excluded by
[.vscodeignore](.vscodeignore), and validates the manifest. The resulting
`qpl-0.1.0.vsix` can be shared directly and installed with **Extensions view →
··· → Install from VSIX…** or `code --install-extension qpl-0.1.0.vsix` — no
Marketplace account needed.

To publish to the Marketplace instead, you need a `publisher` matching an Azure
DevOps organisation and a Personal Access Token: `vsce login <publisher>` then
`vsce publish` (which also bumps the version if you pass `patch`/`minor`/`major`).

For M1 there is no compile step — the grammar and language configuration are
plain JSON, so `vsce package` is the entire build.

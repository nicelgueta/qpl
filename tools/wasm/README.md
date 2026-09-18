# qpl in the browser (`wasm` feature)

`src/wasm.rs` exposes the interpreter to JavaScript through `wasm-bindgen`. It is
a front-end over the same library the CLI uses (`src/lib.rs`), not a
reimplementation: `Repl.eval` calls `repl::eval_capture`, which wraps the same
`run_line` that `repl::start` feeds from rustyline.

> **Status: it builds and links.** `wasm-pack` produces a working `pkg/`
> (`qpl_bg.wasm`, `qpl.js`, `qpl.d.ts`), but only against a **patched Polars** —
> stock Polars 0.55.2 cannot be compiled for `wasm32-unknown-unknown`. The
> patch is four small changes, kept in
> [`patches/`](patches/polars-0.55.2-wasm32-unknown-unknown.patch) and applied
> for you by `make wasm`. The bundle has not yet been exercised in a browser.

## What's exported

| Export | Notes |
|--------|-------|
| `new Repl()` | one long-lived `Vm`; state carries across `eval` calls exactly as it does across REPL lines |
| `repl.eval(line)` | `{ output, error }` — `output` is what the CLI would print to stdout, `error` is `null` or the message it would print to stderr |
| `repl.wantsMore(src)` | the CLI's `qpl) ` vs `  ...  ` rule: is this statement unfinished? |
| `repl.loadDemo()` | binds the demo `trades` / `quotes` tables (`--load-demo`) |
| `repl.version()` | interpreter version, for a banner |
| `qplLangConfig()` | editor configuration — see below |

`ipc` is off in this build, so `hopen` / `dispatch` / `await` / `\port` don't
exist. Anything that touches the filesystem (`load`, `sink`, `\l`, `\i`, `\1`)
surfaces as an ordinary qpl runtime error rather than breaking the session.

```js
import init, { Repl, qplLangConfig } from './pkg/qpl.js';

await init();
const repl = new Repl();
repl.loadDemo();

const { output, error } = repl.eval('select from trades where price > 150');
console.log(error ?? output);
```

## `qplLangConfig()`

Everything an online editor needs to give qpl the treatment
[`tools/vscode/`](../vscode/) gives it, built from the *same*
`tools/vscode/src/vocabulary.json` the extension imports — the Rust side
`include_str!`s that file, so the two cannot drift apart. (This is why the
vocabulary lives in a `.json` and `vocabulary.ts` is a thin typed re-export of
it.) `language-configuration.json` and `snippets/qpl.json` are reused the same
way.

```js
{
  id: 'qpl',
  extensions: ['.qpl'],
  aliases: ['qpl', 'QPL'],
  configuration: { /* IMonaco LanguageConfiguration, regex fields already RegExp */ },
  monarch:       { /* IMonarchLanguage, incl. tokenizer */ },
  completions:   [ { label, kind, detail, insertText?, insertTextRules? } ],
  vocabulary:    { statementKeywords: [...], aggregates: [...], /* ... */ },
}
```

`kind` and `insertTextRules` come back as *names* (`'Keyword'`, `'Snippet'`,
`'InsertAsSnippet'`) rather than the numeric enum values, which change between
Monaco versions. Wiring it up:

```js
const cfg = qplLangConfig();
monaco.languages.register({ id: cfg.id, extensions: cfg.extensions, aliases: cfg.aliases });
monaco.languages.setLanguageConfiguration(cfg.id, cfg.configuration);
monaco.languages.setMonarchTokensProvider(cfg.id, cfg.monarch);
monaco.languages.registerCompletionItemProvider(cfg.id, {
  provideCompletionItems(model, position) {
    const word = model.getWordUntilPosition(position);
    const range = {
      startLineNumber: position.lineNumber, endLineNumber: position.lineNumber,
      startColumn: word.startColumn, endColumn: word.endColumn,
    };
    return {
      suggestions: cfg.completions.map((c) => ({
        ...c,
        range,
        kind: monaco.languages.CompletionItemKind[c.kind],
        insertText: c.insertText ?? c.label,
        insertTextRules: c.insertTextRules
          ? monaco.languages.CompletionItemInsertTextRule[c.insertTextRules]
          : undefined,
      })),
    };
  },
});
```

`cfg.vocabulary` is the raw lists, for anything else the host wants to build
(a hover provider, a toolbar, a cheat sheet).

### Why a Monarch grammar and not the TextMate one?

The extension's `syntaxes/qpl.tmLanguage.json` can be run in a browser, but only
via `monaco-textmate` + `vscode-oniguruma`, which means shipping a *second* wasm
blob (the Oniguruma regex engine) and an async bootstrap. Monarch is Monaco's
native tokenizer, needs no extra runtime, and qpl's lexical surface is small
enough to express in it rule-for-rule. If you already run a TextMate/Oniguruma
setup for other languages, use the `.tmLanguage.json` directly and take only
`configuration` / `completions` / `vocabulary` from here.

## Building

```bash
make wasm
```

That's [`scripts/build-wasm.sh`](../../scripts/build-wasm.sh), which does the
whole thing: clones Polars if you don't have it, checks out a worktree at the
`rs-<version>` tag Cargo.toml pins, applies
[`patches/`](patches/polars-0.55.2-wasm32-unknown-unknown.patch), installs the
`wasm32-unknown-unknown` target and `wasm-pack` if missing, points cargo at the
patched checkout, and builds into `tools/wasm/pkg/` (gitignored). It's
idempotent — a re-run skips straight to the build. Slow on a cold cache: the
whole Polars tree, for a new target.

Three environment variables adjust it:

| | |
|---|---|
| `POLARS_REPO` | a Polars clone to take the worktree from (default `../polars`) |
| `POLARS_WASM_SRC` | where the patched worktree lives (default `../polars-wasm-<ver>`) |
| `WASM_PACK_ARGS` | extra `wasm-pack` args, e.g. `--dev` |

Two things it has to do that are easy to get wrong by hand:

* **Every** Polars crate is redirected at the patched checkout, not just the
  four the patch touches. A patched crate resolves its siblings by path, so
  leaving the rest on crates.io puts two copies of `polars-arrow` in the graph
  and the build dies in a wall of *"expected `PlSmallStr`, found
  `PlSmallStr`"* type mismatches.
* `RUSTFLAGS='--cfg getrandom_backend="wasm_js"'` is required on this target.
  Polars' own `make -C crates check-wasm` sets the same flag.

The `[patch.crates-io]` block holds absolute, machine-specific paths, so it is
appended to `Cargo.toml` only for the duration of the build and removed
afterwards (`Cargo.lock` too) — including on `^C`. Nothing to commit and
nothing to clean up.

If Polars is bumped without the patch being rebased onto the new `rs-<version>`
tag, `make wasm` stops with a message saying so rather than building against a
stale checkout.

`wasm-opt` is disabled in `Cargo.toml`: wasm-pack's bundled binaryen rejects
the `memory.copy` rustc emits (it predates bulk-memory being on by default),
and even with `--enable-bulk-memory` an `-O` pass over a Polars-sized module
takes far longer than the compile. The commented-out flag list next to it is
the working incantation when the ~22 MB unoptimised module matters.

The JS-facing logic is unit-tested on the host target, where it compiles
without any of the above:

```bash
cargo test --no-default-features --features wasm
```

Note that `[profile.release]` sets `panic = "abort"`; a panic inside Polars
therefore traps the wasm instance rather than unwinding, and the `Repl` object
has to be recreated. Build with a profile that unwinds if you'd rather turn
those into catchable JS exceptions.

## Why stock Polars doesn't build

Polars 0.55.2 — and `main`, as of `fe841f959e` — does not build for
`wasm32-unknown-unknown`, the target `wasm-bindgen` needs. Everything below is
what the patch in [`patches/`](patches/) fixes.

### What upstream actually does

Polars has two wasm stories, and neither is the one we want:

* **`wasm32-unknown-emscripten`**, for Pyodide. This is the *supported* one, but
  it is `polars-python` built through `maturin`, with Emscripten's own JS glue —
  `wasm-bindgen` has no part in it. Even there, the build deletes a long list of
  features first (`crates`' CI does
  `csv|ipc|ipc_streaming|parquet|async|scan_lines|json|extract_jsonpath|catalog|cloud|polars_cloud|tokio|clipboard|decompress|new_streaming`).
  The workflow that tests it, `.github/workflows/test-pyodide.yml`, is currently
  parked on a branch that doesn't exist (`disabled-pyodide-tests`), so nothing
  runs it.

* **`wasm32-unknown-unknown`**, via `make -C crates check-wasm`. That target does
  exist and it names the exact feature surface upstream considers wasm-safe — it
  runs `cargo hack check --each-feature` with `async`, `aws`, `azure`, `cloud`,
  `decompress`, `default`, `docs-selection`, `extract_jsonpath`, **`fmt`**,
  `gcp`, **`csv`**, `ipc`, `ipc_streaming`, `json`, `nightly`, **`parquet`**,
  `performant`, `streaming`, `http`, `full` and `test` all excluded, under
  `RUSTFLAGS='--cfg getrandom_backend="wasm_js"'`. It is **not wired into CI**
  (nothing in `.github/workflows/` invokes it), and it has bit-rotted: running
  even `cargo check --target wasm32-unknown-unknown -p polars
  --no-default-features` on `main`, with Polars' own pinned toolchain, fails in
  `mio` (*"This wasm target is unsupported by mio"*).

Two things to take from that list. `fmt` is excluded because it pulls
`comfy-table/tty` → `crossterm`, which doesn't build for wasm — so qpl has to
switch to Polars' `fmt_no_tty` feature, which still gives `DataFrame: Display`
and so still gives us `repl.rs`'s `df.to_string()`. And `--each-feature` checks
features *one at a time*, so even the un-excluded ones aren't known to work in
the combination qpl needs.

### The five concrete blockers

Each verified here by building the dependency graph for
`wasm32-unknown-unknown`:

1. **`polars-core` depends on `tokio` with `net` + `fs`.** `tokio`'s build
   script rejects both on wasm (*"Only features sync, macros, io-util, rt, time
   are supported on wasm"*), and `net` pulls `mio`, which fails outright. This
   arrived with `58eae590be` ("fix: Rayon deadlock with re-entrant io sources",
   2026-05-13) and is what broke `check-wasm`. `polars-core`'s own source never
   mentions `tokio` — but the dependency is not dead: `polars-io` declares
   `tokio = { workspace = true }` with *no* features and relies on Cargo
   unification to hand it `fs`/`sync`/`time` from here. So gating it behind
   `cfg(not(target_family = "wasm"))` is a one-line manifest fix that has to be
   paired with (4).
2. **`polars-async` builds a multi-threaded tokio runtime** (`new_multi_thread`,
   `enable_io`, `task::block_in_place`), none of which exist on wasm. Needs a
   `new_current_thread` variant — about 15 lines.
3. **`polars-core`'s `THREAD_POOL` is only defined for non-wasm and
   emscripten**, so a plain-wasm build fails with `cannot find value
   THREAD_POOL`. The emscripten arm (one rayon thread, `use_current_thread`)
   works verbatim; it just needs its `cfg` widened.
4. **`polars-io` calls `tokio::fs`** in `utils/byte_source.rs` and
   `utils/mkdir.rs`. Both call sites have a `std::fs` equivalent.
5. **`polars`'s `csv` and `parquet` features force `streaming`**, which pulls
   `polars-stream` → `polars-io/file_cache` → `cloud` → `object_store`,
   `hyper`, `reqwest`, `rustls` and `ring`. `ring`, `zstd-sys` and `lz4-sys`
   are C/assembly builds needing a wasm C toolchain. This is why upstream
   excludes `csv` and `parquet` outright rather than fixing it.

With (1)-(4) patched, `fmt` swapped for `fmt_no_tty`, and `csv`/`parquet` left
off, the whole of qpl compiles, links and passes through `wasm-bindgen` — so
the ceiling here was upstream packaging, not anything about qpl or about lazy
evaluation. qpl's own side of (5) and the `fmt` swap is the per-target feature
split in `Cargo.toml` plus the `cfg` on `vm::load_file` / `vm::sink_file`.

On the runtime side, the single-threaded, eager environment is fine: Polars
already degrades to a serial code path on wasm (`RAYON::install` calls the
closure directly), and qpl's lazy bindings are just stored query plans — the
in-memory engine runs them on `collect` / `sink` like any other target. Only
*scans* of remote/cloud paths are genuinely unavailable, and a browser has no
filesystem to scan anyway.

### Options from here

- **Carry the patch** (what the build instructions above do). All four changes
  are worth upstreaming — upstream already *wants* this target to work
  (`check-wasm` exists), it just isn't tested. Getting `check-wasm` into CI is
  arguably the higher-leverage contribution, since it would stop the target
  rotting again.
- **Target `wasm32-unknown-emscripten`**, the one Polars supports. This costs
  `wasm-bindgen`: the JS glue would have to come from Emscripten
  (`ccall`/Embind), `src/wasm.rs` would need a matching `extern "C"` surface,
  and the build needs `emsdk` + a matching LLVM. Note that even Polars' own
  Pyodide job is disabled, so this path isn't currently exercised either.
- **Wait for upstream**, then drop the patch and the `[patch.crates-io]` block.
  Nothing in `src/wasm.rs` or the feature split changes when that happens.

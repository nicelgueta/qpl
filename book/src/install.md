# Install

If you already have a Rust toolchain, building from source is one command:

```bash
cargo install --path .
```

Be warned that Polars is a large crate and a cold build takes a few minutes.
If you'd rather not wait, or don't have Rust installed at all, there are
prebuilt binaries on the
[releases page](https://github.com/nicelgueta/qpl/releases) for Linux (both
gnu and musl), Linux ARM64, and macOS on both Intel and Apple silicon.

Either route leaves you with a single `qpl` executable and nothing else. No
interpreter to keep in step with it, no wheel to install, no virtual
environment to remember to activate. That property is deliberate rather than
incidental: a lot of the appeal of a tool like this is being able to drop it
into a container, a CI job, or a colleague's laptop as easily as `jq`.

## The IPC feature

qpl can act as a client and server, so that one qpl process can query the
tables held in another running one. That's built in by default. If you don't
want the two extra dependencies it pulls in (`tokio`, `zeromq`), drop it:

```bash
cargo install --path . --no-default-features
```

Nothing about that is a dead end, since you can rebuild with the feature
whenever the need shows up. [IPC](language/ipc.md) covers what it does, near
the end of the book.

## Checking it worked

```bash
qpl --version
```

Once that responds, [Quickstart](quickstart.md) has you querying data within
a couple of lines.

## Editor support

### VSCode extension

The [VSCode extension](https://marketplace.visualstudio.com/items?itemName=nicelgueta.qpl)
(`nicelgueta.qpl` on the Marketplace) adds syntax highlighting, autocomplete
for keywords/builtins/table and column names, and a Ctrl+Enter binding that
sends the current line or selection to a REPL running alongside the editor —
install it and go straight to editing `.qpl` files with the interpreter one
keystroke away.

### Try it in a browser, no install

qpl also compiles to WebAssembly, and that build is running live at
[FastBoard](https://nicelgueta.github.io/fastboard/board/) — free to try with
nothing to install. Open the board, add a **Code Editor** widget, and set its
language to `qpl`. The editor is Monaco, the same editor VSCode is built on,
so it comes with the same syntax highlighting and autocomplete as the
extension above, straight from the wasm-compiled interpreter.

You can also add a **Data Table** widget alongside it, upload your own CSV or Parquet, and link the Code Editor's target table to it to route query results straight
into the table — a way to poke at qpl against real data without leaving the
browser tab. Because the interpreter is compiled to wasm and runs entirely
client-side, in a Web Worker, nothing you upload is ever sent to a server —
there isn't one for this to talk to. Query it, chart it, close the tab, and
none of it left your machine. Check the network tab under dev tools if you don't believe me. 

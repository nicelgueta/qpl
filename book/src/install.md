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

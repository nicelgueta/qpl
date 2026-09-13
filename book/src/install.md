## Install

```bash
cargo install --path .
```

Or grab a pre-built binary from [Releases](https://github.com/nicelgueta/qpl/releases). Add `--features
ipc` for the [IPC](language/ipc.md) client/server (`hopen`/`dispatch`/`\port`) — off by
default, and pulls in no extra dependencies unless enabled.

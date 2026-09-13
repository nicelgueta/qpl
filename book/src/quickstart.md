## Quickstart

```bash
qpl                 # REPL, with demo tables `trades` and `quotes` preloaded
qpl script.qpl      # run a script
qpl -i script.qpl   # run a script, then drop into the REPL
```

```qpl
qpl) select sym, price from trades where price > 200
qpl) select avg price by sym from trades
qpl) t: select from trades where size > 100     / bind a table
qpl) t sink "big.parquet"                       / write it out
```

Runnable scripts are in [`examples/`](examples/) — e.g.
`qpl examples/lazy_join_pipeline.qpl`.

Editor support (syntax highlighting + a Ctrl+Enter REPL) is in
[`tools/vscode/`](https://github.com/nicelgueta/qpl/tree/main/tools/vscode).

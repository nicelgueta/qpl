# qpl examples

Runnable `.qpl` scripts. All paths assume you run them from the repo root.

| Script | What it shows |
|--------|---------------|
| [`basics.qpl`](basics.qpl) | Core language: projection, aliases, scalar vars, `where`, `by` aggregation, `?[...]` conditional, `order`, casts |
| [`config_and_round.qpl`](config_and_round.qpl) | `.qpl.cfg` session knobs (`maxrow`/`maxcol`/`round_type`) and the `round` column function |
| [`window_functions.qpl`](window_functions.qpl) | `<expr> over `key`, and the ranking verbs `rn` / `rank` / `drank` with an `order` sub-clause |
| [`multiline.qpl`](multiline.qpl) | Statements spanning multiple lines via tab / 4-space indentation |
| [`logging.qpl`](logging.qpl) | `log` / `1` stdout writes and `\1 <path>` stdout mirroring |
| [`lazy_and_collect.qpl`](lazy_and_collect.qpl) | `lazy` to keep a query plan, extend it by re-assignment, print the plan, `collect` to a table |
| [`lazy_join_pipeline.qpl`](lazy_join_pipeline.qpl) | A full pipeline that scans two parquet files, joins, derives + aggregates + sorts, and sinks to parquet **without ever collecting** |
| [`setup_data.qpl`](setup_data.qpl) | Regenerates the sample parquet inputs (already committed under `data/`) |
| [`namespaces.qpl`](namespaces.qpl) | `\i "<path>"` imports [`namespace_lib.qpl`](namespace_lib.qpl), namespacing its bindings under `.namespace_lib.*` |
| [`ipc_server.qpl`](ipc_server.qpl) / [`ipc_client.qpl`](ipc_client.qpl) | `hopen` / `dispatch` / `async dispatch` / `await` / `\port` — requires `--features ipc`, two processes |


## Sample data

`data/trades.parquet` and `data/quotes.parquet` are small extracts of the demo
tables that ship with the REPL (`qpl --load-demo`). They're not committed so the
scripts run out of the box. To recreate them:

```bash
cargo run -- --load-demo examples/setup_data.qpl
```


## Running

```bash
cargo run -- examples/basics.qpl
cargo run -- examples/lazy_and_collect.qpl
cargo run -- examples/lazy_join_pipeline.qpl
```

Or, with an installed binary:

```bash
qpl examples/lazy_join_pipeline.qpl
```


`lazy_join_pipeline.qpl` writes `data/market_summary.parquet` when it runs.


## Namespaces & imports

```bash
cargo run -- --load-demo examples/namespaces.qpl
```

`\i "<path>"` works the same nested inside a script (as above) or typed
directly at the REPL prompt:

```bash
cargo run -- --load-demo
qpl) \i "examples/namespace_lib.qpl"
qpl) .namespace_lib.double 21
qpl) log .namespace_lib.greeting
qpl) .namespace_lib.lookup
```

## IPC (optional feature)

`ipc_server.qpl` / `ipc_client.qpl` need two terminals and the `ipc` feature:

```bash
# terminal 1 — server: runs the script, then drops into the REPL (`-i`)
cargo run --features ipc -- -i --load-demo examples/ipc_server.qpl
qpl) \port 5001

# terminal 2 — client
cargo run --features ipc -- examples/ipc_client.qpl
```

# qpl examples

Runnable `.qpl` scripts. All paths assume you run them from the repo root.

| Script | What it shows |
|--------|---------------|
| [`basics.qpl`](basics.qpl) | Core language: projection, aliases, scalar vars, `where`, `by` aggregation, `$[...]` case, `order`, casts |
| [`lazy_and_collect.qpl`](lazy_and_collect.qpl) | `lazy` to keep a query plan, extend it by re-assignment, print the plan, `collect` to a table |
| [`lazy_join_pipeline.qpl`](lazy_join_pipeline.qpl) | A full pipeline that scans two parquet files, joins, derives + aggregates + sorts, and sinks to parquet **without ever collecting** |
| [`setup_data.qpl`](setup_data.qpl) | Regenerates the sample parquet inputs (already committed under `data/`) |


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

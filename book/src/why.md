## Why?

I often need to query large parquet files on cloud storage under time
pressure, or knock out a quick transform job. DuckDB is great for this, but I
always forget the syntax — and prompting an agent for it is often slower than
just writing the query myself.

So: a language fast enough to type without thinking, but with DuckDB-level
performance. As a bonus, one that LLM agents can drive easily too — and can't
do much damage with, sandboxed or not.

### Polars
Writing a DuckDB-grade engine from scratch solo isn't realistic, so I cheated:
Polars (a dataframe library written in Rust) is the backend. Its API is clean
enough that the "language" is really just a VM translating instructions into
Polars queries — which left me free to build whatever front-end I wanted.

That front-end borrows from kdb+/q, a language I'd been lightly exposed to at
work and wanted an excuse to actually learn, while still getting to write Rust.

Thus: `qpl`.

### Example

Read two parquet files, left-join them, derive a couple of columns, tag every
row with a conditional, dictionary-encode a key, aggregate, sort, and write the
result back out.

DuckDB:

```sql
CREATE TYPE sym_t AS ENUM (SELECT DISTINCT sym FROM read_parquet('trades.parquet'));
COPY (
    SELECT
        CAST(t.sym AS sym_t)        AS csym,
        t.side,
        CASE WHEN t.size >= 1000 THEN 'large'
             WHEN t.size >= 250  THEN 'mid'
             ELSE 'small' END       AS band,
        SUM(t.price * t.size)       AS tot,
        AVG(q.ask - q.bid)          AS avg_spread,
        COUNT(*)                    AS n
    FROM read_parquet('trades.parquet') t
    LEFT JOIN read_parquet('quotes.parquet') q USING (sym)
    WHERE t.price > 0
    GROUP BY csym, t.side, band
    ORDER BY tot DESC
) TO 'summary.parquet' (FORMAT PARQUET);
```

qpl — the whole thing is one statement:

```qpl
select tot: sum price * size, avg_spread: avg ask - bid, n: count price
    by csym: `$sym, side, band: ?[size >= 1000; `large; size >= 250; `mid; `small]
    from load "trades.parquet" `sym lj load "quotes.parquet" `sym where price > 0
    order tot desc
    sink "summary.parquet"
```

But the same pipeline is more naturally built up **one statement at a time** in
the REPL — see [Working incrementally](working-incrementally.md):

```qpl
j: select sym, side, price, size, bid, ask from load "trades.parquet" `sym lj load "quotes.parquet" `sym where price > 0
j  / check the table so far
j: update spread: ask - bid, notional: price * size from j
/ try the next step without committing — don't assign, just look
update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from j
/ happy with it — now assign to persist
j: update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from j
cols j                                     / check the schema so far
select tot: sum notional, avg_spread: avg spread, n: count price by csym: `$sym, side, band from j order tot desc
j sink "summary.parquet"
```

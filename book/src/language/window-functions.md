# Window functions

`` <expr> over `key `` computes `<expr>` per partition and broadcasts the
result back to every row (SQL `<expr> OVER (PARTITION BY key)`). Any column
expression or aggregate works:

```qpl
select sym, price, top: max price over `sym from trades
select sym, gap: (max price over `sym) - price from trades   / `over` binds tighter than `-`
```

Partition keys are backtick symbols — one (`` `sym ``) or several (`` `sym`day ``).

An `order` sub-clause (space-separated `` `col asc|desc `` pairs, per-column
direction) enables the ranking verbs, which stand alone in place of `<expr>`:

| Verb | SQL | Ties |
|---|---|---|
| `rn`    | `row_number()` | broken by row order — strict `1..n` |
| `rank`  | `rank()`       | share the lowest rank, then a gap (`1,1,3`) |
| `drank` | `dense_rank()` | share a rank, no gap (`1,1,2`) |

```qpl
select emp, country, role,
    seat: rank over `country`role order `desk asc `date desc,
    seniority: rn over `country order `hired asc
    from staff
```

The ranking verbs **require** `order`. Any other aggregate **may** take it: with
an `order` sub-clause the partition is sorted before the aggregate runs, so
`cumsum` / `diff` / `ffill` and friends compose in a defined order (a single
direction applies to every key — mixed asc/desc is ranking-verb only):

```qpl
select sym, ts, px,
    run:  cumsum px over `sym order `ts asc,       / running total in time order
    prev: lag px    over `sym order `ts asc         / previous row's price
    from trades
```

**Rolling windows** — a trailing `rolling <n>` sub-clause turns the aggregate
into a fixed `n`-row rolling reduction over the ordered partition
(`sum`/`avg`/`min`/`max`/`std`/`var`/`median`):

```qpl
select sym, ts, px, ma5: avg px over `sym order `ts asc rolling 5 from trades
```

The first `n-1` rows of each partition are `null` (the window isn't full yet).

**Virtual column `i`** is the row index (aliased to `x` in output, per q):

```qpl
select i, sym from trades
select from trades where i < 5
```

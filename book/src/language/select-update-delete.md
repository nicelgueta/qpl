# select / update / delete

```
select <cols> from <table-expr> [by <keys>] [where <preds>] [order <col> <asc|desc>, ...]
```

```qpl
select from trades
select sym, price from trades
select px: price, qty: size from trades          / aliasing
select avg price by sym from trades              / group-by aggregation
select total: sum size by sym from trades where side = "buy"
select from trades where size > 100
select from trades where size > 100, price < 400  / comma-separated preds = AND
select from trades where (size > 400) | (side = "buy")   / & / | for and / or
select from trades where 10100000b               / boolean-vector mask
select from trades order sym asc, price desc
```

`from` takes any table expression, not just a table name — a nested `select`, a
`load`, `distinct`, `lazy`, and so on all work, and this composes with the
table operators below too:

```qpl
select from select price from trades where price > 100  / from a nested select
cols select from trades where size > 100                / cols on a nested select
select sym, price from load "data/trades.parquet"        / from a load directly
```

A join's right side (`` `sym lj/ij/rj <table> `sym ``) is a bare table name or
`load "path"` by default; wrap it in parens to join against any other table
expression — the parens give the parser an explicit end point, otherwise a
nested select's own join would swallow the outer `right_on` symbols:

```qpl
select price, bid from trades `sym lj quotes `sym                            / bare name
select price, bid from trades `sym lj (distinct quotes) `sym                 / any table expr, parenthesised
select price, bid from trades `sym lj (select sym, bid from quotes where bid > 0) `sym
```

`update` returns the whole table with the named columns replaced or added:

```qpl
update price: price * 2 from trades
update price: price * 2 by sym from trades where size > 100
update notional: price * size from trades where price > 0   / new column; null where the filter misses
```

With a `where`, rows that don't match keep the column's old value — or `null` if
it's a brand-new column.

`delete` removes rows (with `where`) or columns (with a symbol list):

```qpl
delete from trades where size < 100
delete `price`size from trades
```

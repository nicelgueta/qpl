# Assignment

`:` binds a name. The right-hand side decides what kind of binding it is:

```qpl
threshold: 150                                 / scalar
t: select from trades where size > threshold   / table
lvl: `low`mid`high                             / symbol vector
px: trades`price                               / column expression → a list
top: max trades`price                          / reduction → a scalar
```

Scalar variables are evaluated in Rust and substituted into later queries as
Polars literals, so they compose transparently with column expressions. See
[Column expressions & lists](column-expressions.md) for `` table`col ``,
reductions, slicing (`n#`) and indexing.

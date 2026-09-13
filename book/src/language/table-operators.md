# Table operators

```qpl
distinct select sym from trades

10 limit select from trades      / first N rows
10#select from trades            / `#` is the same
10#trades

`price`size drop select from trades   / drop columns
`price`size _ trades                  / `_` is the same

`sym`price!01b trades            / sort by a `col!bool` map (0 asc, 1 desc)
sorted: `sym`price!01b select from trades where size > 100
```

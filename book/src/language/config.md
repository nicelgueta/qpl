# Config

A few behaviours are properties of the session rather than of any one
statement. `.qpl.cfg` sets them, taking one or more `key=value` pairs:

```qpl
.qpl.cfg maxrow=50 maxcol=20
```

A bare `.qpl.cfg` prints the current settings, which is useful when a result
looks unexpected and you want to check what's in effect. Both forms work in
scripts and in the REPL. Over [IPC](ipc.md), only a write handle may change
settings; a read-only client can still print them.

The knobs are:

| Key | Meaning | Default |
|---|---|---|
| `maxcol` | how many columns are printed when rendering a table | `8` |
| `maxrow` | how many rows are printed when rendering a table | `10` |
| `tblwidth` | how many characters wide a printed table may be, `-1` for unlimited | `-1` |
| `strlen` | how many characters of a cell are shown before an ellipsis, `-1` for unlimited | `30` |
| `round_type` | rounding mode for `round`: `HALF_UP` or `HALF_TO_EVEN` | `HALF_TO_EVEN` |
| `useqepoch` | read/write raw integers at the `timestamp` boundary as ns since kdb's `2000.01.01` rather than the Unix epoch | `false` |

`maxrow`, `maxcol`, `tblwidth` and `strlen` affect display only. They never
change a result, only how much of it reaches your terminal, which is why the
demo tables in this book print in full at eight rows and would start eliding
at eleven. Raise them while exploring something wide or long — `tblwidth=200`
for a table with many columns, `strlen=-1` when a string column is being cut
off mid-value.

`round_type` is different, because it changes an answer. It decides what
`round` from [Expressions](expressions.md) does with a value sitting exactly
on a boundary:

```qpl
.qpl.cfg round_type=HALF_UP
select mv: 2 round price from trades
```

The default, `HALF_TO_EVEN`, rounds a tie to the nearest even digit, which is
what most programming languages and IEEE 754 do, and which avoids the upward
bias you get from always rounding ties away from zero. `HALF_UP` is the
convention most people are taught at school and the one many accounting rules
require. Neither is more correct in general, so if the numbers you produce
have to agree with somebody else's, set this explicitly at the top of the
script rather than inheriting a default.

`useqepoch` also changes answers, and is covered where it matters, in
[Temporal types](temporal-types.md): it only affects a raw integer crossing
the `timestamp` boundary (`` `timestamp$n ``, `` `long$ts ``), choosing
between the Unix epoch (the default) and kdb's `2000.01.01`.

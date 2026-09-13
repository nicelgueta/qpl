# Config

`.qpl.cfg key=value ...` sets session-wide knobs. A bare `.qpl.cfg` prints the
current settings. Works in scripts and the REPL.

| Key | Meaning | Default |
|---|---|---|
| `maxcol` | max columns physically printed when rendering a table | `8` |
| `maxrow` | max rows physically printed when rendering a table | `10` |
| `round_type` | rounding mode for `round`: `HALF_UP` or `HALF_TO_EVEN` | `HALF_TO_EVEN` |

```qpl
.qpl.cfg maxrow=50 maxcol=20
.qpl.cfg round_type=HALF_UP
select mv: 2 round market_value from trades
```

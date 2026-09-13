## REPL

| Command | Action |
|---|---|
| `\d <stmt>` | disassemble — show bytecode without executing |
| `\l <path>` | run a `.qpl` script in the current session |
| `\1 <path>` | tee all stdout to `<path>` (bare `\1` detaches) |
| `log <expr>` / `1 <expr>` | print a scalar |
| `cols <name>` | show a table's schema |
| Ctrl-C | abandon a partial statement (or exit at an empty prompt) |
| Ctrl-D | exit |

```
qpl) \d select avg price by sym from trades where size > 100
0000: FROM_SRC InMem("trades")
0001: PUSH_COL_REF size
0002: PUSH_CONST Int(100)
0003: BIN_OP >
0004: FILTER 1
0005: PUSH_COL_REF sym
0006: ALIAS Some("sym")
0007: BUILD_KEYS 1
0008: PUSH_COL_REF price
0009: CALL avg 1
0010: ALIAS Some("price")
0011: BUILD_PROJ 1
0012: SELECT_BY
0013: RESULT
```

# Operator reference

| Operator | Meaning |
|---|---|
| `+` `-` `*` | arithmetic |
| `%` | division (q convention) |
| `=` `<>` `!=` | equality |
| `<` `<=` `>` `>=` | comparison |
| `&` `\|` | logical and / or |
| `?[...]` | vectorised conditional |
| `round` | `<precision> round <col>` — round a float column (mode: `.qpl.cfg round_type`) |
| `over` | window: `<expr> over `p [order `k asc]`; verbs `rn` / `rank` / `drank` |
| `$` | cast (`f64$x`, `` `date$x ``, `"p"$s`); `` `$x `` -> categorical |
| `.qpl.d` `.qpl.t` `.qpl.p` `.qpl.n` | now: date / time / timestamp / timespan (UTC) |
| `!` | `col!bool` sort map; `` u8!`$x `` -> categorical physical width; `` `k1`k2!v1 v2 `` -> dict literal |
| `::` | enum cast (`` lvl::`$x ``) |
| `#` | limit (`10#t`); take / slice a list (`3#l`, `-3#l`) |
| `[...]` | positional index into a list (`l[0]`, `l[1 2 3]`) |
| `_` | drop columns (`` `a`b _ t ``) |
| `hopen` `` `w!hopen `` `dispatch` `async dispatch` `await` | IPC client — see [IPC](ipc.md) (`--features ipc`) |
| `<list> where <pred>` | elementwise filter on a list; `x` is the current element |
| `til` | `til n` -> `0..n-1`; `lo til hi` -> `lo..hi-1` |
| `zip` | `zip `k1`k2!v1 v2` — build a table from a dict of named lists |

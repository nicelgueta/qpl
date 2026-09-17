# Operator reference

Every operator in the language on one page, for when you know what you want
and need the spelling. The right-hand column points at the chapter that
explains it.

| Operator | Meaning | |
|---|---|---|
| `+` `-` `*` | arithmetic | [Expressions](expressions.md) |
| `%` | division, since `/` is a comment | [Expressions](expressions.md) |
| `=` `<>` `!=` | equality and inequality | [Expressions](expressions.md) |
| `<` `<=` `>` `>=` | comparison | [Expressions](expressions.md) |
| `&` `\|` | logical and / or | [Expressions](expressions.md) |
| `/` | comment to end of line | [Comments](comments.md) |
| `:` | bind a name; also alias a column in a query | [Assignment](assignment.md) |
| `` `x `` | a symbol | [Symbols](symbols.md) |
| `?[...]` | vectorised conditional | [Expressions](expressions.md) |
| `like` | glob pattern match | [Expressions](expressions.md) |
| `round` | `<n> round <col>`, mode from `.qpl.cfg round_type` | [Expressions](expressions.md) |
| `quantile` `shift` `lag` `lead` `diff` `pctchange` | parameter-on-the-left verbs | [Expressions](expressions.md) |
| `over` | window: `` <expr> over `p [order `k asc] [rolling n] `` | [Window functions](window-functions.md) |
| `rn` `rank` `drank` | ranking verbs, require `order` | [Window functions](window-functions.md) |
| `i` | virtual row-index column, printed as `x` | [Window functions](window-functions.md) |
| `$` | cast: `f64$x`, `` `date$x ``, `"p"$s` | [Casts](casts.md), [Temporal types](temporal-types.md) |
| `` `$x `` | intern a string as a symbol; in a query, cast to categorical | [Symbols](symbols.md), [Categoricals & enums](categoricals-enums.md) |
| `::` | enum cast, `` lvl::`$x `` | [Categoricals & enums](categoricals-enums.md) |
| `!` | dict literal; sort map `` `col!01b ``; `` u8!`$x `` code width | [Column expressions](column-expressions.md), [Table operators](table-operators.md) |
| `#` | first n rows of a table; take from a list (`3#l`, `-3#l`) | [Table operators](table-operators.md), [Column expressions](column-expressions.md) |
| `[...]` | index into a list (`l[0]`, `l[1 2 3]`); call a function | [Column expressions](column-expressions.md), [Functions](functions.md) |
| `_` | drop columns, `` `a`b _ t `` | [Table operators](table-operators.md) |
| `where` | filter rows in a query; filter a list elementwise, where `x` is the element | [select](select-update-delete.md), [Column expressions](column-expressions.md) |
| `til` | `til n` gives `0..n-1`; `lo til hi` gives `lo..hi-1` | [Column expressions](column-expressions.md) |
| `zip` | build a table from a dict of named lists | [Column expressions](column-expressions.md) |
| `lj` `ij` `rj` | left / inner / right join | [select](select-update-delete.md) |
| `{...}` | lambda | [Functions](functions.md) |
| `.qpl.dt` `.qpl.tm` `.qpl.ts` `.qpl.dlta` | current date / time / timestamp / timespan, UTC | [Temporal types](temporal-types.md) |
| `.qpl.cfg` | session settings | [Config](config.md) |
| `hopen` `` `w!hopen `` `dispatch` `async dispatch` `await` | IPC client (`ipc` feature, on by default) | [IPC](ipc.md) |

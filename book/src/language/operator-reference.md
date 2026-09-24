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
| `?[...]` (prefix) | conditional: vectorised in a select; outside one an atom picks a branch, a vector gives an elementwise result of the same length | [Expressions](expressions.md) |
| `while[test; s1; ...]` | loop: run statements in the current scope while `test` holds | [Control flow](control-flow.md) |
| `noop` | nothing: prints nothing, can't be assigned or used as a value | [Control flow](control-flow.md) |
| `like` | glob pattern match | [Expressions](expressions.md) |
| `round` | `<n> round <col>`, mode from `.qpl.cfg round_type` | [Expressions](expressions.md) |
| `quantile` `shift` `lag` `lead` `diff` `pctchange` | parameter-on-the-left verbs | [Expressions](expressions.md) |
| `fill` | `<v> fill <col>` replaces nulls with `v` | [Expressions](expressions.md#nulls) |
| `isnull` `notnull` | boolean per row: is / isn't null | [Expressions](expressions.md#nulls) |
| `dropnull` | `` `a`b dropnull <table> `` drops rows with a null in those columns | [Table operators](table-operators.md#dropping-rows-with-nulls) |
| `distinct` | table: unique rows; column: count of distinct values (`n_unique`) | [Table operators](table-operators.md), [Expressions](expressions.md) |
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
| `enlist` | `enlist x` gives the one-element list of the atom `x` (a string is one atom) | [Column expressions](column-expressions.md) |
| `?` (infix) | roll: `n?6` gives `n` random ints below 6, `n?2.5` uniform floats, `n?list` random elements; with replacement | [Column expressions](column-expressions.md) |
| `zip` | build a table from a dict of named lists | [Column expressions](column-expressions.md) |
| `lj` `ij` `rj` | left / inner / right join | [select](select-update-delete.md) |
| `{...}` | lambda | [Functions](functions.md) |
| `.qpl.dt` `.qpl.tm` `.qpl.ts` `.qpl.dlta` | current date / time / timestamp / timespan, UTC | [Temporal types](temporal-types.md) |
| `.qpl.cfg` | session settings | [Config](config.md) |
| `.ns.name` | a namespaced name: an ordinary binding under a dotted prefix | [Scripts, imports & namespaces](imports.md) |
| `\l <path>` `\i "<path>"` | run a script flat; import one under `.<file-stem>.*` | [Scripts, imports & namespaces](imports.md) |
| `hopen` `` `w!hopen `` `dispatch` `async dispatch` `await` | IPC client (`ipc` feature, on by default) | [IPC](ipc.md) |

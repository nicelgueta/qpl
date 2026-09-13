# Expressions

| Kind | |
|---|---|
| arithmetic | `+` `-` `*` `%` (`%` is division, q convention) |
| comparison | `=` `<>` `!=` `<` `<=` `>` `>=` |
| pattern match | `<str/sym> like <pattern>` — q-style glob, see below |
| logical | `&` `\|` |
| conditional | `?[cond; then; cond2; then2; ...; else]` — vectorised, nests for else-if |
| cast | `type$expr` — see [Casts](casts.md) |
| round | `<precision> round <col>` — round a float column to N places |
| dyadic verbs | `<param> verb <col>` — `quantile`/`pctl`, `shift`/`lag`, `lead`, `diff`, `pctchange` (and `round`) |
| window | `<expr> over `p1`p2 [order `k1 asc `k2 desc] [rolling n]` — see [Window functions](window-functions.md) |

A leading `-` negates: `-45.3` is a negative literal, `-col` / `-x` folds to
`0 - …` (works in scalars, column expressions and filters).

```qpl
l: int$-45.3                                         / scalar: -45
select price_bin: ?[price>400;`high;price>200;`mid;`low] from trades
select neg_mv: -market_value from trades
select mv: 2 round market_value from trades          / round to 2 dp
select p95: 0.95 quantile price by sym from trades   / 95th percentile
select sym, price, ret: 1 diff price by sym from trades   / row-over-row change
```

`like` tests a string or symbol against a glob pattern, [same as q](https://code.kx.com/q/ref/like/):
`*` matches any sequence (including empty), `?` matches exactly one character,
and `[abc]` / `[a-z]` / `[^abc]` are character classes (case-sensitive; no
pattern characters means an exact match). Escape a pattern character by
putting it in its own one-character class — `[*]`, `[?]`, `[[]`, `[]]`:

```qpl
select sym from trades where sym like "AA*"          / starts with AA
select sym from trades where sym like "[AM]*"        / starts with A or M
select sym from trades where sym like "?A?L"         / exactly 4 chars, A then L
select from trades where not sym like "AAPL"         / negate with `not`
```

`round` and the other **dyadic verbs** are q-style: the left operand is a
parameter (a literal), the right is the column. `<p> quantile <col>` takes a
fraction in `[0,1]`; `<n> shift <col>` (alias `lag`) moves values `n` rows later,
`lead` `n` rows earlier; `<n> diff <col>` is the difference from `n` rows back;
`<n> pctchange <col>` the fractional change. `round`'s rounding mode is the
session config `round_type` (`HALF_TO_EVEN` by default; see [Config](config.md)).

**Aggregates:** `sum`, `avg`/`mean`, `min`, `max`, `count`, `first`, `last`,
`std`/`dev`, `var`, `med`/`median`, `mode`/`modal` (modal average — most
frequent value; ties resolve to the smallest), `skew`, `kurt`/`kurtosis`,
`any`, `all`, `prod`/`product`, `argmin`, `argmax`, `nnull`/`null_count`,
`abs`, `neg`, `not`, `string`, `distinct`/`n_unique`.

**Ordered / cumulative** (most useful with `over` + an `order` sub-clause, which
sorts each partition before applying): `cumsum`, `cummax`, `cummin`, `cumprod`,
`cumcount`, `ffill` (forward-fill nulls), `bfill` (backward-fill).

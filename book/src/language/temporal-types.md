# Temporal types

kdb+/q-style date & time literals. Each has an underlying integer offset that
`` `int$ `` / `` `long$ `` exposes.

| type | literal | offset |
|---|---|---|
| date | `2024.03.15` | days since `2000.01.01` |
| month | `2024.03m` | months since `2000.01` |
| time | `12:30:00.000` | ms of day (stored as ns) |
| minute | `12:30` | minutes of day |
| second | `12:30:00` | seconds of day |
| timestamp | `2024.03.15D12:30:00.000000000` | ns since `2000.01.01` |
| timespan | `0D12:30:00.000000000` | ns duration |

```qpl
d: 2024.03.15
d + 10                              / 2024.03.25   (days)
2024.03.20 - 2024.03.15             / 5
p: 2024.03.15D09:30:00.000000000
p - 0D00:05:00.000000000            / 2024.03.15D09:25:00.000000000
p < 2024.03.15D16:00:00.0           / 1b
```

Adding an integer shifts by one unit of the operand's own resolution — `date`+n
days, `month`+n months, `time`+n ms, `minute`+n minutes, `second`+n seconds,
`timestamp`/`timespan`+n ns. Comparison works across variants of the same
family (`date`↔`timestamp`, `time`↔`minute`↔`second`).

Casts use the backtick form `` `date$x ``, `` `month$x ``, `` `timestamp$x ``,
or a kdb single-char type code on a string — `"p"$` timestamp, `"d"$` date,
`"t"$` time, `"m"$` month, `"u"$` minute, `"v"$` second, `"n"$` timespan:

```qpl
`date$2024.03.15D12:30:00.0         / 2024.03.15
`month$2024.03.15                    / 2024.03m
`timestamp$2024.03.15               / 2024.03.15D00:00:00.000000000
"p"$"2024.03.15D12:30:00"           / parse a string
```

In **column** context, a string→temporal cast parses through Polars' string
parser (`expr.cast(<temporal>)` on a string is deprecated). The format is
inferred per value — ISO *and* kdb's dotted `2024.03.15` both work:

| cast | parser | result column |
|---|---|---|
| `` `date$s `` / `` `month$s `` | `str.to_datetime` → date | `Date` |
| `` `timestamp$s `` (`"p"$s`) | `str.to_datetime` | `Datetime` (keeps the time part) |
| `` `time$s `` (`"t"$s`) | `str.to_time` | `Time` |

```qpl
select d: `date$date_str from t        / "2024.03.15"          -> 2024-03-15
select ts: `timestamp$ts_str from t    / "2024-03-15T09:30:00" -> 2024-03-15 09:30:00
```

A value the inferred format cannot read aborts the query (strict by default).

Now-functions (**UTC** — there is no timezone database): `.qpl.d` today's date,
`.qpl.t` time, `.qpl.p` timestamp (ns), `.qpl.n` timespan since midnight. They
are ordinary expressions:

```qpl
log .qpl.d
```

Temporal literals project as Polars-native columns (`Date`, `Datetime[ns]`,
`Time`, `Duration[ns]`); month maps to `Date` at the 1st. Column output is
Polars' ISO form, not the kdb form. Not yet in the language: `xbar` bucketing,
the `within` window operator, `.minute` / `.date` unit accessors, and
temporal arithmetic on a **column** (`date_col + n` — scalar arithmetic is
fully supported) — those are planned.

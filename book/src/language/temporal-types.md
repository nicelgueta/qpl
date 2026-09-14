# Temporal types

Dates and times in qpl follow kdb+/q, which means they are written as
literals directly in the language rather than parsed from strings. If you've
used q this will be entirely familiar. If you haven't, the notation to get
used to is that dates use dots rather than dashes, and a `D` separates the
date part of a timestamp from the time part.

```qpl
qpl) d: 2024.03.15
qpl) d
```

```
date: 2024.03.15
```

There are seven temporal types, differing in what they measure and how finely:

| Type | Literal | Counts |
|---|---|---|
| date | `2024.03.15` | days since `2000.01.01` |
| month | `2024.03m` | months since `2000.01` |
| time | `12:30:00.000` | milliseconds into the day |
| minute | `12:30` | minutes into the day |
| second | `12:30:00` | seconds into the day |
| timestamp | `2024.03.15D12:30:00.000000000` | nanoseconds since `2000.01.01` |
| timespan | `0D12:30:00.000000000` | nanoseconds of duration |

The right-hand column isn't trivia. Each of these really is an integer
underneath, counting from the epoch shown, and you can see that number with a
cast:

```qpl
qpl) int$2024.03.15
```

```
i64: 8840
```

The last two rows deserve a distinction: a **timestamp** is a point in time,
whereas a **timespan** is a length of time. They look similar because both
use the `D`, but `2024.03.15D09:30:00.000000000` is half past nine that
morning, while `0D00:05:00.000000000` is five minutes.

## Arithmetic

Since each type is a count, adding a plain integer moves by one unit *of that
type's own resolution*. Add 10 to a date and you move ten days:

```qpl
qpl) 2024.03.15 + 10
```

```
date: 2024.03.25
```

Add 10 to a month and you move ten months; to a time, ten milliseconds; to a
timestamp or timespan, ten nanoseconds. You never have to remember a
conversion factor, but you do have to be aware of which type you're holding.

Subtracting two values of the same type gives the count between them:

```qpl
qpl) 2024.03.20 - 2024.03.15
```

```
i64: 5
```

And subtracting a duration from a point in time gives an earlier point:

```qpl
qpl) p: 2024.03.15D09:30:00.000000000
qpl) p - 0D00:05:00.000000000
```

```
timestamp: 2024.03.15D09:25:00.000000000
```

Comparison works across types in the same family, so a date can be compared
with a timestamp, and time with minute or second, without an explicit
conversion:

```qpl
qpl) p < 2024.03.15D16:00:00.0
```

```
bool: true
```

## Converting

Casts use the backtick form of a type name:

```qpl
qpl) `date$2024.03.15D12:30:00.0        / drop the time part
```

```
date: 2024.03.15
```

```qpl
qpl) `month$2024.03.15
```

```
month: 2024.03m
```

```qpl
qpl) `timestamp$2024.03.15              / midnight on that date
```

```
timestamp: 2024.03.15D00:00:00.000000000
```

For parsing text, q's single-character type codes also work, applied to a
string: `"p"` for timestamp, `"d"` date, `"t"` time, `"m"` month, `"u"`
minute, `"v"` second, `"n"` timespan.

```qpl
qpl) "p"$"2024.03.15D12:30:00"
```

```
timestamp: 2024.03.15D12:30:00.000000000
```

## Temporal columns

A datetime column in a table behaves as you'd hope. The demo `trades` table
has one, and pulling it out as a list (as
[Column expressions & lists](column-expressions.md) described) gives you
timestamps rather than raw integers:

```qpl
qpl) trades`ts
```

```
timestamp[8]: 2024.03.15D09:30:00.000000000 2024.03.15D09:31:15.000000000 …
```

Converting a *string* column to a temporal type is slightly different from
converting a scalar, because Polars parses string columns through a dedicated
parser rather than a plain cast. The upside is that the format is inferred
per value, so both ISO and kdb's dotted notation are understood without you
having to say which you have:

| Cast | Produces |
|---|---|
| `` `date$s `` / `` `month$s `` | a `Date` column |
| `` `timestamp$s `` or `"p"$s` | a `Datetime` column, keeping the time |
| `` `time$s `` or `"t"$s` | a `Time` column |

```qpl
select d: `date$date_str from t        / "2024.03.15"          -> 2024-03-15
select ts: `timestamp$ts_str from t    / "2024-03-15T09:30:00" -> 2024-03-15 09:30:00
```

Parsing is strict: a value the inferred format can't read aborts the query
rather than becoming null. That's the right default, because a malformed
timestamp almost always means something upstream is wrong, and you'd much
rather find out now than discover a silently-dropped day's data inside an
aggregate later.

Note also that once a temporal value is in a column it prints in Polars' ISO
style rather than the kdb style you typed it in, as you can see in the `ts`
column of any `trades` output. Month becomes a `Date` on the first of the
month.

## Current time

Four expressions read the clock, all in **UTC** — qpl bundles no timezone
database, so there's no ambiguity about which zone you're getting, and no
opportunity for a machine's local settings to change a query's meaning.

| | |
|---|---|
| `.qpl.d` | today's date |
| `.qpl.t` | the time |
| `.qpl.p` | the timestamp, to nanoseconds |
| `.qpl.n` | timespan since midnight |

They're ordinary expressions and can be used anywhere one is allowed:

```qpl
log .qpl.d
```

## Not there yet

A few things q users reach for by reflex aren't implemented. Rather than let
you find out by trial and error: `xbar` bucketing, the `within` operator,
the `.minute` and `.date` unit accessors, and arithmetic between a temporal
**column** and an integer. That last one is the only real limitation in
practice, since scalar temporal arithmetic (everything above) is complete.
All are on the [roadmap](../roadmap.md).

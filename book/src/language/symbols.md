# Symbols

A backtick in front of a word makes a **symbol**:

```qpl
qpl) `AAPL
qpl) `price
qpl) `low
```

A symbol is a value, like a number or a string, but it's used for naming
things rather than for holding text. When you tell a query which columns to
join on, which columns to sort by, or which columns to remove, you name them
with symbols. When you tag rows with one of a small set of labels, those
labels are usually symbols too.

The distinction from a string is worth drawing early, because both look like
text and they are not interchangeable. `"AAPL"` is a string: a sequence of
characters you might slice, match against a pattern, or print. `` `AAPL `` is
a symbol: an atomic name, compared as a unit. In a table, the `sym` column of
the demo data holds strings, so filtering it uses a string:

```qpl
qpl) select from trades where sym = "AAPL"
```

whereas telling a query to *group by* or *join on* that column names it with a
symbol. You'll see both in the next chapter, and the rule of thumb is simple:
data is a string, the name of a column is a symbol.

## Writing them

A symbol literal is a bareword. Letters, digits, and the characters `_`, `-`,
`.` and `/` are all allowed, but a space is not, since the space is what tells
qpl the symbol has ended.

When you need a symbol containing a space or some other punctuation, build it
from a string instead. `` `$ `` interns a string into a symbol:

```qpl
qpl) role: `$"Analytics Engineer"
```

## Lists of symbols

Symbols are most often written in runs, with no separator at all between them:

```qpl
qpl) lvl: `low`mid`high
```

```qpl
qpl) lvl
```

```
sym[3]: `low`mid`high
```

That's three symbols, not one. The backtick both starts a symbol and ends the
previous one, so `` `low`mid`high `` needs no commas. It reads strangely for
about a day.

This run-of-symbols form is how you'll pass several column names to the
operations that take them, so it turns up constantly:

```qpl
delete `price`size from trades      / remove two columns
```

## One thing that is *not* a symbol

Tables are named plainly, never with a backtick. You write `trades`, and
`` `trades `` would be a symbol that happens to spell the same word:

```qpl
qpl) trades                   / the table
qpl) select from trades       / also the table
```

So a leading backtick always means "here is a piece of data that names
something", and never "here is the table I want you to read".

## Where symbols go from here

Symbols do one more job, which is to stand in for a compact encoding of a
text column: turning a column of repeated strings into a small integer code
makes grouping, joining and sorting on it considerably faster. That's a
performance topic rather than a syntax one, so it waits until
[Categoricals & enums](categoricals-enums.md), by which point you'll have the
query vocabulary to make sense of the examples.

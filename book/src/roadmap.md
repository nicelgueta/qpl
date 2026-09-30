# Roadmap

A few directions, some concrete and one open-ended.

**A cleaner WASM build.** qpl already runs in the browser (see
[Install](install.md#try-it-in-a-browser-no-install)), but stock Polars
doesn't compile for `wasm32-unknown-unknown`, so the build currently applies
a small patch to Polars first. The goal is to drop that patch once upstream
builds for the target unaided.

**Python extensions.** [Rust extensions](extensions.md) declare a read or
write permission that every session enforces. Extending the same model to
Python functions is an option for consideration further down the line.

**Extension functions inside queries.** [Rust extensions](extensions.md)
work on values, so they can't be called inside a `select` projection. Polars
has its own framework for running Rust functions over whole columns; building
on it would let an extension function take columns in a query and run as part
of the lazy plan.

**Deciding on nulls.** Whether null should be a first-class value in the
language itself, with its own literal and semantics, is still an open design
question.

**Float list literals.** Ints, strings and symbols can be written directly
as lists (`1 2 3`, `("a" "b")`, `` `x`y ``), but floats can't: `1.5 2.5` is a
parse error. For now a float list comes from a table column
(``trades`price``) or a computation, and an int list works wherever a float
list is expected. Writing one directly should work the same way as it does
for the other types.

**More of the language.** The gaps that exist today are called out in the
chapter each one belongs to rather than collected here, on the grounds that a
missing feature is most useful to know about while you're reading about the
thing it's missing from. The temporal ones in
[Temporal types](language/temporal-types.md) are the most likely to be missed
in practice: `xbar` bucketing, `within`, the unit accessors, and arithmetic
between a temporal column and an integer.

Issues and suggestions are welcome on
[GitHub](https://github.com/nicelgueta/qpl/issues).

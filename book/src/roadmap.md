# Roadmap

Two directions, one concrete and one open-ended.

**A WASM build**, so that qpl can run in a browser. That would make it
possible to try the language, and to run real queries against a modest
dataset, without installing anything at all.

**More of the language.** The gaps that exist today are called out in the
chapter each one belongs to rather than collected here, on the grounds that a
missing feature is most useful to know about while you're reading about the
thing it's missing from. The temporal ones in
[Temporal types](language/temporal-types.md) are the most likely to be missed
in practice: `xbar` bucketing, `within`, the unit accessors, and arithmetic
between a temporal column and an integer.

Issues and suggestions are welcome on
[GitHub](https://github.com/nicelgueta/qpl/issues).

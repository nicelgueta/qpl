# Roadmap

Two directions, one concrete and one open-ended.

**A cleaner WASM build.** qpl already runs in the browser (see
[Install](install.md#try-it-in-a-browser-no-install)), but stock Polars
doesn't compile for `wasm32-unknown-unknown`, so the build currently applies
a small patch to Polars first. The goal is to drop that patch once upstream
builds for the target unaided.

**More of the language.** The gaps that exist today are called out in the
chapter each one belongs to rather than collected here, on the grounds that a
missing feature is most useful to know about while you're reading about the
thing it's missing from. The temporal ones in
[Temporal types](language/temporal-types.md) are the most likely to be missed
in practice: `xbar` bucketing, `within`, the unit accessors, and arithmetic
between a temporal column and an integer.

Issues and suggestions are welcome on
[GitHub](https://github.com/nicelgueta/qpl/issues).

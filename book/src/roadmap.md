# Roadmap

A few directions, some concrete and one open-ended.

**A cleaner WASM build.** qpl already runs in the browser (see
[Install](install.md#try-it-in-a-browser-no-install)), but stock Polars
doesn't compile for `wasm32-unknown-unknown`, so the build currently applies
a small patch to Polars first. The goal is to drop that patch once upstream
builds for the target unaided.

**A basic standard library.** A set of commonly needed functions shipped
with qpl itself, so everyday helpers don't have to be rewritten in every
script or passed around as `\i` imports.

**A permissioned Rust extension framework.** qpl is meant to be safe to hand
to an agent: it can run any qpl it likes, but it has no way out to arbitrary
code the way a Python session does, and a
[read-only session](language/read-only.md) can't write at all, so there's a
hard limit on how much damage a confused agent can do. Extensions need to keep that property. The plan is to
let you write native functions in Rust and expose them to qpl with a proc
macro that declares each one as either a **read** or a **write** action, so
that a read-only session refuses every write action at once, built-in or
extension alike. Permissioning then lives in the language itself rather than
in whatever sandbox happens to be wrapped around it: an agent given a
read-only qpl session can call anything it likes and still can't change
state.

Extending the same model to Python functions is an option for consideration
further down the line.

**Deciding on nulls.** Whether null should be a first-class value in the
language itself, with its own literal and semantics, is still an open design
question.

**More of the language.** The gaps that exist today are called out in the
chapter each one belongs to rather than collected here, on the grounds that a
missing feature is most useful to know about while you're reading about the
thing it's missing from. The temporal ones in
[Temporal types](language/temporal-types.md) are the most likely to be missed
in practice: `xbar` bucketing, `within`, the unit accessors, and arithmetic
between a temporal column and an integer.

Issues and suggestions are welcome on
[GitHub](https://github.com/nicelgueta/qpl/issues).

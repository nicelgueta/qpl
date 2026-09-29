# qpl — Quick Polars Language

qpl is a language with two rather different influences. The syntax is
inspired by kdb+/q, which means it is terse to the point of looking
cryptic until it suddenly doesn't. The engine underneath is
[Polars](https://pola.rs), which means the queries you write run at a speed
competitive with DuckDB. Nothing else is involved: qpl ships as a single
static binary, with no Python, no Polars installation, and no runtime to set
up.

The result is a language you can type a real query into faster than you could
describe that query to somebody else, and which will then chew through a
parquet file considerably larger than the memory on the machine.

It's worth being clear up front about what "inspired by" means. qpl borrows
a handful of ideas from q: terse right-to-left expressions, `select … by …
from` queries, dotted date and time literals. It is not q, not a q dialect,
and not trying to be compatible with kdb+. Underneath the familiar-looking
surface it is a quite different language, with its own type system, its own
execution model built around lazy Polars plans, and some syntax
that q doesn't have - as well as missing a lot of syntax and features that q does have.

## How this book is arranged

This book is written to be read straight through the first time. Each chapter
assumes the ones before it and nothing after it, so if a piece of syntax
turns up that hasn't been introduced yet, that's a bug in the book rather
than something you were supposed to already know. Where a later chapter is
genuinely the right home for a detail, the text says so and moves on instead
of explaining it twice.

Everything is built around the same two small tables, `trades` and `quotes`,
which qpl can load for you:

```bash
qpl --load-demo
```

They hold one morning of invented market data, eight trades and five quotes.
Being small is the point: every result printed in this book is the real
output of the query above it, short enough to read in full, so you can check
your understanding against what the language actually did rather than
against a description of what it should have done.

The most useful way to read what follows is with that REPL open in another
window. qpl is a language you learn by typing rather than by studying, and
nearly all the examples are a single line long precisely so that you can.

## Before you start

If you have never used kdb+/q, two conventions will look strange on first
contact, and both are explained properly when they come up. First, `/`
starts a comment, which is why division is written `%` rather than `/`.
Second, expressions evaluate right to left, so `10 - 3 - 2` is `9`, not `5`.
Neither has any deeper meaning, and after an hour you will stop noticing.

If you *have* used q, some of your instincts will carry over, but don't lean
on them too hard: plenty of things that look alike behave differently, and
q code won't generally run unchanged. The
[Operator reference](language/operator-reference.md) is the fastest way to
see where qpl differs.

## Elsewhere

There's a [VSCode extension](https://github.com/nicelgueta/qpl/tree/main/tools/vscode)
with syntax highlighting and a Ctrl+Enter REPL, which sends the current line
straight to a live session. It comes into its own once you graduate from the
prompt to writing `.qpl` scripts.

The source, issue tracker, and prebuilt binaries all live on
[GitHub](https://github.com/nicelgueta/qpl).

Next: [Why?](why.md) if you want to know why this exists at all, or skip to
[Install](install.md) and [Quickstart](quickstart.md) to start typing.

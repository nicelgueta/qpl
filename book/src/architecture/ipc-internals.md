# IPC internals

[IPC](../language/ipc.md) covers what `hopen`, `dispatch`, `await` and `\port`
do from the language side. This chapter is about how they're built around a
VM that is otherwise single-threaded from top to bottom. IPC is the one
corner of the codebase that isn't, and it's kept firmly in that corner.

## The rule: `Vm` never crosses a thread

Everywhere else in this book, it's taken for granted that only one thing
touches the `Vm` at a time. IPC needs some concurrency: a client talking to
several servers at once, and a server accepting connections while someone
types at its prompt. The trick is to get that without giving up the
assumption, by keeping all the actual threading behind plain message passing:

- The transport is [ZeroMQ](https://zeromq.org/) (a pure-Rust
  implementation, so there's no C library to install), with one REQ/REP
  socket pair per connection.
- Each client connection gets its own worker thread, and a `\port`
  listener gets one thread of its own. Any async networking code stays
  inside those threads.
- Those threads only ever swap **owned, plain values** (`String`s and byte
  buffers) with the main thread, over ordinary standard-library channels
  (`std::sync::mpsc`). None of them ever holds a reference into the `Vm`.
- The *only* thread that ever runs code on the VM is the main one. A
  request that arrives on a socket is handed to it and evaluated exactly as
  if it had been typed at the local prompt, with the same parser, the same
  compiler and the same `Vm::run_compiled`.

So there's no `Mutex` guarding the `Vm`, because nothing ever needs one. The
Ctrl-C flag is the only exception to the rule. It's a pair of atomics
precisely so the signal handler can flip it without going anywhere near
the `Vm` itself.

## Serving: how a request reaches the main thread

`\port 5001` is an ordinary statement. It compiles to a call to a native
that closes any listener already open, binds a new one, and parks it on the
`Vm`. It doesn't block, and it doesn't read from the socket. Because it's
an ordinary statement, it works the same from a script as from the prompt,
and a script that leaves a port open carries on serving after its last
line rather than exiting. It keeps going until stdin closes.

The actual serving is done by the REPL loop. Once a port is open, it stops
blocking on the keyboard and starts polling instead. A small helper thread
reads stdin lines into a channel, and the loop takes whichever turns up
first, a line you typed or a request from the listener. The two interleave
one at a time and never overlap. With no line editor drawing the prompt,
the loop prints its own, showing the address being served. (The price is
that the line editor's niceties, like history and arrow keys, are gone for
the rest of the session once a port has been opened, because another thread
now owns stdin and can't hand it back.)

Each request is compiled in *result* mode, the same mode `run_vm` uses:
nothing is printed, and the last statement's value is kept to send back.
A `\`-prefixed command anywhere in it is turned away outright. Those
commands (`\d`, `\l`, `\i`, `\1`, and `\port` itself) are local session
housekeeping, not something a remote caller gets to trigger. Assignments,
queries, `.qpl.cfg` and `log` are all fair game, *if* the connection is
allowed to write.

## Read and write handles

Every connection carries a permission, chosen by the client when it opens
the handle. Plain `hopen` gives a read-only handle, and `` `w!hopen `` (which
comes out of the parser as `whopen`) asks for a write handle. The mode rides
along as a single tag byte in front of each request. While the server
evaluates that request it holds the mode on the `Vm`. `Vm::authorize`
refuses anything whose `permission::Effect` isn't `Read` (an assignment,
`sink`, `.qpl.cfg`, `\1`, `whopen`) on a read-only handle. The same check
enforces a read-only session (any session not started with `qpl -w`),
which refuses `Effect::Write` from every source. Input typed at the server's own prompt is only subject to the
session's restriction.

## The client side

`hopen`, `whopen` and `await` are ordinary natives, found through the same
last-resort name lookup as `til`. `<conn> dispatch <cmd>` (and its `async`
form) compiles to a single `DISPATCH` instruction that pops a connection, a
payload and an async flag off the stack. There's no separate grammar for
"IPC statements". The payload is plain source text, rebuilt from the tokens
at parse time, so the server parses it just like something typed locally.

A synchronous dispatch hands the text to the connection's worker thread and
waits for the reply, keeping an eye on the Ctrl-C flag while it does. An
`async dispatch` doesn't wait. It returns a `Future` straight away, and
`await` collects the reply later.

## What goes over the wire

A response has the same shape as the `EvalResult` a local statement
reduces to. A table travels as Parquet bytes, using the writer and reader
already linked in for `load`/`sink`, so no extra Polars feature is needed
just for this. A scalar travels using the value codec described in
[Compiled artifacts](compiled-artifacts.md#the-value-codec). An error
comes back as its message and resurfaces on the client as a runtime error.

`Handle` (an open connection) and `Future` (a dispatch that hasn't been
awaited yet) are the two `Value` variants that exist purely for IPC.
They're deliberately left un-gated, so every `match` over `Value` elsewhere
in the codebase doesn't need a `#[cfg]` arm just in case the `ipc` feature
is off. Only the code that can actually *create* one is gated. If one ever
ends up in a scalar response, it goes over as an `"<unrepresentable>"`
placeholder, since there's nothing meaningful the other side could do with
it anyway.

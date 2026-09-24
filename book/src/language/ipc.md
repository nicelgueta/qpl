# IPC

*This chapter describes the `ipc` feature, on by default (drop it with
`--no-default-features`, as [Install](../install.md) covered), and adds
nothing at all to a build without it.*

Every session so far has been a single process with its own tables in its own
memory. IPC is what you reach for when that stops being enough: it lets one
qpl process send statements to another running one and get real results back,
over a plain REQ/REP socket pair. The transport is
[zeromq](https://github.com/zeromq/zmq.rs) in pure Rust, so there's no system
libzmq to install.

## What it's for

Suppose loading and preparing your tables takes two minutes. Without IPC,
every script that wants to ask a question of that data pays the two minutes
itself. With IPC you pay it once, in a long-lived session, and everything
else asks that session.

That basic move covers several situations:

- **A shared data process.** Load the slow tables once in a long-running
  session started with `qpl -i`, then let any number of short-lived client
  scripts query it for free.
- **Separating compute from callers.** A scheduled job, a web backend or a
  notebook can dispatch a query and receive a real table, without embedding
  qpl or reimplementing the query in another language. The wire protocol is
  ordinary TCP.
- **Fanning out.** One client can open connections to several servers, one
  per dataset or per region, dispatch to all of them without waiting, and
  collect the answers as they arrive. Independent queries then run
  concurrently rather than in sequence.
- **Adjusting a live session.** Because a dispatched request is treated
  exactly like a typed line, a client can bind new names or extend a pipeline
  inside a running process, which is handy for a long-lived job you don't
  want to restart.

## Running a server

`\port <n>` starts listening; a bare `\port` stops.

```qpl
qpl -i --load-demo setup.qpl
qpl) \port 5001        / start serving
qpl) \port             / stop
```

This only works in the interactive REPL, which is why the example uses
`qpl -i`. A script on its own exits as soon as it finishes, before anything
could connect, so there has to be a prompt keeping the process alive.

Every request that arrives is evaluated exactly as though someone had typed
it at the server's own prompt. Assignments change the server's session,
queries run against its tables. The exception is the `\`-prefixed commands
(`\d`, `\l`, `\i`, `\1`, and `\port` itself), which are local administration and
aren't something a remote caller can trigger.

## Connecting as a client

`hopen` opens a connection. A bare port number connects to `127.0.0.1`; a
`"host:port"` string goes further afield.

```qpl
conn: hopen 5001                       / or hopen "db.internal:5001"
```

`dispatch` sends a whole statement and waits for the answer:

```qpl
resp: conn dispatch select from trades where price > 100
resp                                    / a genuine table, queryable further
```

What comes back is a real local value. A table arrives as a table you can
query further, a scalar as a scalar. Nothing about `resp` remembers that it
came from somewhere else.

For work you don't want to wait on, `async dispatch` returns immediately with
a pending handle, and `await` resolves it later:

```qpl
pending: conn async dispatch select avg price by sym from trades
/ ... do other things while the server works ...
result: await pending
```

That pairing is what makes the fan-out case work: dispatch to several
connections first, then await each in turn.

A dispatched assignment has no result to send back, just as it would print
nothing locally, so the client receives a boolean acknowledgement instead:

```qpl
conn dispatch t: select from u
```

## Read and write handles

A client that can run arbitrary statements on a server can also modify it, so
qpl defaults to the cautious option. A bare `hopen` gives a **read-only**
connection, and the server refuses anything that would write to its session:
assignments, `sink`, and changing a [`.qpl.cfg`](config.md) setting (a bare
`.qpl.cfg`, which only prints them, is fine). Queries of every kind still
work.

```qpl
ro: hopen 5001                          / read-only, the default
ro dispatch t: select from trades       / rejected by the server
ro dispatch select from trades          / fine, nothing is written
```

When you do want write access, ask for it explicitly at connection time:

```qpl
rw: `w!hopen 5001                       / a write handle
rw dispatch t: select from trades       / allowed
```

The permission is chosen by the client when it connects and enforced by the
server on every request over that connection. It applies only to statements
arriving over a socket, never to what the person sitting at the server's own
prompt types.

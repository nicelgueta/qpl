# IPC

Optional feature (`--features ipc`, off by default — see [Install](../install.md));
adds no dependencies to a default build. Lets one qpl process act as a client
to another over a plain REQ/REP socket pair ([`zeromq`](https://github.com/zeromq/zmq.rs),
pure Rust, no libzmq system dependency).

The problem this solves: a qpl session is normally a single process holding
its own tables in memory — one script, one address space. IPC lets a second
process reach into a *running* session instead of re-loading and re-computing
everything itself, which is the same reason kdb+ shops lean on IPC so heavily.
Concretely:

- **A shared data process.** Load the big/slow-to-build tables once in a
  long-lived server session (`qpl -i`), then have any number of short-lived
  client scripts query it without paying that load cost themselves.
- **Splitting compute from callers.** A scheduled job, a web backend, or a
  notebook can `dispatch` a query and get back a real table/scalar, without
  embedding qpl or re-implementing the query in whatever language it's
  written in — the wire format is plain TCP, not qpl-specific.
- **Fan-out across sessions.** A single client can `hopen` several servers
  (e.g. one per dataset, or one per region) and `async dispatch` to all of
  them, then `await` each — running independent queries concurrently instead
  of one after another.
- **Remote administration of a live session.** `dispatch` treats the request
  exactly like a typed REPL line, so a client can bind new globals, extend a
  lazy pipeline, or otherwise reshape the server's session state on the fly —
  handy for poking at or updating a long-running process without restarting it.

**Server**: `\port <n>` opens a listener; bare `\port` closes it. Only
available in the interactive REPL (`qpl -i script.qpl`) — a script alone exits
before anything could connect, so there's a REPL to keep the process alive.
Once opened, every request is evaluated exactly like a typed REPL line
(assignments mutate the server's session, `select`/`update`/`delete` all
work), except the `\`-prefixed system commands (`\d`, `\l`, `\1`, `\port`
itself), which are local session administration, not part of what a remote
client dispatches.

```qpl
qpl -i --load-demo setup.qpl
qpl) \port 5001        / start serving
qpl) \port              / stop
```

**Client**: `hopen` opens a connection (a plain port number connects to
`127.0.0.1`; a `"host:port"` string connects elsewhere); `dispatch` sends a
whole statement to it and blocks for the reply; `async dispatch` returns
immediately with a pending handle, resolved later by `await`. A table comes
back as a real table, a scalar as a real scalar — both fully usable locally,
same as if the query had run in-process.

```qpl
conn: hopen 5001                       / or hopen "db.internal:5001"
resp: conn dispatch select from trades where price > 100
resp                                    / a genuine table, queryable further

pending: conn async dispatch select avg price by sym from trades
/ ... do other things while the server works ...
result: await pending
```

A dispatched assignment (`conn dispatch t: select from u`) has nothing to
print, same as it would locally — the client gets back a boolean acknowledgment
rather than a value to bind.

**Read vs. write handles**: bare `hopen` opens a **read-only** connection —
the default. The server rejects anything that writes to its session when
dispatched from a read handle: an assignment (`x: ...`, `t: select ...`),
`sink`, and `\1`. Everything else (`select`/`update`/`delete`, building a
lazy pipeline, etc.) still works. `` `w!hopen `` opens a **write** handle
instead, with none of those restrictions:

```qpl
ro: hopen 5001                          / read-only (default)
ro dispatch t: select from trades       / rejected by the server
ro dispatch select from trades          / fine — no write involved

rw: `w!hopen 5001                       / write handle
rw dispatch t: select from trades       / allowed
```

The permission is decided by the client at `hopen` time and enforced by the
server per request — it only ever applies to commands arriving over a
connection, never to the server operator's own local REPL/script input.

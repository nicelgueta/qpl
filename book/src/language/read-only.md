# Read-only sessions

Read-only sessions are one of qpl's defining features, and the main reason
it's a good language to give an AI agent. An agent can write and run any qpl
it likes, but everything it does goes through qpl's own actions, and in a
read-only session none of those actions can write to disk or change anything
outside the session. Nothing run in the session can lift that restriction.

The guarantee is enforced by the language's own VM, not by a container or
permissions layer around it. So it goes wherever qpl goes: a laptop, a CI
job, or a server shared by a team of agents.

## Read and write

Everything in the language has a permission, and there are two of them:

| Permission | Allows | Examples |
|---|---|---|
| **read** | everything except writing to disk or changing state outside the session | queries, `load`, assignment, functions, `log`, `.qpl.cfg`, `hopen`, `dispatch`, `await`, `\l`, `\i`, `\d`, `\port` |
| **write** | anything | `sink`, `\1 <path>` (the stdout log), `` `w!hopen `` (a write handle to another server) |

A session has a permission too. It's **read** unless qpl is started with
`-w` (`--write`), in which case it's **write**:

```bash
qpl                      # a read-only REPL
qpl script.qpl           # a read-only script
qpl -w script.qpl        # a script that may write
qpl -w --load-demo       # a REPL that may write
```

That works the same way for every way of running qpl: the REPL, a script,
`-i`, `-c`, and a compiled `.qplc` file. The permission is fixed when the
session starts. No statement can change it, so whatever a read-only session
is given to run, it stays read-only until it exits.

Binding names, defining functions and changing settings are all read actions
because they only change the session itself, and that state disappears when
the session ends. A write action changes something that outlives the
session.

A write in a read-only session fails with a runtime error that names it:

```qpl
qpl) t: select from trades where sym = `AAPL
qpl) t sink "aapl.parquet"
'Cannot perform write action in read-only session: sink (start qpl with -w to allow writes)
```

The check runs when the action is reached, not when the statement is
parsed. A function that sinks can be defined in a read-only session, and
calling it fails at the `sink`. So neither a function, a `\l`-loaded script
nor a compiled `.qplc` file can get round it.

## Giving qpl to an agent

The simplest agent tool is one query per call:

```bash
qpl -c "$QUERY"
```

The query's output comes back on stdout, and a refused action or a bad query
comes back on stderr with a non-zero exit code, which is everything a tool
call needs. To give the agent some data to start from, begin the command with
`\l setup.qpl`, a script that loads the tables.

For a longer investigation, where reloading the data on every call would be
wasteful, keep one session running and let the agent query it over IPC:

```bash
qpl -i setup.qpl     # setup.qpl loads the tables, then `\port 5001`
```

Either way the agent gets the whole language to work with, and the only thing
it can't do is leave a mark outside the session.

## Enforcing it from the agent's side

Because writing needs an explicit flag, an agent harness only has to stop the
agent passing it. In [Claude Code](https://code.claude.com/docs/en/permissions),
for example, a project's `.claude/settings.json` can let the agent run qpl
without asking while denying it write permission:

```json
{
  "permissions": {
    "allow": ["Bash(qpl *)"],
    "deny": ["Bash(qpl *-w*)", "Bash(qpl *-iw*)", "Bash(qpl *--write*)"]
  }
}
```

Claude Code checks deny rules before allow rules, and applies them to each
part of a compound command (`a && b`, `$(...)` and so on), so any qpl command
that asks for write permission is refused, however it's combined. `-iw`
needs its own rule because short flags can be combined, and `-i` is the only
one that can come before `-w`. A deny rule can also over-match, for instance
on a `-c` query that happens to contain `-w`. That errs on the safe side: the
command is refused rather than run.

These rules match the text of the command Claude writes, so they're a
guardrail rather than a hard boundary. Claude Code's own documentation says
as much: a command run through `sh -c` or by another path isn't matched. When
the guarantee has to hold against a determined agent, back it up at the
operating-system level. Run the agent as a user that can't write where it
matters, or use Claude Code's [sandbox](https://code.claude.com/docs/en/sandboxing).

## Read-only servers

A read-only session can still open a [`\port`](ipc.md). The session's
permission applies to every request it serves, on top of the per-connection
one. Over a read handle, clients can only query. Over a `` `w!hopen `` handle,
they can also assign and change settings, but a `sink` is still refused. A
server whose clients should be able to write has to be started with `-w`.

## What it doesn't cover

Read-only mode stops a session changing things. It doesn't limit what the
session can *see*. `load` and `\l` can read any file the qpl process can,
and `hopen` can connect to any server it can reach. It also doesn't cap how
much CPU or memory a query uses. So run the process as a user that can only
read the data the agent should see, and put the usual resource limits around
it, just as you would for any other process.

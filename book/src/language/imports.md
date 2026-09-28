# Scripts, imports & namespaces

Sooner or later the settled parts of a session want to live in a file: the
`load` lines you type at the start of every session, a handful of helper
functions, a few thresholds. This chapter covers the ways qpl runs a script
and the two ways one script can pull in another. The first shares
everything; the second tucks the imported names away under a prefix.

## Running a script

A script is a plain text file of statements, conventionally ending `.qpl`,
laid out by the rules in [Multi-line statements](multiline.md). There are
three ways to run one:

```bash
qpl script.qpl        # run it, print its output, exit
qpl -i script.qpl     # run it, then stay in the REPL with everything it bound
```

```qpl
qpl) \l script.qpl    / run it inside the session you already have
```

All three behave the same: each statement runs in turn, exactly as if you
had typed it at the prompt, and anything it binds persists afterwards. The
first statement that fails stops the whole script, and the error names the
file and line:

```
'setup.qpl:12: 'undefined name 'trade' (not a variable, table or lazy frame)
```

Run from the command line, a failed script exits with status 1 (130 if it was
interrupted with Ctrl-C). Statements that ran before a *runtime* failure keep
their effects: there is no rollback. A *parse or compile* error is different:
the whole file (and any `\l`/`\i` target it reaches, however deeply nested)
is read and compiled before the first statement runs, so a syntax mistake
anywhere in the file — even on the very last line — means nothing in it runs
at all, not even the statements before the mistake.

`\l` takes a bare path, with no quotes, and works nested inside another
script as well as at the prompt, so a top-level `main.qpl` can `\l` its setup
file before getting on with the real work.

Typed at the prompt, a relative path means relative to the directory you
started qpl in. Written **inside a script**, it means relative to *that
script's* directory, so `\l setup.qpl` in `jobs/main.qpl` loads
`jobs/setup.qpl` wherever qpl was launched from. (`load`, `sink` and `\1`
paths stay relative to the starting directory, since they name data rather
than code.)

## Namespaced names

A name can carry dots: `.ns.name`, or deeper, `.ns.sub.name`. This is a
**namespaced name**, and it is an ordinary variable, table or function
reference that happens to live under a prefix instead of in the flat
session scope. Nothing about it is special beyond the spelling:

```qpl
qpl) .cfg.threshold: 150
qpl) select from trades where size > .cfg.threshold
qpl) .fx.double: {[x] x*2}
qpl) .fx.double 21
```

```
i64: 42
```

You've already met one namespace: `.qpl`, which holds the now-functions from
[Temporal types](temporal-types.md) and the `.qpl.cfg` directive from
[Config](config.md). Those particular names are built-ins and can't be
rebound, but the namespace isn't otherwise locked: nothing stops you binding
`.qpl.foo`. Leaving `.qpl` to qpl itself is still good manners.

Namespaced names are just a convention you can adopt by hand. Where they
become genuinely useful is in combination with `\i`.

## Importing with `\i`

`\l` loads a script *flat*: a helper called `clean` in the script becomes
`clean` in your session. That's fine for your own setup file, and awkward
for a library of helpers you'd like to reuse across projects, where a
helper's name may well clash with something you've already bound.

`\i` runs a script the same way, except that every table, lazy plan, scalar
and function it binds at its top level lands under a namespace named after
the file:

```qpl
/ lib/utils.qpl
double: {[x] x*2}
greeting: "hello from utils"
big: select from trades where size > 200
```

```qpl
qpl) \i "lib/utils.qpl"
qpl) .utils.double 21
```

```
i64: 42
```

```qpl
qpl) log .utils.greeting
qpl) count .utils.big
```

```
hello from utils
i64: 3
```

The bare names `double`, `greeting` and `big` never exist. Only the
namespaced versions do.

That holds even when your session already has a name the library uses: a
library line `thr: 99` binds `.utils.thr` and leaves your own `thr` alone.
Nothing an import does can overwrite a bare name in your session.

The reverse isn't true, though, and it's worth being deliberate about:
**inside the imported script, a bare name it binds anywhere at its own top
level always means its own namespaced binding — never the session's — even
on the statement that defines it.** This is decided once, when the script is
compiled, by scanning its whole top level for assignment targets; it isn't a
"falls back to the session if not yet bound" rule. So a library line like

```qpl
trades: select from trades where size > 200
```

does *not* read your session's `trades` and bind `.utils.trades` from it —
the bare `trades` on the right already means `.utils.trades`, which doesn't
exist yet at that point in the script, so this fails with an undefined-name
error instead. A library that wants to build on the caller's table needs a
name of its own for it, e.g. `filtered: select from trades where size > 200`
(reading the session's `trades`, since `trades` isn't one of this script's
own top-level bindings, and writing `.utils.filtered`).

Two details of the syntax:

- **The path is a quoted string**, unlike `\l`'s bare one: `\i "lib/utils.qpl"`.
  An unquoted path is an error. The namespace is derived from that path, and
  writing it as a string keeps it visually distinct from the namespaced
  identifiers that tend to follow it.
- **The namespace is the file's stem**, cleaned up to be a valid name. Any
  character other than a letter, digit or `_` becomes `_`, and a leading
  digit gets an `_` in front. So `utils.qpl` gives `.utils`,
  `lib/my-lib.qpl` gives `.my_lib`, and `9lives.qpl` gives `._9lives`. The
  directory plays no part, so `a/utils.qpl` and `b/utils.qpl` both import
  as `.utils`, and the second import replaces the first.

Like `\l`, `\i` works typed at the prompt or nested inside another script,
with the same rule for relative paths: inside a script they're relative to
that script. It also works in a script started with `qpl script.qpl` or
`qpl -i`, not just in an interactive session.

## Functions that call their siblings

The import renames a script's top-level bindings. It doesn't rewrite the
*bodies* of the functions it moves. A library function that calls another
helper from the same file still says `helper`, not `.utils.helper`:

```qpl
/ lib/lg.qpl
_log: {[lvl,s] log[str$.qpl.ts " " lvl " " s]; noop}
info: {[s] _log["INFO"; s]}
warn: {[s] _log["WARN"; s]}
```

This still works after `\i "lib/lg.qpl"`. The qualification happens once,
when `lib/lg.qpl` is *compiled*: every bare reference to one of the file's
own top-level names, anywhere in the file including inside a function body
(as long as it isn't shadowed by that function's own parameters/locals), is
rewritten to the qualified form before the function's body is ever compiled
to bytecode. So `_log` inside `info`'s body is baked in as `.lg._log`, not
looked up dynamically each call — a session name can't hijack a library's
helper (bind your own `_log` afterwards and `.lg.info` still calls the
library's), and the same rule covers scalars and tables the library defines,
and carries through any number of levels of sibling-calls-sibling:

```qpl
qpl) \i "lib/lg.qpl"
qpl) .lg.info["service started"]
```

```
2024.03.15D09:30:00.000000000 INFO service started
```

(`_log` ends in `noop` so that each call prints one line rather than echoing
its message back as a result; see
[Logging](logging.md#inside-a-function-body).)

Because the qualification is baked into the function's compiled body, not
looked up again at call time, this survives being copied around: `f: .lg.info`
then calling `f[..]` still resolves `_log` to `.lg._log`, the same as calling
`.lg.info[..]` directly.

## Imports that import

A library can `\i` another library. The nested import's bindings land under
their own namespace, and when the outer import finishes, anything that's
already namespaced (anything whose name starts with `.`) is left alone
rather than prefixed a second time:

```qpl
/ lib/report.qpl
\i "lg.qpl"          / lib/lg.qpl: relative to this script
run: {[] .lg.info["report starting"]; select avg price by sym from trades}
```

After `\i "lib/report.qpl"` you have `.report.run` and the `.lg.*` helpers,
not `.report.lg.info`. The same rule means a library can choose its own
namespace by binding `.mylib.x: ...` explicitly, and `\i` won't touch it.

A `\l` inside an imported script is part of the import: whatever the loaded
file binds lands in the importer's namespace too.

## Re-importing, and failures

Re-importing a file is a clean reload. The namespace's previous contents are
cleared and the script runs again, so an edited helper picks up its new
definition and a binding you deleted from the library disappears from the
session. That's the quickest way to pick up an edit to a library during a
session. (Anything you bound under that namespace by hand is cleared too.)

An import is all or nothing. If any statement in the script fails, the
session's bindings are put back exactly as they were before the `\i`: nothing
half-imported is left behind, and a failed *re*-import leaves the previous
version of the library in place. The only things that can't be undone are
effects outside the session, such as a `sink` that already wrote a file, or
lines already printed.

## Things to watch

- **A library can't read a session value under the name it means to define.**
  Since qualification is decided once, at compile time, from the whole file's
  top level, a bare name the file binds *anywhere* at its top level always
  means its own namespaced binding throughout the file — including in the
  very statement that defines it. `t: select from t where ...` in a library
  can't read the caller's `t`; give the result a name the library doesn't
  otherwise bind.
- **Private isn't enforced.** An `_` prefix on a library's helper names is a
  convention only; `.lg._log` is as reachable as `.lg.info`.
- **Not everywhere.** Like every `\` command, `\l` and `\i` can't be sent
  over [IPC](ipc.md). They also can't run in the browser build, which has no
  filesystem to read from.

## A worked example

[examples/namespaces.qpl](https://github.com/nicelgueta/qpl/blob/main/examples/namespaces.qpl)
imports
[examples/namespace_lib.qpl](https://github.com/nicelgueta/qpl/blob/main/examples/namespace_lib.qpl)
and uses a function, a scalar and a table from it. Run it from the repository
root:

```bash
qpl --load-demo examples/namespaces.qpl
```

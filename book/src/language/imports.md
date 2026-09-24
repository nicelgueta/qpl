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
interrupted with Ctrl-C). Statements that ran before the failure keep their
effects: there is no rollback.

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

That holds even when your session already has a name the library uses. A
library line `thr: 99` binds `.utils.thr` and leaves your own `thr` alone;
`trades: select from trades where size > 200` reads your `trades` and binds
`.utils.trades`. Inside the script, a bare name means the library's own
binding once it has one, and falls back to the session's otherwise, so the
script reads naturally from top to bottom. Nothing an import does can
overwrite a bare name in your session.

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

This still works after `\i "lib/lg.qpl"`. When `.lg.info` runs, a bare name
in its body is looked up in its own namespace first (after its parameters
and locals), so `_log` resolves to `.lg._log`, and only then in the session.
A session name can't hijack a library's helper: bind your own `_log`
afterwards and `.lg.info` still calls the library's. The same rule covers
scalars and tables the library defines, and it carries through any number of
levels of sibling-calls-sibling:

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

Because the namespace comes from the name the function was *called through*,
call imported functions by their qualified name. Copying one into a plain
variable first, `f: .lg.info`, then calling `f[..]`, loses the namespace, and
the unqualified `_log` inside it no longer resolves.

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

- **Call imported functions by their qualified name.** As described above,
  the namespace comes from the name a function is called through.
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

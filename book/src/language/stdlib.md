# Standard library

`.std` is qpl's standard library: string, list, filesystem and environment
helpers that ship with every build (`qpl-std`), registered before any user
[Rust extension](../extensions.md) so a user extension can never claim the
`std` namespace. It's built entirely on the public extension API — no access
to VM internals — so it also serves as that API's reference example.

The guiding rule is that a keyword or verb that already works on a value
(`asc`, `desc`, `distinct`, `where`, `#`/`limit`, `drop`/`_`, `dropnull`,
`quantile`) stays in the core language, with the same spelling it has in a
query. `.std` holds only operations with no keyword of their own. A path,
string or list is always the first argument; a string argument also accepts
a symbol. Errors come back as `.std.<namespace>.<function>: <message>`.

## `.std.str`

Every function is **read**. `s` is a string, a symbol, or a list of either;
a list is processed element by element and gives a list back. Positions and
lengths count characters, not bytes.

| Function | Result | Semantics |
|---|---|---|
| `startswith[s; p]` | bool | literal prefix match |
| `endswith[s; p]` | bool | literal suffix match |
| `contains[s; p]` | bool | literal substring match — no glob or regex |
| `slice[s; start; end]` | string | `end` exclusive; negative indices count from the end; out-of-range is clamped |
| `rv[s]` | string | reversed |
| `l[s]` | string | lowercase (Unicode) |
| `u[s]` | string | uppercase (Unicode) |
| `len[s]` | int | character count (`count s` is 1, since a string is an atom) |
| `trim[s]` | string | whitespace stripped from both ends |
| `split[s; sep]` | string list | `s` must be a single string, not a list |
| `join[xs; sep]` | string | joins a string list |
| `replace[s; a; b]` | string | every literal occurrence of `a` replaced with `b` |
| `find[s; p]` | int | character index of the first match, or `-1` |

```q
.std.str.slice["hello"; 1; 4]           / "ell"
.std.str.startswith[("ab"; "ba"); "a"] / 10b
.std.str.split["a,b,c"; ","]           / ("a";"b";"c")
```

## `.std.arr`

Every function is **read**. `xs` is any list; a result keeps its list's kind
(a `DateVec` argument gives back a `DateVec`).

| Function | Result | Semantics |
|---|---|---|
| `rv[xs]` | list | reversed |
| `iasc[xs]` | int list | ascending sort order, as indices |
| `idesc[xs]` | int list | descending sort order, as indices |
| `has[xs; x]` | bool | whether `x` is in `xs`; `x` must match `xs`'s element type |
| `find[xs; x]` | int | first index of `x`, or `-1` |
| `findall[xs; x]` | int list | every index of `x` |
| `dedup[xs]` | list | removes consecutive repeats only — unlike `distinct`, which removes every repeat |
| `rotate[xs; n]` | list | rotated left by `n`; negative `n` rotates right |
| `cat[xs; ys]` | list | `ys` appended to `xs`; both must be the same kind |

```q
.std.arr.rotate[1 2 3 4; 1]    / 2 3 4 1
.std.arr.dedup 1 1 2 2 1 3     / 1 2 1 3
.std.arr.find[10 20 30; 20]   / 1
```

## `.std.fs`

Outside the browser only (the `os` feature, on by default). `parent`,
`join`, `name` and `ext` are lexical (**read**) and never touch the
filesystem; every other read is **iread** (refused over a read-only IPC
handle); every mutation is **write** (refused in a read-only session). There
is no `cd`: it would change process-wide state that `load` and `\l` depend
on.

| Function | Effect | Result | Semantics |
|---|---|---|---|
| `mkdir[p]` | write | — | `create_dir_all`; running it twice is harmless |
| `exists[p]` | iread | bool | |
| `isdir[p]` | iread | bool | `false` if the path doesn't exist |
| `isfile[p]` | iread | bool | `false` if the path doesn't exist |
| `abspath[p]` | iread | string | lexical: the path needn't exist, and symlinks aren't resolved |
| `parent[p]` | read | string | `"a"` -> `"."`, `"/"` -> `"/"` |
| `ls[p]` | iread | string list | entry names, sorted |
| `size[p]` | iread | int | bytes |
| `mtime[p]` | iread | timestamp | |
| `join[a; b]` | read | string | path join |
| `name[p]` | read | string | final path component |
| `ext[p]` | read | string | extension without the dot, or `""` |
| `cwd` | iread | string | no arguments |
| `rm[p]` | write | — | a file or an empty directory; never recursive |
| `mv[a; b]` | write | — | rename or move |
| `cp[a; b]` | write | — | files only |

```q
.std.fs.exists "Cargo.toml"     / true
.std.fs.join["a"; "b.txt"]      / "a/b.txt"
.std.fs.mkdir "scratch"         / refused without -w
```

## `.std.env`

Outside the browser only (the `os` feature). `r`, `has` and `all` check an
in-process overlay first, then the real process environment; `s` writes only
to that overlay, never to the process environment — an environment variable
is set from outside the process, so a read-only session must not be able to
override one, and child processes and Polars never see an overlay value.

| Function | Effect | Result | Semantics |
|---|---|---|---|
| `r[k]` | iread | string | the overlay, then the environment; `""` when unset |
| `s[k; v]` | write | — | writes to the overlay only |
| `has[k]` | iread | bool | |
| `all` | iread | table | columns `name`, `value`; overlay entries win; sorted by name |

```q
.std.env.r "HOME"
.std.env.s["MY_FLAG"; "1"]   / refused without -w; never touches the real environment
```

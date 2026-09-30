# Rust extensions

qpl can be extended with native functions written in Rust. An extension
function is an ordinary Rust function with one attribute on it, and the
attribute's one required argument is its permission:

```rust
#[qpl::native(read)]
fn km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    // great-circle distance
}

#[qpl::native(write)]
fn note(path: String, text: String) -> std::io::Result<()> {
    // append a line to a file
}
```

The permission is one of `read`, `iread` or `write` — the same three every
built-in action has (see [Read-only sessions](language/read-only.md)). A
`read` function can be called from any session, including over a read-only
IPC handle. An `iread` function is refused over a read-only IPC handle, but
allowed in any local session, read-only or not. A `write` function is
refused in a read-only session, just as `sink` is, with the same error. The
VM checks the permission before every call, so an extension can't widen what
an agent in a read-only session is able to do, however it's called. Leaving
the permission out is a compile error.

## A qpl binary with extensions

Extensions are linked in when qpl is compiled, not loaded at run time. Rust
has no stable ABI for loading them safely. An extension is a small Rust crate
that builds its own `qpl` binary with its functions added:

```toml
# Cargo.toml
[dependencies]
qpl = { git = "https://github.com/nicelgueta/qpl" }
qpl-cli = { git = "https://github.com/nicelgueta/qpl" }
```

```rust
// src/main.rs
#[qpl::native(read)]
fn km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let a = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().asin()
}

fn main() {
    let geo = qpl::ext::Extension::new("geo").with::<km>();
    qpl_cli::run(vec![geo]);
}
```

`qpl_cli::run` is the whole `qpl` command line, the same one the stock
binary uses. So the result takes every flag `qpl` does, including `-w`, and
behaves identically apart from the new functions:

```bash
cargo build --release
./target/release/geo-qpl -c '.geo.km[51.51;-0.13;40.71;-74.01]'
```

The qpl repository has two complete versions of this in `qpl-cli/examples/`:
`extension.rs` is the one above plus a write function. `extension_toolkit.rs`
registers two extensions with several functions each, covering every kind of
argument and result: lists in and out, a table in and out, a lazy plan, a
function with no arguments, a renamed function, and errors from a `Result`.

```bash
cargo run -p qpl-cli --example extension -- -c '.geo.km[51.51;-0.13;40.71;-74.01]'
cargo run -p qpl-cli --example extension_toolkit -- --load-demo -c '.stats.top[trades; `price; 3]'
```

An extension with several functions chains `with` once per function, and a
binary can register as many extensions as it likes:

```rust
let stats = Extension::new("stats")
    .with::<mean>()
    .with::<zscore>()
    .with::<top_n>();
let files = Extension::new("files").with::<exists>().with::<append>();
qpl_cli::run(vec![stats, files]);
```

One namespace can mix read and write functions. Each function's own
permission is what counts: in a read-only session `.files.exists` works and
`.files.append` is refused.

## Calling them

An extension's functions live under its namespace: `Extension::new("geo")`
makes `km` available as `.geo.km`. They're called like any other function:

```qpl
qpl) .geo.km[51.51;-0.13;40.71;-74.01]
f64: 5570.444571708345
qpl) d: .geo.km[51.51;-0.13;40.71;-74.01]
```

Like qpl's own `.qpl.*` builtins, an extension function can't be rebound, so
a script can't replace it with something else. A function that takes no
arguments is called by naming it, the same way `.qpl.dt` is. The namespace
`qpl` is reserved, and registering two functions under the same name fails.

Extension functions work on values: scalars, lists and whole tables. Like
user-defined functions, they can't be called inside a `select` projection. To
work on a column, take a list or a table.

## Dotted namespaces and ownership

`Extension::new` accepts a dotted namespace (`"std.fs"`), so a crate can
group several extensions under a shared root: `.std.fs.exists`,
`.std.env.r`, and so on. Each dot-separated segment is validated like any
other identifier; an empty segment is rejected.

The first extension registered under a root (`std`, here) owns it. A second
extension registering under the same root is refused unless it declares the
same owner with `Extension::owner`, which defaults to the extension's own
namespace:

```rust
let str_ext = Extension::new("std.str").owner("qpl-std").with::<upper>();
let arr_ext = Extension::new("std.arr").owner("qpl-std").with::<rv>();
vm.register(str_ext)?;
vm.register(arr_ext)?; // same owner, same root: allowed
```

Without an explicit owner, two unrelated extensions can't collide on a root
by accident — each is refused the other's namespace unless they opt in by
naming the same owner.

## String arguments

`ext::StrArg` accepts either a string/symbol scalar or a string/symbol list,
so one function handles both:

```rust
#[qpl::native(read)]
fn shout(s: StrArg) -> Value {
    s.map(str::to_uppercase)
}
```

`StrArg::map` applies an elementwise transform and returns the matching
shape: a scalar for a scalar argument, a string list for a list argument
(nulls pass through unchanged).

## Preserving a list's kind

A function that transforms a whole list without changing its element type
uses `Value::map_vec`, which rewraps the result in the same vector variant:
a `DateVec` argument comes back as a `DateVec`, not a plain list:

```rust
#[qpl::native(read)]
fn rv(xs: Value) -> Result<Value, String> {
    xs.map_vec(|s| Ok(s.reverse()))
}
```

`map_vec` errors if the argument isn't a list.

## Arguments and results

Parameters and return values convert to and from qpl values by type:

| Rust type | qpl value |
|---|---|
| `i64` | int |
| `f64` | float (an int is accepted as an argument) |
| `bool` | bool |
| `String` | string, or a symbol as an argument |
| `Vec<i64>`, `Vec<f64>`, `Vec<bool>`, `Vec<String>` | a list of that type |
| `Series` | any list, as an argument |
| `qpl::ext::StrArg` | a string/symbol or a list of either, as an argument |
| `DataFrame` | a table (a lazy argument is collected first) |
| `LazyFrame` | a table, kept lazy |
| `qpl::ast::Value` | any value, unconverted |

Arguments are passed by value, so a parameter takes an owned type (`String`,
not `&str`).

Take the Polars types from `qpl::ext::polars` (`use
qpl::ext::polars::prelude::*;`) rather than adding your own `polars`
dependency. They have to be the exact version qpl is built against, and a
separate dependency can easily resolve to a different one.

A function can also return `()`, in which case it prints nothing, or a
`Result` of any of the above whose error implements `Display`. An `Err`
becomes an ordinary qpl runtime error, prefixed with the function's name:

```qpl
qpl) .geo.note["/no/such/dir/notes.txt"; "hi"]
'.geo.note: No such file or directory (os error 2)
```

An argument of the wrong type is reported the same way, naming the argument:

```qpl
qpl) .geo.km["x";0;0;0]
'.geo.km: argument 1: expected a float, got a string
```

`name = "..."` in the attribute changes the name the function has in qpl,
for when the Rust name doesn't suit: `#[qpl::native(read, name = "dist")]`.

## Choosing read, iread or write

Declare a function `write` if it changes anything that outlives the session:
it writes a file, sends a message, changes a database, or makes a request
that changes state somewhere else.

Declare it `iread` if it only reads, but reads something outside the
session: a file's contents, an environment variable, a network request. A
read-only IPC handle shouldn't be able to make the server read arbitrary
files or secrets on its behalf, so `iread` keeps that reachable locally
(and over a write handle) while a read-only connection can't trigger it.

Everything else — a function that only looks at the arguments it's given —
is `read`.

The attribute is a declaration, and qpl can't check the function body
against it. A function declared `read` that reads a file or writes to disk
anyway breaks the guarantee for every session it's linked into. Extensions
are trusted code, so review them the way you'd review anything else that
goes into the binary you give an agent.

## Other hosts

A program that embeds qpl without its command line can register extensions
on a session directly, with `Vm::register(extension)`, before it runs
anything. The rules are the same.

A [compiled `.qplc` file](architecture/compiled-artifacts.md) refers to
extension functions by name, so it runs on any binary that has the same
extensions registered.

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

The permission is the same one every built-in action has (see
[Read-only sessions](language/read-only.md)). A `read` function can be called
from any session. A `write` function is refused in a read-only session, just
as `sink` is, with the same error. The VM checks it before every call, so an
extension can't widen what an agent in a read-only session is able to do,
however it's called. Leaving the permission out is a compile error.

## A qpl binary with extensions

Extensions are linked in when qpl is compiled, not loaded at run time. Rust
has no stable ABI for loading them safely. An extension is a small Rust crate
that builds its own `qpl` binary with its functions added:

```toml
# Cargo.toml
[dependencies]
qpl = { git = "https://github.com/nicelgueta/qpl" }
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
    qpl::cli::run(vec![geo]);
}
```

`qpl::cli::run` is the whole `qpl` command line, the same one the stock
binary uses. So the result takes every flag `qpl` does, including `-w`, and
behaves identically apart from the new functions:

```bash
cargo build --release
./target/release/geo-qpl -c '.geo.km[51.51;-0.13;40.71;-74.01]'
```

The qpl repository has two complete versions of this. `examples/extension.rs`
is the one above plus a write function. `examples/extension_toolkit.rs`
registers two extensions with several functions each, covering every kind of
argument and result: lists in and out, a table in and out, a lazy plan, a
function with no arguments, a renamed function, and errors from a `Result`.

```bash
cargo run --example extension -- -c '.geo.km[51.51;-0.13;40.71;-74.01]'
cargo run --example extension_toolkit -- --load-demo -c '.stats.top[trades; `price; 3]'
```

An extension with several functions chains `with` once per function, and a
binary can register as many extensions as it likes:

```rust
let stats = Extension::new("stats")
    .with::<mean>()
    .with::<zscore>()
    .with::<top_n>();
let files = Extension::new("files").with::<exists>().with::<append>();
qpl::cli::run(vec![stats, files]);
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

## Choosing read or write

Declare a function `write` if it changes anything that outlives the session:
it writes a file, sends a message, changes a database, or makes a request
that changes state somewhere else. Everything else is `read`.

The attribute is a declaration, and qpl can't check the function body
against it. A function declared `read` that writes to disk anyway breaks the
read-only guarantee for every session it's linked into. Extensions are
trusted code, so review them the way you'd review anything else that goes
into the binary you give an agent.

## Other hosts

A program that embeds qpl without its command line can register extensions
on a session directly, with `Vm::register(extension)`, before it runs
anything. The rules are the same.

A [compiled `.qplc` file](architecture/compiled-artifacts.md) refers to
extension functions by name, so it runs on any binary that has the same
extensions registered.

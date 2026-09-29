# The VM

The VM is one struct, `Vm`, with one map of bindings, a stack, and a handful
of registers. Tables, lazy pipelines, functions and plain numbers all share
that one map, and the stack does double duty as the scope chain. So there
are only a few places anything can live. This chapter goes through each of
them, then follows a query, a function call and a namespaced import as they
run.

## Where things live

**`globals`** is a single `HashMap<String, Value>` holding every
session-level binding: scalars, vectors, tables, lazy pipelines and
user-defined functions (which are just `Closure` values). The result of a
`select` and a number you typed at the prompt are stored the same way. The
only difference is which `Value` variant each one is. A namespaced name like
`.lib.x` (more on those [below](#namespaces)) is an ordinary key that happens
to have dots in it.

**`builtins`** is a second, much smaller map of reserved native functions,
such as the `.qpl.dt`-style "now" functions. It's built once when the `Vm`
is created and never changes after that. A name in this map is off limits.
Trying to assign to it is an error rather than a quiet shadowing, so there's
never any doubt about which one you'll get.

**Registers** keep track of where execution has got to:

- `prog`: the `Program` currently running.
- `ip`: the instruction pointer into `prog`'s code.
- `cp`: a second cursor, into `prog`'s operand stream, which only moves
  when a `Push` reads from it.
- `fp`: where on the stack the current function call's frame sits, if
  there is one.
- `call_depth`: how many calls deep we are, checked against a fixed limit
  (128), so runaway recursion fails with a tidy error instead of growing
  the stack forever.

**`stack`** holds everything else, meaning the actual working state of
whatever query or calculation is in flight. Each entry is a `Slot`:

| `Slot` | Holds |
|---|---|
| `Expr` | a Polars column expression under construction |
| `Frame` | a table under construction, plus a flag saying whether it's lazy |
| `Scalar` | a single value |
| `List` | a projection, key or predicate list being assembled |
| `Operand` | whatever `Push` just put there, waiting for the next instruction to take it |
| `Call` | a function activation (see below) |

Laziness travels with the `Frame` itself rather than being tracked off to
the side. Reading a `Value::Lazy` binding, or hitting the `lazy` keyword,
sets the flag. `collect` (or `cols`) clears it.
That's why `lazy select ...` and a plain `select ...` compile to almost the
same instructions. The difference comes down to that one flag and what
happens at the very end: an eager frame gets collected into a `DataFrame`,
and a lazy one gets its plan explained instead.

The `Vm` does carry a few other fields, such as the session settings from
`.qpl.cfg`, the Ctrl-C flag, the `\1` log file and an open `\port` listener.
They're configuration and plumbing, though, not execution state. Nothing
about *where the program is* lives in them.

## Instructions and operands

An instruction (`Op`) is exactly one byte, and none of them carry data
inline. Instead, `Push` reads the next value off the side `operands`
stream and leaves it on the stack as a `Slot::Operand`, and the instruction
*after* it picks it up from there. So `select from trades where size > 100`
comes out as:

```
0000  PUSH       Name(trades)
0001  SOURCE
0002  PUSH       Name(size)
0003  LOAD_COL
0004  PUSH       Value(Int(100))
0005  PUSH       BinOp(Gt)
0006  BINOP
0007  PUSH       Count(1)
0008  FILTER
0009  PUSH       Count(0)
0010  LIST
0011  SELECT
```

Read it top to bottom and the query more or less narrates itself: fetch
`trades`, load the `size` column, push `100`, push the comparison, apply it,
filter by that one predicate, build an empty projection list (no columns
named means all of them), and select. `\d` (see the
[REPL chapter](../repl.md#seeing-what-a-statement-compiles-to)) prints exactly
this for any statement you give it, and `qpl -d` does the same for a whole
file. It's by far the quickest way to get a feel for this model.

Jump targets (the top of a `while` loop, a branch of `?[..]`, a function's
entry point, a call's return address) are operands too, `Target { ip, cp }`.
A jump sets *both* registers at once. That's what lets a loop body re-read
its own operands every time round, and lets a skipped branch skip its
operands as well instead of leaving the cursor stranded halfway through
them.

## Function calls are more stack, not more Rust stack

Calling a qpl function doesn't recurse in Rust. `CALL` checks the arity,
the call depth and the interrupt flag. It then pushes a `Slot::Call` holding
the caller's `(prog, ip, cp, fp)`, with the arguments bound as the callee's
locals, and jumps to the function's entry point. If the function was
defined in a different `Program` (inside an embedded include, say), `prog`
switches over too.

`RET` runs the same film backwards. It pops the result, cuts the stack back
down to `fp`, restores the caller's registers from the frame it's
discarding, and pushes the result for the caller to carry on with.

Name lookup follows the same structure. A bare name is checked against the
reserved `builtins` first, then the *innermost* call's locals, then
`globals`. It never looks in some caller's locals further up. That means no
dynamic scoping and no separate scope stack to keep in sync. The call frames
on the stack are the scoping.

A call that doesn't resolve to a closure or a reserved builtin isn't a
failure yet. It falls through to a last-resort dispatch by name
(`ops::call_by_name`), which is where the everyday primitives like `til`,
`log`, `hopen` and `await` live. Because they're only reached when nothing
else matched, a function of your own with the same name takes priority.

## Namespaces

Importing a file under a namespace with `\i` is settled entirely at
**compile time**. There's no runtime search order at all. When the compiler
meets an `\i`, it first scans the imported file's top-level assignments to
collect every name the file binds. It then rewrites every assignment to, and
every bare mention of, one of those names to its namespaced form. That
covers the whole file, function bodies included, except where a parameter or
local shadows the name. Only after that does it compile anything. The
namespace itself comes from the file's name (see
[Scripts, imports & namespaces](../language/imports.md)).

The upshot is that a bare name a file binds at top level *always* means the
file's own binding, even in a statement that runs before the binding does.
A library can't read a session-level `t` and quietly rebind it with
`t: select from t where ...`. It reads its own `.lib.t`, which doesn't exist
yet, and fails right there.

At run time, the `\i` native handles the transactional part. It snapshots
`globals`, clears out any `.lib.*` keys left by a previous import, runs the
embedded program, and puts the snapshot back if anything goes wrong. An
import either lands completely or leaves the session exactly as it found it.

## The row index `i`

The virtual row-index column `i` is sorted out at compile time as well. When
a query mentions `i`, the compiler notices and inserts a `RowIndex`
instruction right after the table source it belongs to. By the time the VM
sees the query, `i` is just another column, and queries that never mention
it don't pay anything for it.

## Reducing to a result

A top-level statement finishes in one of four shapes (`EvalResult`): a
`Table`, a `Scalar`, `Lazy` (the plan text you see when a lazy pipeline is
shown rather than collected), or `Stored` for an assignment, which prints
nothing. In script mode each statement's result is printed as it goes. In
result mode (`run_vm`, an IPC request) the last one is handed back to
whoever asked, and for IPC it's then encoded and sent back down the wire.

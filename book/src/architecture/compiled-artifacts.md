# Compiled artifacts

Since the compiler finishes its work before anything runs, it's a small step
to save that work and skip it next time. `qpl -C` does exactly that. It
compiles a script and writes the result to a `.qplc` file without running
any of it:

```bash
qpl -C script.qpl              # writes script.qplc
qpl -C script.qpl -o out.qplc  # or wherever -o says
```

The file is written to a temporary path first and renamed into place at the
end, so a script that fails to compile never leaves a half-written artifact
behind.

Running one looks exactly like running a script:

```bash
qpl script.qplc
```

qpl doesn't go by the extension. It looks for the `"QPLC"` magic bytes at
the start of the file, and if it finds them it skips lexing, parsing and
compiling and goes straight to the VM. If you're curious what's inside,
`qpl -d` prints the bytecode as text, and it works on a `.qplc` or a
plain `.qpl` alike:

```bash
qpl -d script.qplc | less
```

## Why this works

A `.qplc` file is just the three parts of a `Program` (`code`, `operands`
and `lines`) written to disk, recursing into any embedded sub-programs on
the way. Because [`\l`/`\i` targets are resolved and embedded at compile
time](pipeline.md#where-includes-fit-in), a compiled artifact never has to
go looking for its source files again. It's completely self-contained, and
running it has exactly the same effect as running the script it came from.

It's worth being clear about what that does and doesn't mean. `.qplc` is a
*convenience* for skipping recompilation. It's not a security boundary and
it's not obfuscation. It does exactly what the source would do, no more and
no less, and `qpl -d` will happily show anyone what that is.

## Versioning

After the magic bytes comes a `u16` format version, followed by the
version of qpl that wrote the file (that one is purely informational and
never checked). The format version gets bumped whenever the meaning of the
bytes changes: a new, removed or renumbered instruction, a change to how an
operand is tagged, a new `NativeId`, or a change to how a value is encoded.
That bump is what turns a stale artifact that would otherwise decode into
the wrong program into a clean, immediate error telling you it was compiled
with an incompatible qpl and should be recompiled with `qpl -C`.

Loading a `.qplc` never panics, however mangled it is. A truncated file,
an unknown instruction, operand or value tag, a jump target pointing off
the end of the program, or leftover bytes after the program has been read
all come back as an ordinary error rather than a crash.

## The value codec

Literal values in a program's operands and scalar results sent over IPC are
both encoded by the same small binary codec (`codec`). It's lossless for
every scalar and vector `Value`, nulls included, which get a proper
per-element validity flag instead of being dropped or turned into a
placeholder string.

Five kinds of value have no encoding at all: tables, lazy pipelines,
closures, and IPC's connection handles and pending futures. None of them
can ever turn up as a literal operand. A function literal in the source
compiles to its own dedicated operand form (a prototype of the function,
not a live closure), and the other four only exist at run time. If one ever
did reach an operand, `to_bytes` would refuse with an error rather than
write out garbage.

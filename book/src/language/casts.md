# Casts

`type$expr` casts a column or scalar:

```qpl
select f: f64$size from trades
select f64$size, str$sym from trades

l: int$45.3                    / scalar: 45
ok: bool$"true"                / scalar: 1b
n: 1 + int$"42"                / string parses, then composes: 43
n:  (int$"42") - 1             / remember right to left evaluation, so parens for subtraction
```

Types: `f64`/`float`, `f32`, `i64`/`int`, `i32`, `i16`, `i8`, `u64`, `u32`,
`u16`, `u8`, `bool`, `str`/`string`. In a scalar context every integer width
folds to a single 64-bit integer and `f32`/`f64` to a single float — the width
only takes effect once the value lands in a column. A string (or symbol) scalar
is parsed: `int$"42"`, `f64$"3.5"`, `bool$"false"`.

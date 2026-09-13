## Architecture

```
source -> lexer -> tokens -> parser -> AST -> compiler -> instructions -> VM (Polars LazyFrame) -> DataFrame
```

| Module | Role |
|---|---|
| `lexer` | tokenise source text |
| `parser` | build the typed AST |
| `compiler` | emit stack-machine instructions |
| `vm` | execute instructions, build & collect a `LazyFrame` |
| `repl` | interactive loop + script runner |

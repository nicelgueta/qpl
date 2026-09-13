## Working incrementally

This is the part qpl really leans on: **a transformation is a sequence of
statements, and you build it one statement at a time.**

Every step binds a name; the binding persists, so the next step starts from it.
There's no re-running a growing query, no stacking CTEs, no scrolling up to edit
and resubmit a 30-line block — the loop is *type a line, look, type the next*.

```qpl
qpl) t: load "trades.parquet"      / bind a table
qpl) t                              / look at it
qpl) cols t                         / ...or just its schema

qpl) t: select sym, side, price, size from t where price > 0
qpl) t: update notional: price * size from t

qpl) / not sure about the next step? run it WITHOUT assigning — the source is untouched
qpl) update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from t
qpl) t: update band: ?[size >= 1000; `large; size >= 250; `mid; `small] from t   / keep it

qpl) select traded: sum notional by sym, side, band from t order traded desc
qpl) t sink "out.parquet"
```

Things that make the loop tight:

- **`t` / `cols t`** — peek at a table or its schema between steps.
- **Run a statement without assigning it** — you see the result, nothing changes.
  Assign only once you're happy.
- **`\d <stmt>`** — show the compiled plan without executing anything.
- **`lazy`** binds a *plan* rather than a table: nothing touches disk until a
  `collect` or `sink`, so building a large pipeline is instant and you pay for it
  once, at the end.
- In the [VSCode extension](https://github.com/nicelgueta/qpl/tree/main/tools/vscode), **Ctrl+Enter** sends the current line
  or selection to this same session.

The SQL equivalent is: edit the query, re-run the whole thing, eyeball the
result, comment a block out to isolate a step, uncomment it, repeat.

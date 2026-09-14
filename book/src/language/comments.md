# Comments

We may as well start with the smallest piece of syntax in the language, since
it appears in nearly every example from here on.

`/` begins a comment, which runs to the end of the line:

```qpl
/ a whole line of commentary
select from trades  / or tacked onto the end of a statement
```

That's all there is to it. There are no block comments and no nesting, which
suits a language where the average statement is one line long. If you find
yourself wanting to write three paragraphs of explanation, that probably
belongs in a README next to the script rather than inside it.

## The one consequence

Giving `/` to comments means it isn't available for division, so qpl follows
q and uses `%` instead:

```qpl
qpl) 10.0 % 4
```

```
f64: 2.5
```

This is the single most common thing to trip over when you're new to the
language, and the fix is muscle memory rather than understanding. It's worth
knowing now, before arithmetic turns up properly in
[Expressions](expressions.md), so that the first `%` you see doesn't read as
a modulo operator.

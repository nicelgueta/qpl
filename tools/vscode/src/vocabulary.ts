/**
 * Static qpl vocabulary — single source of truth for completion. Kept in sync
 * by hand with syntaxes/qpl.tmLanguage.json (`keyword`, `aggregate`, `cast`
 * repository rules) and src/lexer.rs / src/tokens.rs in the parent crate.
 */

export const STATEMENT_KEYWORDS = [
  'select', 'by', 'from', 'where', 'over', 'order', 'asc', 'desc',
  'distinct', 'limit', 'drop', 'update', 'delete',
];

// `hopen`/`await`/`async` are grouped with the other builtins (not
// AGGREGATES/WORD_OPERATORS) so they share load/sink/cols's highlighting
// colour — `ipc` feature only.
export const BUILTIN_KEYWORDS = [
  'load', 'sink', 'cols', 'lazy', 'collect', 'til', 'zip', 'hopen', 'await', 'async',
];

export const JOIN_OPERATORS = ['lj', 'ij', 'rj'];

// `dispatch` is a bareword infix operator (`<conn> [async] dispatch <rest>`),
// same shape as `like` — `ipc` feature only.
export const WORD_OPERATORS = ['like', 'dispatch'];

export const AGGREGATES = [
  'sum', 'avg', 'mean', 'min', 'max', 'count', 'first', 'last', 'std', 'dev',
  'var', 'med', 'median', 'mode', 'modal', 'skew', 'kurt', 'kurtosis', 'any',
  'all', 'prod', 'product', 'argmin', 'argmax', 'nnull', 'null_count',
  'cumsum', 'cummax', 'cummin', 'cumprod', 'cumcount', 'ffill', 'bfill',
  'abs', 'neg', 'not', 'string', 'n_unique', 'round', 'quantile', 'pctl',
  'shift', 'lag', 'lead', 'diff', 'pctchange', 'rolling', 'rn', 'rank',
  'drank',
];

export const CAST_TYPES = [
  'f64', 'float', 'f32', 'i64', 'int', 'i32', 'i16', 'i8',
  'u64', 'u32', 'u16', 'u8', 'bool', 'str', 'string',
];

export const REPL_COMMANDS = ['\\d', '\\1', '\\l', '\\i', '\\port', 'log'];

/** Detail strings shown alongside completion items, keyed by identifier. */
export const AGGREGATE_DETAIL: Record<string, string> = {
  sum: 'sum(expr) — aggregate: sum',
  avg: 'avg(expr) — aggregate: mean',
  mean: 'mean(expr) — aggregate: mean',
  min: 'min(expr) — aggregate: minimum',
  max: 'max(expr) — aggregate: maximum',
  count: 'count(expr) — aggregate: row count',
  first: 'first(expr) — aggregate: first value',
  last: 'last(expr) — aggregate: last value',
  std: 'std(expr) — aggregate: standard deviation',
  dev: 'dev(expr) — aggregate: standard deviation',
  var: 'var(expr) — aggregate: variance',
  med: 'med(expr) — aggregate: median',
  median: 'median(expr) — aggregate: median',
  round: 'round(expr) — round to nearest integer',
  n_unique: 'n_unique(expr) — count of distinct values',
  rn: 'rn — row number window function (with `over`)',
  rank: 'rank — rank window function (with `over`)',
  drank: 'drank — dense rank window function (with `over`)',
  shift: 'shift(expr) — shift values (with `over`)',
  lag: 'lag(expr) — previous value (with `over`)',
  lead: 'lead(expr) — next value (with `over`)',
  diff: 'diff(expr) — difference from previous value',
  rolling: 'rolling(expr) — rolling window aggregate',
};

export const KEYWORD_DETAIL: Record<string, string> = {
  select: 'select <cols> from <table> [by <keys>] [where <preds>] [order ...]',
  update: 'update <col>: <expr> from <table> [by <keys>] [where <preds>]',
  delete: 'delete from <table> where <preds>  |  delete `col1`col2 from <table>',
  from: 'source table for a select/update/delete',
  by: 'group-by keys',
  where: 'row filter predicate(s), comma-separated = AND',
  over: 'window-function partition (with rn/rank/drank/shift/lag/lead)',
  order: 'sort the result — followed by <col> asc|desc, ...',
  asc: 'ascending sort direction',
  desc: 'descending sort direction',
  distinct: 'deduplicate rows',
  limit: 'limit the number of returned rows',
  drop: 'drop named columns from a table',
  load: 'load <path> — read a table from disk',
  sink: 'sink <table> <path> — write a table to disk',
  cols: 'cols <table> — list column names',
  lazy: 'bind a lazy query plan instead of a materialised table',
  collect: 'collect a lazy plan into a table',
  lj: 'left join',
  ij: 'inner join',
  rj: 'right join',
  like: '<str/sym> like <pattern> — q-glob match (* any sequence, ? one char, [..] a class)',
  dispatch: '<conn> dispatch <stmt> — send a statement to a connection, block for the reply (`ipc` feature)',
  async: '<conn> async dispatch <stmt> — like dispatch, but returns immediately (`ipc` feature)',
  hopen: 'hopen <port | "host:port"> — open a read-only IPC connection; `w!hopen` opens a write handle (`ipc` feature)',
  await: 'await <pending> — resolve an `async dispatch` reply (`ipc` feature)',
  '\\port': '\\port <n> — start serving on port n; bare \\port stops (`ipc` feature, REPL only)',
  '\\i': '\\i "<path>" — run a script, namespacing its new tables/globals/functions under `.<file-stem>.*`',
  til: 'til <n> — list 0..n-1  |  <lo> til <hi> — list lo..hi-1',
  zip: 'zip `k1`k2!v1 v2 — build a table from a dict of named lists',
};

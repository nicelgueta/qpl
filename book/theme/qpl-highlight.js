// highlight.js grammar for qpl code fences (```qpl).
//
// mdbook's own book.js already ran its hljs.highlightBlock() pass by the
// time this file (an `additional-js` entry) loads — that pass silently
// no-ops on our blocks since `qpl` wasn't a registered language yet, leaving
// them as plain unhighlighted text. So after registering the language here,
// we re-run highlighting ourselves on just the `language-qpl` blocks.
//
// Kept in sync by hand with the VS Code extension's TextMate grammar
// (tools/vscode/syntaxes/qpl.tmLanguage.json) and its vocabulary
// (tools/vscode/src/vocabulary.ts) — same keyword lists, same intent, just a
// different grammar format (highlight.js has no TextMate importer).
(function () {
  if (typeof hljs === 'undefined') return;

  hljs.registerLanguage('qpl', function (hljs) {
    var STATEMENT_KEYWORDS = [
      'select', 'by', 'from', 'where', 'over', 'order', 'asc', 'desc',
      'distinct', 'limit', 'drop', 'update', 'delete', 'lj', 'ij', 'rj',
      'like', 'dispatch',
    ];

    var BUILTIN_KEYWORDS = [
      'load', 'sink', 'cols', 'lazy', 'collect', 'til', 'zip', 'hopen',
      'await', 'async',
    ];

    var AGGREGATES = [
      'sum', 'avg', 'mean', 'min', 'max', 'count', 'first', 'last', 'std',
      'dev', 'var', 'med', 'median', 'mode', 'modal', 'skew', 'kurt',
      'kurtosis', 'any', 'all', 'prod', 'product', 'argmin', 'argmax',
      'nnull', 'null_count', 'cumsum', 'cummax', 'cummin', 'cumprod',
      'cumcount', 'ffill', 'bfill', 'abs', 'neg', 'not', 'string',
      'n_unique', 'round', 'quantile', 'pctl', 'shift', 'lag', 'lead',
      'diff', 'pctchange', 'rolling', 'rn', 'rank', 'drank',
    ];

    var CAST_TYPES = [
      'f64', 'float', 'f32', 'i64', 'int', 'i32', 'i16', 'i8', 'u64', 'u32',
      'u16', 'u8', 'bool', 'str', 'string',
    ];

    var COMMENT = { className: 'comment', begin: '/', end: '$' };

    var STRING = {
      className: 'string',
      begin: '"',
      end: '"',
      contains: [{ begin: '\\\\[ntr"\\\\]' }],
    };

    // backtick symbols, possibly chained: `sym `sym`day `$expr
    var SYMBOL = {
      className: 'symbol',
      begin: '`[A-Za-z0-9_./-]*(?:`[A-Za-z0-9_./-]*)*',
    };

    // REPL-only system commands / log, at the start of a line
    var REPL_COMMAND = {
      className: 'keyword',
      begin: '^\\s*(\\\\(?:[d1l]|port)|log)\\b',
    };

    // `.qpl.cfg`, `.qpl.dt`, etc.
    var NAMESPACED_BUILTIN = {
      className: 'keyword',
      begin: '\\.qpl\\.[A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*)*',
    };

    var TEMPORAL = {
      className: 'number',
      // dates/months/timestamps: 2024.03.15, 2024.03m, 2024.03.15D12:30:00.000000000
      begin: '\\b\\d{4}\\.\\d{2}(?:\\.\\d{2}(?:D\\d{2}:\\d{2}:\\d{2}(?:\\.\\d+)?)?|m)?\\b',
    };

    var TIMESPAN = {
      className: 'number',
      // 0D12:30:00.000000000
      begin: '\\b\\d+D\\d{2}:\\d{2}:\\d{2}(?:\\.\\d+)?\\b',
    };

    var TIME_OF_DAY = {
      className: 'number',
      // 12:30:00.000 / 12:30:00 / 12:30
      begin: '\\b\\d{2}:\\d{2}(?::\\d{2}(?:\\.\\d+)?)?\\b',
    };

    var BOOL_VEC = { className: 'number', begin: '\\b[01]+b\\b' };
    var FLOAT = { className: 'number', begin: '\\b\\d+\\.\\d*\\b' };
    var INT = { className: 'number', begin: '\\b\\d+\\b' };

    var CAST = {
      begin: '\\b(?:' + CAST_TYPES.join('|') + ')\\$',
      returnBegin: true,
      contains: [
        { className: 'type', begin: '\\b(?:' + CAST_TYPES.join('|') + ')\\b' },
        { className: 'operator', begin: '\\$' },
      ],
    };

    var ASSIGNMENT = {
      className: 'variable',
      begin: '^\\s*[A-Za-z_]\\w*(?=\\s*:(?!=))',
    };

    var INDEX_COL = { className: 'variable.language', begin: '\\bi\\b' };

    var OPERATOR = {
      className: 'operator',
      begin: '<>|!=|<=|>=|::|[-+*%=<>&|?$!#]',
    };

    return {
      name: 'qpl',
      case_insensitive: false,
      keywords: {
        keyword: STATEMENT_KEYWORDS.concat(BUILTIN_KEYWORDS).join(' '),
        built_in: AGGREGATES.join(' '),
      },
      contains: [
        COMMENT,
        REPL_COMMAND,
        STRING,
        SYMBOL,
        NAMESPACED_BUILTIN,
        TEMPORAL,
        TIMESPAN,
        TIME_OF_DAY,
        BOOL_VEC,
        FLOAT,
        INT,
        ASSIGNMENT,
        CAST,
        INDEX_COL,
        OPERATOR,
      ],
    };
  });

  document.querySelectorAll('code.language-qpl').forEach(function (block) {
    // skip if something already highlighted it (e.g. a future mdbook that
    // registers additional-js languages before its own pass)
    if (block.querySelector('span')) return;
    hljs.highlightBlock(block);
    block.classList.add('hljs');
  });
})();

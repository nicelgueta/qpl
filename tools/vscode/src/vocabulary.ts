/**
 * Static qpl vocabulary — single source of truth for completion. The data
 * itself lives in `vocabulary.json` so the Rust side can `include_str!` it too:
 * `qplLangConfig()` (the `wasm` feature, see `src/wasm.rs` in the parent crate)
 * builds its Monaco language configuration from the same file, which is what
 * keeps a browser editor and this extension from drifting apart.
 *
 * Kept in sync by hand with syntaxes/qpl.tmLanguage.json (`keyword`,
 * `aggregate`, `cast` repository rules) and src/lexer.rs / src/tokens.rs in the
 * parent crate.
 */

import vocabulary from './vocabulary.json';

export const STATEMENT_KEYWORDS: string[] = vocabulary.statementKeywords;

// `hopen`/`await`/`async` are grouped with the other builtins (not
// AGGREGATES/WORD_OPERATORS) so they share load/sink/cols's highlighting
// colour — `ipc` feature only.
export const BUILTIN_KEYWORDS: string[] = vocabulary.builtinKeywords;

export const JOIN_OPERATORS: string[] = vocabulary.joinOperators;

// `dispatch` is a bareword infix operator (`<conn> [async] dispatch <rest>`),
// same shape as `like` — `ipc` feature only.
export const WORD_OPERATORS: string[] = vocabulary.wordOperators;

export const AGGREGATES: string[] = vocabulary.aggregates;

export const CAST_TYPES: string[] = vocabulary.castTypes;

export const REPL_COMMANDS: string[] = vocabulary.replCommands;

/** Detail strings shown alongside completion items, keyed by identifier. */
export const AGGREGATE_DETAIL: Record<string, string> = vocabulary.aggregateDetail;

export const KEYWORD_DETAIL: Record<string, string> = vocabulary.keywordDetail;

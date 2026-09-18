//! Browser bindings (`wasm` feature).
//!
//! Two exports:
//!
//! * [`Repl`] — a long-lived [`Vm`] behind an `eval(line)` method. It is the
//!   same statement pipeline the terminal REPL uses (`repl::eval_capture`
//!   wraps the same `run_line` that `repl::start` calls), so every language
//!   feature, `\` command and `.qpl.cfg` knob behaves identically; the only
//!   difference is that output is returned as a string instead of printed.
//!   Anything touching the filesystem (`load`, `sink`, `\l`, `\i`, `\1`) or a
//!   socket (`ipc`, which is off in this build) fails as an ordinary qpl
//!   runtime error rather than breaking the session.
//!
//! * [`qpl_lang_config`] — the editor configuration (Monarch tokenizer,
//!   language configuration, completion items) built from the *same*
//!   `tools/vscode/src/vocabulary.json` the VS Code extension reads, so a
//!   browser editor and the extension cannot drift apart.

use crate::repl;
use crate::vm::Vm;
use js_sys::{Object, Reflect};
use serde_json::{Value as Json, json};
use wasm_bindgen::prelude::*;

/// `tools/vscode/src/vocabulary.json` — keyword/aggregate lists plus their
/// hover-detail strings. Shared verbatim with the VS Code extension.
const VOCABULARY: &str = include_str!("../tools/vscode/src/vocabulary.json");
/// `tools/vscode/language-configuration.json` — brackets, comments,
/// auto-closing pairs, indentation rules.
const LANG_CONFIG: &str = include_str!("../tools/vscode/language-configuration.json");
/// `tools/vscode/snippets/qpl.json` — VS Code snippet definitions, re-shaped
/// into Monaco completion items below.
const SNIPPETS: &str = include_str!("../tools/vscode/snippets/qpl.json");

/// A qpl session: one [`Vm`], fed one line at a time.
///
/// ```js
/// const repl = new Repl();
/// repl.loadDemo();
/// const { output, error } = repl.eval("select from trades");
/// ```
#[wasm_bindgen]
pub struct Repl {
    vm: Vm,
}

#[wasm_bindgen]
impl Repl {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Repl {
        Repl { vm: Vm::new() }
    }

    /// Evaluate one submitted statement. Returns `{ output, error }`:
    /// `output` is what the CLI would have printed to stdout (`""` for an
    /// assignment), `error` is `null` unless the statement failed, in which
    /// case it holds the message the CLI would have put on stderr.
    pub fn eval(&mut self, line: &str) -> JsValue {
        let (output, error) = repl::eval_capture(line, &mut self.vm);
        let obj = Object::new();
        set(&obj, "output", &JsValue::from_str(&output));
        match error {
            Some(msg) => set(&obj, "error", &JsValue::from_str(&msg)),
            None => set(&obj, "error", &JsValue::NULL),
        }
        obj.into()
    }

    /// Does `src` look like an unfinished statement the editor should keep
    /// reading (unbalanced brackets, trailing comma, parse cut off at EOF)?
    /// Exactly what the terminal REPL uses to decide between `qpl) ` and
    /// `  ...  `.
    #[wasm_bindgen(js_name = wantsMore)]
    pub fn wants_more(&self, src: &str) -> bool {
        repl::wants_more(src)
    }

    /// Bind the demo `trades` / `quotes` tables — the `--load-demo` flag.
    #[wasm_bindgen(js_name = loadDemo)]
    pub fn load_demo(&mut self) {
        repl::load_demo_tables(&mut self.vm);
    }

    /// The interpreter version, for a banner.
    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

impl Default for Repl {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything an online editor needs to give qpl the same treatment the VS
/// Code extension does. Returns:
///
/// ```js
/// {
///   id: "qpl", extensions: [".qpl"], aliases: [...],
///   configuration: { ...brackets/comments/autoClosingPairs, wordPattern: RegExp, ... },
///   monarch: { ...IMonarchLanguage, tokenizer: {...} },
///   completions: [ { label, kind, detail, insertText?, insertTextRules? } ],
///   vocabulary: { statementKeywords: [...], aggregates: [...], ... }
/// }
/// ```
///
/// Wiring it up is three calls — `monaco.languages.register({ id, extensions,
/// aliases })`, `setLanguageConfiguration(id, configuration)`,
/// `setMonarchTokensProvider(id, monarch)` — plus a completion provider that
/// maps each item's `kind` through `monaco.languages.CompletionItemKind` and
/// its `insertTextRules` through `monaco.languages.CompletionItemInsertTextRule`
/// (both are returned as names, not the numbers, which change between Monaco
/// versions). `configuration`'s regex-valued fields are already real `RegExp`
/// objects, so it can be handed over as-is.
#[wasm_bindgen(js_name = qplLangConfig)]
pub fn qpl_lang_config() -> JsValue {
    let vocab: Json = serde_json::from_str(VOCABULARY).expect("vocabulary.json is valid JSON");
    let config: Json = serde_json::from_str(LANG_CONFIG).expect("language-configuration.json is valid JSON");
    let snippets: Json = serde_json::from_str(SNIPPETS).expect("snippets/qpl.json is valid JSON");

    let value = json!({
        "id": "qpl",
        "extensions": [".qpl"],
        "aliases": ["qpl", "QPL"],
        "configuration": config,
        "monarch": monarch(&vocab),
        "completions": completions(&vocab, &snippets),
        "vocabulary": vocab,
    });

    let js = js_sys::JSON::parse(&value.to_string()).expect("serde_json output is valid JSON");
    regexify_configuration(&js);
    js
}

/// The Monarch language definition, mirroring `syntaxes/qpl.tmLanguage.json`
/// rule for rule. The `@`-prefixed names in the `cases` block resolve against
/// the sibling keyword arrays on this same object, which is why they're
/// emitted here rather than left in `vocabulary`.
fn monarch(vocab: &Json) -> Json {
    let cast_types = list(vocab, "castTypes").join("|");
    json!({
        "defaultToken": "",
        "ignoreCase": false,
        "statementKeywords": vocab["statementKeywords"],
        "builtinKeywords": vocab["builtinKeywords"],
        "joinOperators": vocab["joinOperators"],
        "wordOperators": vocab["wordOperators"],
        "aggregates": vocab["aggregates"],
        "brackets": [
            { "open": "{", "close": "}", "token": "delimiter.curly" },
            { "open": "[", "close": "]", "token": "delimiter.square" },
            { "open": "(", "close": ")", "token": "delimiter.parenthesis" },
        ],
        "tokenizer": {
            "root": [
                // `/` always starts a comment — qpl's division operator is `%`.
                ["/.*$", "comment"],
                // `\d` / `\1` / `\l` / `\i` / `\port` / `log`, statement-leading only
                ["^\\s*(?:\\\\(?:[d1li]|port)|log)\\b", "keyword"],
                // `.qpl.*` builtins, then any other dotted (user namespace) name
                ["\\.qpl\\.[A-Za-z_]\\w*(?:\\.[A-Za-z_]\\w*)*", "keyword"],
                ["\\.[A-Za-z_]\\w*(?:\\.[A-Za-z_]\\w*)+", "identifier"],
                ["(?:`[A-Za-z0-9_./-]*)+", "type"],
                ["\"", { "token": "string.quote", "bracket": "@open", "next": "@string" }],
                // a cast type only counts as one directly before `$`
                [format!("\\b(?:{cast_types})(?=\\$)"), "type"],
                ["\\d+\\.\\d*(?:[eE][+-]?\\d+)?", "number.float"],
                ["[01]+b\\b", "number"],
                ["\\d+(?:[eE][+-]?\\d+)?", "number"],
                ["[{}()\\[\\]]", "@brackets"],
                ["[A-Za-z_]\\w*", { "cases": {
                    "@statementKeywords": "keyword",
                    "@builtinKeywords": "keyword",
                    "@joinOperators": "operator",
                    "@wordOperators": "operator",
                    "@aggregates": "predefined",
                    "@default": "identifier",
                } }],
                ["<>|!=|<=|>=|::|[?$!#]|[-+*%=<>&|:;,]", "operator"],
                ["[ \\t\\r\\n]+", "white"],
            ],
            "string": [
                ["\\\\[ntr\"\\\\]", "string.escape"],
                ["[^\\\\\"]+", "string"],
                ["\"", { "token": "string.quote", "bracket": "@close", "next": "@pop" }],
            ],
        },
    })
}

/// Completion items, in the same categories the VS Code provider offers:
/// statement/builtin keywords, join and word operators, aggregates, cast
/// types, then the snippets.
fn completions(vocab: &Json, snippets: &Json) -> Json {
    let mut items: Vec<Json> = Vec::new();
    let keyword_detail = &vocab["keywordDetail"];
    let aggregate_detail = &vocab["aggregateDetail"];

    let mut push = |label: &str, kind: &str, detail: Option<&str>| {
        items.push(json!({ "label": label, "kind": kind, "detail": detail }));
    };

    for key in ["statementKeywords", "builtinKeywords", "replCommands"] {
        for word in list(vocab, key) {
            push(word, "Keyword", keyword_detail[word].as_str());
        }
    }
    for key in ["joinOperators", "wordOperators"] {
        for word in list(vocab, key) {
            push(word, "Operator", keyword_detail[word].as_str());
        }
    }
    for word in list(vocab, "aggregates") {
        push(word, "Function", aggregate_detail[word].as_str());
    }
    for word in list(vocab, "castTypes") {
        push(word, "TypeParameter", Some("cast target type"));
    }

    if let Some(map) = snippets.as_object() {
        for snippet in map.values() {
            let Some(prefix) = snippet["prefix"].as_str() else { continue };
            items.push(json!({
                "label": prefix,
                "kind": "Snippet",
                "detail": snippet["description"].as_str(),
                "insertText": snippet_body(&snippet["body"]),
                "insertTextRules": "InsertAsSnippet",
            }));
        }
    }
    Json::Array(items)
}

/// A VS Code snippet body is either one string or an array of lines.
fn snippet_body(body: &Json) -> String {
    match body {
        Json::Array(lines) => lines
            .iter()
            .filter_map(Json::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.as_str().unwrap_or_default().to_string(),
    }
}

/// The string values under `key`, which is always an array of strings here.
fn list<'a>(vocab: &'a Json, key: &str) -> Vec<&'a str> {
    vocab[key]
        .as_array()
        .map(|a| a.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default()
}

/// Monaco wants real `RegExp` objects where `language-configuration.json`
/// stores pattern strings, and there is no way to express one in JSON — so
/// convert the four such fields in place, after the round-trip through
/// `JSON.parse`.
fn regexify_configuration(root: &JsValue) {
    let Ok(config) = Reflect::get(root, &"configuration".into()) else { return };
    to_regex(&config, "wordPattern");
    if let Ok(rules) = Reflect::get(&config, &"indentationRules".into()) {
        to_regex(&rules, "increaseIndentPattern");
        to_regex(&rules, "decreaseIndentPattern");
    }
    if let Ok(rules) = Reflect::get(&config, &"onEnterRules".into())
        && let Ok(rules) = rules.dyn_into::<js_sys::Array>()
    {
        for rule in rules.iter() {
            to_regex(&rule, "beforeText");
            to_regex(&rule, "afterText");
            to_regex(&rule, "previousLineText");
        }
    }
}

/// Replace `obj[key]`, a pattern string, with the equivalent `RegExp`.
fn to_regex(obj: &JsValue, key: &str) {
    let Ok(current) = Reflect::get(obj, &key.into()) else { return };
    let Some(pattern) = current.as_string() else { return };
    set(obj, key, &js_sys::RegExp::new(&pattern, "").into());
}

fn set(obj: &JsValue, key: &str, value: &JsValue) {
    let _ = Reflect::set(obj, &key.into(), value);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three JSON files are embedded at compile time; a typo in any of
    /// them (or a key renamed on the TypeScript side) has to fail here rather
    /// than at `qplLangConfig()` call time in a browser.
    #[test]
    fn embedded_vocabulary_has_every_list() {
        let vocab: Json = serde_json::from_str(VOCABULARY).unwrap();
        for key in [
            "statementKeywords", "builtinKeywords", "joinOperators", "wordOperators",
            "aggregates", "castTypes", "replCommands",
        ] {
            assert!(!list(&vocab, key).is_empty(), "{key} is missing or empty");
        }
        assert!(vocab["keywordDetail"]["select"].is_string());
        assert!(vocab["aggregateDetail"]["sum"].is_string());
        serde_json::from_str::<Json>(LANG_CONFIG).unwrap();
        serde_json::from_str::<Json>(SNIPPETS).unwrap();
    }

    /// Monarch resolves each `@name` in a `cases` block against a sibling
    /// attribute of the language object, so every one of them must be emitted.
    #[test]
    fn monarch_cases_resolve_to_emitted_lists() {
        let vocab: Json = serde_json::from_str(VOCABULARY).unwrap();
        let m = monarch(&vocab);
        let cases = &m["tokenizer"]["root"]
            .as_array().unwrap().iter()
            .find_map(|rule| rule.get(1).and_then(|a| a.get("cases")))
            .expect("the identifier rule has a cases block")
            .as_object().unwrap().clone();
        for name in cases.keys().filter(|k| *k != "@default") {
            let attr = name.trim_start_matches('@');
            assert!(m[attr].is_array(), "monarch is missing the `{attr}` list");
        }
    }

    #[test]
    fn completions_cover_keywords_aggregates_and_snippets() {
        let vocab: Json = serde_json::from_str(VOCABULARY).unwrap();
        let snippets: Json = serde_json::from_str(SNIPPETS).unwrap();
        let items = completions(&vocab, &snippets);
        let labelled = |label: &str| -> Json {
            items.as_array().unwrap().iter()
                .find(|i| i["label"] == label).cloned().unwrap_or(Json::Null)
        };
        assert_eq!(labelled("select")["kind"], "Keyword");
        assert!(labelled("select")["detail"].as_str().unwrap().contains("from"));
        assert_eq!(labelled("lj")["kind"], "Operator");
        assert_eq!(labelled("sum")["kind"], "Function");
        assert_eq!(labelled("f64")["kind"], "TypeParameter");
        // `sel` is the `select` snippet's prefix in snippets/qpl.json
        assert_eq!(labelled("sel")["kind"], "Snippet");
        assert_eq!(labelled("sel")["insertTextRules"], "InsertAsSnippet");
    }

    /// A multi-line snippet body (a JSON array) is joined, not dropped.
    #[test]
    fn multiline_snippet_bodies_are_joined() {
        assert_eq!(snippet_body(&json!(["a", "b"])), "a\nb");
        assert_eq!(snippet_body(&json!("a")), "a");
    }
}

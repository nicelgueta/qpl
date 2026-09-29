//! Browser bindings (`wasm` feature).
//!
//! * [`Repl`]: a long-lived [`Vm`] behind `eval(line)`, running the same
//!   pipeline as the terminal REPL but returning output instead of printing
//!   it. Filesystem and socket features fail as ordinary runtime errors.
//! * [`qpl_lang_config`]: Monaco editor configuration built from the same
//!   `tools/vscode/src/vocabulary.json` the VS Code extension reads.

use crate::arrow_io;
use crate::repl;
use crate::vm::Vm;
use js_sys::{Object, Reflect, Uint8Array};
use serde_json::{Value as Json, json};
use wasm_bindgen::prelude::*;

/// Keyword/aggregate lists and hover text, shared with the VS Code extension.
const VOCABULARY: &str = include_str!("../tools/vscode/src/vocabulary.json");
/// Brackets, comments, auto-closing pairs, indentation rules.
const LANG_CONFIG: &str = include_str!("../tools/vscode/language-configuration.json");
/// VS Code snippets, reshaped into Monaco completion items.
const SNIPPETS: &str = include_str!("../tools/vscode/snippets/qpl.json");

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(msg: &str);
}

/// Runs on instantiation. With `panic = "abort"` a panic reaches JS as a bare
/// `RuntimeError: unreachable`, so log the real message first.
#[wasm_bindgen(start)]
fn start() {
    std::panic::set_hook(Box::new(|info| {
        console_error(&format!("qpl panic: {info}"))
    }));
}

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

    /// Evaluate one statement. Returns `{ output, error }`: what the CLI would
    /// print to stdout (`""` for an assignment), and the stderr message or `null`.
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

    /// Bind `name` to the table in `ipc`, an uncompressed Arrow IPC stream
    /// (`tableToIPC(t, 'stream')`), replacing any existing binding. Throws on a
    /// bad name or payload. This is how a host gets data in without `load`.
    #[wasm_bindgen(js_name = registerTable)]
    pub fn register_table(&mut self, name: &str, ipc: &[u8]) -> Result<(), JsError> {
        arrow_io::register_table(&mut self.vm, name, ipc).map_err(|e| JsError::new(&e.to_string()))
    }

    /// Rows in the table bound to `name`, for paging (the language's `count`
    /// counts non-nulls in the first column). Throws if `name` isn't a table.
    #[wasm_bindgen(js_name = rowCount)]
    pub fn row_count(&self, name: &str) -> Result<f64, JsError> {
        arrow_io::row_count(&self.vm, name)
            .map(|n| n as f64)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Like [`eval`](Self::eval), plus `ipc`: the whole untruncated table as an
    /// Arrow IPC stream when the statement produced one (`output` is then
    /// empty), else `null`.
    #[wasm_bindgen(js_name = evalArrow)]
    pub fn eval_arrow(&mut self, line: &str) -> JsValue {
        let r = repl::eval_capture_table(line, &mut self.vm);
        // A table that fails to serialise is reported like any other error.
        let (ipc, ser_err) = match r.table.as_ref().map(arrow_io::df_to_ipc) {
            Some(Ok(bytes)) => (Some(bytes), None),
            Some(Err(e)) => (None, Some(e.to_string())),
            None => (None, None),
        };
        let obj = Object::new();
        set(&obj, "output", &JsValue::from_str(&r.output));
        match r.error.or(ser_err) {
            Some(msg) => set(&obj, "error", &JsValue::from_str(&msg)),
            None => set(&obj, "error", &JsValue::NULL),
        }
        match ipc {
            Some(bytes) => set(&obj, "ipc", &Uint8Array::from(bytes.as_slice())),
            None => set(&obj, "ipc", &JsValue::NULL),
        }
        obj.into()
    }

    /// Whether `src` is an unfinished statement (unbalanced brackets, trailing
    /// comma, cut off mid-parse), as the terminal REPL decides it.
    #[wasm_bindgen(js_name = wantsMore)]
    pub fn wants_more(&self, src: &str) -> bool {
        repl::wants_more(src)
    }

    /// Current bindings for completion:
    /// `{ tables: [{ name, columns, rows? }], variables: [name], functions: [name] }`,
    /// sorted by name. Lazy plans are listed as tables without `columns`/`rows`
    /// (resolving a schema can be expensive).
    pub fn symbols(&self) -> JsValue {
        js_sys::JSON::parse(&symbols_json(&self.vm).to_string())
            .expect("serde_json output is valid JSON")
    }

    /// Bind the demo `trades` / `quotes` tables (like `--load-demo`).
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

/// Monaco configuration for qpl:
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
/// Register with `monaco.languages.register`, `setLanguageConfiguration` and
/// `setMonarchTokensProvider`, plus a completion provider. `kind` and
/// `insertTextRules` are names (map them through Monaco's enums, whose
/// numbers vary by version); `configuration`'s regex fields are real `RegExp`s.
#[wasm_bindgen(js_name = qplLangConfig)]
pub fn qpl_lang_config() -> JsValue {
    let vocab: Json = serde_json::from_str(VOCABULARY).expect("vocabulary.json is valid JSON");
    let config: Json =
        serde_json::from_str(LANG_CONFIG).expect("language-configuration.json is valid JSON");
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

/// The Monarch definition, mirroring `syntaxes/qpl.tmLanguage.json`. The
/// `@`-names in `cases` resolve against sibling arrays on this object.
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
                // `/` always starts a comment (division is `%`)
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

/// Completion items, in the VS Code provider's categories.
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
            let Some(prefix) = snippet["prefix"].as_str() else {
                continue;
            };
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

/// Convert `language-configuration.json`'s pattern strings to `RegExp`s,
/// which JSON can't express.
fn regexify_configuration(root: &JsValue) {
    let Ok(config) = Reflect::get(root, &"configuration".into()) else {
        return;
    };
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
    let Ok(current) = Reflect::get(obj, &key.into()) else {
        return;
    };
    let Some(pattern) = current.as_string() else {
        return;
    };
    set(obj, key, &js_sys::RegExp::new(&pattern, "").into());
}

/// The session's top-level bindings as JSON (see [`Repl::symbols`]).
fn symbols_json(vm: &Vm) -> Json {
    use crate::ast::Value;
    let mut tables: Vec<Json> = Vec::new();
    let mut variables = Vec::new();
    let mut functions = Vec::new();
    for (name, v) in &vm.globals {
        match v {
            Value::Table(df) => {
                let columns: Vec<String> = df
                    .get_column_names()
                    .iter()
                    .map(|c| c.to_string())
                    .collect();
                tables.push(json!({ "name": name, "columns": columns, "rows": df.height() }));
            }
            Value::Lazy(_) => tables.push(json!({ "name": name })),
            Value::Closure(_) => functions.push(name.clone()),
            _ => variables.push(name.clone()),
        }
    }
    tables.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    variables.sort();
    functions.sort();
    json!({ "tables": tables, "variables": variables, "functions": functions })
}

fn set(obj: &JsValue, key: &str, value: &JsValue) {
    let _ = Reflect::set(obj, &key.into(), value);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded JSON files must parse here, not fail later in a browser.
    #[test]
    fn embedded_vocabulary_has_every_list() {
        let vocab: Json = serde_json::from_str(VOCABULARY).unwrap();
        for key in [
            "statementKeywords",
            "builtinKeywords",
            "joinOperators",
            "wordOperators",
            "aggregates",
            "castTypes",
            "replCommands",
        ] {
            assert!(!list(&vocab, key).is_empty(), "{key} is missing or empty");
        }
        assert!(vocab["keywordDetail"]["select"].is_string());
        assert!(vocab["aggregateDetail"]["sum"].is_string());
        serde_json::from_str::<Json>(LANG_CONFIG).unwrap();
        serde_json::from_str::<Json>(SNIPPETS).unwrap();
    }

    /// Every `@name` in a `cases` block must be emitted as a sibling attribute.
    #[test]
    fn monarch_cases_resolve_to_emitted_lists() {
        let vocab: Json = serde_json::from_str(VOCABULARY).unwrap();
        let m = monarch(&vocab);
        let cases = &m["tokenizer"]["root"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|rule| rule.get(1).and_then(|a| a.get("cases")))
            .expect("the identifier rule has a cases block")
            .as_object()
            .unwrap()
            .clone();
        for name in cases.keys().filter(|k| *k != "@default") {
            let attr = name.trim_start_matches('@');
            assert!(m[attr].is_array(), "monarch is missing the `{attr}` list");
        }
    }

    /// Monarch takes the first matching `cases` entry in key order and
    /// `@default` matches everything, so it must come last. Checked on the
    /// string that crosses to JS.
    #[test]
    fn default_is_the_last_case_so_keywords_are_reachable() {
        let vocab: Json = serde_json::from_str(VOCABULARY).unwrap();
        let wire = monarch(&vocab).to_string();
        let parsed: Json = serde_json::from_str(&wire).unwrap();
        let cases = parsed["tokenizer"]["root"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|rule| rule.get(1).and_then(|a| a.get("cases")))
            .unwrap()
            .as_object()
            .unwrap();
        let keys: Vec<&String> = cases.keys().collect();
        assert_eq!(
            keys.last().map(|k| k.as_str()),
            Some("@default"),
            "{keys:?}"
        );
    }

    #[test]
    fn symbols_lists_tables_variables_and_functions() {
        let mut vm = Vm::new();
        repl::load_demo_tables(&mut vm);
        repl::eval_capture("n: 3", &mut vm);
        repl::eval_capture("f: {[x] x + 1}", &mut vm);
        let s = symbols_json(&vm);
        let trades = s["tables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "trades")
            .unwrap();
        assert!(
            trades["columns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "price")
        );
        assert_eq!(s["variables"], json!(["n"]));
        assert_eq!(s["functions"], json!(["f"]));
    }

    #[test]
    fn completions_cover_keywords_aggregates_and_snippets() {
        let vocab: Json = serde_json::from_str(VOCABULARY).unwrap();
        let snippets: Json = serde_json::from_str(SNIPPETS).unwrap();
        let items = completions(&vocab, &snippets);
        let labelled = |label: &str| -> Json {
            items
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["label"] == label)
                .cloned()
                .unwrap_or(Json::Null)
        };
        assert_eq!(labelled("select")["kind"], "Keyword");
        assert!(
            labelled("select")["detail"]
                .as_str()
                .unwrap()
                .contains("from")
        );
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

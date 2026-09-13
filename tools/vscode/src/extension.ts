import * as vscode from 'vscode';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { spawnSync } from 'child_process';
import {
  AGGREGATES, AGGREGATE_DETAIL, BUILTIN_KEYWORDS, CAST_TYPES,
  JOIN_OPERATORS, KEYWORD_DETAIL, STATEMENT_KEYWORDS, WORD_OPERATORS,
} from './vocabulary';
import { demoTables, scanDocument } from './docScan';

const TERMINAL_NAME = 'qpl REPL';
let terminal: vscode.Terminal | undefined;
let scratchFile: string | undefined;

export function activate(context: vscode.ExtensionContext) {
  context.subscriptions.push(
    vscode.commands.registerCommand('qpl.startRepl', () => getTerminal().show()),
    vscode.commands.registerCommand('qpl.restartRepl', () => {
      terminal?.dispose();
      terminal = undefined;
      getTerminal().show();
    }),
    vscode.commands.registerCommand('qpl.runFileOrSelection', runFileOrSelection),
    vscode.window.onDidCloseTerminal((t) => {
      if (t === terminal) {
        terminal = undefined;
      }
    }),
    vscode.languages.registerCompletionItemProvider('qpl', new QplCompletionProvider(), ' ', '$', '`', ':'),
  );
}

export function deactivate() {
  if (scratchFile) {
    try {
      fs.unlinkSync(scratchFile);
    } catch {
      /* ignore */
    }
  }
}

function workspaceRoot(): string | undefined {
  return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}

/** How to invoke qpl: a command plus any leading args (e.g. `cargo run …`). */
function resolveQpl(): { cmd: string; args: string[] } {
  const configured = vscode.workspace.getConfiguration('qpl').get<string>('path')?.trim();
  if (configured) {
    return { cmd: configured, args: [] };
  }

  const probe = process.platform === 'win32' ? 'where' : 'which';
  try {
    if (spawnSync(probe, ['qpl'], { stdio: 'ignore' }).status === 0) {
      return { cmd: 'qpl', args: [] };
    }
  } catch {
    /* fall through */
  }

  const root = workspaceRoot();
  if (root) {
    const exe = process.platform === 'win32' ? '.exe' : '';
    for (const profile of ['release', 'debug']) {
      const full = path.join(root, 'target', profile, `qpl${exe}`);
      if (fs.existsSync(full)) {
        return { cmd: full, args: [] };
      }
    }
    const cargoToml = path.join(root, 'Cargo.toml');
    if (fs.existsSync(cargoToml) && /name\s*=\s*"qpl"/.test(fs.readFileSync(cargoToml, 'utf8'))) {
      return { cmd: 'cargo', args: ['run', '--quiet', '--manifest-path', cargoToml, '--'] };
    }
  }

  return { cmd: 'qpl', args: [] };
}

function shellQuote(s: string): string {
  return /[^\w@%+=:,./-]/.test(s) ? `'${s.replace(/'/g, `'\\''`)}'` : s;
}

function getTerminal(): vscode.Terminal {
  if (terminal && terminal.exitStatus === undefined) {
    return terminal;
  }
  const root = workspaceRoot();
  terminal = vscode.window.createTerminal({ name: TERMINAL_NAME, cwd: root });

  const { cmd, args } = resolveQpl();
  const loadDemo = vscode.workspace.getConfiguration('qpl').get<boolean>('loadDemo', false);
  const parts = [shellQuote(cmd), ...args.map(shellQuote)];
  if (loadDemo) {
    parts.push('--load-demo');
  }
  terminal.sendText(parts.join(' '));
  return terminal;
}

/**
 * Fold physical lines into logical statements, mirroring the interpreter's
 * script rule: a line indented by a tab or 4+ spaces continues the statement
 * above it; blank lines and full-line `/` comments are dropped.
 */
function logicalStatements(src: string): string[] {
  const out: string[] = [];
  let buf: string[] = [];
  const flush = () => {
    if (buf.length) {
      out.push(buf.join('\n'));
      buf = [];
    }
  };
  for (const raw of src.split(/\r?\n/)) {
    if (buf.length && (raw.startsWith('\t') || raw.startsWith('    '))) {
      buf.push(raw);
      continue;
    }
    flush();
    const trimmed = raw.trim();
    if (trimmed === '' || trimmed.startsWith('/')) {
      continue;
    }
    buf.push(raw);
  }
  flush();
  return out;
}

async function runFileOrSelection() {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== 'qpl') {
    vscode.window.showInformationMessage('qpl: open a .qpl file to run it.');
    return;
  }

  const sel = editor.selection;
  const code = sel.isEmpty ? editor.document.getText() : editor.document.getText(sel);

  const statements = logicalStatements(code);
  if (statements.length === 0) {
    return;
  }

  const term = getTerminal();
  term.show(true);

  if (statements.length === 1 && !statements[0].includes('\n')) {
    term.sendText(statements[0], true);
    return;
  }

  // multi-line / multi-statement: hand it to the interpreter as a script so its
  // own continuation-folding and comment handling apply.
  scratchFile ??= path.join(os.tmpdir(), `qpl-vscode-${process.pid}.qpl`);
  fs.writeFileSync(scratchFile, code.endsWith('\n') ? code : code + '\n');
  term.sendText(`\\l ${scratchFile}`, true);
}

const TABLE_CONTEXT_RE = /\b(from|by|drop|collect|sink|load)\s+[A-Za-z_`.\w/-]*$/;

/** Static + document-derived completion for qpl: keywords, aggregates, cast
 * types, join operators, snippets-adjacent identifiers, and names seen in
 * the open document (assignment targets and table references). */
class QplCompletionProvider implements vscode.CompletionItemProvider {
  provideCompletionItems(
    document: vscode.TextDocument,
    position: vscode.Position,
  ): vscode.CompletionItem[] {
    const linePrefix = document.lineAt(position).text.slice(0, position.character);
    const trimmedPrefix = linePrefix.trimStart();

    // `type$expr` cast — only cast type names right after a bare `$` (or a
    // partial type name being typed before it).
    if (/\$[A-Za-z0-9_]*$/.test(linePrefix) && !/\$\s/.test(linePrefix)) {
      return CAST_TYPES.map((t) => this.item(t, vscode.CompletionItemKind.TypeParameter, 'cast target type'));
    }

    // after from / by / drop / collect / sink / load -> table names
    if (TABLE_CONTEXT_RE.test(linePrefix)) {
      const { assigned, tableRefs } = scanDocument(document);
      const names = new Set([...demoTables(), ...tableRefs, ...assigned]);
      return [...names].map((n) => this.item(n, vscode.CompletionItemKind.Struct, 'table'));
    }

    const items: vscode.CompletionItem[] = [];

    // start of line (or start of statement) -> statement/builtin keywords + REPL commands
    if (trimmedPrefix === '') {
      for (const kw of [...STATEMENT_KEYWORDS, ...BUILTIN_KEYWORDS]) {
        items.push(this.item(kw, vscode.CompletionItemKind.Keyword, KEYWORD_DETAIL[kw]));
      }
      items.push(
        this.item('\\d', vscode.CompletionItemKind.Keyword, 'disassemble a statement'),
        this.item('\\1', vscode.CompletionItemKind.Keyword, 'toggle stdout logging to a file'),
        this.item('\\l', vscode.CompletionItemKind.Keyword, 'run a script file'),
        this.item('\\port', vscode.CompletionItemKind.Keyword, KEYWORD_DETAIL['\\port']),
        this.item('log', vscode.CompletionItemKind.Keyword, 'print an expression to stdout'),
      );
    } else {
      for (const kw of STATEMENT_KEYWORDS) {
        items.push(this.item(kw, vscode.CompletionItemKind.Keyword, KEYWORD_DETAIL[kw]));
      }
      for (const kw of BUILTIN_KEYWORDS) {
        items.push(this.item(kw, vscode.CompletionItemKind.Keyword, KEYWORD_DETAIL[kw]));
      }
      for (const op of JOIN_OPERATORS) {
        items.push(this.item(op, vscode.CompletionItemKind.Operator, KEYWORD_DETAIL[op]));
      }
      for (const op of WORD_OPERATORS) {
        items.push(this.item(op, vscode.CompletionItemKind.Operator, KEYWORD_DETAIL[op]));
      }
    }

    for (const agg of AGGREGATES) {
      items.push(this.item(agg, vscode.CompletionItemKind.Function, AGGREGATE_DETAIL[agg] ?? `${agg}(expr) — aggregate`));
    }

    const { assigned } = scanDocument(document);
    for (const name of assigned) {
      items.push(this.item(name, vscode.CompletionItemKind.Variable, 'defined in this document'));
    }
    for (const t of demoTables()) {
      items.push(this.item(t, vscode.CompletionItemKind.Struct, 'demo table'));
    }

    return items;
  }

  private item(label: string, kind: vscode.CompletionItemKind, detail?: string): vscode.CompletionItem {
    const it = new vscode.CompletionItem(label, kind);
    if (detail) {
      it.detail = detail;
    }
    return it;
  }
}

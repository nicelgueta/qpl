import * as vscode from 'vscode';

/** Names bound by `name: ...` assignments anywhere in the document. */
const ASSIGNMENT_RE = /^\s*([A-Za-z_]\w*)\s*:(?!=)/gm;

/**
 * Names that look like table references: the operand of `from`, `load`,
 * `<<`, or `sink`/`>>`. Best-effort regex scan, not a real parse — good
 * enough to seed completion after `from`/`by`/`drop`.
 */
const TABLE_REF_RE = /\b(?:from|load|sink)\s+([A-Za-z_]\w*)\b|<<\s*([A-Za-z_]\w*)\b/g;

export interface DocSymbols {
  assigned: string[];
  tableRefs: string[];
}

export function scanDocument(doc: vscode.TextDocument): DocSymbols {
  const text = doc.getText();
  const assigned = new Set<string>();
  const tableRefs = new Set<string>();

  for (const m of text.matchAll(ASSIGNMENT_RE)) {
    assigned.add(m[1]);
  }
  for (const m of text.matchAll(TABLE_REF_RE)) {
    const name = m[1] ?? m[2];
    if (name) {
      tableRefs.add(name);
    }
  }

  return { assigned: [...assigned], tableRefs: [...tableRefs] };
}

export function demoTables(): string[] {
  return vscode.workspace.getConfiguration('qpl').get<string[]>('demoTables', ['trades', 'quotes']);
}

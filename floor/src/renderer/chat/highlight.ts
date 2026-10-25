// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A small owned tokenizer for fenced code. One forward pass per line with a single mode value
// carried between lines: no backtracking, no regular expression, and the output is only text and
// one of nine fixed class names. The language comes from a fixed table keyed by the first word of
// the fence's info string; that word is never copied anywhere else. A line longer than
// LINE_LIMIT is drawn plain and the mode resets after it.

export type Language = 'script' | 'json' | 'rust' | 'python' | 'shell' | 'diff';
export type TokenClass = 'tok-keyword' | 'tok-string' | 'tok-number' | 'tok-comment' | 'tok-literal'
  | 'tok-key' | 'tok-add' | 'tok-remove' | 'tok-meta';
export type Token = { readonly text: string; readonly cls: TokenClass | null };

export const LINE_LIMIT = 500;
const INFO_LIMIT = 64;

const LANGUAGES: ReadonlyMap<string, Language> = new Map<string, Language>([
  ['ts', 'script'], ['tsx', 'script'], ['typescript', 'script'], ['js', 'script'], ['jsx', 'script'],
  ['mjs', 'script'], ['cjs', 'script'], ['javascript', 'script'],
  ['json', 'json'], ['jsonc', 'json'],
  ['rust', 'rust'], ['rs', 'rust'],
  ['python', 'python'], ['py', 'python'],
  ['sh', 'shell'], ['bash', 'shell'], ['zsh', 'shell'], ['shell', 'shell'],
  ['diff', 'diff'], ['patch', 'diff'],
]);

/** The language for a fence info string, or null. Only the first word is read, and only as a lookup key. */
export function languageOf(info: string): Language | null {
  if (info.length > INFO_LIMIT) return null;
  let end = 0;
  const trimmed = info.trim();
  while (end < trimmed.length && trimmed[end] !== ' ' && trimmed[end] !== '\t') end += 1;
  const word = trimmed.slice(0, end);
  for (let i = 0; i < word.length; i += 1) if (word.charCodeAt(i) > 127) return null;
  return LANGUAGES.get(word.toLowerCase()) ?? null;
}

type Mode = 'code' | 'block' | 'template' | 'triple-single' | 'triple-double';

type Rules = {
  readonly keywords: ReadonlySet<string>;
  readonly literals: ReadonlySet<string>;
  readonly lineComment: string | null;
  readonly commentAtWord: boolean;
  readonly slashComments: boolean;
  readonly template: boolean;
  readonly triple: boolean;
  readonly quotes: string;
  readonly lifetimes: boolean;
  readonly keys: boolean;
  readonly signedNumbers: boolean;
};

const words = (list: string) => new Set(list.split(' '));
const NONE: ReadonlySet<string> = new Set();

const SCRIPT_KEYWORDS = words('abstract as async await break case catch class const continue debugger declare default delete do else '
  + 'enum export extends finally for from function get if implements import in instanceof interface keyof let namespace new of '
  + 'private protected public readonly return satisfies set static super switch this throw try type typeof var void while with yield');
const RUST_KEYWORDS = words('as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move '
  + 'mut pub ref return self Self static struct super trait type unsafe use where while');
const PYTHON_KEYWORDS = words('and as assert async await break class continue def del elif else except finally for from global if '
  + 'import in is lambda nonlocal not or pass raise return try while with yield');
const SHELL_KEYWORDS = words('case do done elif else esac exit export fi for function if in local return select then until while');

const RULES: Readonly<Record<Exclude<Language, 'diff'>, Rules>> = {
  script: { keywords: SCRIPT_KEYWORDS, literals: words('true false null undefined NaN Infinity'), lineComment: null, commentAtWord: false, slashComments: true,
    template: true, triple: false, quotes: '\'"', lifetimes: false, keys: false, signedNumbers: false },
  json: { keywords: NONE, literals: words('true false null'), lineComment: null, commentAtWord: false, slashComments: false, template: false, triple: false,
    quotes: '"', lifetimes: false, keys: true, signedNumbers: true },
  rust: { keywords: RUST_KEYWORDS, literals: words('true false'), lineComment: null, commentAtWord: false, slashComments: true, template: false, triple: false,
    quotes: '\'"', lifetimes: true, keys: false, signedNumbers: false },
  python: { keywords: PYTHON_KEYWORDS, literals: words('True False None'), lineComment: '#', commentAtWord: false, slashComments: false, template: false,
    triple: true, quotes: '\'"', lifetimes: false, keys: false, signedNumbers: false },
  shell: { keywords: SHELL_KEYWORDS, literals: NONE, lineComment: '#', commentAtWord: true, slashComments: false, template: false, triple: false,
    quotes: '\'"', lifetimes: false, keys: false, signedNumbers: false },
};

const MODE_CLASS: Readonly<Record<Mode, TokenClass | null>> = {
  code: null, block: 'tok-comment', template: 'tok-string', 'triple-single': 'tok-string', 'triple-double': 'tok-string',
};

const isDigit = (ch: string | undefined) => ch !== undefined && ch >= '0' && ch <= '9';
const isIdentStart = (ch: string | undefined) => ch !== undefined && ((ch >= 'a' && ch <= 'z') || (ch >= 'A' && ch <= 'Z') || ch === '_' || ch === '$');
const isIdent = (ch: string | undefined) => isIdentStart(ch) || isDigit(ch);
const isNumberPart = (ch: string | undefined) => isIdent(ch) || ch === '.';

class Output {
  steps = 0;
  readonly tokens: Token[] = [];

  push(text: string, cls: TokenClass | null): void {
    if (text === '') return;
    const last = this.tokens[this.tokens.length - 1];
    if (last !== undefined && last.cls === cls) this.tokens[this.tokens.length - 1] = { text: last.text + text, cls };
    else this.tokens.push({ text, cls });
  }
}

/** The index just past the end of an open construct on this line, and whether it closed. */
function closeOf(line: string, from: number, mode: Mode): { end: number; closed: boolean } {
  const n = line.length;
  const quote = mode === 'triple-single' ? '\'' : '"';
  for (let i = from; i < n; i += 1) {
    const ch = line[i];
    if (mode === 'block') {
      if (ch === '*' && line[i + 1] === '/') return { end: i + 2, closed: true };
    } else if (ch === '\\') i += 1;
    else if (mode === 'template') {
      if (ch === '`') return { end: i + 1, closed: true };
    } else if (ch === quote && line[i + 1] === quote && line[i + 2] === quote) return { end: i + 3, closed: true };
  }
  return { end: n, closed: false };
}

/** The end of a single-line string that starts at `from` with `quote`, running to the line's end if unterminated. */
function stringEnd(line: string, from: number, quote: string): number {
  const n = line.length;
  for (let i = from + 1; i < n; i += 1) {
    const ch = line[i];
    if (ch === '\\') i += 1;
    else if (ch === quote) return i + 1;
  }
  return n;
}

function scanLine(line: string, rules: Rules, start: Mode, out: Output): Mode {
  const n = line.length;
  let mode = start;
  let i = 0;
  const take = (to: number, cls: TokenClass | null) => {
    const end = Math.min(to, n);
    out.push(line.slice(i, end), cls);
    out.steps += end - i;
    i = end;
  };
  while (i < n) {
    if (mode !== 'code') {
      const { end, closed } = closeOf(line, i, mode);
      take(end, MODE_CLASS[mode]);
      if (closed) mode = 'code';
      continue;
    }
    const ch = line[i];
    if (ch === undefined) break;
    if (ch === '/' && rules.slashComments) {
      out.steps += 1;
      if (line[i + 1] === '/') { take(n, 'tok-comment'); continue; }
      if (line[i + 1] === '*') { mode = 'block'; take(i + 2, 'tok-comment'); continue; }
    }
    if (ch === rules.lineComment && (ch !== '#' || !rules.commentAtWord || i === 0 || ' \t;'.includes(line[i - 1] ?? 'x'))) {
      take(n, 'tok-comment');
      continue;
    }
    if (rules.template && ch === '`') { mode = 'template'; take(i + 1, 'tok-string'); continue; }
    if (rules.quotes.includes(ch)) {
      out.steps += 1;
      if (rules.triple && line[i + 1] === ch && line[i + 2] === ch) {
        mode = ch === '\'' ? 'triple-single' : 'triple-double';
        take(i + 3, 'tok-string');
        continue;
      }
      if (rules.lifetimes && ch === '\'') {
        // A character literal is `'x'` or `'\...'`; anything else after a quote is a lifetime and stays plain.
        if (line[i + 1] === '\\') take(stringEnd(line, i, ch), 'tok-string');
        else if (line[i + 2] === '\'') take(i + 3, 'tok-string');
        else take(i + 1, null);
        continue;
      }
      const end = stringEnd(line, i, ch);
      let key = false;
      if (rules.keys) {
        let k = end;
        while (k < n && k < end + 3 && (line[k] === ' ' || line[k] === '\t')) k += 1;
        key = line[k] === ':';
      }
      take(end, key ? 'tok-key' : 'tok-string');
      continue;
    }
    if (isDigit(ch) || (rules.signedNumbers && ch === '-' && isDigit(line[i + 1]))) {
      let end = i + 1;
      while (end < n && isNumberPart(line[end])) end += 1;
      take(end, 'tok-number');
      continue;
    }
    if (isIdentStart(ch)) {
      let end = i + 1;
      while (end < n && isIdent(line[end])) end += 1;
      const word = line.slice(i, end);
      take(end, rules.keywords.has(word) ? 'tok-keyword' : rules.literals.has(word) ? 'tok-literal' : null);
      continue;
    }
    take(i + 1, null);
  }
  return mode;
}

function diffClass(line: string): TokenClass | null {
  const ch = line[0];
  if (ch === '@' && line[1] === '@') return 'tok-meta';
  if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff ') || line.startsWith('index ')) return 'tok-meta';
  if (ch === '+') return 'tok-add';
  if (ch === '-') return 'tok-remove';
  return null;
}

/** Tokens for `code` (adjacent pieces of one class merged) and the steps the pass took. */
export function tokenize(code: string, language: Language): { tokens: Token[]; steps: number } {
  const out = new Output();
  const rules = language === 'diff' ? null : RULES[language];
  let mode: Mode = 'code';
  let from = 0;
  while (from <= code.length) {
    const newline = code.indexOf('\n', from);
    const to = newline < 0 ? code.length : newline;
    const line = code.slice(from, to);
    if (line.length > LINE_LIMIT) {
      out.push(line, null);
      out.steps += line.length;
      mode = 'code';
    } else if (rules === null) {
      out.push(line, diffClass(line));
      out.steps += line.length;
    } else {
      mode = scanLine(line, rules, mode, out);
    }
    if (newline < 0) break;
    out.push('\n', MODE_CLASS[mode]);
    from = newline + 1;
  }
  return { tokens: out.tokens, steps: out.steps };
}

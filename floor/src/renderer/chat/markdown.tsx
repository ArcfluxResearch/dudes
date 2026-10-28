// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Agent Markdown drawn as a closed set of React elements. Each level is one linear pass: no
// regular expression here can backtrack, link addresses are matched in one pass up front, a
// failed search for a closing delimiter is remembered so it never runs again, and nesting, table
// size and element count are capped. Nothing sets innerHTML, and no attribute is taken from the
// text: a link is its words followed by the address as text, never an anchor, and an image or raw
// HTML stays literal. One surface, a ticket's note, may turn links on: then an `http` or `https` address becomes an
// anchor that opens in a new tab, and every other scheme stays text.

import { Fragment, useEffect, useMemo, useState } from 'react';
import type { ReactNode } from 'react';
import { splitHidden } from '../memory/MemoryText.tsx';
import { cn } from '../../ui/index.ts';
import { languageOf, tokenize } from './highlight.ts';
import { MARKER } from './look.ts';
import type { Language } from './highlight.ts';

export const PARSE_LIMIT = 16_384;
export const MAX_DEPTH = 6;
export const MAX_ROWS = 20;
export const MAX_COLUMNS = 8;
export const MAX_ELEMENTS = 2_000;
const SOFT_ELEMENTS = MAX_ELEMENTS - 200;

/**
 * How the Markdown's own elements look, set from the container because the elements carry only the closed set of
 * class names the renderer writes: spacing between blocks, lists, quotes, tables, code and its token colours.
 */
export const MARKDOWN = cn(
  'min-w-0 [overflow-wrap:anywhere]',
  '[&_:where(p,ul,ol,blockquote,.md-heading,.md-table)]:my-2 [&_li>:where(p,ul,ol,blockquote,.md-heading,.md-table)]:my-0',
  '[&>:first-child]:mt-0 [&>:last-child]:mb-0 [&_li]:my-0.5 [&_:where(ul,ol)]:ps-5',
  '[&_blockquote]:ms-0 [&_blockquote]:border-s-[3px] [&_blockquote]:border-line-strong [&_blockquote]:ps-3 [&_blockquote]:text-muted',
  '[&_.md-heading]:font-semibold [&_.md-plain]:whitespace-pre-wrap [&_.md-table]:overflow-x-auto',
  '[&_table]:border-collapse [&_table]:text-small [&_:where(th,td)]:border [&_:where(th,td)]:border-line [&_:where(th,td)]:px-2',
  '[&_:where(th,td)]:py-1 [&_:where(th,td)]:text-start [&_:where(th,td)]:align-top [&_th]:bg-surface-2 [&_th]:font-semibold',
  '[&_.chat-inline-code]:rounded-sm [&_.chat-inline-code]:bg-surface-2 [&_.chat-inline-code]:px-1 [&_.chat-inline-code]:py-0.5',
  '[&_.chat-inline-code]:font-mono [&_.chat-inline-code]:text-small',
  '[&_.chat-code]:my-2 [&_.chat-code]:overflow-x-auto [&_.chat-code]:whitespace-pre [&_.chat-code]:rounded-md',
  '[&_.chat-code]:border [&_.chat-code]:border-line [&_.chat-code]:bg-surface-2 [&_.chat-code]:p-3',
  '[&_.chat-code:focus-visible]:outline-2 [&_.chat-code:focus-visible]:outline-solid [&_.chat-code:focus-visible]:outline-focus',
  '[&_.chat-code:focus-visible]:outline-offset-2',
  '[&_.chat-code]:font-mono [&_.chat-code]:text-small [&_.chat-code]:leading-normal [&_.chat-code_code]:font-[inherit]',
  '[&_.tok-keyword]:text-tok-keyword [&_.tok-string]:text-tok-string [&_.tok-number]:text-tok-number',
  '[&_.tok-comment]:text-tok-comment [&_.tok-literal]:text-tok-literal [&_.tok-key]:text-tok-key',
  '[&_.tok-add]:text-tok-add [&_.tok-remove]:text-tok-remove [&_.tok-meta]:text-tok-meta',
  MARKER,
);

/** Every element the renderer can produce, so no tag name ever comes from the text. */
type Tag = 'a' | 'p' | 'br' | 'strong' | 'em' | 'code' | 'pre' | 'div' | 'span' | 'ul' | 'ol' | 'li' | 'blockquote'
  | 'table' | 'thead' | 'tbody' | 'tr' | 'th' | 'td';
type Props = { readonly className?: string; readonly start?: number; readonly href?: string };

function tag(type: Tag, key: number, { className, start, href }: Props, c: ReactNode): ReactNode {
  switch (type) {
    case 'a': return <a key={key} href={href} target="_blank" rel="noopener noreferrer" className={className}>{c}</a>;
    case 'p': return <p key={key} className={className}>{c}</p>;
    case 'br': return <br key={key} className={className} />;
    case 'strong': return <strong key={key} className={className}>{c}</strong>;
    case 'em': return <em key={key} className={className}>{c}</em>;
    case 'code': return <code key={key} className={className}>{c}</code>;
    case 'pre': return <pre key={key} className={className}>{c}</pre>;
    case 'div': return <div key={key} className={className}>{c}</div>;
    case 'span': return <span key={key} className={className}>{c}</span>;
    case 'ul': return <ul key={key} className={className}>{c}</ul>;
    case 'ol': return <ol key={key} start={start} className={className}>{c}</ol>;
    case 'li': return <li key={key} className={className}>{c}</li>;
    case 'blockquote': return <blockquote key={key} className={className}>{c}</blockquote>;
    case 'table': return <table key={key} className={className}>{c}</table>;
    case 'thead': return <thead key={key} className={className}>{c}</thead>;
    case 'tbody': return <tbody key={key} className={className}>{c}</tbody>;
    case 'tr': return <tr key={key} className={className}>{c}</tr>;
    case 'th': return <th key={key} className={className}>{c}</th>;
    case 'td': return <td key={key} className={className}>{c}</td>;
    default: return type satisfies never;
  }
}

class Context {
  count = 0;
  private key = 0;

  constructor(readonly links: boolean) {}

  el(type: Tag, props: Props, children: ReactNode): ReactNode {
    this.key += 1;
    if (this.count >= MAX_ELEMENTS) return <Fragment key={this.key}>{children}</Fragment>;
    this.count += 1;
    return tag(type, this.key, props, children);
  }

  get low(): boolean {
    return this.count >= SOFT_ELEMENTS;
  }

  text(text: string): ReactNode[] {
    return splitHidden(text, true).map((piece) => typeof piece === 'string' ? piece
      : this.low ? piece.marker : this.el('span', { className: 'memory-marker' }, piece.marker));
  }

  /** Code as class-named spans over text nodes; once the element budget runs low the rest is plain text. */
  code(text: string, language: Language): ReactNode[] {
    const { tokens } = tokenize(text, language);
    const out: ReactNode[] = [];
    for (let k = 0; k < tokens.length; k += 1) {
      if (this.low) {
        out.push(...this.text(tokens.slice(k).map((token) => token.text).join('')));
        break;
      }
      const token = tokens[k];
      if (token === undefined) break;
      if (token.cls === null) out.push(...this.text(token.text));
      else out.push(this.el('span', { className: token.cls }, this.text(token.text)));
    }
    return out;
  }

  plain(text: string): ReactNode {
    return this.el('div', { className: 'md-plain' }, this.text(text));
  }
}

const isSpace = (ch: string | undefined) => ch === ' ' || ch === '\t' || ch === '\n';
const isWord = (ch: string | undefined) => ch !== undefined && /^[\p{L}\p{N}]$/u.test(ch);
const isPunct = (ch: string | undefined) => ch !== undefined && ch.length === 1 && ch > ' ' && ch < '\x7f' && !/^[0-9A-Za-z]$/.test(ch);

function codeSpanEnd(text: string, from: number, to: number, run: number): number {
  let k = from;
  while (k < to) {
    if (text[k] !== '`') { k += 1; continue; }
    let m = k;
    while (m < to && text[m] === '`') m += 1;
    if (m - k === run) return k;
    k = m;
  }
  return -1;
}

/** Where the last backtick run of each length starts. */
function tickRuns(text: string, from: number, to: number): Map<number, number> {
  const last = new Map<number, number>();
  for (let k = from; k < to;) {
    if (text[k] !== '`') { k += 1; continue; }
    let m = k;
    while (m < to && text[m] === '`') m += 1;
    last.set(m - k, k);
    k = m;
  }
  return last;
}

/** Each `(` mapped to the `)` that balances it with no space between: every possible link address. */
function addressEnds(text: string, from: number, to: number): Map<number, number> {
  const ends = new Map<number, number>();
  const open: number[] = [];
  for (let k = from; k < to; k += 1) {
    const ch = text[k];
    if (ch === '(') open.push(k);
    else if (ch === ')') {
      const at = open.pop();
      if (at !== undefined) ends.set(at, k);
    } else if (isSpace(ch) || ch === '\r') open.length = 0;
  }
  return ends;
}

type Bracket = { readonly close: number; readonly urlEnd: number };

function bracket(text: string, at: number, to: number, ends: ReadonlyMap<number, number>): Bracket | 'open' | null {
  let k = at + 1;
  for (; k < to; k += 1) {
    const ch = text[k];
    if (ch === '\\') k += 1;
    else if (ch === '[') return null;
    else if (ch === ']') break;
  }
  if (k >= to) return 'open';
  const urlEnd = ends.get(k + 1);
  return urlEnd === undefined ? null : { close: k, urlEnd };
}

/**
 * An address a note may link to: `http` or `https`, with no username or password, and one the browser parses as such.
 * It never throws: a browser without `URL.canParse` relies on the anchored pattern alone.
 */
export function webAddress(url: string): boolean {
  if (!/^https?:\/\/(?![^/?#]*@)/i.test(url)) return false;
  return typeof URL.canParse !== 'function' || URL.canParse(url);
}

function inline(ctx: Context, text: string, from: number, to: number, allowLink: boolean): ReactNode[] {
  const out: ReactNode[] = [];
  const failed = new Set<string>();
  let ticks: Map<number, number> | null = null;
  let ends: Map<number, number> | null = null;
  let buffer = '';
  const flush = () => {
    if (buffer !== '') out.push(...ctx.text(buffer));
    buffer = '';
  };
  let i = from;
  while (i < to) {
    if (ctx.low) {
      buffer += text.slice(i, to);
      break;
    }
    const ch = text[i];
    if (ch === '\\') {
      if (i + 1 < to && isPunct(text[i + 1])) { buffer += text[i + 1]; i += 2; } else { buffer += '\\'; i += 1; }
    } else if (ch === '\n') {
      flush();
      out.push(ctx.el('br', {}, null));
      i += 1;
    } else if (ch === '`') {
      let run = i;
      while (run < to && text[run] === '`') run += 1;
      const n = run - i;
      // After one search has failed, the runs it passed tell every later opener whether a closer exists.
      const end = ticks !== null && (ticks.get(n) ?? -1) <= i ? -1 : codeSpanEnd(text, run, to, n);
      if (end < 0) {
        ticks ??= tickRuns(text, run, to);
        buffer += text.slice(i, run);
        i = run;
      } else {
        flush();
        out.push(ctx.el('code', { className: 'chat-inline-code' }, ctx.text(text.slice(run, end).replaceAll('\n', ' '))));
        i = end + n;
      }
    } else if (ch === '*' || ch === '_') {
      let run = i;
      while (run < to && text[run] === ch) run += 1;
      const n = run - i;
      const opens = n <= 2 && run < to && !isSpace(text[run]) && !(ch === '_' && i > from && isWord(text[i - 1]))
        && !failed.has(`${ch}${n}`);
      let close = -1;
      if (opens) {
        for (let k = run; k < to;) {
          if (text[k] === '\\') { k += 2; continue; }
          if (text[k] !== ch) { k += 1; continue; }
          let m = k;
          while (m < to && text[m] === ch) m += 1;
          if (m - k === n && k > run && !isSpace(text[k - 1]) && !(ch === '_' && isWord(text[m]))) { close = k; break; }
          k = m;
        }
        if (close < 0) failed.add(`${ch}${n}`);
      }
      if (close < 0) {
        buffer += text.slice(i, run);
        i = run;
      } else {
        flush();
        out.push(ctx.el(n === 2 ? 'strong' : 'em', {}, inline(ctx, text, run, close, allowLink)));
        i = close + n;
      }
    } else if (ch === '[' || (ch === '!' && text[i + 1] === '[')) {
      const open = ch === '!' ? i + 1 : i;
      const found = allowLink && !failed.has('[') ? bracket(text, open, to, ends ??= addressEnds(text, from, to)) : null;
      if (found === 'open') failed.add('[');
      if (found !== null && found !== 'open') {
        if (ch === '!') {
          buffer += text.slice(i, found.urlEnd + 1);
        } else {
          const label = inline(ctx, text, i + 1, found.close, false);
          const url = text.slice(found.close + 2, found.urlEnd);
          flush();
          if (ctx.links && webAddress(url)) {
            out.push(ctx.el('a', { href: url }, label.length > 0 ? label : ctx.text(url)));
          } else {
            out.push(...label);
            buffer += `${label.length > 0 ? ' ' : ''}(${url})`;
            flush();
          }
        }
        i = found.urlEnd + 1;
      } else {
        buffer += text.slice(i, open + 1);
        i = open + 1;
      }
    } else {
      let j = i + 1;
      while (j < to && !'\\\n`*_[!'.includes(text[j] ?? '')) j += 1;
      buffer += text.slice(i, j);
      i = j;
    }
  }
  flush();
  return out;
}

type Marker = { readonly ordered: boolean; readonly start: number; readonly content: number; readonly rest: string };

function columns(line: string, limit = line.length): { cols: number; pos: number } {
  let cols = 0;
  let pos = 0;
  while (pos < limit) {
    const ch = line[pos];
    if (ch === ' ') cols += 1;
    else if (ch === '\t') cols += 4 - (cols % 4);
    else break;
    pos += 1;
  }
  return { cols, pos };
}

function strip(line: string, cols: number): string {
  let used = 0;
  let pos = 0;
  while (used < cols && pos < line.length) {
    const ch = line[pos];
    if (ch === ' ') used += 1;
    else if (ch === '\t') used += 4 - (used % 4);
    else break;
    pos += 1;
  }
  return line.slice(pos);
}

function blank(line: string): boolean {
  for (let i = 0; i < line.length; i += 1) if (line[i] !== ' ' && line[i] !== '\t' && line[i] !== '\r') return false;
  return true;
}

function fenceOpen(line: string): { ch: string; n: number; indent: number; info: string } | null {
  const { cols, pos } = columns(line);
  const ch = line[pos];
  if (cols > 3 || (ch !== '`' && ch !== '~')) return null;
  let end = pos;
  while (line[end] === ch) end += 1;
  if (end - pos < 3) return null;
  if (ch === '`' && line.indexOf('`', end) >= 0) return null;
  return { ch, n: end - pos, indent: cols, info: line.slice(end) };
}

function fenceClose(line: string, fence: { ch: string; n: number }): boolean {
  const { cols, pos } = columns(line);
  if (cols > 3 || line[pos] !== fence.ch) return false;
  let end = pos;
  while (line[end] === fence.ch) end += 1;
  return end - pos >= fence.n && blank(line.slice(end));
}

function heading(line: string): string | null {
  const { cols, pos } = columns(line);
  if (cols > 3 || line[pos] !== '#') return null;
  let end = pos;
  while (line[end] === '#') end += 1;
  if (end - pos > 6 || (end < line.length && line[end] !== ' ' && line[end] !== '\t')) return null;
  let rest = line.slice(end).trim();
  let cut = rest.length;
  while (cut > 0 && rest[cut - 1] === '#') cut -= 1;
  if (cut < rest.length && (cut === 0 || rest[cut - 1] === ' ' || rest[cut - 1] === '\t')) rest = rest.slice(0, cut).trimEnd();
  return rest;
}

function quote(line: string): string | null {
  const { cols, pos } = columns(line);
  if (cols > 3 || line[pos] !== '>') return null;
  return line[pos + 1] === ' ' ? line.slice(pos + 2) : line.slice(pos + 1);
}

function marker(line: string): Marker | null {
  const { cols, pos } = columns(line);
  if (cols > 3) return null;
  let width = 0;
  let ordered = false;
  let start = 1;
  const ch = line[pos];
  if (ch === '-' || ch === '*' || ch === '+') width = 1;
  else if (ch !== undefined && ch >= '0' && ch <= '9') {
    let end = pos;
    while (line[end] !== undefined && line[end]! >= '0' && line[end]! <= '9') end += 1;
    if (end - pos > 9 || (line[end] !== '.' && line[end] !== ')')) return null;
    ordered = true;
    start = Number.parseInt(line.slice(pos, end), 10);
    width = end - pos + 1;
  } else return null;
  const after = line[pos + width];
  if (after !== undefined && after !== ' ' && after !== '\t') return null;
  let spaces = 0;
  while (line[pos + width + spaces] === ' ') spaces += 1;
  if (spaces === 0 || spaces > 4 || pos + width + spaces >= line.length) spaces = 1;
  const rest = line.slice(pos + width + spaces);
  return { ordered, start, content: cols + width + spaces, rest };
}

/** At most `limit` cells; anything past them stays in the last cell, pipes and all, so no word is dropped. */
function cells(line: string, limit: number): string[] {
  let body = line.trim();
  if (body.startsWith('|')) body = body.slice(1);
  if (body.endsWith('|') && !body.endsWith('\\|')) body = body.slice(0, -1);
  const out: string[] = [];
  let current = '';
  for (let i = 0; i < body.length; i += 1) {
    const ch = body[i];
    if (ch === '\\' && body[i + 1] === '|') { current += '|'; i += 1; } else if (ch === '|' && out.length < limit - 1) { out.push(current.trim()); current = ''; } else current += ch;
  }
  out.push(current.trim());
  return out;
}

function delimiter(line: string, count: number): boolean {
  if (!line.includes('-')) return false;
  const row = cells(line, MAX_COLUMNS + 1);
  if (row.length !== count) return false;
  return row.every((cell) => {
    let from = cell.startsWith(':') ? 1 : 0;
    const to = cell.endsWith(':') && cell.length > from ? cell.length - 1 : cell.length;
    if (to <= from) return false;
    for (; from < to; from += 1) if (cell[from] !== '-') return false;
    return true;
  });
}

function tableHead(lines: readonly string[], at: number): number {
  const line = lines[at] ?? '';
  const next = lines[at + 1];
  if (next === undefined || !line.includes('|')) return 0;
  const head = cells(line, MAX_COLUMNS + 1);
  return head.length <= MAX_COLUMNS && delimiter(next, head.length) ? head.length : 0;
}

function startsBlock(lines: readonly string[], at: number, depth: number): boolean {
  const line = lines[at] ?? '';
  if (fenceOpen(line) !== null || heading(line) !== null || tableHead(lines, at) > 0) return true;
  if (depth >= MAX_DEPTH) return false;
  if (quote(line) !== null) return true;
  const item = marker(line);
  return item !== null && (!item.ordered || item.start === 1);
}

function blocks(ctx: Context, lines: readonly string[], depth: number): ReactNode[] {
  const out: ReactNode[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i] ?? '';
    if (blank(line)) { i += 1; continue; }
    if (ctx.low) { out.push(ctx.plain(lines.slice(i).join('\n'))); break; }
    const fence = fenceOpen(line);
    if (fence !== null) {
      const code: string[] = [];
      i += 1;
      while (i < lines.length && !fenceClose(lines[i] ?? '', fence)) { code.push(strip(lines[i] ?? '', fence.indent)); i += 1; }
      i += 1;
      const body = code.join('\n');
      const language = languageOf(fence.info);
      out.push(ctx.el('pre', { className: 'chat-code' }, ctx.el('code', {}, language === null ? ctx.text(body) : ctx.code(body, language))));
      continue;
    }
    const title = heading(line);
    if (title !== null) {
      out.push(ctx.el('div', { className: 'md-heading' }, ctx.el('strong', {}, inline(ctx, title, 0, title.length, true))));
      i += 1;
      continue;
    }
    if (depth < MAX_DEPTH && quote(line) !== null) {
      const inner: string[] = [];
      while (i < lines.length && quote(lines[i] ?? '') !== null) { inner.push(quote(lines[i] ?? '') ?? ''); i += 1; }
      out.push(ctx.el('blockquote', {}, blocks(ctx, inner, depth + 1)));
      continue;
    }
    const first = depth < MAX_DEPTH ? marker(line) : null;
    if (first !== null) {
      const items: ReactNode[] = [];
      let item: Marker | null = first;
      while (item !== null) {
        const content = [item.rest];
        const width = item.content;
        i += 1;
        while (i < lines.length) {
          const next = lines[i] ?? '';
          if (blank(next)) {
            let k = i;
            while (k < lines.length && blank(lines[k] ?? '')) k += 1;
            if (k >= lines.length || columns(lines[k] ?? '').cols < width) break;
            for (; i < k; i += 1) content.push('');
          } else if (columns(next).cols >= width) { content.push(strip(next, width)); i += 1; } else break;
        }
        items.push(ctx.el('li', {}, blocks(ctx, content, depth + 1)));
        let k = i;
        while (k < lines.length && blank(lines[k] ?? '')) k += 1;
        const following: Marker | null = k < lines.length ? marker(lines[k] ?? '') : null;
        item = following !== null && following.ordered === first.ordered ? following : null;
        if (item !== null) i = k;
      }
      out.push(first.ordered && first.start !== 1 ? ctx.el('ol', { start: first.start }, items)
        : ctx.el(first.ordered ? 'ol' : 'ul', {}, items));
      continue;
    }
    const width = tableHead(lines, i);
    if (width > 0) {
      let end = i + 2;
      while (end < lines.length && !blank(lines[end] ?? '') && (lines[end] ?? '').includes('|')) end += 1;
      const row = (kind: 'th' | 'td', text: string) => {
        const row = cells(text, width);
        while (row.length < width) row.push('');
        return ctx.el('tr', {}, row.map((cell) => ctx.el(kind, {}, inline(ctx, cell, 0, cell.length, true))));
      };
      const shown = Math.min(end, i + 1 + MAX_ROWS);
      const body: ReactNode[] = [];
      for (let r = i + 2; r < shown; r += 1) body.push(row('td', lines[r] ?? ''));
      out.push(ctx.el('div', { className: 'md-table' }, ctx.el('table', {}, [
        ctx.el('thead', {}, row('th', line)), ctx.el('tbody', {}, body)])));
      if (shown < end) out.push(ctx.plain(lines.slice(shown, end).join('\n')));
      i = end;
      continue;
    }
    let end = i + 1;
    while (end < lines.length && !blank(lines[end] ?? '') && !startsBlock(lines, end, depth)) end += 1;
    const text = lines.slice(i, end).map((l) => l.trim()).join('\n');
    out.push(ctx.el('p', {}, inline(ctx, text, 0, text.length, true)));
    i = end;
  }
  return out;
}

export function parseAgentMarkdown(text: string, links = false): ReactNode[] {
  const ctx = new Context(links);
  let cut = text.length;
  if (text.length > PARSE_LIMIT) {
    cut = text.lastIndexOf('\n', PARSE_LIMIT);
    if (cut <= 0) cut = PARSE_LIMIT;
    const last = text.charCodeAt(cut - 1);
    if (last >= 0xd800 && last <= 0xdbff) cut -= 1;
  }
  const lines = text.slice(0, cut).split('\n').map((line) => line.endsWith('\r') ? line.slice(0, -1) : line);
  const out = blocks(ctx, lines, 0);
  if (cut < text.length) out.push(ctx.plain(text.slice(cut).replace(/^\n/, '')));
  return out;
}

/** The latest text, taken at most once per animation frame so a streaming preview parses at frame rate. */
function useFrameText(text: string): string {
  const [shown, setShown] = useState(text);
  useEffect(() => {
    if (shown === text) return undefined;
    const frame = requestAnimationFrame(() => setShown(text));
    return () => cancelAnimationFrame(frame);
  }, [text, shown]);
  return shown;
}

export function AgentMarkdown({ text, links = false }: { readonly text: string; readonly links?: boolean }) {
  const shown = useFrameText(text);
  return <>{useMemo(() => parseAgentMarkdown(shown, links), [shown, links])}</>;
}

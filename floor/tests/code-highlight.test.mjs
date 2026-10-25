// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Fenced code in agent replies: which words colour which language, what hostile code and hostile
// info strings cannot make it do, how much work the tokenizer may spend, and that the colours read.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement, isValidElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { themeTokens } from './helpers/theme-colour.mjs';

let server, md, hl;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  md = await server.ssrLoadModule('/src/renderer/chat/markdown.tsx');
  hl = await server.ssrLoadModule('/src/renderer/chat/highlight.ts');
});
after(async () => { await server?.close(); });

const ELEMENTS = new Set(['p', 'br', 'strong', 'em', 'code', 'pre', 'div', 'span', 'ul', 'ol', 'li', 'blockquote',
  'table', 'thead', 'tbody', 'tr', 'th', 'td']);
const CLASSES = new Set(['tok-keyword', 'tok-string', 'tok-number', 'tok-comment', 'tok-literal', 'tok-key', 'tok-add',
  'tok-remove', 'tok-meta']);
const OTHER_CLASSES = new Set(['chat-inline-code', 'chat-code', 'md-heading', 'md-plain', 'md-table', 'memory-marker']);

function walk(node, seen = { elements: 0, classes: [] }) {
  if (Array.isArray(node)) { node.forEach((child) => walk(child, seen)); return seen; }
  if (!isValidElement(node)) { if (node !== null && node !== undefined) assert.equal(typeof node, 'string'); return seen; }
  const { children, ...props } = node.props;
  if (typeof node.type === 'string') {
    seen.elements += 1;
    assert.ok(ELEMENTS.has(node.type), `element ${node.type}`);
    assert.deepEqual(Object.keys(props).filter((name) => name !== 'className' && !(name === 'start' && node.type === 'ol')), []);
    if (props.className !== undefined) {
      assert.ok(CLASSES.has(props.className) || OTHER_CLASSES.has(props.className), `class ${props.className}`);
      if (CLASSES.has(props.className)) { assert.equal(node.type, 'span'); seen.classes.push(props.className); }
    }
  }
  walk(children, seen);
  return seen;
}

const fenced = (info, code) => `\`\`\`${info}\n${code}\n\`\`\``;
const render = (text) => {
  const start = performance.now();
  const tree = md.parseAgentMarkdown(text);
  const ms = performance.now() - start;
  assert.ok(ms < 2000, `parsed in ${ms.toFixed(0)} ms`);
  const seen = walk(tree);
  assert.ok(seen.elements <= md.MAX_ELEMENTS, `${seen.elements} elements`);
  const out = renderToStaticMarkup(createElement('div', null, tree));
  assert.doesNotMatch(out, /<(?:a|img|script|iframe|style)[\s>]|<[^>]*\s(?:href|src|lang|title|data-[a-z]+|on[a-z]+|style)=/i);
  return { out, seen, ms };
};
const spans = (tokens) => tokens.filter((token) => token.cls !== null).map((token) => `${token.cls.slice(4)}:${token.text}`);
const lex = (language, code) => spans(hl.tokenize(code, language).tokens);

test('each of the six languages colours its own words', () => {
  assert.deepEqual(lex('script', 'const x = \'a\\\'b\' + 12.5; // hi\n/* a\nb */ let t = `x\ny`; return true'), [
    'keyword:const', 'string:\'a\\\'b\'', 'number:12.5', 'comment:// hi', 'comment:/* a\nb */', 'keyword:let',
    'string:`x\ny`', 'keyword:return', 'literal:true']);
  assert.deepEqual(lex('json', '{"a": 1, "b" :[true,-2.5e3,null,"s"]}'), [
    'key:"a"', 'number:1', 'key:"b"', 'literal:true', 'number:-2.5e3', 'literal:null', 'string:"s"']);
  assert.deepEqual(lex('rust', 'fn main<\'a>(x: &\'a str) { let c = \'\\n\'; /* c */ }'), [
    'keyword:fn', 'keyword:let', 'string:\'\\n\'', 'comment:/* c */']);
  assert.deepEqual(lex('python', 'def f():\n  \'\'\'doc\n  more\'\'\'\n  return None # c'), [
    'keyword:def', 'string:\'\'\'doc\n  more\'\'\'', 'keyword:return', 'literal:None', 'comment:# c']);
  assert.deepEqual(lex('shell', 'if [ -f a#b ]; then echo "hi" # c\nfi'), [
    'keyword:if', 'keyword:then', 'string:"hi"', 'comment:# c', 'keyword:fi']);
  assert.deepEqual(lex('diff', 'diff --git a b\n--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n ctx'), [
    'meta:diff --git a b', 'meta:--- a', 'meta:+++ b', 'meta:@@ -1 +1 @@', 'remove:-old', 'add:+new']);
});

test('an unterminated construct ends with the block, and a quote ends with its line', () => {
  assert.deepEqual(lex('script', '/* open\nstill'), ['comment:/* open\nstill']);
  assert.deepEqual(lex('script', 'a = `open\nstill'), ['string:`open\nstill']);
  assert.deepEqual(lex('script', 'a = "open\nlet'), ['string:"open', 'keyword:let']);
});

test('the info string is a lookup key for the six languages and nothing else', () => {
  for (const [info, language] of [['ts', 'script'], ['TS', 'script'], ['tsx title="x"', 'script'], ['  rust  ', 'rust'], ['rs', 'rust'],
    ['py', 'python'], ['bash', 'shell'], ['patch', 'diff'], ['json', 'json'], ['jsx', 'script']]) {
    assert.equal(hl.languageOf(info), language, info);
  }
  for (const info of ['', 'text', 'ts" onclick="x', 'js,ts', '__proto__', 'constructor', 'toString', '\u202etxt', 'ṭs', 'ts'.padEnd(65, ' ') + 'x',
    'a'.repeat(4096), 'ts'.padStart(65, ' ')]) {
    assert.equal(hl.languageOf(info), null, JSON.stringify(info.slice(0, 20)));
  }
});

test('a reply with TypeScript, Rust, JSON, a diff and an unknown language gets only the nine classes', () => {
  const { out, seen } = render([
    fenced('typescript', 'const n: number = 1; // one'), fenced('rust', 'fn main() { let s = "x"; }'),
    fenced('json', '{"k": null}'), fenced('diff', '@@ -1 +1 @@\n-a\n+b'), fenced('klingon', 'const n = 1;')].join('\n\n'));
  assert.deepEqual([...new Set(seen.classes)].sort(), ['tok-add', 'tok-comment', 'tok-key', 'tok-keyword', 'tok-literal', 'tok-meta',
    'tok-number', 'tok-remove', 'tok-string']);
  assert.match(out, /<span class="tok-keyword">const<\/span> n: <span class="tok-keyword">number<\/span>|<span class="tok-keyword">const<\/span> n: number/);
  assert.match(out, /<pre class="chat-code"><code>const n = 1;<\/code><\/pre>/);
  assert.doesNotMatch(out, /klingon/);
});

test('hostile info strings never reach the output', () => {
  for (const info of ['ts" onclick="x', '__proto__', 'TS', 'js,ts', '\u202etxt', 'x'.repeat(4096)]) {
    const { out } = render(fenced(info, 'const a = 1;'));
    assert.doesNotMatch(out, /onclick|__proto__|x{20}|\u202e/);
    assert.match(out, /<pre class="chat-code">/);
  }
  assert.match(render(fenced('TS', 'const a = 1;')).out, /tok-keyword/);
});

test('hostile code stays inside two steps per character on every language', () => {
  const cases = [['/*', 8192], ['"', 16384], ['\\', 16384], ['`', 16384], ['\'\'\'', 5461], ['{', 16384], ['\'', 16384], ['#', 16384], ['/', 16384]];
  for (const language of ['script', 'json', 'rust', 'python', 'shell', 'diff']) {
    for (const [unit, times] of cases) {
      for (const separator of ['', '\n', ' ']) {
        const code = (unit + separator).repeat(Math.ceil(times / (unit.length + separator.length))).slice(0, 16384);
        const start = performance.now();
        const { tokens, steps } = hl.tokenize(code, language);
        assert.ok(steps <= 2 * code.length, `${language} ${JSON.stringify(unit + separator)}: ${steps} steps for ${code.length}`);
        assert.equal(tokens.map((token) => token.text).join(''), code);
        assert.ok(performance.now() - start < 1000);
      }
    }
  }
  const nested = '{'.repeat(8000) + '['.repeat(8000);
  assert.ok(hl.tokenize(nested, 'json').steps <= 2 * nested.length);
  assert.ok(render(fenced('json', nested)).seen.elements <= md.MAX_ELEMENTS);
});

test('a 1 MiB line is drawn plain with a bounded count', () => {
  const code = 'const a = "'.repeat(1024 * 1024 / 11);
  const { tokens, steps } = hl.tokenize(code, 'script');
  assert.deepEqual(tokens, [{ text: code, cls: null }]);
  assert.equal(steps, code.length);
});

test('a line is highlighted up to 500 characters and plain beyond, and the mode resets after it', () => {
  const pad = (n) => 'let ' + 'a'.repeat(n - 4);
  assert.deepEqual(hl.tokenize(pad(499), 'script').tokens[0], { text: 'let', cls: 'tok-keyword' });
  assert.deepEqual(hl.tokenize(pad(500), 'script').tokens[0], { text: 'let', cls: 'tok-keyword' });
  assert.deepEqual(hl.tokenize(pad(501), 'script').tokens, [{ text: pad(501), cls: null }]);
  assert.equal(pad(499).length, 499);
  assert.equal(pad(501).length, 501);
  const reset = `/* open\n${'x'.repeat(501)}\nlet y`;
  assert.deepEqual(spans(hl.tokenize(reset, 'script').tokens), ['comment:/* open\n', 'keyword:let']);
});

test('the element limit turns the rest of a block plain, and hidden characters inside a token are marked', () => {
  const many = Array.from({ length: 3000 }, (_, i) => `let a${i} = 1 2 3 4 5 6 7 8;`).join('\n');
  const { out, seen } = render(fenced('ts', many.slice(0, 15000)));
  assert.ok(seen.elements <= md.MAX_ELEMENTS && seen.elements >= md.MAX_ELEMENTS - 200, `${seen.elements} elements`);
  assert.match(out, /\nlet a\d+ = 1 2 3 4 5 6 7 8;\nlet a\d+ = 1 2 3 4 5 6 7 8;\n/);
  assert.match(out, /^<div><pre class="chat-code"><code><span class="tok-keyword">let<\/span> a0 = /);
  const hidden = render(fenced('ts', 'const s = "a\u202eb\u200bc"; // x\u2066y'));
  assert.equal((hidden.out.match(/memory-marker/g) ?? []).length, 3);
  assert.match(hidden.out, /<span class="tok-string">&quot;a<span class="memory-marker">\\u\{202e\}<\/span>b/);
  assert.doesNotMatch(hidden.out, /[\u202e\u200b\u2066]/);
});

test('the person\'s own messages and unknown fences stay plain', async () => {
  const { ChatText } = await server.ssrLoadModule('/src/renderer/chat/ChatText.tsx');
  const out = renderToStaticMarkup(createElement(ChatText, { text: '```ts\nconst a = 1;\n```' }));
  assert.doesNotMatch(out, /tok-/);
});

test('the nine colours read at 4.5:1 on the code background in both themes, and the page policy is unchanged', async () => {
  const code = await readFile(resolve(import.meta.dirname, '../src/renderer/chat/code.css'), 'utf8');
  const page = await readFile(resolve(import.meta.dirname, '../src/style.css'), 'utf8');
  const channel = (value) => { const c = value / 255; return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; };
  const luminance = (hex) => { const n = Number.parseInt(hex.slice(1), 16);
    return 0.2126 * channel((n >> 16) & 255) + 0.7152 * channel((n >> 8) & 255) + 0.0722 * channel(n & 255); };
  const contrast = (a, b) => { const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x); return (hi + 0.05) / (lo + 0.05); };
  const blocks = {
    light: /\n:root\s*\{([^}]+)\}/.exec(code)?.[1],
    dark: /:root:has\(\.shell\[data-theme='dark'\]\)\s*\{([^}]+)\}/.exec(code)?.[1],
    media: /prefers-color-scheme: dark\)\s*\{\s*:root:not\([^{]+\{([^}]+)\}/.exec(code)?.[1],
  };
  const parse = (block) => Object.fromEntries([...block.matchAll(/--(tok-[a-z]+):\s*(#[0-9a-f]{6})/g)].map((m) => [m[1], m[2]]));
  assert.deepEqual(parse(blocks.media), parse(blocks.dark));
  for (const theme of ['light', 'dark']) {
    const colours = parse(blocks[theme]);
    assert.deepEqual(Object.keys(colours).sort(), [...CLASSES].sort());
    const background = themeTokens(page, theme)['surface-muted'];
    for (const [name, colour] of Object.entries(colours)) {
      assert.ok(contrast(colour, background) >= 4.5, `${name} ${colour} on ${background} in ${theme}: ${contrast(colour, background).toFixed(2)}`);
      assert.match(code, new RegExp(`\\.chat-code \\.${name} \\{ color: var\\(--${name}\\); \\}`));
    }
  }
  assert.doesNotMatch(code, /style=|url\(|@import|content:/);
});

// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The agent's Markdown drawn as a closed set of elements: what it shows, what hostile text cannot
// make it do, and that the person's own messages and every other text surface stay as they were.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement, isValidElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

let server, md, ChatText, MemoryText;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  md = await server.ssrLoadModule('/src/renderer/chat/markdown.tsx');
  ({ ChatText } = await server.ssrLoadModule('/src/renderer/chat/ChatText.tsx'));
  ({ MemoryText } = await server.ssrLoadModule('/src/renderer/memory/MemoryText.tsx'));
});
after(async () => { await server?.close(); });

const ELEMENTS = new Set(['p', 'br', 'strong', 'em', 'code', 'pre', 'div', 'span', 'ul', 'ol', 'li', 'blockquote',
  'table', 'thead', 'tbody', 'tr', 'th', 'td']);
const CLASSES = new Set(['chat-inline-code', 'chat-code', 'md-heading', 'md-plain', 'md-table', 'memory-marker']);

function walk(node, seen = { elements: 0, types: new Set(), maxDepth: 0 }, depth = 0) {
  if (Array.isArray(node)) { node.forEach((child) => walk(child, seen, depth)); return seen; }
  if (!isValidElement(node)) return seen;
  const { children, ...props } = node.props;
  if (typeof node.type === 'string') {
    seen.elements += 1;
    seen.types.add(node.type);
    assert.ok(ELEMENTS.has(node.type), `element ${node.type}`);
    for (const name of Object.keys(props)) assert.ok(name === 'className' || (name === 'start' && node.type === 'ol'), `attribute ${name}`);
    if (props.className !== undefined) assert.ok(CLASSES.has(props.className), `class ${props.className}`);
  }
  seen.maxDepth = Math.max(seen.maxDepth, depth);
  walk(children, seen, depth + 1);
  return seen;
}

const html = (text) => renderToStaticMarkup(createElement('div', null, md.parseAgentMarkdown(text)));
const timed = (text) => {
  const start = performance.now();
  const tree = md.parseAgentMarkdown(text);
  const ms = performance.now() - start;
  assert.ok(ms < 1000, `parsed in ${ms.toFixed(0)} ms`);
  const seen = walk(tree);
  assert.ok(seen.elements <= md.MAX_ELEMENTS, `${seen.elements} elements`);
  const out = renderToStaticMarkup(createElement('div', null, tree));
  assert.doesNotMatch(out, /<(?:a|img|script|iframe|style)[\s>]|<[^>]*\s(?:href|src|on[a-z]+|style)=/i);
  return { out, seen, ms };
};

test('a summary with headings, lists, a quote, a table, code and a link reads as formatted text', () => {
  const out = html([
    '# Result', '', 'It **works** and `code` is *inline*.', '', '- one', '- two', '  - nested', '',
    '3. third', '4. fourth', '', '> quoted', '', '| a | b |', '| --- | :-: |', '| 1 | 2 |', '',
    '```rust', 'let x = 1;', '```', '', 'See [the docs](https://example.com/a?b=1).'].join('\n'));
  assert.match(out, /<div class="md-heading"><strong>Result<\/strong><\/div>/);
  assert.match(out, /<strong>works<\/strong>/);
  assert.match(out, /<code class="chat-inline-code">code<\/code>/);
  assert.match(out, /<em>inline<\/em>/);
  assert.match(out, /<ul><li><p>one<\/p><\/li><li><p>two<\/p><ul><li><p>nested<\/p><\/li><\/ul><\/li><\/ul>/);
  assert.match(out, /<ol start="3">/);
  assert.match(out, /<blockquote><p>quoted<\/p><\/blockquote>/);
  assert.match(out, /<table><thead><tr><th>a<\/th><th>b<\/th><\/tr><\/thead><tbody><tr><td>1<\/td><td>2<\/td><\/tr><\/tbody><\/table>/);
  assert.match(out, /<pre class="chat-code"><code>let x = 1;<\/code><\/pre>/);
  assert.ok(out.includes('See the docs (https://example.com/a?b=1).'), out);
  assert.doesNotMatch(out, /rust/, 'the fence info string reaches no class');
  assert.doesNotMatch(out, /align|style=/);
});

test('ordered lists keep a parsed first number and refuse anything else', () => {
  assert.match(html('0. zero\n1. one'), /<ol start="0">/);
  assert.match(html('999999999. top'), /<ol start="999999999">/);
  assert.doesNotMatch(html('1. one\n2. two'), /start=/);
  const long = html('1000000000. too long');
  assert.doesNotMatch(long, /<ol|start=/);
  assert.ok(long.includes('1000000000. too long'));
});

test('hostile text yields only allowlisted elements and no href or src', () => {
  const cases = {
    script: '<script>alert(1)</script><img src=x onerror=alert(1)> <b>bold</b>',
    javascript: '[x](javascript:alert(1)) [y](data:text/html;base64,AAAA) [z](vbscript:x)',
    image: '![alt](https://evil.example/p.png) ![a](data:image/png;base64,AA)',
    quotes: '>'.repeat(10_000) + ' deep',
    spacedQuotes: '> '.repeat(5_000) + 'deep',
    table: ['| a | b |', '| - | - |', ...Array.from({ length: 5_000 }, () => '| x | y |')].join('\n'),
    wideRow: ['|' + 'a|'.repeat(10_000), '|' + '-|'.repeat(10_000), '|' + 'b|'.repeat(10_000)].join('\n'),
    longLine: 'x'.repeat(16_000),
    bidi: 'a‮b⁦c‏d'.repeat(2_000),
    stars: '*a '.repeat(5_000),
    underscores: '_a '.repeat(5_000),
    ticks: '`a '.repeat(5_000),
    mixedTicks: Array.from({ length: 600 }, (_, n) => '`'.repeat(n % 40 + 1) + 'a').join(' '),
    brackets: '['.repeat(10_000),
    bracketPairs: '[a]('.repeat(3_000),
    nestedLists: Array.from({ length: 40 }, (_, n) => ' '.repeat(n * 2) + '- item').join('\n'),
    manyCodes: '`a` '.repeat(4_000),
    manyBold: '**a** '.repeat(4_000),
    manyItems: '- a\n'.repeat(5_000),
    fences: '```\n'.repeat(3_000),
  };
  for (const [name, text] of Object.entries(cases)) {
    const { out, ms } = timed(text);
    assert.ok(out.length > 0 || text === '', name);
    assert.ok(ms < 1000, name);
  }
  const quoted = walk(md.parseAgentMarkdown(cases.quotes));
  assert.ok(quoted.types.has('blockquote'));
  assert.ok(quoted.maxDepth < 60, 'quotes stop nesting');
  assert.equal(html(cases.script).includes('<script'), false);
  assert.ok(html(cases.script).includes('&lt;script&gt;'));
  assert.ok(html(cases.javascript).includes('x (javascript:alert(1))'));
  assert.ok(html(cases.image).includes('![alt](https://evil.example/p.png)'));
});

test('nesting stops at six levels, a table at 20 rows and 8 columns, and the tree at 2,000 elements', () => {
  const nested = (marker) => walk(md.parseAgentMarkdown(Array.from({ length: 12 }, (_, n) => marker.repeat(n + 1) + ' x').join('\n')));
  assert.ok(nested('> ').types.has('blockquote'));
  const quotes = html('> > > > > > > > > deep');
  assert.equal(quotes.split('<blockquote>').length - 1, md.MAX_DEPTH);
  assert.ok(quotes.includes('&gt; &gt; &gt; deep'), 'the rest stays text');
  const rows = ['| a | b | c | d | e | f | g | h | i |', '|-|-|-|-|-|-|-|-|-|', '| 1 |'];
  assert.doesNotMatch(html(rows.join('\n')), /<table/, 'nine columns stay text');
  const table = html(['| a | b |', '|-|-|', ...Array.from({ length: 30 }, (_, n) => `| r${n} | y |`)].join('\n'));
  assert.equal(table.split('<tr>').length - 1, md.MAX_ROWS);
  assert.ok(table.includes('r29 | y |'), 'rows past the cap are plain text');
  const many = timed('`a` '.repeat(4_000));
  assert.ok(many.seen.elements <= md.MAX_ELEMENTS);
  assert.ok(many.out.includes('`a` `a`'), 'the rest after the cap is plain text');
});

test('text past 16 KiB is shown plain and the parse stays bounded', () => {
  const head = '**bold** line\n'.repeat(1_500);
  const out = html(head + '**tail** and `rest`');
  assert.ok(out.includes('<strong>bold</strong>'));
  assert.ok(out.includes('<div class="md-plain">') && out.includes('**tail** and `rest`'));
  assert.ok(!out.includes('<strong>tail'));
});

test('hidden characters are marked on every leaf and entities stay literal', () => {
  const out = html('a‮b `c​d` [x⁦](https://e.example/‮) | h‮ |\n|-|\n| c‮ |');
  assert.equal((out.match(/\\u\{/g) ?? []).length, 6, out);
  assert.ok(out.includes('\\u{202e}'));
  assert.ok(out.includes('&amp;#8238;') === false);
  assert.ok(html('&#8238; &amp; &lt;b&gt;').includes('&amp;#8238; &amp;amp; &amp;lt;b&amp;gt;'));
  assert.ok(html('Note: (redacted)').includes('<p>Note: (redacted)</p>'));
});

test('emoji with joiners, selectors and flags are unmarked, stray joiners are marked', () => {
  const family = '\u{1f468}‍\u{1f469}‍\u{1f467}';
  const heart = '❤️';
  const flag = '\u{1f1fa}\u{1f1f8}';
  const england = '\u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}';
  const keycap = '1️⃣';
  const good = html(`${family} ${heart} ${flag} ${england} ${keycap}`);
  assert.ok(!good.includes('\\u{'), good);
  for (const joiner of [family, heart, england, keycap]) assert.ok(good.includes(joiner));
  assert.ok(html('a‍b').includes('\\u{200d}'), 'a joiner between letters');
  assert.ok(html('\u{1f468}‍ b').includes('\\u{200d}'), 'a joiner before a letter');
  assert.ok(html('\u{e0067}\u{e007f}').includes('\\u{e0067}'), 'tags outside a flag');
  assert.ok(html('\u{1f3f4}\u{e0067}x').includes('\\u{e0067}'), 'tags without the cancel tag');
});

test('the person\'s own messages and other text keep today\'s rendering', () => {
  const text = 'Use **bold** and `code`\n# not a heading\n- not a list\n[a](http://x.example)\n```\nfenced\n```';
  const own = renderToStaticMarkup(createElement(ChatText, { text }));
  assert.equal(own, '<div class="chat-text"><span>Use <strong>bold</strong> and <code class="chat-inline-code">code</code>\n'
    + '# not a heading\n- not a list\n[a](http://x.example)</span><pre class="chat-code"><code>fenced</code></pre></div>');
  const agent = renderToStaticMarkup(createElement(ChatText, { text, mode: 'agent', final: true }));
  assert.match(agent, /^<div class="chat-text chat-md chat-final-reply">/);
  assert.match(agent, /md-heading/);
  assert.match(agent, /<ul>/);
  const memory = renderToStaticMarkup(createElement(MemoryText, { text: '**x** a‮b' }));
  assert.equal(memory, '<div class="memory-text" dir="ltr">**x** a<span class="memory-marker">\\u{202e}</span>b</div>');
});

test('the 5 second budget holds for the whole hostile set at once', () => {
  const start = performance.now();
  for (let n = 0; n < 5; n += 1) timed('*a '.repeat(5_000) + '[a]('.repeat(2_000) + '`'.repeat(3_000));
  assert.ok(performance.now() - start < 5_000);
});

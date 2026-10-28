// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A ticket's note as Markdown: what the renderer links, what it keeps literal, which notes open in the rich editor,
// what the editor may write back, and that a note nobody changed is saved as it was read.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

let server, md, rich, text, tickets;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  md = await server.ssrLoadModule('/src/renderer/chat/markdown.tsx');
  rich = await server.ssrLoadModule('/src/renderer/ticket/noteRich.ts');
  text = await server.ssrLoadModule('/src/renderer/ticket/noteText.ts');
  tickets = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
});
after(async () => { await server?.close(); });

const note = (source) => renderToStaticMarkup(createElement('div', null, md.parseAgentMarkdown(source, true)));
const agent = (source) => renderToStaticMarkup(createElement('div', null, md.parseAgentMarkdown(source)));

test('a note links http and https addresses in a new tab and nothing else', () => {
  const out = note('See [the docs](https://example.com/a?b=1) and [plain](http://example.com).');
  assert.ok(out.includes('<a href="https://example.com/a?b=1" target="_blank" rel="noopener noreferrer">the docs</a>'), out);
  assert.ok(out.includes('<a href="http://example.com" target="_blank" rel="noopener noreferrer">plain</a>'), out);
  for (const url of ['javascript:alert(1)', 'data:text/html;base64,AAAA', 'vbscript:x', 'mailto:a@b.c', 'ftp://example.com',
    '//example.com', 'HTTPS:/example.com']) {
    const hostile = note(`[x](${url})`);
    assert.doesNotMatch(hostile, /<a[\s>]|href=/, url);
    assert.ok(hostile.includes(`x (${url})`), hostile);
  }
});

test('a note keeps raw HTML literal and loads no image', () => {
  const out = note('<script>alert(1)</script><img src=x onerror=alert(1)> ![alt](https://evil.example/p.png)');
  assert.doesNotMatch(out, /<(?:script|img|iframe|style)[\s>]/i);
  assert.ok(out.includes('&lt;script&gt;'), out);
  assert.ok(out.includes('![alt](https://evil.example/p.png)'), out);
  assert.doesNotMatch(out, /<[^>]*\ssrc=/);
});

test('agent text still shows a link as its words followed by the address', () => {
  const out = agent('See [the docs](https://example.com/a?b=1).');
  assert.ok(out.includes('See the docs (https://example.com/a?b=1).'), out);
  assert.doesNotMatch(out, /<a[\s>]|href=/);
});

test('the note counts UTF-8 bytes, not characters, and sends line feeds only', () => {
  assert.equal(text.noteBytes('é'), 2);
  assert.equal(text.noteBytes('€'), 3);
  assert.equal(text.noteBytes('😀'), 4);
  assert.equal(text.toLf('a\r\nb\rc\n'), 'a\nb\nc\n');
  assert.equal(text.noteTooLong('a'.repeat(text.NOTE_LIMIT_BYTES)), null);
  assert.equal(text.noteTooLong('é'.repeat(text.NOTE_LIMIT_BYTES / 2)), null);
  assert.match(text.noteTooLong('é'.repeat(text.NOTE_LIMIT_BYTES / 2 + 1)), /8,194 bytes and the limit is 8,192/);
  assert.equal(text.noteTooLong('a\r\n'.repeat(text.NOTE_LIMIT_BYTES / 3)), null, 'CRLF counts as one byte');
});

const OPENS = {
  empty: '',
  paragraph: 'Fix the sign-in page.\n\nIt returns a 500 on a blank password.',
  star_bullets: '* one\n* two\n  * nested',
  underscore_bold: '__bold__ and _italic_ and `code`',
  extra_blank_lines: 'one\n\n\n\n\ntwo\n\n',
  numbered: '1. first\n2. second',
  heading_quote_code: '# Title\n\n> quoted\n\n```js\nlet x = 1;\n```',
  link: 'See [the docs](https://example.com) or https://example.org.',
  soft_break: 'one line\nnext line',
  hard_break: 'one line  \nnext line',
};

const KEEPS_AS_TEXT = {
  table: '| a | b |\n| --- | --- |\n| 1 | 2 |',
  task_list: '- [ ] todo\n- [x] done',
  strike: 'this is ~~gone~~',
  rule: 'above\n\n---\n\nbelow',
  image: '![alt](https://example.com/p.png)',
  html: '<b>bold</b> and <script>x</script>',
  html_block: '<div>\nblock\n</div>',
};

test('a note the editor keeps opens in it, whatever markers and blank lines it used', () => {
  for (const [name, source] of Object.entries(OPENS)) assert.equal(rich.opensRich(source), true, name);
});

test('a note the editor would drop or rewrite opens as text', () => {
  for (const [name, source] of Object.entries(KEEPS_AS_TEXT)) assert.equal(rich.opensRich(source), false, name);
});

test('a change of meaning is seen, and a change of marker is not', () => {
  assert.notEqual(rich.noteShape('one\n\ntwo'), rich.noteShape('one two'));
  assert.notEqual(rich.noteShape('[a](https://x.example)'), rich.noteShape('[a](https://y.example)'));
  assert.equal(rich.noteShape('* a\n* b'), rich.noteShape('- a\n\n- b'.replace('\n\n', '\n')));
  assert.equal(rich.noteShape('**a** *b*'), rich.noteShape('__a__ _b_'));
});

test('an edited note is written only in syntax the renderer shows', () => {
  const everything = [
    '# Heading', '', 'Text with **bold**, *italic*, `code` and a [link](https://example.com).', '', '- one', '- two', '',
    '1. first', '2. second', '', '> quote', '', '```', 'code block', '```', '',
    '~~struck~~ <u>underlined</u>', '', '---', '', '- [ ] task',
  ].join('\n');
  const out = rich.reserialize(everything);
  assert.doesNotMatch(out, /~~|<u>|<\/u>|^---$|\[ \]|&nbsp;| /m, out);
  assert.match(out, /\*\*bold\*\*/);
  assert.match(out, /\[link\]\(https:\/\/example\.com\)/);
  const shown = note(out);
  assert.doesNotMatch(shown, /&amp;nbsp;|~~|<u>/);
  assert.ok(shown.includes('<a href="https://example.com"'));
  const links = rich.reserialize('[mail](mailto:a@b.c) and [js](javascript:alert(1))');
  assert.doesNotMatch(note(links), /<a[\s>]/, 'the editor never makes a link the page would not open');
});

test('an empty paragraph is written as nothing, not as a literal non-breaking space', () => {
  const paragraph = (value) => (value === undefined ? { type: 'paragraph' } : { type: 'paragraph', content: [{ type: 'text', text: value }] });
  const out = rich.serialize({ type: 'doc', content: [paragraph('first'), paragraph(), paragraph(), paragraph('last')] });
  assert.doesNotMatch(out, /&nbsp;| /, JSON.stringify(out));
  assert.ok(out.startsWith('first') && out.endsWith('last'));
});

test('a note nobody changed is saved as it was read, and an edited one as the editor wrote it', () => {
  const original = '* one\n* two';
  const opened = rich.reserialize(original);
  assert.notEqual(opened, original, 'the editor spells the bullets differently');
  assert.equal(text.nextNote(original, opened, opened), original);
  const changed = text.nextNote(original, opened, `${opened}\n- three`);
  assert.equal(changed, '- one\n- two\n- three');
  const card = { id: 'c', seq: 3 };
  const sent = tickets.changePayload(card, { verb: 'edit', title: 'T', note: text.toLf(original), check: null });
  assert.equal(sent.description, original);
});

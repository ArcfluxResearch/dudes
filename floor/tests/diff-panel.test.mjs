// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { fakePage, press } from './helpers/fake-page.mjs';
import { DIFF_CARD, DIFF_ENTRY, DIFF_MANIFEST, diffBinding, diffContent, diffEntry, diffFile, diffPage } from './helpers/diff-fixtures.mjs';

let server, DiffPanel, DiffReview;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), configFile: false,
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ DiffPanel, DiffReview } = await server.ssrLoadModule('/src/renderer/diff/DiffPanel.tsx'));
});
after(async () => { await server?.close(); });

const response = (value) => ({ kind: 'response', response: value });
const text = (node) => node.nodeType === 3 ? node.nodeValue : node.childNodes.map(text).join('');
const elements = (node) => node.childNodes.flatMap((child) => child.nodeType === 1 ? [child, ...elements(child)] : []);
const buttons = (app) => elements(app.page.host).filter((node) => node.tagName === 'BUTTON');
const button = (app, label) => buttons(app).find((node) => text(node) === label);
async function mount(reply = () => response(diffPage())) {
  const page = fakePage();
  const root = createRoot(page.host);
  const seen = [];
  let generation = 1;
  const transport = { generation: () => generation, async request(request) { seen.push(request); return reply(request); } };
  const app = { page, root, transport, seen, changeSession() { generation += 1; },
    async draw(fields = {}) { await act(async () => root.render(createElement(DiffPanel, { card: DIFF_CARD, expectedSeq: 4, transport, ...fields }))); },
    async click(label) { const found = button(app, label); assert.ok(found, label); await act(async () => press(found)); },
    async close() { await act(async () => root.unmount()); page.restore(); } };
  await app.draw();
  return app;
}

test('open, selection, page, content and refresh are separate explicit actions', async () => {
  const cursor = 'd'.repeat(32), second = 'f'.repeat(32);
  const app = await mount((request) => response(request.mode === 'content' ? diffContent()
    : request.mode === 'page' ? diffPage({ mode: 'page', entries: [diffEntry({ entry_id: second, path: 'new.rs', kind: 'added' })] })
      : diffPage({ next_cursor: cursor })));
  try {
    assert.deepEqual(app.seen, []);
    await app.click('Open differences');
    assert.deepEqual(app.seen, [{ mode: 'manifest', card: DIFF_CARD, expected_seq: 4 }]);
    await app.click('Select file');
    assert.equal(app.seen.length, 1);
    await app.click('Read selected files');
    assert.deepEqual(app.seen[1], { mode: 'content', manifest_id: DIFF_MANIFEST, entries: [DIFF_ENTRY] });
    assert.match(text(app.page.host), /fn count\(\) -> u8/);
    await app.click('Next files');
    assert.deepEqual(app.seen[2], { mode: 'page', manifest_id: DIFF_MANIFEST, cursor });
    assert.match(text(app.page.host), /new.rs/);
    await app.click('Refresh differences');
    assert.deepEqual(app.seen[3], { mode: 'manifest', card: DIFF_CARD, expected_seq: 4 });
    assert.doesNotMatch(text(app.page.host), /fn count/);
    await app.click('Close differences');
    assert.deepEqual(app.seen.length, 4);
  } finally { await app.close(); }
});

test('stale and expired answers clear content and wait for an explicit refresh', async () => {
  for (const reason of ['stale_ticket', 'expired', 'session_limit']) {
    const app = await mount((request) => response(request.mode === 'content' ? { mode: 'refused', reason } : diffPage()));
    try {
      await app.click('Open differences');
      await app.click('Select file');
      await app.click('Read selected files');
      assert.equal(app.seen.length, 2);
      assert.doesNotMatch(text(app.page.host), /src\/example.rs|fn count/);
      assert.match(text(app.page.host), reason === 'session_limit' ? /session.*allowance/i : /Refresh differences/);
      await app.draw();
      assert.equal(app.seen.length, 2);
    } finally { await app.close(); }
  }
});

test('selection stays within eight files and changing it clears previously displayed content', async () => {
  const entries = Array.from({ length: 9 }, (_, index) => diffEntry({ entry_id: index.toString(16).padStart(32, '0'), path: `src/file-${index}.rs` }));
  const app = await mount((request) => response(request.mode === 'content' ? diffContent({ files: entries.slice(0, 8).map((entry) => diffFile({ entry_id: entry.entry_id, path: entry.path })) }) : diffPage({ entries })));
  try {
    await app.click('Open differences');
    for (let index = 0; index < 8; index += 1) await app.click('Select file');
    assert.equal(button(app, 'Select file').getAttribute('disabled'), '');
    assert.equal(app.seen.length, 1);
    await app.click('Read selected files');
    assert.deepEqual(app.seen[1].entries, entries.slice(0, 8).map((entry) => entry.entry_id));
    assert.match(text(app.page.host), /fn count/);
    await app.click('Deselect file');
    assert.doesNotMatch(text(app.page.host), /fn count/);
    assert.match(text(app.page.host), /7 of 8 selected/);
    assert.equal(app.seen.length, 2);
  } finally { await app.close(); }
});

test('rendered source text retains tabs, CRLF, CR, blank lines and an absent final newline', async () => {
  const old = 'a\t b\r\nc\rd\n\nlast';
  const current = 'a\t b\r\nc\rd\n\nlast changed';
  const app = await mount((request) => response(request.mode === 'content' ? diffContent({ files: [diffFile({ old, current })] }) : diffPage()));
  try {
    await app.click('Open differences'); await app.click('Select file'); await app.click('Read selected files');
    const panes = elements(app.page.host).filter((node) => node.getAttribute('class') === 'diff-source');
    const source = (pane) => elements(pane).filter((node) => node.tagName === 'CODE').map(text).join('');
    assert.equal(source(panes[0]), old);
    assert.equal(source(panes[1]), current);
    for (const pane of panes) assert.equal(elements(pane).filter((node) => node.getAttribute('class') === 'diff-line-number').map(text).join('\n'), '1\n2\n3\n4\n5');
    assert.match(text(app.page.host), /No final newline\./);
  } finally { await app.close(); }
});

test('line numbers count final LF, CRLF and CR once and empty sides have no line numbers', async () => {
  for (const [old, current, expected] of [['', '', []], ['\n', 'a\n', ['1', '1']],
    ['a\r\nb\r\n', 'a\rb\r', ['1\n2', '1\n2']], ['a\nb', 'a\r\nb', ['1\n2', '1\n2']]]) {
    const app = await mount((request) => response(request.mode === 'content' ? diffContent({ files: [diffFile({ old, current })] }) : diffPage()));
    try {
      await app.click('Open differences'); await app.click('Select file'); await app.click('Read selected files');
      const panes = elements(app.page.host).filter((node) => node.getAttribute('class') === 'diff-source');
      assert.deepEqual(panes.map((pane) => elements(pane).filter((node) => node.getAttribute('class') === 'diff-line-number').map(text).join('\n')), expected);
      if (old === '') assert.match(text(app.page.host), /Empty side\./);
    } finally { await app.close(); }
  }
});

test('late answers cannot cross a close, a ticket sequence or a session change', async () => {
  for (const change of ['close', 'sequence', 'session']) {
    let resolveReply;
    const pending = new Promise((resolve) => { resolveReply = resolve; });
    const app = await mount(() => pending);
    try {
      await app.click('Open differences');
      assert.match(text(app.page.host), /Reading/);
      if (change === 'close') await app.click('Close differences');
      if (change === 'sequence') await app.draw({ expectedSeq: 5 });
      if (change === 'session') app.changeSession();
      await act(async () => resolveReply(response(diffPage())));
      assert.doesNotMatch(text(app.page.host), /src\/example.rs/);
      assert.equal(app.seen.length, 1);
    } finally { await app.close(); }
  }
});

test('a replacement transport with the same generation clears pending and ready reads', async () => {
  for (const phase of ['pending', 'ready']) {
    let release;
    const pending = new Promise((resolve) => { release = resolve; });
    const app = await mount((request) => phase === 'pending' ? pending
      : response(request.mode === 'content' ? diffContent() : diffPage()));
    try {
      await app.click('Open differences');
      if (phase === 'ready') { await app.click('Select file'); await app.click('Read selected files'); }
      const replacement = { generation: () => 1, async request() { return response(diffPage({ entries: [diffEntry({ path: 'replacement.rs' })] })); } };
      await app.draw({ transport: replacement });
      assert.match(text(app.page.host), /Open differences/);
      assert.doesNotMatch(text(app.page.host), /Reading|src\/example.rs|fn count/);
      await act(async () => release(response(diffPage())));
      await app.click('Open differences');
      assert.match(text(app.page.host), /replacement.rs/);
      assert.doesNotMatch(text(app.page.host), /src\/example.rs|fn count/);
    } finally { await app.close(); }
  }
});

test('a content reply must match the chosen manifest binding and exact selected rows', async () => {
  for (const result of [diffContent({ manifest_id: 'd'.repeat(32) }), diffContent({ binding: diffBinding({ round: 3 }) }),
    diffContent({ files: [diffFile({ path: 'unexpected.rs' })] }), diffContent({ files: [diffFile({ entry_id: 'f'.repeat(32) })] }),
    diffContent({ files: [] }), diffContent({ files: [], truncated: true })]) {
    const app = await mount((request) => response(request.mode === 'content' ? result : diffPage()));
    try {
      await app.click('Open differences'); await app.click('Select file'); await app.click('Read selected files');
      assert.match(text(app.page.host), /reply this page could not read/);
      assert.doesNotMatch(text(app.page.host), /fn count|unexpected.rs/);
      assert.equal(app.seen.length, 2);
    } finally { await app.close(); }
  }
});

const renderReview = (page, content = null) => renderToStaticMarkup(createElement(DiffReview, {
  review: { page, entries: page.entries, selected: [], content }, loading: false,
  onSelect() {}, onPage() {}, onRead() {},
}));

test('the rendered review uses the exact sample copy and states its limits and dirty baseline', () => {
  for (const [freshness, sentence] of [
    ['matches_review_sample', 'Matches the review sample taken after this check.'],
    ['changed_after_check', 'The reviewed checkout content changed after this check.'],
    ['unknown', 'Current checkout differences; content freshness against this check is unknown.'],
  ]) {
    const html = renderReview(diffPage({ freshness, binding: diffBinding({ preexisting_tracked: true }),
      omitted: 3, omitted_lower_bound: true, untracked: 4, untracked_lower_bound: true, truncated: true }));
    assert.ok(html.includes(sentence));
    assert.match(html, /Untracked and omitted files, and changes hidden from the bounded candidate read, are outside this comparison\./);
    assert.match(html, /Tracked changes already existed before this round/);
    assert.match(html, /At least 4 untracked files/);
    assert.match(html, /At least 3 candidate files/);
    assert.doesNotMatch(html, /checkout is unchanged|diff passed/);
  }
});

test('a missing check sample never says a sample was taken after the check', () => {
  const html = renderReview(diffPage({ freshness: 'unknown', binding: diffBinding({ check_lsn: null }) }));
  assert.match(html, /Current checkout differences; content freshness against this check is unknown\./);
  assert.match(html, /No check sample is recorded\./);
  assert.match(html, /These differences do not show what the check read or prove continuous content identity\./);
  assert.doesNotMatch(html, /sample was taken after the check/);
});

test('binary and oversize files show whole-pair omissions and the editor fallback', () => {
  const page = diffPage({ entries: [diffEntry({ entry_id: null, path: 'image.bin', availability: { state: 'unavailable', reason: 'binary' } }),
    diffEntry({ entry_id: null, path: 'large.rs', availability: { state: 'unavailable', reason: 'oversize_file' } })] });
  const html = renderReview(page);
  assert.match(html, /Binary content is not shown\./);
  assert.match(html, /This file exceeds the whole-file preview limit\./);
  assert.match(html, /dudes ticket open/);
  assert.match(html, /16 KiB per side/);
  assert.doesNotMatch(html, /Select file/);
});

test('hostile paths and source remain text and never enter any attributes or resource markup', () => {
  const sentinel = 'HOSTILE-TEXT';
  const path = `${sentinel}-<img src=x onerror=alert(1)>.rs`;
  const current = `<script>${sentinel}</script>\n[link](https://example.invalid/${sentinel})\n/tmp/${sentinel}\n`;
  const html = renderReview(diffPage({ entries: [diffEntry({ path, kind: 'renamed', old_path: 'old.rs' })] }),
    diffContent({ files: [diffFile({ path, current, kind: 'renamed', old_path: 'old.rs' })] }));
  assert.match(html, /&lt;script&gt;HOSTILE-TEXT&lt;\/script&gt;/);
  assert.match(html, /&lt;img/);
  assert.match(html, /Old/); assert.match(html, /Current/); assert.match(html, /Renamed/);
  const attributes = [...html.matchAll(/\s[a-z-]+="([^"]*)"/g)].map((match) => match[1]);
  assert.deepEqual(attributes.filter((value) => value.includes(sentinel)), []);
  assert.doesNotMatch(html, /<(?:script|img|a)\b|(?:href|src|title|style|data-[a-z-]+)="/);
});

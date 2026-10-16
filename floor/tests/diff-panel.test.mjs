// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The ticket drawer's Changes view: what each freshness label says, what every refusal tells the
// operator to do, and that nothing is read or retried unless the operator asks.
//
// Mutation controls, each named by the test it turns red:
//   - the unknown label worded as fresh or as a pass ......... "each freshness label…"
//   - a read made on draw, or a refusal retried ............... "nothing is read…"
//   - an expired or stale answer keeping old text ............. "a stale, expired or limited answer…"
//   - the view drawn without the snapshot signal .............. "the view exists only when…"
//   - a binary or oversize row given an Open button ........... "binary and oversize rows…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { fakePage, press } from './helpers/fake-page.mjs';
import { DIFF_CARD, DIFF_ENTRY, DIFF_MANIFEST, diffBinding, diffContent, diffEntry, diffFile, diffPage } from './helpers/diff-fixtures.mjs';
import { ADA, agent, cardId, subscribe, ticketCard } from './helpers/tickets.mjs';

let server, DiffPanel, DiffReview, DIFF_REFUSALS, store, TicketPanel, postDiff;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), configFile: false,
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-diff-panel'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ DiffPanel, DiffReview } = await server.ssrLoadModule('/src/renderer/diff/DiffPanel.tsx'));
  ({ DIFF_REFUSALS, postDiff } = await server.ssrLoadModule('/src/renderer/diff/diff.ts'));
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ TicketPanel } = await server.ssrLoadModule('/src/renderer/ticket/TicketPanel.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

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

test('nothing is read until the operator asks, and each action is one request', async () => {
  const cursor = 'd'.repeat(32), second = 'f'.repeat(32);
  const app = await mount((request) => response(request.mode === 'content' ? diffContent()
    : request.mode === 'page' ? diffPage({ mode: 'page', entries: [diffEntry({ entry_id: second, path: 'new.rs', kind: 'added' })] })
      : diffPage({ next_cursor: cursor })));
  try {
    assert.deepEqual(app.seen, [], 'drawing reads nothing');
    await app.click('Show changes');
    assert.deepEqual(app.seen, [{ mode: 'manifest', card: DIFF_CARD, expected_seq: 4 }]);
    await app.click('Open side by side');
    assert.deepEqual(app.seen[1], { mode: 'content', manifest_id: DIFF_MANIFEST, entries: [DIFF_ENTRY] });
    assert.match(text(app.page.host), /fn count\(\) -> u8/);
    await app.click('Show more files');
    assert.deepEqual(app.seen[2], { mode: 'page', manifest_id: DIFF_MANIFEST, cursor });
    assert.match(text(app.page.host), /new\.rs/);
    assert.match(text(app.page.host), /fn count/, 'paging keeps the open file');
    await app.click('Refresh');
    assert.deepEqual(app.seen[3], { mode: 'manifest', card: DIFF_CARD, expected_seq: 4 });
    assert.doesNotMatch(text(app.page.host), /fn count/, 'a new list starts with no file open');
    await app.click('Hide changes');
    await app.draw();
    assert.equal(app.seen.length, 4, 'closing and redrawing read nothing');
  } finally { await app.close(); }
});

test('two clicks in one frame send one request', async () => {
  const app = await mount();
  try {
    const found = button(app, 'Show changes');
    await act(async () => { press(found); press(found); });
    assert.equal(app.seen.length, 1);
    await app.click('Refresh');
    assert.equal(app.seen.length, 2, 'the guard lets the next read through once the first has ended');
  } finally { await app.close(); }
});

test('an open file shows its old and current text side by side, and closes again', async () => {
  const app = await mount((request) => response(request.mode === 'content' ? diffContent() : diffPage()));
  try {
    await app.click('Show changes');
    await app.click('Open side by side');
    const sides = elements(app.page.host).filter((node) => node.getAttribute('class') === 'diff-sides');
    assert.equal(sides.length, 1);
    const headings = elements(sides[0]).filter((node) => node.tagName === 'H6').map(text);
    assert.deepEqual(headings, ['Old', 'Current']);
    const code = elements(sides[0]).filter((node) => node.tagName === 'CODE').map(text);
    assert.deepEqual(code, ['fn count() -> u8 {\n    1\n}\n', 'fn count() -> u8 {\n    2\n}\n']);
    await app.click('Close file');
    assert.equal(elements(app.page.host).filter((node) => node.getAttribute('class') === 'diff-sides').length, 0);
    assert.equal(app.seen.length, 2, 'closing a file reads nothing');
  } finally { await app.close(); }
});

const html = (page, extra = {}) => renderToStaticMarkup(createElement(DiffReview, {
  review: { page, entries: page.entries, opened: null, content: null, ...extra }, loading: false, onOpen() {}, onPage() {},
}));

test('each freshness label says what it means and never reads as fresh or as a pass', () => {
  const matches = html(diffPage({ freshness: 'matches_review_sample' }));
  assert.match(matches, /<span class="diff-badge" data-freshness="matches_review_sample">Unchanged since the check<\/span>/);
  assert.match(matches, /does not show that the check passed on these changes/);

  const changed = html(diffPage({ freshness: 'changed_after_check' }));
  assert.match(changed, /<span class="diff-badge" data-freshness="changed_after_check">Changed since the check<\/span>/);
  assert.match(changed, /not the files the check saw/);

  const unknown = html(diffPage({ freshness: 'unknown', binding: diffBinding({ check_lsn: null }) }));
  assert.match(unknown, /<span class="diff-badge" data-freshness="unknown">Unknown<\/span>/);
  assert.match(unknown, /Dudes cannot tell whether this is what the check saw\./);
  assert.match(unknown, /It cannot make a complete comparison with the sample from just after the check/);
  assert.match(unknown, /leaving out any one file makes a sample incomplete/);
  assert.match(unknown, /Unknown does not mean fresh\./);
  assert.doesNotMatch(unknown, /Unchanged since|Changed since|passed the check|is unchanged/);
  for (const page of [matches, changed, unknown]) assert.match(page, /Files Git does not track yet, files left out, and changes the bounded read cannot see\s+are outside this comparison/);
});

test('tracked changes from before the round are warned about, and only then', () => {
  assert.match(html(diffPage({ binding: diffBinding({ preexisting_tracked: true }) })),
    /Some tracked files already had changes when this round began, so the list below can include changes the agent did not make\./);
  assert.doesNotMatch(html(diffPage()), /already had changes/);
});

test('omitted and untracked counts are stated, with a lower bound when the daemon only has one', () => {
  const page = html(diffPage({ omitted: 3, omitted_lower_bound: true, untracked: 4, untracked_lower_bound: true, truncated: true }));
  assert.match(page, /At least 3 changed files were left out of this list\. The list reached its limit\./);
  assert.match(page, /At least 4 untracked files are not listed\./);
});

test('binary and oversize rows say why and have no Open button', () => {
  const page = diffPage({ entries: [
    diffEntry({ entry_id: null, path: 'image.bin', availability: { state: 'unavailable', reason: 'binary' } }),
    diffEntry({ entry_id: null, path: 'large.rs', availability: { state: 'unavailable', reason: 'oversize_file' } }),
    diffEntry({ path: 'small.rs' }),
  ] });
  const out = html(page);
  assert.match(out, /Binary file\. Its contents are not shown\./);
  assert.match(out, /Too large to show here\. Files over 16 KiB per side are not shown\./);
  assert.equal(out.match(/Open side by side/g).length, 1, 'only the readable row can be opened');
  assert.match(out, /dudes ticket open/);
});

test('a stale, expired or limited answer clears the text and waits for an explicit Refresh', async () => {
  const wanted = {
    stale_ticket: /Choose Refresh to see its current changes/,
    stale_manifest: /out of date\. Choose Refresh to make a new one/,
    expired: /expired after a minute\. Choose Refresh to make a new one/,
    session_limit: /used all the reading it is allowed \(about 1 MiB\), and Refresh does not renew it/,
  };
  for (const [reason, pattern] of Object.entries(wanted)) {
    const app = await mount((request) => response(request.mode === 'content' ? { mode: 'refused', reason } : diffPage()));
    try {
      await app.click('Show changes');
      await app.click('Open side by side');
      assert.equal(app.seen.length, 2);
      assert.doesNotMatch(text(app.page.host), /src\/example\.rs|fn count/);
      assert.match(text(app.page.host), pattern);
      await app.draw();
      assert.equal(app.seen.length, 2, 'a refusal is not retried');
      assert.ok(button(app, 'Refresh'), 'Refresh is offered');
    } finally { await app.close(); }
  }
});

test('every refusal reason has its own plain sentence', async () => {
  const sentences = new Map();
  for (const reason of DIFF_REFUSALS) {
    const app = await mount(() => response({ mode: 'refused', reason }));
    try {
      await app.click('Show changes');
      const status = elements(app.page.host).find((node) => node.getAttribute('role') === 'status');
      const sentence = text(status);
      assert.ok(sentence.length > 20, reason);
      assert.doesNotMatch(sentence, /_/, `${reason} shows no code name`);
      assert.equal(sentences.has(sentence), false, `${reason} repeats another reason's sentence`);
      sentences.set(sentence, reason);
      assert.equal(app.seen.length, 1, 'nothing retries');
    } finally { await app.close(); }
  }
});

test('failures that are not a refusal tell the operator what to do', async () => {
  for (const [kind, pattern] of [['forbidden', /no current Floor session/], ['unavailable', /did not answer\. Choose Refresh/],
    ['unknown', /could not read\. Choose Refresh/]]) {
    const app = await mount(() => ({ kind }));
    try {
      await app.click('Show changes');
      assert.match(text(app.page.host), pattern);
      assert.equal(app.seen.length, 1);
    } finally { await app.close(); }
  }
});

test('late answers cannot cross a close, a ticket sequence or a session change', async () => {
  for (const change of ['close', 'sequence', 'session']) {
    let resolveReply;
    const pending = new Promise((resolve) => { resolveReply = resolve; });
    const app = await mount(() => pending);
    try {
      await app.click('Show changes');
      assert.match(text(app.page.host), /Reading/);
      if (change === 'close') await app.click('Hide changes');
      if (change === 'sequence') await app.draw({ expectedSeq: 5 });
      if (change === 'session') app.changeSession();
      await act(async () => resolveReply(response(diffPage())));
      assert.doesNotMatch(text(app.page.host), /src\/example\.rs/);
      assert.equal(app.seen.length, 1);
    } finally { await app.close(); }
  }
});

test('a content reply must match the manifest and the one file that was opened', async () => {
  for (const result of [diffContent({ manifest_id: 'd'.repeat(32) }), diffContent({ binding: diffBinding({ round: 3 }) }),
    diffContent({ files: [diffFile({ path: 'unexpected.rs' })] }), diffContent({ files: [diffFile({ entry_id: 'f'.repeat(32) })] }),
    diffContent({ files: [] }), diffContent({ files: [], truncated: true })]) {
    const app = await mount((request) => response(request.mode === 'content' ? result : diffPage()));
    try {
      await app.click('Show changes'); await app.click('Open side by side');
      assert.match(text(app.page.host), /reply this page could not read/);
      assert.doesNotMatch(text(app.page.host), /fn count|unexpected\.rs/);
    } finally { await app.close(); }
  }
});

test('a file the daemon left out of a content reply is explained instead of shown', async () => {
  const app = await mount((request) => response(request.mode === 'content'
    ? diffContent({ files: [], omitted: [{ entry_id: DIFF_ENTRY, reason: 'raced' }] }) : diffPage()));
  try {
    await app.click('Show changes'); await app.click('Open side by side');
    assert.match(text(app.page.host), /This file changed while it was being read, so it is not shown\./);
  } finally { await app.close(); }
});

test('source text keeps tabs, CRLF, CR and an absent final newline, and numbers lines once', async () => {
  const old = 'a\t b\r\nc\rd\n\nlast';
  const current = 'a\t b\r\nc\rd\n\nlast changed';
  const app = await mount((request) => response(request.mode === 'content' ? diffContent({ files: [diffFile({ old, current })] }) : diffPage()));
  try {
    await app.click('Show changes'); await app.click('Open side by side');
    const panes = elements(app.page.host).filter((node) => node.getAttribute('class') === 'diff-source');
    const source = (pane) => elements(pane).filter((node) => node.tagName === 'CODE').map(text).join('');
    assert.equal(source(panes[0]), old);
    assert.equal(source(panes[1]), current);
    for (const pane of panes) assert.equal(elements(pane).filter((node) => node.getAttribute('class') === 'diff-line-number').map(text).join('\n'), '1\n2\n3\n4\n5');
    assert.match(text(app.page.host), /No final newline\./);
  } finally { await app.close(); }
});

test('an empty side says why it is empty', async () => {
  for (const [kind, old, current, expected] of [
    ['added', '', 'x\n', 'Not in the commit this round started from.'],
    ['deleted', 'x\n', '', 'Deleted: nothing is left in the checkout.'],
  ]) {
    const app = await mount((request) => response(request.mode === 'content'
      ? diffContent({ files: [diffFile({ kind, old, current })] }) : diffPage({ entries: [diffEntry({ kind })] })));
    try {
      await app.click('Show changes'); await app.click('Open side by side');
      assert.ok(text(app.page.host).includes(expected), kind);
    } finally { await app.close(); }
  }
});

test('hostile paths and source remain text and never enter any attribute or resource markup', () => {
  const sentinel = 'HOSTILE-TEXT';
  const path = `${sentinel}-<img src=x onerror=alert(1)>.rs`;
  const current = `<script>${sentinel}</script>\n[link](https://example.invalid/${sentinel})\n/tmp/${sentinel}\n`;
  const page = diffPage({ entries: [diffEntry({ path, kind: 'renamed', old_path: 'old.rs' })] });
  const out = html(page, { opened: DIFF_ENTRY, content: diffContent({ files: [diffFile({ path, current, kind: 'renamed', old_path: 'old.rs' })] }) });
  assert.match(out, /&lt;script&gt;HOSTILE-TEXT&lt;\/script&gt;/);
  assert.match(out, /&lt;img/);
  assert.match(out, /Old/); assert.match(out, /Current/); assert.match(out, /Renamed/);
  const attributes = [...out.matchAll(/\s[a-z-]+="([^"]*)"/g)].map((match) => match[1]);
  assert.deepEqual(attributes.filter((value) => value.includes(sentinel)), []);
  assert.doesNotMatch(out, /<(?:script|img|a)\b|(?:href|src|title|style|data-(?!freshness)[a-z-]+)="/);
});

const reviewCard = (status = 'review') => ticketCard(1, status, { claimed_by: ADA });
const drawer = (status) => {
  const floor = store.getFloorState();
  return renderToStaticMarkup(createElement(TicketPanel, { floor, card: floor.cards.find((c) => c.id === cardId(1)) }));
};

test('the view exists only when the snapshot carries the diff signal and the ticket is under review', () => {
  for (const [diff, status, shown] of [[false, 'review', false], [true, 'review', true], [true, 'blocked', true],
    [true, 'todo', false], [true, 'doing', false], [true, 'done', false], [false, 'blocked', false]]) {
    store.resetFloorStore();
    subscribe(store, { agents: [agent()], cards: [reviewCard(status)], diff });
    const out = drawer(status);
    assert.equal(/<section class="diff-panel" aria-label="Changes">/.test(out), shown, `diff=${diff} status=${status}`);
    assert.equal(out.includes('Show changes'), shown, `diff=${diff} status=${status}`);
  }
});

test('a snapshot signals the view only with an empty diff object, and a closed stream forgets it', () => {
  const frame = (extra) => {
    store.resetFloorStore();
    const epoch = store.beginFloorEpoch();
    store.noteTransport('open');
    store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 10, agents: [], cards: [] }, ...extra }), epoch);
    return store.getFloorState().diff;
  };
  assert.equal(frame({}), false);
  assert.equal(frame({ diff: {} }), true);
  assert.equal(frame({ diff: { path: '/tmp/x' } }), false, 'a signal that carries anything is off');
  assert.equal(frame({ diff: true }), false);
  assert.equal(frame({ diff: [] }), false);
  assert.equal(frame({ git: {} }), false, 'the git signal alone does not show the view');
  assert.equal(frame({ diff: {} }), true);
  store.noteTransport('close');
  assert.equal(store.getFloorState().diff, false);
});

test('the request is one POST with the CSRF header, and every status has one reading', async () => {
  const run = async (status, body, csrf = 'c'.repeat(43)) => {
    const calls = [];
    const outcome = await postDiff({ mode: 'manifest', card: DIFF_CARD, expected_seq: 4 }, csrf, async (url, init) => {
      calls.push([url, init]);
      return { status, text: async () => body };
    });
    return { outcome, calls };
  };
  const ok = await run(200, JSON.stringify(diffPage()));
  assert.equal(ok.outcome.kind, 'response');
  assert.equal(ok.calls[0][0], '/floor/diff');
  assert.equal(ok.calls[0][1].method, 'POST');
  assert.equal(ok.calls[0][1].credentials, 'same-origin');
  assert.equal(ok.calls[0][1].headers['X-Dudes-CSRF'], 'c'.repeat(43));
  assert.equal(ok.calls[0][1].body, JSON.stringify({ mode: 'manifest', card: DIFF_CARD, expected_seq: 4 }));
  assert.equal((await run(200, '{"mode":"manifest"}')).outcome.kind, 'unknown');
  assert.equal((await run(200, JSON.stringify({ mode: 'refused', reason: 'session_limit' }))).outcome.response.reason, 'session_limit');
  assert.equal((await run(403, '')).outcome.kind, 'forbidden');
  assert.equal((await run(503, '')).outcome.kind, 'unavailable');
  assert.equal((await run(404, '')).outcome.kind, 'unknown');
  const none = await run(200, '', null);
  assert.equal(none.outcome.kind, 'forbidden');
  assert.equal(none.calls.length, 0, 'no session, no request');
  const offline = await postDiff({ mode: 'manifest', card: DIFF_CARD, expected_seq: 4 }, 'c'.repeat(43), async () => { throw new Error('offline'); });
  assert.equal(offline.kind, 'unavailable');
});

// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Memory tab: the strict decoder, the inert text renderer and the panel's one request
// lifecycle, driven through a mounted panel and a stand-in daemon.
//
// Mutation controls, each named by the test it turns red:
//   - memory text drawn through the Chat renderer ...... "hostile text renders inert…"
//   - only Cc and Cf drawn as markers ................... "hostile text renders inert…"
//   - a withheld marker's text member accepted .......... "the decoder refuses…", "the panel shows a read error…"
//   - a non-ok page carrying entries accepted ........... "the decoder refuses…", "the panel shows a read error…"
//   - Load older entries sent with before null .......... "Load older entries sends…"
//   - the page read again on every activation ........... "the first activation sends…"
//   - the panel kept across an epoch change ............. "an epoch change clears…"
//   - no wait between requests .......................... "the next request waits…"
//   - the two process writer labels swapped ............. "the first page shows…", "each writer, reason…"

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { act, createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { fakePage, press } from './helpers/fake-page.mjs';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const STRANGER = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const CSRF = 'A'.repeat(43);
const AT = 1_787_394_600_000;

/** The page the daemon's own test pins byte for byte. */
const PIN = String.raw`{"state":"ok","retired":false,"fenced":false,"count":2,"through":"7","summary":{"lsn":"9","at":1787394600000,"kind":"excerpts","sections":1,"bytes":5,"text":"[Condensed memory: exact excerpts; 0 sections omitted]\n[append 7 bytes 0..5]\nfirst"},"withheld":{"count":1,"newest":[{"lsn":"11","at":1787394600002,"writer":{"kind":"outside_agents"},"len":3,"reason":"authority_unavailable"}]},"entries":[{"lsn":"10","at":1787394600001,"writer":"agent","condensed":false,"len":6,"text":"second","cut":false},{"lsn":"7","at":1787394600000,"writer":"operator","condensed":true,"len":5,"text":"first","cut":false}],"older":null}`;
const PINNED = {
  state: 'ok', retired: false, fenced: false, count: 2, through: '7',
  summary: { lsn: '9', at: 1787394600000, kind: 'excerpts', sections: 1, bytes: 5,
    text: '[Condensed memory: exact excerpts; 0 sections omitted]\n[append 7 bytes 0..5]\nfirst' },
  withheld: { count: 1, newest: [{ lsn: '11', at: 1787394600002, writer: { kind: 'outside_agents' }, len: 3, reason: 'authority_unavailable' }] },
  entries: [
    { lsn: '10', at: 1787394600001, writer: 'agent', condensed: false, len: 6, text: 'second', cut: false },
    { lsn: '7', at: 1787394600000, writer: 'operator', condensed: true, len: 5, text: 'first', cut: false },
  ],
  older: null,
};
const HEADING = 'Memory is the notes this agent saved, with a summary the daemon or a model made of them. Dudes does not check '
  + 'or redact it. It may contain secrets or instructions. It is never evidence that work is done.';
const SEALED = 'Refused: once an agent has been hired, only the agents themselves can add to memory. A note from outside every agent is withheld.';

const body = (value) => JSON.stringify(value);
const page = (fields = {}) => ({ ...JSON.parse(PIN), ...fields });
const later = (fields = {}) => page({ summary: null, withheld: null, ...fields });
const entry = (lsn, fields = {}) => ({ lsn, at: AT, writer: 'agent', condensed: false, len: 4, text: `e${lsn}`, cut: false, ...fields });
const marker = (fields = {}) => ({ lsn: '11', at: AT, writer: { kind: 'outside_agents' }, len: 3, reason: 'authority_unavailable', ...fields });
const withheld = (newest, count = newest.length) => ({ count, newest });
const refusal = (state) => ({ state, retired: false, fenced: false, count: 0, through: null, summary: null, withheld: null,
  entries: [], older: null });

const ESC = '\u{1b}';
/** Every hostile shape memory text can take, built from escapes so no invisible character sits in this file. */
const HOSTILE = [
  'MEMORY-SENTINEL <img src=x onerror=alert(1)>', '<script>alert(1)</script>', '[x](javascript:alert(1))', '&amp;',
  `${ESC}[31mred`, `${ESC}]52;c;aGk=\u{7}`, 'a\u{202e}b', 'c\u{200b}\u{200d}d', 'e\u{fe0f}f', 'g\u{e0100}h',
  'i\u{e0041}j', 'k\u{3164}l', 'm\u{e000}n', 'o\rp', 'tab\there', 'line\nnext',
].join(' ');
const SHOWN = [
  'MEMORY-SENTINEL <img src=x onerror=alert(1)>', '<script>alert(1)</script>', '[x](javascript:alert(1))', '&amp;',
  '\\u{1b}[31mred', '\\u{1b}]52;c;aGk=\\u{7}', 'a\\u{202e}b', 'c\\u{200b}\\u{200d}d', 'e\\u{fe0f}f', 'g\\u{e0100}h',
  'i\\u{e0041}j', 'k\\u{3164}l', 'm\\u{e000}n', 'o\\u{d}p', 'tab\there', 'line\nnext',
].join(' ');
const MARKERS = ['\\u{1b}', '\\u{1b}', '\\u{7}', '\\u{202e}', '\\u{200b}\\u{200d}', '\\u{fe0f}', '\\u{e0100}', '\\u{e0041}',
  '\\u{3164}', '\\u{e000}', '\\u{d}'];

let server;
let MemoryPanel;
let AgentDetail;
let AgentDrawer;
let ArchivedAgents;
let store;
let view;
let decodeMemoryPage;
let decodeMemoryEntry;
let memoryPacer;
before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-memory-tab'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ MemoryPanel } = await server.ssrLoadModule('/src/renderer/memory/MemoryPanel.tsx'));
  ({ decodeMemoryPage, decodeMemoryEntry, memoryPacer } = await server.ssrLoadModule('/src/renderer/memory/memory.ts'));
  ({ AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx'));
  ({ AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx'));
  ({ ArchivedAgents } = await server.ssrLoadModule('/src/renderer/ArchivedAgents.tsx'));
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  view = await server.ssrLoadModule('/src/renderer/view.ts');
});
after(async () => { await server?.close(); });

const ada = { id: ADA, name: 'Ada', role: 'engineer', provider: 'claude', status: 'idle', mode: 'managed' };
const grace = { id: GRACE, name: 'Grace', role: 'reviewer', provider: 'pi', hired_at: AT, retired_at: AT + 1, turns: 3,
  spend: { usd: 0, tokens: 0, work_tokens: 0 } };
const crew = { agents: [ada], retiredAgents: [grace] };

/** A daemon that answers each request from `reply(url, request)`, and the requests it saw. */
function daemon(reply) {
  const seen = [];
  const auth = { epoch: 1, session: 1, csrf: CSRF, refreshed: 0 };
  const deps = {
    fetch: async (url, init) => {
      seen.push({ url, init });
      const { status, text } = await reply(url, JSON.parse(init.body));
      return { status, text: async () => text };
    },
    csrf: () => auth.csrf,
    refresh: () => { auth.refreshed += 1; },
    epoch: () => auth.epoch,
    session: () => auth.session,
    pace: (send) => send(),
  };
  return { deps, seen, auth };
}
const ok = (value) => async () => ({ status: 200, text: typeof value === 'string' ? value : body(value) });
/** A reply that waits until `release()` is called. */
function held() {
  let release;
  const gate = new Promise((go) => { release = go; });
  return { gate, release: () => release() };
}

const elements = (node) => node.childNodes.flatMap((child) => (child.nodeType === 1 ? [child, ...elements(child)] : []));
const classed = (node, name) => elements(node).filter((element) => (element.getAttribute('class') ?? '').split(' ').includes(name));
function one(node, name) {
  const found = classed(node, name);
  assert.equal(found.length, 1, `exactly one .${name}`);
  return found[0];
}
const textOf = (node) => (node.nodeType === 3 ? node.nodeValue : node.childNodes.map(textOf).join(''));
const lsns = (node) => classed(node, 'memory-entry').map((element) => element.getAttribute('data-memory-lsn'));
const settle = () => act(async () => {
  for (let tick = 0; tick < 20; tick += 1) await new Promise((go) => { setImmediate(go); });
});

async function mount(props, component = MemoryPanel) {
  const dom = fakePage();
  const { createRoot } = await import('react-dom/client');
  const reactRoot = createRoot(dom.host);
  let current = props;
  const show = async (next = {}) => {
    current = { ...current, ...next };
    await act(async () => { reactRoot.render(createElement(component, current)); });
    await settle();
  };
  await show();
  return {
    host: dom.host,
    show,
    click: async (name) => {
      await act(async () => { press(one(dom.host, name)); });
      await settle();
    },
    close: async () => {
      await act(async () => { reactRoot.unmount(); });
      dom.restore();
    },
  };
}

test('the decoder keeps exactly the members of the daemon\'s page and entry replies', () => {
  assert.deepEqual(decodeMemoryPage(PIN, true), PINNED);
  assert.deepEqual(decodeMemoryPage(body(later({ through: '9', older: '7' })), false),
    { ...PINNED, through: '9', summary: null, withheld: null, older: '7' });
  assert.deepEqual(decodeMemoryEntry(body({ state: 'ok', entry: entry('10', { text: 'whole', len: 5 }) })),
    { state: 'ok', entry: { lsn: '10', at: AT, writer: 'agent', condensed: false, len: 5, text: 'whole', cut: false } });
  assert.deepEqual(decodeMemoryPage(body(refusal('agent_unknown')), true), { state: 'agent_unknown' });
  assert.deepEqual(decodeMemoryPage(body(refusal('busy')), false), { state: 'busy' });
  for (const state of ['agent_unknown', 'not_found', 'busy']) {
    assert.deepEqual(decodeMemoryEntry(body({ state, entry: null })), { state });
  }
  const decoded = (fields) => decodeMemoryPage(body(page(fields)), true);
  assert.deepEqual(['agent', 'operator', 'unrecorded'].map((writer) => decoded({ entries: [entry('10', { writer })] }).entries[0].writer),
    ['agent', 'operator', 'unrecorded']);
  assert.equal(decoded({ summary: { ...page().summary, kind: 'model' } }).summary.kind, 'model');
  const reasons = ['over_ceiling', 'provenance_mismatch', 'target_mismatch', 'authority_unavailable', 'role_mismatch', 'other'];
  assert.deepEqual(decoded({ withheld: withheld(reasons.map((reason, at) => marker({ lsn: String(20 - at), reason }))) })
    .withheld.newest.map((item) => item.reason), reasons);
  assert.deepEqual(decoded({ withheld: withheld([marker({ writer: { kind: 'agent', agent: GRACE } }),
    marker({ lsn: '10', writer: { kind: 'unresolved' } })], 30) }).withheld,
  { count: 30, newest: [marker({ writer: { kind: 'agent', agent: GRACE } }), marker({ lsn: '10', writer: { kind: 'unresolved' } })] });
  assert.deepEqual(decoded({ entries: [entry('18446744073709551615'), entry('0')], older: '0' }).entries.map((item) => item.lsn),
    ['18446744073709551615', '0']);
  const hidden = `a${ESC}[31m\u{202e}\u{200b}\u{e000}\u{0}\u{85}\u{2028}`;
  assert.equal(decoded({ entries: [entry('10', { text: hidden })] }).entries[0].text, hidden);
  assert.equal(decoded({ summary: { ...page().summary, text: hidden } }).summary.text, hidden);
});

test('the decoder refuses every reply the daemon never sends', () => {
  const bad = (what, value, first = true) => assert.equal(
    decodeMemoryPage(typeof value === 'string' ? value : body(value), first), null, what);
  bad('an unknown member', { ...page(), extra: 1 });
  bad('a missing member', (({ older: _, ...rest }) => rest)(page()));
  bad('an unknown member of an entry', page({ entries: [{ ...entry('10'), author: 'x' }] }));
  bad('a missing member of an entry', page({ entries: [(({ cut: _, ...rest }) => rest)(entry('10'))] }));
  bad('an unknown member of the summary', page({ summary: { ...page().summary, model: 'x' } }));
  bad('an unknown member of the withheld list', page({ withheld: { ...withheld([marker()]), oldest: [] } }));
  bad('an unknown state', page({ state: 'fine' }));
  bad('an unknown summary kind', page({ summary: { ...page().summary, kind: 'digest' } }));
  bad('an unknown reason', page({ withheld: withheld([marker({ reason: 'too_long' })]) }));
  bad('an unknown writer', page({ entries: [entry('10', { writer: 'model' })] }));
  bad('an unknown writer kind', page({ withheld: withheld([marker({ writer: { kind: 'daemon' } })]) }));
  bad('an agent writer without its agent', page({ withheld: withheld([marker({ writer: { kind: 'agent' } })]) }));
  bad('an agent writer with a malformed id', page({ withheld: withheld([marker({ writer: { kind: 'agent', agent: 'ada' } })]) }));
  bad('a writer kind with an extra member', page({ withheld: withheld([marker({ writer: { kind: 'unresolved', agent: ADA } })]) }));
  bad('a withheld marker with a text member', page({ withheld: withheld([marker({ text: 'the withheld words' })]) }));
  for (const lsn of ['01', '-1', '18446744073709551616', '1.0', '', 7]) {
    bad(`an entry at ${JSON.stringify(lsn)}`, page({ entries: [entry(lsn)] }));
    bad(`through ${JSON.stringify(lsn)}`, page({ through: lsn }));
    bad(`older ${JSON.stringify(lsn)}`, page({ older: lsn }));
    bad(`a summary at ${JSON.stringify(lsn)}`, page({ summary: { ...page().summary, lsn } }));
    bad(`a marker at ${JSON.stringify(lsn)}`, page({ withheld: withheld([marker({ lsn })]) }));
  }
  bad('a preview of 2,049 bytes', page({ entries: [entry('10', { text: 'x'.repeat(2_049) })] }));
  bad('a preview of 2,049 bytes in 2,047 code units', page({ entries: [entry('10', { text: `${'x'.repeat(2_045)}\u{1f600}` })] }));
  bad('a summary of 8,193 bytes', page({ summary: { ...page().summary, text: 'x'.repeat(8_193) } }));
  bad('33 entries', page({ entries: Array.from({ length: 33 }, (_, at) => entry(String(100 - at))) }));
  bad('9 markers', page({ withheld: withheld(Array.from({ length: 9 }, (_, at) => marker({ lsn: String(50 - at) }))) }));
  bad('entries oldest first', page({ entries: [entry('7'), entry('10')] }));
  bad('one position twice', page({ entries: [entry('10'), entry('10')] }));
  bad('a non-ok state carrying entries', { ...refusal('busy'), entries: [entry('10')] });
  bad('a non-ok state carrying a count', { ...refusal('agent_unknown'), count: 1 });
  bad('a non-ok state carrying a withheld list', { ...refusal('busy'), withheld: withheld([]) });
  bad('a first page without its withheld list', page({ withheld: null }));
  bad('a later page with a summary', later({ summary: page().summary }), false);
  bad('a later page with a withheld list', later({ withheld: page().withheld }), false);
  bad('a page body over 131,072 bytes', `${PIN}${' '.repeat(131_072)}`);
  bad('a negative count', page({ count: -1 }));
  bad('a fractional time', page({ entries: [entry('10', { at: 1.5 })] }));
  bad('not JSON', 'nope');
  bad('an array', '[]');
  const badEntry = (what, value) => assert.equal(decodeMemoryEntry(typeof value === 'string' ? value : body(value)), null, what);
  badEntry('a whole entry of 65,537 bytes', { state: 'ok', entry: entry('10', { text: 'x'.repeat(65_537) }) });
  badEntry('ok without its entry', { state: 'ok', entry: null });
  badEntry('a refusal carrying an entry', { state: 'not_found', entry: entry('10') });
  badEntry('an unknown state', { state: 'gone', entry: null });
  badEntry('an unknown member', { state: 'busy', entry: null, extra: 1 });
  badEntry('a non-canonical position', { state: 'ok', entry: entry('01') });
  badEntry('an entry reply over 409,600 bytes', `${body({ state: 'busy', entry: null })}${' '.repeat(409_600)}`);
  // Each bound admits its own size.
  assert.equal(decodeMemoryPage(body(page({ entries: [entry('10', { text: 'x'.repeat(2_048) })] })), true)
    .entries[0].text.length, 2_048);
  assert.equal(decodeMemoryPage(body(page({ summary: { ...page().summary, text: 'x'.repeat(8_192) } })), true)
    .summary.text.length, 8_192);
  assert.equal(decodeMemoryEntry(body({ state: 'ok', entry: entry('10', { text: 'x'.repeat(65_536) }) })).entry.text.length, 65_536);
  assert.equal(decodeMemoryPage(body(page({ entries: Array.from({ length: 32 }, (_, at) => entry(String(100 - at))) })), true)
    .entries.length, 32);
  assert.equal(decodeMemoryPage(body(page({ withheld: withheld(Array.from({ length: 8 }, (_, at) => marker({ lsn: String(50 - at) })), 9) })), true)
    .withheld.newest.length, 8);
});

test('the panel shows a read error for a refused reply and keeps nothing from it', async () => {
  const UNREADABLE = 'The daemon sent a reply this page could not read.';
  for (const [what, reply, sentence] of [
    ['an unknown member', ok({ ...page(), extra: 'refused-SENTINEL' }), UNREADABLE],
    ['a marker with text', ok(page({ withheld: withheld([marker({ text: 'refused-SENTINEL' })]) })), UNREADABLE],
    ['a non-ok state with entries', ok({ ...refusal('busy'), entries: [entry('10', { text: 'refused-SENTINEL' })] }), UNREADABLE],
    ['400', async () => ({ status: 400, text: '' }), UNREADABLE],
    ['413', async () => ({ status: 413, text: '' }), UNREADABLE],
    ['500', async () => ({ status: 500, text: PIN }), UNREADABLE],
    ['a network failure', async () => { throw new Error('offline'); }, UNREADABLE],
    ['503', async () => ({ status: 503, text: '' }), 'The daemon did not answer.'],
  ]) {
    const { deps, seen } = daemon(reply);
    const panel = await mount({ agent: ADA, active: true, crew, deps });
    assert.equal(seen.length, 1, what);
    assert.equal(textOf(one(panel.host, 'memory-status')), sentence, what);
    assert.deepEqual(lsns(panel.host), [], what);
    assert.ok(!textOf(panel.host).includes('SENTINEL'), what);
    await panel.close();
  }
});

/** The `overflow` and `contain` declarations of every top-level rule whose selector list names `selector` exactly. */
function clipsOf(css, selector) {
  const rules = [...css.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/([^{}]+)\{([^{}]*)\}/g)];
  return rules.filter(([, selectors]) => selectors.split(',').map((part) => part.trim()).includes(selector))
    .flatMap(([, , body]) => [...body.matchAll(/(?:^|;)\s*(overflow|contain)\s*:\s*([^;]+)/g)]
      .map(([, property, value]) => `${property}: ${value.trim()}`));
}

test('every memory text clips what it draws, so stacked marks cannot paint over its header', async () => {
  const css = await readFile(resolve(root, 'src/style.css'), 'utf8');
  assert.deepEqual(clipsOf(css, '.memory-text'), ['overflow: hidden'], 'an entry preview');
  assert.deepEqual(clipsOf(css, '.memory-summary .memory-text'), ['overflow: auto'], 'the summary scrolls within its bound');
  assert.deepEqual(clipsOf(css, '.memory-whole-text'), ['overflow: auto'], 'a whole entry scrolls within its bound');
});

test('hostile text renders inert, with each hidden run drawn as one marker', async () => {
  const replies = {
    page: page({ summary: { ...page().summary, text: HOSTILE },
      entries: [entry('10', { text: HOSTILE }), entry('7', { text: 'preview', len: 9_000, cut: true })] }),
    entry: { state: 'ok', entry: entry('7', { text: HOSTILE, len: 9_000 }) },
  };
  const { deps } = daemon(async (url) => ({ status: 200, text: body(url.endsWith('entry') ? replies.entry : replies.page) }));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  await panel.click('memory-whole');
  const texts = classed(panel.host, 'memory-text');
  assert.deepEqual(texts.map(textOf), [SHOWN, SHOWN, SHOWN], 'the summary, an entry and a whole entry');
  for (const text of texts) {
    assert.deepEqual(classed(text, 'memory-marker').map(textOf), MARKERS);
    assert.deepEqual([...text.attributes], [['class', 'memory-text'], ['dir', 'ltr']]);
  }
  assert.equal(textOf(one(panel.host, 'memory-whole-text')), SHOWN);
  const tags = new Set(elements(panel.host).map((element) => element.tagName));
  for (const tag of ['A', 'IMG', 'SCRIPT', 'STYLE', 'IFRAME']) assert.ok(!tags.has(tag), `a ${tag} element was built`);
  const attributes = elements(panel.host).flatMap((element) => [...element.attributes.values()]);
  assert.ok(attributes.length > 20);
  assert.deepEqual(attributes.filter((value) => /SENTINEL|alert|script|aGk|31m|preview/.test(value)), [],
    'memory text reached an attribute');
  await panel.close();
});

test('the first page shows the fixed heading, the summary, each entry and the withheld marker in fixed words', async () => {
  const { deps } = daemon(ok(PIN));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  const host = panel.host;
  assert.ok(textOf(host).startsWith(`Memory${HEADING}Refresh`));
  assert.equal(textOf(one(host, 'memory-status')), '');
  const summary = textOf(one(host, 'memory-summary'));
  assert.match(summary, /^SummaryExact excerpts of the memory, chosen by the daemon\. It covers 1 section and 5 bytes of memory\. Recorded .*2026.* · segment 0 · byte 9\.\[Condensed memory: exact excerpts; 0 sections omitted\]\n\[append 7 bytes 0\.\.5\]\nfirst$/);
  assert.deepEqual(lsns(host), ['10', '7']);
  const [newer, older] = classed(host, 'memory-entry');
  assert.match(textOf(newer), /2026.* · segment 0 · byte 10 · This agentsecond$/);
  assert.match(textOf(older), /2026.* · segment 0 · byte 7 · Operator Condensedfirst$/);
  assert.equal(textOf(one(older, 'memory-badge')), 'Condensed');
  assert.equal(classed(newer, 'memory-badge').length, 0);
  const refused = one(host, 'memory-withheld');
  assert.ok(textOf(refused).startsWith('Refused appends1 append to this memory was refused. The daemon kept none of the text.'));
  const sealed = one(refused, 'memory-withheld-marker');
  assert.match(textOf(sealed), /2026.* · segment 0 · byte 11 · A process outside every agent · 3 bytes/);
  const sentence = sealed.childNodes.at(-1);
  assert.equal(textOf(sentence), SEALED);
  assert.deepEqual(elements(sentence).map((element) => [element.tagName, textOf(element)]), []);
  assert.match(textOf(host), /2 entries in all\./);
  assert.equal(classed(host, 'memory-older').length, 0, 'the oldest entry is shown');
  assert.equal(classed(host, 'memory-whole').length, 0, 'no preview is cut');
  await panel.close();
});

test('each writer, reason and state reads its own fixed words', async () => {
  const markers = [
    marker({ lsn: '30', writer: { kind: 'agent', agent: ADA }, reason: 'over_ceiling', len: 65_537 }),
    marker({ lsn: '29', writer: { kind: 'agent', agent: GRACE }, reason: 'target_mismatch' }),
    marker({ lsn: '28', writer: { kind: 'agent', agent: STRANGER }, reason: 'provenance_mismatch' }),
    marker({ lsn: '27', writer: { kind: 'unresolved' }, reason: 'role_mismatch' }),
    marker({ lsn: '26', writer: { kind: 'outside_agents' }, reason: 'other' }),
    marker({ lsn: '25', writer: { kind: 'agent', agent: ADA }, reason: 'authority_unavailable' }),
  ];
  const { deps } = daemon(ok(page({ retired: true, fenced: true, withheld: withheld(markers, 12),
    summary: { ...page().summary, kind: 'model', sections: 3, bytes: 1_200 },
    entries: [entry('10', { writer: 'unrecorded' })] })));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  const host = panel.host;
  assert.deepEqual(classed(host, 'memory-notice').map(textOf), [
    'This agent has retired. Its memory is read from the journal.',
    'The daemon stopped memory work after an integrity failure. The summary may be out of date until the daemon restarts.',
  ]);
  assert.match(textOf(one(host, 'memory-summary')),
    /^SummarySummary written by a model\. It is a model’s prose about the memory, not the memory itself\. It covers 3 sections and 1,200 bytes of memory\./);
  assert.match(textOf(one(host, 'memory-entry')), / · segment 0 · byte 10 · Writer not recordede10$/);
  assert.ok(textOf(one(host, 'memory-withheld')).startsWith(
    'Refused appends12 appends to this memory were refused. The daemon kept none of the text. Only the newest 6 are listed.'));
  assert.deepEqual(classed(host, 'memory-withheld-marker').map((item) => [
    textOf(item.childNodes[0]).replace(/^.* · segment 0 · byte /, ''), textOf(item.childNodes[1])]), [
    ['30 · This agent · 65,537 bytes', 'Refused: the text was longer than one memory append may be.'],
    ['29 · Another agent, Grace · 3 bytes', 'Refused: the writer tried to write another agent’s memory.'],
    [`28 · Another agent, ${STRANGER} · 3 bytes`, 'Refused: the connection did not come from the process allowed to write this memory.'],
    ['27 · A process whose origin could not be resolved · 3 bytes', 'Refused: the daemon could not prove who sent it.'],
    ['26 · A process outside every agent · 3 bytes', 'Refused for a reason this page does not name.'],
    ['25 · This agent · 3 bytes', 'Refused: the operator’s authority to write memory was not available.'],
  ]);
  await panel.close();
});

test('the first activation sends one page request with before null, and an inactive tab sends nothing', async () => {
  const { deps, seen } = daemon(ok(PIN));
  const panel = await mount({ agent: ADA, active: false, crew, deps });
  assert.equal(seen.length, 0, 'a hidden tab reads nothing');
  assert.deepEqual(lsns(panel.host), []);
  await panel.show({ active: true });
  assert.equal(seen.length, 1);
  assert.equal(seen[0].url, '/floor/memory/page');
  assert.deepEqual(seen[0].init, {
    method: 'POST', credentials: 'same-origin', cache: 'no-store', referrerPolicy: 'no-referrer',
    headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': CSRF },
    body: `{"agent_id":"${ADA}","before":null}`,
  });
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  await panel.show({ active: false });
  await panel.show({ active: true });
  assert.equal(seen.length, 1, 'showing the tab again reads nothing');
  await panel.close();
});

test('Load older entries sends the previous reply\'s older position and appends the next page', async () => {
  const pages = { null: page({ entries: [entry('10'), entry('7')], older: '7' }), 7: later({ entries: [entry('5'), entry('3')] }) };
  const { deps, seen } = daemon(async (_, request) => ({ status: 200, text: body(pages[request.before]) }));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  await panel.click('memory-older');
  assert.equal(seen.length, 2);
  assert.equal(seen[1].url, '/floor/memory/page');
  assert.equal(seen[1].init.body, `{"agent_id":"${ADA}","before":"7"}`);
  assert.deepEqual(lsns(panel.host), ['10', '7', '5', '3']);
  assert.equal(classed(panel.host, 'memory-older').length, 0, 'the oldest entry is shown');
  assert.equal(classed(panel.host, 'memory-summary').length, 1, 'the first page\'s summary stays');
  assert.equal(classed(panel.host, 'memory-notice').length, 0, 'the same through says nothing');
  await panel.close();
});

test('a changed through on a later page says memory was condensed, and Refresh reads the newest page again', async () => {
  const pages = { null: page({ entries: [entry('10'), entry('7')], older: '7' }), 7: later({ through: '9', entries: [entry('5')] }) };
  const { deps, seen } = daemon(async (_, request) => ({ status: 200, text: body(pages[request.before]) }));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  await panel.click('memory-older');
  assert.deepEqual(classed(panel.host, 'memory-notice').map(textOf),
    ['Memory was condensed after this tab opened. Press Refresh to read the new summary.']);
  await panel.click('memory-refresh');
  assert.equal(seen[2].init.body, `{"agent_id":"${ADA}","before":null}`);
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  assert.deepEqual(classed(panel.host, 'memory-notice'), []);
  await panel.close();
});

test('Read whole entry sends one entry request for that position and shows the whole text', async () => {
  const replies = {
    page: page({ entries: [entry('10', { text: 'the start', len: 3_000, cut: true }), entry('7')] }),
    entry: { state: 'ok', entry: entry('10', { text: 'the start and the whole rest', len: 3_000 }) },
  };
  const { deps, seen } = daemon(async (url) => ({ status: 200, text: body(url.endsWith('entry') ? replies.entry : replies.page) }));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  const [cut] = classed(panel.host, 'memory-entry');
  assert.match(textOf(cut), /the startA preview of an entry of 3,000 bytes\. Read whole entry$/);
  await panel.click('memory-whole');
  assert.equal(seen.length, 2);
  assert.equal(seen[1].url, '/floor/memory/entry');
  assert.equal(seen[1].init.body, `{"agent_id":"${ADA}","lsn":"10"}`);
  assert.equal(textOf(one(panel.host, 'memory-whole-text')), 'the start and the whole rest');
  assert.equal(classed(panel.host, 'memory-whole').length, 0);
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  await panel.close();
});

test('a busy reply shows the retry notice, and Try again sends the same request', async () => {
  let replies = [refusal('busy'), PIN];
  const { deps, seen } = daemon(async () => ({ status: 200, text: (() => {
    const [next, ...rest] = replies;
    replies = rest;
    return typeof next === 'string' ? next : body(next);
  })() }));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  assert.equal(textOf(one(panel.host, 'memory-status')), 'The daemon is busy, or this page read memory a moment ago. Try again');
  await panel.click('memory-retry');
  assert.equal(seen.length, 2);
  assert.equal(seen[1].init.body, seen[0].init.body);
  assert.equal(textOf(one(panel.host, 'memory-status')), '');
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  await panel.close();
});

test('the request controls are disabled while a request is in flight, and pressing one sends nothing', async () => {
  const hold = held();
  let calls = 0;
  const { deps, seen } = daemon(async () => {
    calls += 1;
    if (calls === 2) await hold.gate;
    return { status: 200, text: body(page({ entries: [entry('10', { cut: true, len: 3_000 })], older: '10' })) };
  });
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  const disabled = () => ['memory-refresh', 'memory-older', 'memory-whole']
    .map((name) => one(panel.host, name).getAttribute('aria-disabled'));
  assert.deepEqual(disabled(), [null, null, null]);
  await panel.click('memory-refresh');
  assert.equal(textOf(one(panel.host, 'memory-status')), 'Reading memory…');
  assert.deepEqual(disabled(), ['true', 'true', 'true']);
  await panel.click('memory-older');
  await panel.click('memory-whole');
  await panel.click('memory-refresh');
  assert.equal(seen.length, 2, 'one request in flight');
  hold.release();
  await settle();
  assert.deepEqual(disabled(), [null, null, null]);
  await panel.close();
});

test('the next request waits until 250 ms after the previous reply came back, a failed one included', async () => {
  let clock = 1_000;
  let status = 200;
  const sleeps = [];
  let wake = () => {};
  const { deps, seen } = daemon(async () => {
    clock += 40;
    return { status, text: status === 200 ? PIN : '' };
  });
  deps.pace = memoryPacer(() => clock, (ms) => {
    sleeps.push(ms);
    return new Promise((go) => { wake = go; });
  });
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  assert.equal(seen.length, 1, 'sent at 1,000, answered at 1,040');
  clock = 1_100;
  await panel.click('memory-refresh');
  assert.deepEqual(sleeps, [190], 'the wait counts from the reply at 1,040, not the send at 1,000');
  assert.equal(seen.length, 1, 'nothing is sent before the interval ends');
  assert.equal(one(panel.host, 'memory-refresh').getAttribute('aria-disabled'), 'true');
  clock = 1_290;
  wake();
  await settle();
  assert.equal(seen.length, 2);
  status = 503;
  clock = 2_000;
  await panel.click('memory-refresh');
  assert.deepEqual(sleeps, [190], 'a request after the interval does not wait');
  assert.equal(seen.length, 3, 'sent at 2,000, failed at 2,040');
  assert.equal(textOf(one(panel.host, 'memory-status')), 'The daemon did not answer.');
  status = 200;
  clock = 2_200;
  await panel.click('memory-refresh');
  assert.deepEqual(sleeps, [190, 90], 'a failed reply starts the interval too');
  assert.equal(seen.length, 3);
  wake();
  await settle();
  assert.equal(seen.length, 4);
  await panel.close();
});

test('changing the agent clears the panel and reads the new agent\'s memory', async () => {
  const { deps, seen } = daemon(async (_, request) => ({ status: 200, text: body(page({
    entries: [entry('10', { text: request.agent_id === ADA ? 'ada-SENTINEL' : 'grace notes' })] })) }));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  assert.ok(textOf(panel.host).includes('ada-SENTINEL'));
  await panel.show({ agent: GRACE });
  assert.ok(!textOf(panel.host).includes('ada-SENTINEL'));
  assert.equal(seen[1].init.body, `{"agent_id":"${GRACE}","before":null}`);
  assert.ok(textOf(panel.host).includes('grace notes'));
  await panel.close();
});

test('an epoch change clears the panel, reads nothing by itself, and drops a reply already in flight', async () => {
  const hold = held();
  let calls = 0;
  const { deps, seen, auth } = daemon(async () => {
    calls += 1;
    if (calls === 2) await hold.gate;
    return { status: 200, text: PIN };
  });
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  auth.epoch = 2;
  await panel.show();
  assert.deepEqual(lsns(panel.host), []);
  assert.equal(textOf(one(panel.host, 'memory-status')),
    'The connection to the daemon changed, so the memory shown was cleared. Press Refresh to read it again.');
  assert.equal(seen.length, 1, 'the new epoch reads nothing by itself');
  await panel.click('memory-refresh');
  assert.equal(seen.length, 2);
  auth.epoch = 3;
  hold.release();
  await settle();
  assert.deepEqual(lsns(panel.host), [], 'a reply for the earlier epoch is not shown');
  await panel.close();
});

test('losing authentication clears the panel: a 403, no session, or a new session', async () => {
  let status = 200;
  const { deps, seen, auth } = daemon(async () => ({ status, text: status === 200 ? PIN : '' }));
  const panel = await mount({ agent: ADA, active: true, crew, deps });
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  status = 403;
  await panel.click('memory-refresh');
  assert.deepEqual(lsns(panel.host), []);
  assert.equal(textOf(one(panel.host, 'memory-status')),
    'The daemon did not accept this page’s session, so the memory shown was cleared.');
  assert.equal(auth.refreshed, 1, 'a 403 asks the stream for a fresh snapshot');

  status = 200;
  await panel.click('memory-refresh');
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  auth.csrf = null;
  await panel.click('memory-refresh');
  assert.deepEqual(lsns(panel.host), []);
  assert.equal(textOf(one(panel.host, 'memory-status')), 'This page has no Floor session, so nothing was sent.');
  assert.equal(seen.length, 3, 'nothing is sent without a session');

  auth.csrf = CSRF;
  await panel.click('memory-refresh');
  assert.deepEqual(lsns(panel.host), ['10', '7']);
  auth.session = 2;
  await panel.show();
  assert.deepEqual(lsns(panel.host), []);
  assert.equal(seen.length, 4);
  await panel.close();
});

test('the Memory tab sits after Git, and the drawer always offers it for a live agent', () => {
  const tabs = (html) => [...html.matchAll(/role="tab" class="agent-detail-tab"[^>]*>([^<]+)<\/button>/g)].map((match) => match[1]);
  const panel = (label) => createElement('p', null, label);
  assert.deepEqual(tabs(renderToStaticMarkup(createElement(AgentDetail, { row: ada, git: panel('Git'), memory: panel('Memory'),
    messages: panel('Messages') }))), ['Overview', 'Git', 'Memory', 'Messages']);
  const drawer = (git) => tabs(renderToStaticMarkup(createElement(AgentDrawer, {
    floor: { ...store.getFloorState(), phase: 'live', stale: false, agents: [ada], git },
    row: ada, selectedId: ADA, onClose() {}, leaveFocusTo: 'crew-toggle',
  })));
  assert.deepEqual(drawer(false).filter((tab) => ['Git', 'Memory'].includes(tab)), ['Memory']);
  assert.deepEqual(drawer(true).filter((tab) => ['Git', 'Memory'].includes(tab)), ['Git', 'Memory']);
  const values = new Map();
  const storage = () => ({ getItem: (key) => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) });
  view.writeDetailTab('memory', storage);
  assert.equal(view.readDetailTab(storage), 'memory', 'the chosen tab is remembered');
});

test('an archive row\'s Memory disclosure renders the panel only while it is open', async () => {
  const floor = { ...store.getFloorState(), phase: 'live', stale: false, retiredAgents: [grace] };
  const archive = await mount({ floor }, ArchivedAgents);
  const [disclosure, ...others] = classed(archive.host, 'archive-memory');
  assert.deepEqual(others, []);
  assert.equal(disclosure.tagName, 'DETAILS');
  assert.equal(textOf(disclosure.childNodes[0]), 'Memory');
  assert.deepEqual(classed(disclosure, 'memory-panel'), []);
  const toggle = async (open) => {
    disclosure.open = open;
    const props = disclosure[Object.keys(disclosure).find((key) => key.startsWith('__reactProps$'))];
    await act(async () => { props.onToggle({ currentTarget: disclosure, target: disclosure }); });
    await settle();
  };
  await toggle(true);
  const opened = one(disclosure, 'memory-panel');
  assert.equal(textOf(one(opened, 'memory-status')), 'This page has no Floor session, so nothing was sent.',
    'the open panel tried its first read');
  await toggle(false);
  assert.deepEqual(classed(disclosure, 'memory-panel'), []);
  await archive.close();
});

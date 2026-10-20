// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Mutation controls, each named by the test it turns red:
//   - the close button only inside Live work ......... "the drawer's close button stays in the detail header…"
//   - the drawer drawn with no selection standing ..... "the drawer's close button stays in the detail header…"
//   - the waiting question back in Overview ........... "a waiting question and its answer form…"
//   - the stream status back inside Live work ......... "the stream status is one live region…"
//   - no terminal hint in the header action ........... "with the terminal switch off…"
//   - a switched-off hint left outside the disclosure .. "the switches that are off fold into one Enable more controls…"
//   - the bare "Ended" or "Live" stream word back ...... "the header names the turn an output state is about…"
//   - the row scrolled to the tab's start every time ... "a selected tab past the edge of the scrolling tab row…"
//   - the drawer's scroll-into-view effect deleted ..... "the drawer scrolls its tab row so the selected tab shows…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { act, createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { fakePage } from './helpers/fake-page.mjs';

let server;
let AgentDetail;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx'));
});
after(async () => { await server?.close(); });
const row = { id: '01ARZ3NDEKTSV4RRFFQ69G5FAV', name: 'Ada', role: 'Builder',
  provider: 'pi', status: 'idle', mode: 'managed' };
const git = createElement('p', null, 'Git details');
const render = (props = {}) => renderToStaticMarkup(createElement(AgentDetail, { row, ...props }));
const tabs = html => [...html.matchAll(/role="tab" class="agent-detail-tab"[^>]*>([^<]+)<\/button>/g)].map(match => match[1]);
const header = html => html.match(/<header class="agent-detail-head">([\s\S]*?)<\/header>/)?.[1] ?? '';
const panel = (html, id) => html.match(
  new RegExp(`<section id="agent-detail-panel-${id}"[\\s\\S]*?(?=<section id="agent-detail-panel-|</aside>)`))?.[0] ?? '';
const CLOSE = /<button type="button" id="selection-clear" class="drawer-close" aria-label="Close agent details"[^>]*>×<\/button>/;
const STREAM_STATE = /<(?:p|span) class="stream-state" role="status">([^<]*)<\/(?:p|span)>/g;

async function drawer() {
  const { AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx');
  const { getFloorState } = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  const base = getFloorState();
  const output = { ...base.output, targetId: row.id, status: 'waiting', reason: 'unbound' };
  const floor = { ...base, phase: 'live', stale: false, agents: [row], output };
  const center = (patch = {}, selected = row) => renderToStaticMarkup(createElement(AgentDrawer, {
    floor: { ...floor, ...patch }, row: selected, selectedId: selected?.id ?? null, onClose() {}, leaveFocusTo: 'crew-toggle',
  }));
  return { center, output };
}

test('only available detail tabs appear and Terminal is never one of them', () => {
  assert.deepEqual(tabs(render()), ['Overview']);
  const all = render({ git });
  assert.deepEqual(tabs(all), ['Overview', 'Git']);
  assert.doesNotMatch(all, /role="tab"[^>]*>Messages/);
  const terminalOff = render({ git });
  assert.deepEqual(tabs(terminalOff), ['Overview', 'Git']);
  assert.doesNotMatch(all, /Traces/);
});

test('Chat is an optional detail panel with the existing reciprocal tab links', () => {
  const on = render({ tab: 'chat', chat: createElement('p', null, 'Durable Chat thread') });
  assert.deepEqual(tabs(on), ['Overview', 'Chat']);
  assert.match(on, /id="agent-detail-tab-chat"[^>]*aria-controls="agent-detail-panel-chat"[^>]*aria-selected="true"/);
  assert.match(on, /id="agent-detail-panel-chat"[^>]*role="tabpanel"[^>]*aria-labelledby="agent-detail-tab-chat"/);
  assert.doesNotMatch(render({ tab: 'chat' }), /Durable Chat thread|agent-detail-tab-chat/);
  assert.match(render({ tab: 'chat' }), /id="agent-detail-tab-overview"[^>]*aria-selected="true"/);
});

test('panels have reciprocal ARIA links, one selected tab and mounted hidden drafts', () => {
  const html = render({ tab: 'git', git,
    goal: createElement('textarea', { defaultValue: 'Unsubmitted goal draft', 'aria-label': 'Goal draft' }) });
  assert.match(html, /role="tablist" aria-label="Agent detail sections"/);
  assert.match(html, /id="agent-detail-tab-git"[^>]*aria-controls="agent-detail-panel-git"[^>]*aria-selected="true"[^>]*tabindex="0"/);
  assert.match(html, /id="agent-detail-panel-git"[^>]*role="tabpanel"[^>]*aria-labelledby="agent-detail-tab-git"/);
  assert.match(html, /id="agent-detail-panel-overview"[^>]*hidden=""/);
  assert.match(html, /Unsubmitted goal draft/);
  assert.equal((html.match(/aria-selected="true"/g) ?? []).length, 1);
  assert.equal((html.match(/role="tab"[^>]*tabindex="0"/g) ?? []).length, 1);
  assert.deepEqual(tabs(render({ tab: 'memory' })), ['Overview']);
  assert.match(render({ tab: 'memory' }), /id="agent-detail-tab-overview"[^>]*aria-selected="true"/);
});

test('the header retains identity and actions outside every panel', () => {
  const html = render({ control: createElement('button', null, 'Pause'),
    retire: createElement('button', null, 'Retire'), terminal: createElement('button', null, 'Attach') });
  const header = html.match(/<header class="agent-detail-head">([\s\S]*?)<\/header>/)?.[1];
  assert.ok(header);
  for (const exact of ['Ada', 'Builder', 'idle', 'Pause', 'Retire', 'Attach', 'agent-detail-portrait']) {
    assert.ok(header.includes(exact), `header omitted ${exact}`);
  }
});

test('horizontal tab keys wrap and support Home and End without consuming scrolling keys', async () => {
  const { detailTabTarget } = await server.ssrLoadModule('/src/renderer/detailTabs.ts');
  const available = ['overview', 'chat', 'memory', 'git'];
  assert.equal(detailTabTarget(available, 'overview', 'ArrowLeft'), 'git');
  assert.equal(detailTabTarget(available, 'git', 'ArrowRight'), 'overview');
  assert.equal(detailTabTarget(available, 'chat', 'ArrowRight'), 'memory');
  assert.equal(detailTabTarget(available, 'chat', 'Home'), 'overview');
  assert.equal(detailTabTarget(available, 'chat', 'End'), 'git');
  for (const key of ['ArrowUp', 'ArrowDown', 'Tab', 'Enter', ' ']) {
    assert.equal(detailTabTarget(available, 'chat', key), null);
  }
});

test('a selected tab past the edge of the scrolling tab row scrolls just into view', async () => {
  const { revealedTabScroll } = await server.ssrLoadModule('/src/renderer/detailTabs.ts');
  assert.equal(revealedTabScroll(0, 314, 75, 340), 49, 'Messages past the right edge of a 340 pixel row');
  assert.equal(revealedTabScroll(49, 0, 72, 340), 0, 'Overview past the left edge');
  assert.equal(revealedTabScroll(20, 120, 60, 340), 20, 'a tab already in view moves nothing');
  assert.equal(revealedTabScroll(0, 200, 400, 340), 200, 'a tab wider than the row shows its start');
});

test('multiple detail sections show a scroll hint linked to their tab row', () => {
  const html = render({ git });
  assert.match(html, /role="tablist"[^>]*aria-describedby="agent-detail-tab-hint"/);
  assert.match(html, /<p id="agent-detail-tab-hint"[^>]*>Scroll for more sections/);
  assert.doesNotMatch(render(), /id="agent-detail-tab-hint"/);
});

test('a vertical wheel scrolls overflowing detail tabs and leaves browser zoom and native horizontal scrolling alone', async () => {
  const page = fakePage({ clientWidth: element => element.getAttribute('role') === 'tablist' ? 200 : 0 });
  const prototype = page.host.constructor.prototype;
  const add = prototype.addEventListener;
  const remove = prototype.removeEventListener;
  const listeners = new WeakMap();
  prototype.addEventListener = function (type, callback, options) {
    if (type === 'wheel') listeners.set(this, callback);
    else add.call(this, type, callback, options);
  };
  prototype.removeEventListener = function (type, callback, options) {
    if (type === 'wheel') listeners.delete(this);
    else remove.call(this, type, callback, options);
  };
  try {
    const { createRoot } = await import('react-dom/client');
    const root = createRoot(page.host);
    const panel = createElement('p', null, 'panel');
    await act(() => root.render(createElement(AgentDetail, { row, git: panel, messages: panel })));
    const strip = page.host.querySelector('[role="tablist"]');
    strip.scrollWidth = 480;
    const wheel = (patch = {}) => {
      let prevented = false;
      listeners.get(strip)?.({ deltaX: 0, deltaY: 120, deltaMode: 0, altKey: false, ctrlKey: false, metaKey: false, shiftKey: false,
        preventDefault() { prevented = true; }, ...patch });
      return prevented;
    };
    assert.equal(wheel(), true);
    assert.equal(strip.scrollLeft, 120);
    assert.equal(wheel({ deltaY: -40 }), true);
    assert.equal(strip.scrollLeft, 80);
    assert.equal(wheel({ ctrlKey: true }), false);
    assert.equal(wheel({ deltaX: 30 }), false);
    assert.equal(wheel({ shiftKey: true }), false);
    assert.equal(wheel({ altKey: true }), false);
    assert.equal(wheel({ metaKey: true }), false);
    assert.equal(strip.scrollLeft, 80);
    assert.equal(wheel({ deltaMode: 1, deltaY: 3 }), true);
    assert.equal(strip.scrollLeft, 128);
    assert.equal(wheel({ deltaMode: 2, deltaY: -1 }), true);
    assert.equal(strip.scrollLeft, 0);
    assert.equal(wheel({ deltaY: 1000 }), true);
    assert.equal(strip.scrollLeft, 280);
    assert.equal(wheel(), false);
    strip.scrollWidth = 200;
    strip.scrollLeft = 0;
    assert.equal(wheel(), false);
    assert.equal(strip.scrollLeft, 0);
    await act(() => root.unmount());
  } finally {
    prototype.addEventListener = add;
    prototype.removeEventListener = remove;
    page.restore();
  }
});

test('the drawer scrolls its tab row so the selected tab shows, and again when the selection moves', async () => {
  const isRow = (element) => element.getAttribute('role') === 'tablist';
  const page = fakePage({
    clientWidth: (element) => (isRow(element) ? 200 : 0),
    offsetWidth: (element) => (element.getAttribute('role') === 'tab' ? 80 : 0),
    rect: (element) => {
      const row = element.parentNode;
      const left = isRow(element) ? 0 : row !== null && isRow(row) ? row.childNodes.indexOf(element) * 80 - row.scrollLeft : 0;
      return { left, right: left + (isRow(element) ? 200 : 80), width: isRow(element) ? 200 : 80, top: 0, bottom: 30, height: 30 };
    },
  });
  try {
    const { createRoot } = await import('react-dom/client');
    const root = createRoot(page.host);
    const panel = createElement('p', null, 'panel');
    const show = (tab) => act(() => root.render(createElement(AgentDetail, { row, tab,
      chat: panel, git: panel, messages: panel })));
    const strip = () => page.host.querySelector('[role="tablist"]');
    await show('messages');
    assert.equal(strip().childNodes.length, 4);
    assert.equal(strip().scrollLeft, 120, 'the fourth tab, 240 to 320 pixels along a 200 pixel row, is out of view');
    await show('chat');
    assert.equal(strip().scrollLeft, 80, 'the second tab is out of view to the left');
    await act(() => root.unmount());
  } finally {
    page.restore();
  }
});

test('a browser tab preference survives switching agents and a temporary missing panel', async () => {
  const { readDetailTab, writeDetailTab } = await server.ssrLoadModule('/src/renderer/view.ts');
  const values = new Map();
  const storage = () => ({ getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) });
  writeDetailTab('git', storage);
  assert.equal(readDetailTab(storage), 'git');
  assert.match(render({ tab: readDetailTab(storage), git }), /id="agent-detail-tab-git"[^>]*aria-selected="true"/);
  assert.match(render({ row: { ...row, id: '01ARZ3NDEKTSV4RRFFQ69G5FAX', name: 'Grace' }, tab: readDetailTab(storage) }), /id="agent-detail-tab-overview"[^>]*aria-selected="true"/);
  assert.match(render({ tab: readDetailTab(storage), git }), /id="agent-detail-tab-git"[^>]*aria-selected="true"/);
  for (const retired of ['traces', 'output', 'terminal']) {
    values.set('dudes.floor.agent-detail-tab', retired);
    assert.equal(readDetailTab(storage), 'overview', `a remembered ${retired} tab opens Overview`);
  }
  for (const raw of ['{', '', 'x'.repeat(1000)]) {
    assert.equal(readDetailTab(() => ({ getItem: () => raw })), 'overview');
  }
  assert.equal(readDetailTab(() => { throw Error('refused'); }), 'overview');
  assert.doesNotThrow(() => writeDetailTab('overview', () => { throw Error('refused'); }));
});


test('the drawer never offers an Output or Terminal tab, and Attach follows the terminal capability switch', async () => {
  const { center: drawn, output } = await drawer();
  const floor = { output };
  const center = patch => drawn(patch);
  const attach = html => (html.match(/class="terminal-attach"/g) ?? []).length;
  assert.deepEqual(tabs(center({})), ['Overview', 'Memory']);
  assert.equal(attach(center({})), 0);
  assert.doesNotMatch(center({}), /Show full output/, 'an agent with no output has nothing to open');
  const enabled = center({ terminal: true, output: { ...floor.output, status: 'live', chunks: ['Actual assistant output'] } });
  assert.deepEqual(tabs(enabled), ['Overview', 'Memory']);
  assert.equal(attach(enabled), 1, 'Attach stays in the action row');
  assert.match(enabled, /<button[^>]*aria-expanded="false"[^>]*aria-controls="overview-full-output"[^>]*>Show full output<\/button><div id="overview-full-output" hidden="">[\s\S]*Actual assistant output/,
    'the output sits behind a collapsed toggle in Overview');
  for (const patch of [{ terminal: true, stale: true }, { terminal: true, phase: 'resyncing' },
    { terminal: true, epoch: 2, snapshotEpoch: 1 }, { terminal: true, agents: [] }]) {
    assert.equal(attach(center(patch)), 0);
  }
  const diagnostic = center({ output: { ...floor.output, status: 'gap' } });
  assert.deepEqual(tabs(diagnostic).filter(tab => tab === 'Output'), []);
  assert.match(diagnostic, /Gap — output incomplete/, 'the header still names the stream state');
  assert.doesNotMatch(center({ output: { ...floor.output, targetId: '01ARZ3NDEKTSV4RRFFQ69G5FAX', chunks: ['Other agent output'] } }), /Other agent output/);
});




test('tab selection follows a focused fallback section until focus leaves it', async () => {
  const { detailTabSelection } = await server.ssrLoadModule('/src/renderer/detailTabs.ts');
  const available = ['overview', 'git', 'memory'];
  assert.equal(detailTabSelection(available, 'git', 'overview'), 'overview');
  assert.equal(detailTabSelection(available, 'git', null), 'git');
  assert.equal(detailTabSelection(['overview', 'memory'], 'git', 'git'), 'overview');
  assert.equal(detailTabSelection(['overview', 'memory'], 'git', 'memory'), 'memory');
});

test('the drawer\'s close button stays in the detail header with or without output, and while the view is unconfirmed', async () => {
  const { center, output } = await drawer();
  for (const patch of [{ status: 'selecting' }, { status: 'waiting', reason: 'unbound' }, { status: 'ended' },
    { status: 'live', chunks: ['Actual assistant output'] }]) {
    const html = center({ output: { ...output, ...patch } });
    assert.match(header(html), CLOSE, `no close button in the header for ${patch.status} output`);
    assert.equal(html.match(/aria-label="Close agent details"/g)?.length, 1, `the close button is not unique for ${patch.status} output`);
  }
  for (const patch of [{ stale: true }, { phase: 'resyncing', agents: [] }, { phase: 'closed', agents: [] }]) {
    const html = center(patch, null);
    assert.match(header(html), CLOSE, `no close button while ${JSON.stringify(patch)}`);
    assert.ok(header(html).includes(row.id), 'the header does not name the standing selection');
    assert.doesNotMatch(html, /data-selected-agent-id/);
  }
  const none = center({ output: { ...output, targetId: null, status: 'idle', reason: null } }, null);
  assert.equal(none, '', 'the drawer is drawn with nothing selected');
});

test('a waiting question and its answer form stay in the header while Git is the stored tab', () => {
  const question = 'Ship now, or wait for a review?';
  const html = render({ row: { ...row, status: 'waiting', question }, tab: 'git', git,
    answer: createElement('textarea', { name: 'answer', 'aria-label': 'Your answer' }) });
  assert.match(html, /id="agent-detail-tab-git"[^>]*aria-selected="true"/);
  assert.match(header(html),
    /<section class="agent-detail-question" aria-label="Waiting question" tabindex="-1"><h3>Waiting question<\/h3><p>Ship now, or wait for a review\?<\/p><textarea name="answer"/);
  assert.doesNotMatch(panel(html, 'overview'), /Waiting question|<textarea/);
});

test('the stream status is one live region in the header, outside every tab panel', async () => {
  const { center, output } = await drawer();
  for (const [patch, text] of [
    [{ status: 'live', chunks: ['Actual assistant output'] }, 'Turn in progress'],
    [{ status: 'gap' }, 'Gap — output incomplete; retry to get a fresh snapshot'],
    [{ status: 'waiting', reason: 'unbound' }, 'Waiting — no Managed turn bound yet'],
  ]) {
    const html = center({ output: { ...output, ...patch } });
    const detail = html.slice(html.indexOf('id="selected-agent-detail"'));
    assert.deepEqual([...detail.matchAll(STREAM_STATE)].map((match) => match[1]), [text], `${patch.status} output`);
    assert.deepEqual([...header(html).matchAll(STREAM_STATE)].map((match) => match[1]), [text]);
    assert.doesNotMatch(panel(html, 'overview'), /role="status"/);
  }
  const unconfirmed = center({ stale: true }, null);
  assert.deepEqual([...header(unconfirmed).matchAll(STREAM_STATE)].map((match) => match[1]),
    ['Unconfirmed — waiting for the stream']);
});

test('the header names the turn an output state is about, so a waiting agent never reads as ended', async () => {
  const { center, output } = await drawer();
  const waiting = { ...row, status: 'waiting' };
  for (const [patch, text] of [
    [{ status: 'ended', chunks: ['Asked a question.'] }, 'Last turn ended'],
    [{ status: 'ended', chunks: ['Asked a question.'], truncated: true }, 'Last turn ended — earlier output truncated'],
    [{ status: 'live', chunks: ['Working on it.'] }, 'Turn in progress'],
    [{ status: 'live', chunks: ['Working on it.'], truncated: true }, 'Turn in progress — earlier output truncated'],
  ]) {
    const html = center({ output: { ...output, ...patch } }, waiting);
    assert.deepEqual([...header(html).matchAll(STREAM_STATE)].map((match) => match[1]), [text],
      `${patch.status} output, truncated ${patch.truncated === true}`);
  }
});

test('with the terminal switch off, the header says how to turn it on', async () => {
  const { center } = await drawer();
  const settings = {
    switches: { answer: false, control: false, goal: false, retire: false, terminal: false, hire: false },
    budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: null, per_turn_tokens: null },
    providers: ['pi'], workspaces: [], about: { daemon_version: '0.48.0', browser_wire_version: 3 },
  };
  const hint = /<p class="settings-guidance">Terminal is off\. Turn on Attach terminals under Floor switches in Settings\.<\/p>/;
  assert.match(header(center({ settings, terminal: false })), hint);
  const on = { ...settings, switches: { ...settings.switches, terminal: true } };
  assert.doesNotMatch(center({ settings: on, terminal: true }), hint);
  assert.doesNotMatch(center({ settings: null, terminal: false }), hint);
});

test('the switches that are off fold into one Enable more controls disclosure, with each hint\'s words', async () => {
  const { center } = await drawer();
  const settings = {
    switches: { answer: false, control: false, goal: false, retire: false, terminal: false, hire: false },
    budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: null, per_turn_tokens: null },
    providers: ['pi'], workspaces: [], about: { daemon_version: '0.48.0', browser_wire_version: 3 },
  };
  const DISCLOSURE = /<details class="enable-controls"><summary>Enable more controls <span class="enable-controls-count">(\d)<\/span><\/summary>([\s\S]*?)<\/details>/;
  const hints = (html) => [...html.matchAll(/<p class="settings-guidance">([\s\S]*?)<\/p>/g)]
    .map(([, words]) => words.replace(/<[^>]+>/g, '').replace(/\s+/g, ' '));
  const RETIRE = 'Retire is off. Turn on Pause, resume and rename and Retire agents under Floor switches in Settings.';
  const TERMINAL = 'Terminal is off. Turn on Attach terminals under Floor switches in Settings.';

  const both = header(center({ settings, retire: false, terminal: false }));
  const [, count, inside] = DISCLOSURE.exec(both) ?? [];
  assert.equal(count, '2');
  assert.deepEqual(hints(inside ?? ''), [RETIRE, TERMINAL]);
  assert.deepEqual(hints(both.replace(DISCLOSURE, '')), [], 'a hint sits outside the disclosure');

  const terminalOnly = header(center({ settings, retire: true, terminal: false }));
  assert.equal(DISCLOSURE.exec(terminalOnly)?.[1], '1');
  assert.deepEqual(hints(terminalOnly), [TERMINAL]);

  assert.doesNotMatch(center({ settings, retire: true, terminal: true }), /enable-controls/);
  assert.doesNotMatch(center({ settings: null, retire: false, terminal: false }), /enable-controls/);
});

// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const A = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const B = '01ARZ3NDEKTSV4RRFFQ69G5FAX';
const T = '01ARZ3NDEKTSV4RRFFQ69G5FAY';
const G = '01ARZ3NDEKTSV4RRFFQ69G5FAZ';
const row = { id: A, name: 'Ada', mode: 'managed', status: 'done', goal: { id: G, status: 'achieved' } };
const view = () => ({ enabled: true, max_text_bytes: 4096, pending: [], turns: [], context_sent: [] });
const turn = (patch = {}) => ({ agent_id: A, turn_id: T, note_lsn: '7', text: 'Please check',
  state: 'ended', reply: 'A natural ending…', truncated: null, failure: null, ran_models: [], reissued: false, ...patch });
let server, store, ChatPanelView, AgentDrawer, action, chatModule;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ({ ChatPanelView } = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx'));
  ({ AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx'));
  chatModule = await server.ssrLoadModule('/src/renderer/chat/chat.ts');
  action = chatModule.getChatState();
});
after(async () => { await server?.close(); });

function floor(chat = view(), patch = {}) {
  return { ...store.getFloorState(), epoch: 1, snapshotEpoch: 1, phase: 'live', stale: false, agents: [row], chat, ...patch };
}
function render(chat = view(), patch = {}, selected = row, state = action) {
  return renderToStaticMarkup(createElement(ChatPanelView, { floor: floor(chat, patch), row: selected, action: state }));
}

function liveReply(text, patch = {}) {
  return { ...store.getFloorState().output, view: 'reply', targetId: A, agentId: A, turnId: T, status: 'live',
    selectionEpoch: 1, reply: { revision: '1', text, gap: null }, ...patch };
}

test('the Chat drawer tab and its mounted content revoke on absent sibling or unconfirmed epochs', () => {
  const draw = (chat, patch = {}) => renderToStaticMarkup(createElement(AgentDrawer, { floor: floor(chat, patch),
    row, selectedId: A, onClose() {}, leaveFocusTo: 'crew-toggle' }));
  const on = draw({ ...view(), turns: [turn()] });
  assert.match(on, /role="tab"[^>]*>Chat<\/button>/);
  assert.match(on, /chat-final-reply/);
  for (const patch of [{ phase: 'closed' }, { phase: 'resyncing' }, { stale: true }, { epoch: 2 }, { agents: [] }]) {
    assert.doesNotMatch(draw(view(), patch), /chat-final-reply|chat_text|agent-detail-tab-chat|chat-cancel/);
  }
  assert.doesNotMatch(draw(null), /chat-final-reply|chat_text|agent-detail-tab-chat|chat-cancel/);
});

test('a fresh drawer defaults to Chat while explicit preferences and Chat-off fallback remain intact', async () => {
  const { readDetailTab, writeDetailTab } = await server.ssrLoadModule('/src/renderer/view.ts');
  const values = new Map();
  const storage = () => ({ getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) });
  assert.equal(readDetailTab(storage, 'chat'), 'chat');
  for (const preferred of ['overview', 'messages']) {
    writeDetailTab(preferred, storage);
    assert.equal(readDetailTab(storage, 'chat'), preferred);
  }
  values.set('dudes.floor.agent-detail-tab', 'output');
  assert.equal(readDetailTab(storage, 'chat'), 'chat', 'a remembered Output tab falls back to Chat');
  assert.equal(readDetailTab(storage, 'overview'), 'overview', 'and to Overview where Chat is unavailable');
  const draw = chat => renderToStaticMarkup(createElement(AgentDrawer, { floor: floor(chat), row,
    selectedId: A, onClose() {}, leaveFocusTo: 'crew-toggle' }));
  assert.match(draw(view()), /<button(?=[^>]*\bid="agent-detail-tab-chat")(?=[^>]*aria-selected="true")[^>]*>/);
  assert.match(draw(null), /<button(?=[^>]*\bid="agent-detail-tab-overview")(?=[^>]*aria-selected="true")[^>]*>/);
});

test('reserved preparation stays cancellable and uncertain cancellation names its narrow promise', () => {
  const pending = { agent_id: A, note_lsn: '7', goal_id: G, text: 'Pending note', state: 'reserved',
    turn_id: T, cancellable: true, reason: null };
  assert.match(render({ ...view(), pending: [pending] }), /Next to be sent\. Delivery has not started/);
  assert.match(render({ ...view(), pending: [pending] }), />Cancel note<\/button>/);
  const uncertain = render({ ...view(), pending: [{ ...pending, state: 'delivery_uncertain', reason: 'delivery_uncertain' }] });
  assert.match(uncertain, /Delivery before restart is unknown/);
  assert.match(uncertain, /Stop future reissue<\/button>/);
  assert.doesNotMatch(uncertain, /No text was dispatched/);
  assert.doesNotMatch(render({ ...view(), pending: [{ ...pending, cancellable: false, reason: 'delivery_started' }] }), /class="[^"]*\bchat-cancel\b/);
});

test('the chronological conversation joins reserved notes and turns once and puts the editable composer last', () => {
  const pending = { agent_id: A, note_lsn: '9007199254740993', goal_id: G, text: 'Shared reserved message',
    state: 'reserved', turn_id: T, cancellable: true, reason: null };
  const chat = { ...view(), pending: [pending, { ...pending, note_lsn: '9007199254740994', text: 'Last queued message',
    state: 'queued', turn_id: null }], turns: [turn({ note_lsn: pending.note_lsn, text: pending.text, state: 'reserved', reply: null }),
    turn({ note_lsn: '10', turn_id: B, text: 'First user message', reply: 'First reply' })] };
  const html = render(chat);
  assert.equal((html.match(/Shared reserved message/g) ?? []).length, 1);
  assert.ok(html.indexOf('First user message') < html.indexOf('First reply'));
  assert.ok(html.indexOf('First reply') < html.indexOf('Shared reserved message'));
  assert.ok(html.indexOf('Shared reserved message') < html.indexOf('Last queued message'));
  assert.ok(html.indexOf('Last queued message') < html.indexOf('aria-labelledby="chat-compose-title"'));
  const busy = { ...action, attempt: { verb: 'send', payload: { agent: A, text: 'Sending' } }, sending: true };
  const sending = render(view(), {}, row, busy);
  assert.doesNotMatch(sending, /<textarea[^>]*\sdisabled=""/);
  assert.match(sending, /disabled="" class="[^"]*\bchat-send"/);
});

test('the composer follows the thread while the conversation fills and scrolls the remaining sheet height', async () => {
  const html = render({ ...view(), turns: [turn()] });
  assert.ok(html.indexOf('aria-labelledby="chat-thread-title"') < html.indexOf('aria-labelledby="chat-compose-title"'));
  assert.ok(html.indexOf('Conversation details</summary>') < html.indexOf('aria-labelledby="chat-compose-title"'));
  assert.match(html, /class="[^"]*\bchat-scroll\b[^"]*" role="log"/);
  const sheet = renderToStaticMarkup(createElement(AgentDrawer, { floor: floor(), row, selectedId: A, onClose() {}, leaveFocusTo: 'crew-toggle' }));
  const on = (markup, hook) => markup.match(new RegExp(`class="((?:[^"]*\\s)?${hook}(?:\\s[^"]*)?)"`))?.[1].replaceAll('&amp;', '&').split(/\s+/) ?? [];
  for (const utility of ['flex', 'flex-col', 'overflow-hidden']) assert.ok(on(sheet, 'agent-drawer').includes(utility), `the drawer lacks ${utility}`);
  for (const utility of ['flex', 'flex-1', 'flex-col', 'min-h-0']) assert.ok(on(sheet, 'agent-detail').includes(utility), `the detail lacks ${utility}`);
  const tabpanel = /tabpanel" class="([^"]*)"/.exec(sheet)?.[1].split(/\s+/) ?? [];
  assert.ok(tabpanel.includes('flex-1') && !tabpanel.includes('overflow-y-auto') && !tabpanel.includes('min-h-0'), 'a panel grows with its content and the sheet scrolls');
  const wears = hook => html.match(new RegExp(`class="([^"]*\\b${hook}\\b[^"]*)"`))?.[1].split(/\s+/) ?? [];
  assert.ok(wears('chat-thread').includes('flex-1'));
  for (const utility of ['flex-1', 'min-h-0', 'overflow-y-auto']) assert.ok(wears('chat-scroll').includes(utility), utility);
  assert.deepEqual(wears('chat-scroll').filter(utility => /^h-/.test(utility)), []);
  assert.ok(wears('chat-compose').includes('shrink-0'));
  assert.deepEqual(wears('chat-compose').filter(utility => /^overflow/.test(utility)), []);
});

test('the Chat sheet follows the visible viewport while a phone keyboard opens and clears its listeners on leave', async () => {
  const { fakePage } = await import('./helpers/fake-page.mjs');
  const { createRoot } = await import('react-dom/client');
  const { act } = await import('react');
  const page = fakePage();
  const prototype = Object.getPrototypeOf(page.host);
  const closest = prototype.closest;
  const viewport = new EventTarget();
  Object.assign(viewport, { height: 844, offsetTop: 0 });
  window.visualViewport = viewport;
  prototype.closest = function (selector) { return selector === '.agent-drawer' ? page.host : closest.call(this, selector); };
  page.host.style.removeProperty = name => { delete page.host.style[name]; };
  try {
    const root = createRoot(page.host);
    await act(() => root.render(createElement(ChatPanelView, { floor: floor(), row, action })));
    assert.equal(page.host.style['--chat-viewport-height'], '844px');
    assert.equal(page.host.getAttribute('data-chat-short'), 'false');
    Object.assign(viewport, { height: 500, offsetTop: 75 });
    viewport.dispatchEvent(new Event('resize'));
    assert.equal(page.host.style['--chat-viewport-height'], '500px');
    assert.equal(page.host.style['--chat-viewport-top'], '75px');
    assert.equal(page.host.getAttribute('data-chat-short'), 'true');
    viewport.offsetTop = 20;
    viewport.dispatchEvent(new Event('scroll'));
    assert.equal(page.host.style['--chat-viewport-top'], '20px');
    await act(() => root.unmount());
    viewport.dispatchEvent(new Event('resize'));
    assert.equal(page.host.style['--chat-viewport-height'], undefined);
    assert.equal(page.host.style['--chat-viewport-top'], undefined);
    assert.equal(page.host.getAttribute('data-chat-short'), null);
  } finally {
    prototype.closest = closest;
    page.restore();
  }
});

test('the agent panel is one scroll region, so a form above the Chat never squeezes the conversation', async () => {
  const html = renderToStaticMarkup(createElement(AgentDrawer, { floor: floor(), row, selectedId: A,
    onClose() {}, leaveFocusTo: 'crew-toggle' }));
  assert.match(html, /class="[^"]*\bagent-detail-context\b[^"]*">[\s\S]*<header class="[^"]*\bagent-detail-head\b[^"]*">[\s\S]*\bagent-detail-tabs\b/);
  const context = /class="((?:[^"]*\s)?agent-detail-context(?:\s[^"]*)?)"/.exec(html)?.[1].replaceAll('&amp;', '&').split(/\s+/) ?? [];
  assert.ok(!context.includes('overflow-y-auto') && !context.some((name) => name.includes('max-h')), 'the header and tabs do not scroll on their own');
  const panel = /class="((?:[^"]*\s)?agent-detail(?:\s[^"]*)?)"/.exec(html)?.[1].split(/\s+/) ?? [];
  assert.ok(panel.includes('overflow-y-auto'), 'the panel scrolls as one');
  assert.match(html, /<section class="chat-thread [^"]*\bmin-h-72\b/, 'the conversation keeps a usable height');
  assert.ok(context.includes('flex-none'), 'the header keeps its own height');
  assert.match(html, /\bchat-compose\b/);
});

test('resizing the thread follows the newest reply unless the reader has scrolled away', async () => {
  const { fakePage } = await import('./helpers/fake-page.mjs');
  const { createRoot } = await import('react-dom/client');
  const { act } = await import('react');
  const page = fakePage();
  const previousObserver = globalThis.ResizeObserver;
  let resized, observed, disconnected = false;
  globalThis.ResizeObserver = class {
    constructor(callback) { resized = callback; }
    observe(node) { observed = node; }
    disconnect() { disconnected = true; }
  };
  try {
    const root = createRoot(page.host);
    await act(() => root.render(createElement(ChatPanelView, { floor: floor(), row, action })));
    const node = page.host.querySelector('[role="log"]');
    assert.equal(observed, node);
    Object.assign(node, { scrollHeight: 600, clientHeight: 200, scrollTop: 0 });
    resized();
    assert.equal(node.scrollTop, 600);
    const props = node[Object.keys(node).find(key => key.startsWith('__reactProps$'))];
    Object.assign(node, { clientHeight: 100, scrollTop: 200 });
    props.onScroll({ currentTarget: node });
    resized();
    assert.equal(node.scrollTop, 600);
    node.scrollTop = 0;
    props.onScroll({ currentTarget: node });
    node.scrollHeight = 900;
    resized();
    assert.equal(node.scrollTop, 0);
    node.scrollTop = 800;
    props.onScroll({ currentTarget: node });
    resized();
    assert.equal(node.scrollTop, 900);
    await act(() => root.unmount());
    assert.equal(disconnected, true);
  } finally {
    globalThis.ResizeObserver = previousObserver;
    page.restore();
  }
});

test('acknowledged messages remain visible until their snapshot row arrives without duplicate text', () => {
  const acknowledged = [{ agent_id: A, note_lsn: '12', text: 'Durably accepted message' }];
  const state = { ...action, acknowledged };
  const accepted = render(view(), {}, row, state);
  assert.match(accepted, /Durably accepted message/);
  assert.match(accepted, /Message accepted; current status unavailable/);
  assert.doesNotMatch(accepted, /Queued/);
  const pending = { agent_id: A, note_lsn: '12', goal_id: G, text: 'Durably accepted message', state: 'queued',
    turn_id: null, cancellable: true, reason: null };
  const html = render({ ...view(), pending: [pending] }, {}, row, state);
  assert.equal((html.match(/Durably accepted message/g) ?? []).length, 1);
  assert.match(html, /Queued/);
  assert.doesNotMatch(html, /current status unavailable/);
});

test('routine replies use one concise status and keep delivery details collapsed', () => {
  const output = liveReply('An actual received reply');
  const chat = { ...view(), turns: [turn({ state: 'running', reply: null })] };
  const routine = html => html.replace(/<details\b[^>]*>[\s\S]*?<\/details>/g, '').replace(/<[^>]*>/g, '');
  const open = render(chat, { output });
  assert.match(open, /<details[^>]*><summary[^>]*>(?:<svg[^>]*>.*?<\/svg>)?Reply details<\/summary>/);
  const visible = routine(open);
  assert.equal((visible.match(/Replying…/g) ?? []).length, 1);
  assert.doesNotMatch(visible, /provisional|not a final|saved reply|UTF-8|limited history/i);
  assert.match(visible, /Enter to send · Shift\+Enter for a newline/);
  assert.match(visible, /An actual received reply/);
  const ended = routine(render(chat, { output: { ...output, status: 'ended' } }));
  assert.match(ended, /Waiting for reply status/);
  assert.doesNotMatch(ended, /Replying|saved reply|saving/i);
  const final = render({ ...view(), turns: [turn()] });
  assert.match(final, /<details[^>]*><summary[^>]*>(?:<svg[^>]*>.*?<\/svg>)?Reply details<\/summary><div[^>]*><p[^>]*>Final reply completeness is not recorded/);
  assert.doesNotMatch(routine(final), /completeness|Turn ended|verdict/);
});

test('durable final text preserves natural ellipses and renders recorded completeness honestly', () => {
  const natural = render({ ...view(), turns: [turn()] });
  assert.match(natural, /A natural ending…/);
  assert.match(natural, /Final reply completeness is not recorded/);
  assert.doesNotMatch(natural, /Final reply was truncated/);
  assert.match(render({ ...view(), turns: [turn({ truncated: true })] }), /Final reply was truncated/);
  assert.match(render({ ...view(), turns: [turn({ reply: null })] }), /No final reply was reported/);
});

test('a failed turn shows the engine\'s own words once, as plain text with hidden characters marked', () => {
  const said = render({ ...view(), turns: [turn({ state: 'failed', reply: null, failure: 'model_not_found: **no** <b>such</b> model' })] });
  assert.match(said, /The engine said/);
  assert.match(said, /<pre class="chat-failure-text[^"]*">model_not_found: \*\*no\*\* &lt;b&gt;such&lt;\/b&gt; model<\/pre>/);
  assert.match(said, /Turn failed\./);
  assert.doesNotMatch(said, /No final reply was reported|chat-final-reply/);
  const same = render({ ...view(), turns: [turn({ state: 'failed', reply: 'rate limited', failure: 'rate limited' })] });
  assert.equal(same.match(/rate limited/g).length, 1, 'the page shows an equal reply and failure once');
  assert.doesNotMatch(same, /chat-final-reply/);
  const both = render({ ...view(), turns: [turn({ state: 'failed', reply: 'partial answer', failure: 'rate limited' })] });
  assert.match(both, /partial answer/);
  assert.match(both, /rate limited/);
  const hidden = render({ ...view(), turns: [turn({ state: 'failed', reply: null, failure: 'ok\u202Eevil\u0007' })] });
  assert.match(hidden, /ok\[U\+202E\]evil\[U\+0007\]/);
  assert.doesNotMatch(render({ ...view(), turns: [turn()] }), /The engine said/);
});

test('only an explicit matching-turn reissue marker shows the interruption warning', () => {
  const replayed = render({ ...view(), turns: [turn({ reissued: true })] });
  assert.match(replayed, /Restarted after interruption/);
  assert.match(replayed, /Previous delivery may have happened and earlier context may be missing/);
  assert.match(replayed, /<details[^>]*><summary[^>]*>(?:<svg[^>]*>.*?<\/svg>)?What changed<\/summary>/);
  const ordinary = render({ ...view(), turns: [turn({ reissued: false })] });
  assert.doesNotMatch(ordinary, /Restarted after interruption|never delivered|context restored/);
});

test('bounded history omission is unavailable history and only a retained null reply claims no reported final', () => {
  const empty = render(view());
  assert.match(empty, /No messages in the available history/);
  assert.match(empty, /Only recent history is kept, so some earlier work may be missing\./);
  assert.doesNotMatch(empty, /No final reply was reported/);
  const retained = render({ ...view(), turns: [turn()] });
  assert.match(retained, /Only recent history is kept, so some earlier work may be missing\./);
  assert.match(retained, /A natural ending…/);
  assert.doesNotMatch(retained, /No messages in the available history/);
  assert.match(render({ ...view(), turns: [turn({ reply: null })] }), /No final reply was reported/);
});

test('Enter queues through the existing action once, while Shift+Enter and composing keys preserve the draft', async () => {
  const original = { ...chatModule.browserChatDeps };
  const calls = [];
  let prevented = 0;
  function findTextarea(element) {
    if (typeof element !== 'object' || element === null) return null;
    if (element?.type === 'textarea' || element?.type?.name === 'Textarea') return element;
    const children = element?.props?.children;
    for (const child of Array.isArray(children) ? children : [children]) {
      const found = findTextarea(child);
      if (found) return found;
    }
    return null;
  }
  function event(patch = {}) {
    return { key: 'Enter', shiftKey: false, altKey: false, ctrlKey: false, metaKey: false, repeat: false,
      nativeEvent: {}, preventDefault() { prevented += 1; }, ...patch };
  }
  try {
    store.resetFloorStore(); chatModule.resetChat();
    const epoch = store.beginFloorEpoch();
    store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9, agents: [row] }, chat: view() }), epoch);
    Object.assign(chatModule.browserChatDeps, { signedIn: () => true, random: bytes => bytes.fill(0),
      fetch: async (url, init) => {
        calls.push({ url, body: JSON.parse(init.body) });
        return { status: 202, text: async () => JSON.stringify({ request_id: 'A'.repeat(43), state: 'pending' }) };
      } });
    chatModule.editChatDraft(A, 'Queue by Enter');
    const textarea = () => findTextarea(ChatPanelView({ floor: store.getFloorState(), row, action: chatModule.getChatState() }));
    assert.equal(typeof textarea().props.onKeyDown, 'function', 'Chat textarea has no queue keyboard behavior');
    for (const press of [event({ shiftKey: true }), event({ altKey: true }), event({ ctrlKey: true }), event({ metaKey: true }),
      event({ key: 'Escape' }), event({ repeat: true }),
      event({ nativeEvent: { isComposing: true } }), event({ nativeEvent: { keyCode: 229 } })]) {
      textarea().props.onKeyDown(press);
    }
    assert.equal(calls.length, 0);
    assert.equal(prevented, 0);
    assert.equal(chatModule.chatDraft(chatModule.getChatState(), A), 'Queue by Enter');
    textarea().props.onKeyDown(event());
    await new Promise(resolve => setImmediate(resolve));
    assert.deepEqual(calls, [{ url: '/floor/chat/send', body: { request_id: 'A'.repeat(43), agent: A, text: 'Queue by Enter' } }]);
    assert.equal(prevented, 1);
    textarea().props.onKeyDown(event());
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(calls.length, 1, 'busy keyboard queued a second request');
    chatModule.resetChat();
    chatModule.editChatDraft(A, ' ');
    textarea().props.onKeyDown(event());
    assert.equal(calls.length, 1, 'invalid draft queued a request');
    chatModule.editChatDraft(A, 'A valid draft');
    const held = textarea().props.onKeyDown;
    store.invalidateFloorEpoch();
    held(event());
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(calls.length, 1, 'old gate queued through a revoked connection');
    assert.equal(chatModule.getChatState().attempt, null, 'old gate minted a new guarded request after revocation');
  } finally {
    Object.assign(chatModule.browserChatDeps, original);
    store.resetFloorStore(); chatModule.resetChat();
  }
});

test('notes and final replies render inert HTML, command and link text without active surfaces', () => {
  const raw = '<script>alert(1)</script> [link](https://x.invalid) /command';
  const html = render({ ...view(), turns: [turn({ text: raw, reply: raw })] });
  assert.equal((html.match(/&lt;script&gt;alert\(1\)&lt;\/script&gt;/g) ?? []).length, 2);
  assert.match(html, /\[link\]\(https:\/\/x\.invalid\) \/command/);
  assert.doesNotMatch(html, /<script|<a\b|<img\b|<iframe\b/);
});

test('fenced code preserves indentation and stays inert in final and unfinished live replies', () => {
  const text = 'Run this:\n```text\n  npm run typecheck\n<script>alert(1)</script>\n```\nThen read the result.';
  const html = render({ ...view(), turns: [turn({ reply: text })] });
  assert.match(html, /<pre class="chat-code"><code>  npm run typecheck\n&lt;script&gt;alert\(1\)&lt;\/script&gt;<\/code><\/pre>/);
  assert.match(html, /Then read the result/);
  assert.doesNotMatch(html, /<script|<a\b|<img\b/);
  const output = liveReply('```html\n<img src=x onerror=alert(1)>');
  const streaming = render({ ...view(), turns: [turn({ state: 'running', reply: null })] }, { output });
  assert.match(streaming, /<pre class="chat-code"><code>&lt;img src=x onerror=alert\(1\)&gt;<\/code><\/pre>/);
  assert.doesNotMatch(streaming, /<img\b|chat-final-reply/);
});

test('ordinary text supports safe bold and inline code while escapes and fenced content stay literal', () => {
  const text = 'Use **bold** and `code <b>`; **run `check`**.\n\\*\\*literal\\*\\* and \\`literal\\`.\n'
    + '```text\n**fenced** `fenced` <script>\n```\n**unfinished\\**';
  const html = render({ ...view(), turns: [turn({ reply: text })] });
  assert.match(html, /<strong>bold<\/strong>/);
  assert.match(html, /<code class="chat-inline-code">code &lt;b&gt;<\/code>/);
  assert.match(html, /<strong>run <code class="chat-inline-code">check<\/code><\/strong>/);
  assert.match(html, /\*\*literal\*\* and `literal`/);
  assert.match(html, /<pre class="chat-code"><code>\*\*fenced\*\* `fenced` &lt;script&gt;<\/code><\/pre>/);
  assert.match(html, /\*\*unfinished\*\*/);
  assert.doesNotMatch(html, /<strong>literal|<strong>fenced|<strong>unfinished|<script|<a\b|<img\b/);
});

test('only matching live Managed Output is provisional preview and never becomes a final reply', () => {
  const output = liveReply('Provisional assistant text');
  const chat = { ...view(), turns: [turn({ state: 'running', reply: null })] };
  const preview = render(chat, { output });
  assert.match(preview, /data-chat-preview="provisional"/);
  assert.match(preview, /Provisional assistant text/);
  assert.doesNotMatch(preview, /chat-final-reply|Final reply completeness/);
  for (const change of [{ turnId: B }, { agentId: B }, { targetId: B }, { selectionEpoch: null }, { view: 'output' }]) {
    assert.doesNotMatch(render(chat, { output: { ...output, ...change } }), /Provisional assistant text/);
  }
  assert.doesNotMatch(render(chat, { output }, { ...row, mode: 'attached' }), /Provisional assistant text/);
  const ended = render({ ...view(), turns: [turn({ reply: null })] }, { output });
  assert.match(ended, /No final reply was reported/);
  assert.doesNotMatch(ended, /Provisional assistant text/);
});

test('stream end, truncation and gaps stay provisional until a durable reply replaces the preview', () => {
  const output = liveReply('Received before completion', { truncated: true });
  const chat = { ...view(), turns: [turn({ state: 'running', reply: null })] };
  assert.match(render(chat, { output }), /Earlier live output was truncated/);
  const ended = render(chat, { output: { ...output, status: 'ended' } });
  assert.match(ended, /Received before completion/);
  assert.match(ended, /Waiting for reply status/);
  assert.doesNotMatch(ended, /chat-final-reply|Replying/);
  const gap = render(chat, { output: { ...output, status: 'gap', reply: { ...output.reply, gap: 'pending_overflow' } } });
  assert.match(gap, /Received before completion/);
  assert.match(gap, /Live output is incomplete/);
  assert.match(gap, /Refresh live output/);
  const final = render({ ...view(), turns: [turn({ reply: 'Durable final' })] }, { output });
  assert.match(final, /Durable final/);
  assert.doesNotMatch(final, /Received before completion|chat-preview/);
});

test('provider omission warns even without preview text and does not truncate the durable reply', () => {
  const initial = liveReply('');
  const output = { ...initial, status: 'gap', truncated: true,
    reply: { ...initial.reply, gap: 'provider_limit' } };
  const pending = render({ ...view(), turns: [turn({ state: 'running', reply: null })] }, { output });
  assert.match(pending, /Live output is incomplete/);
  const final = render({ ...view(), turns: [turn({ reply: 'Durable final', truncated: false })] }, { output });
  assert.match(final, /Durable final/);
  assert.doesNotMatch(final, /Live output is incomplete|Final reply was truncated|chat-preview/);
});

test('received chunks update the matching running reply and old selections cannot restore text', () => {
  store.resetFloorStore();
  const epoch = store.beginFloorEpoch();
  store.selectOutputAgent(A, false, 'reply');
  const chat = { ...view(), turns: [turn({ state: 'running', reply: null })] };
  const snapshot = () => store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 9, agents: [row] }, chat }), epoch);
  snapshot();
  const context = { localEpoch: store.getFloorState().output.localEpoch, selectionEpoch: 1 };
  const output = { type: 'floor.output.reply', agent_id: A, turn_id: T, selection_epoch: 1,
    revision: '0', text: 'Received first', gap: null, truncated: false, ended: false };
  const draw = () => renderToStaticMarkup(createElement(ChatPanelView, { floor: store.getFloorState(), row, action }));
  store.applyServerFrame(JSON.stringify(output), epoch, context);
  assert.match(draw(), /Received first/);
  assert.doesNotMatch(draw(), /chat-final-reply/);
  store.applyServerFrame(JSON.stringify({ ...output, revision: '1', text: 'Received first and second' }), epoch, context);
  assert.match(draw(), /Received first and second/);
  store.selectOutputAgent(B);
  store.selectOutputAgent(A);
  snapshot();
  store.applyServerFrame(JSON.stringify(output), epoch, context);
  assert.doesNotMatch(draw(), /Received first/);
  const fresh = { localEpoch: store.getFloorState().output.localEpoch, selectionEpoch: 3 };
  store.applyServerFrame(JSON.stringify({ ...output, selection_epoch: 3, turn_id: B,
    text: 'Different turn text' }), epoch, fresh);
  assert.doesNotMatch(draw(), /Different turn text/);
  store.resetFloorStore();
});

test('Chat selection persists and a fresh Managed agent can compose its first message without a goal', async () => {
  const { readDetailTab, writeDetailTab } = await server.ssrLoadModule('/src/renderer/view.ts');
  const values = new Map();
  const storage = () => ({ getItem: key => values.get(key), setItem: (key, value) => values.set(key, value) });
  writeDetailTab('chat', storage);
  assert.equal(readDetailTab(storage), 'chat');
  chatModule.resetChat();
  chatModule.editChatDraft(A, 'Draft before a goal exists');
  const html = render(view(), {}, { ...row, goal: null }, chatModule.getChatState());
  assert.doesNotMatch(html, /A current goal is required|class="chat-send"[^>]*disabled/);
  assert.doesNotMatch(html, /<textarea[^>]*\sdisabled=""/);
  chatModule.resetChat();
});

test('an explicit question target swaps the one composer and preserves the ordinary message draft', () => {
  chatModule.resetChat();
  chatModule.editChatDraft(A, 'An unrelated message draft');
  const waiting = { ...row, goal: null, status: 'waiting', question: 'Which format should I use?', question_lsn: '9007199254740993' };
  const patch = { agents: [waiting], answering: { maxTextBytes: 4096 } };
  const before = render(view(), patch, waiting, chatModule.getChatState());
  assert.equal((before.match(/<textarea\b/g) ?? []).length, 1);
  assert.match(before, /name="chat_text"/);
  assert.match(before, /Reply to question/);
  assert.doesNotMatch(before, /name="answer"/);
  chatModule.selectChatAnswer(A, { questionLsn: waiting.question_lsn, question: waiting.question });
  const answering = render(view(), patch, waiting, chatModule.getChatState());
  assert.equal((answering.match(/<textarea\b/g) ?? []).length, 1);
  assert.match(answering, /Reply to Ada/);
  assert.match(answering, /data-chat-question-lsn="9007199254740993"/);
  assert.match(answering, /data-answer-question-lsn="9007199254740993"/);
  assert.match(answering, /name="answer"/);
  assert.doesNotMatch(answering, /name="chat_text"/);
  assert.equal(chatModule.chatDraft(chatModule.getChatState(), A), 'An unrelated message draft');
  chatModule.selectChatAnswer(A, null);
  assert.match(render(view(), patch, waiting, chatModule.getChatState()), /An unrelated message draft/);
  chatModule.resetChat();
});

test('a question change or missing answer grant cannot silently retarget the composer', () => {
  chatModule.resetChat();
  chatModule.editChatDraft(A, 'Preserved ordinary draft');
  const waiting = { ...row, status: 'waiting', question: 'Original question', question_lsn: '8' };
  chatModule.selectChatAnswer(A, { questionLsn: '8', question: 'Original question' });
  const changed = { ...waiting, question: 'Different question', question_lsn: '9' };
  const html = render(view(), { agents: [changed], answering: { maxTextBytes: 4096 } }, changed, chatModule.getChatState());
  assert.match(html, /Your reply target changed or closed/);
  assert.match(html, /Preserved ordinary draft/);
  assert.doesNotMatch(html, /name="answer"/);
  const unavailable = render(view(), { agents: [waiting], answering: null }, waiting, chatModule.getChatState());
  assert.match(unavailable, /Answering this question from the Floor is unavailable/);
  assert.doesNotMatch(unavailable, /Reply to question|name="answer"/);
  chatModule.editChatDraft(A, 'Continue with an ordinary message');
  assert.equal(chatModule.getChatState().answerTargets.has(A), false);
  chatModule.resetChat();
});

test('Attached receipts promise context emission and no reply history', () => {
  const html = render({ ...view(), context_sent: [{ agent_id: A, note_lsn: '7', state: 'sent_as_context' }] }, {},
    { ...row, mode: 'attached', status: 'attached' });
  assert.match(html, /Sent as context/);
  assert.match(html, /Attached replies are unavailable/);
  assert.doesNotMatch(html, /chat-final-reply|chat-preview/);
});

test('every fixed refusal renders a plain reason without private provider errors', async () => {
  const { CHAT_REASONS } = await server.ssrLoadModule('/src/renderer/chat/model.ts');
  for (const reason of CHAT_REASONS) {
    const state = { ...action, attempt: { verb: 'send', payload: { agent: A, text: 'Note', request_id: 'A'.repeat(43) } },
      outcome: { kind: 'refused', reason } };
    const html = render(view(), {}, row, state);
    assert.match(html, /data-chat-outcome="refused"/);
    assert.doesNotMatch(html, />undefined<|provider_error|data-chat-outcome="queued"/);
  }
  const state = { ...action, attempt: { verb: 'cancel', payload: { agent: A, note_lsn: '7', request_id: 'A'.repeat(43) } },
    outcome: { kind: 'reissue_cancelled', noteLsn: '7' } };
  assert.match(render(view(), {}, row, state), /past delivery may have occurred/);
});

test('context loss displays the refused message separately from the newer draft and requires an explicit choice', () => {
  chatModule.resetChat();
  chatModule.editChatDraft(A, 'My newer draft');
  const attempt = { verb: 'send', payload: { request_id: 'A'.repeat(43), agent: A, text: 'The refused message' }, body: '{}' };
  const state = { ...chatModule.getChatState(), attempt, outcome: { kind: 'context_lost', turnId: T } };
  const html = render(view(), {}, row, state);
  assert.match(html, /The refused message/);
  assert.match(html, /My newer draft<\/textarea>/);
  assert.match(html, /without restoring the previous conversation/);
  assert.match(html, />Start fresh and resend this message<\/button>/);
  assert.match(html, />Continue composing<\/button>/);
  assert.doesNotMatch(html, />Check status<\/button>/);
  const stale = render(view(), {}, row, { ...state, outcome: { kind: 'refused', reason: 'context_changed' } });
  assert.doesNotMatch(stale, /chat-start-fresh/);
  assert.match(stale, /The conversation context changed/);
  chatModule.resetChat();
});

const tabNames = html => [...html.matchAll(/role="tab"[^>]*class="[^"]*\bagent-detail-tab\b[^"]*"[^>]*>([^<]+)<\/button>/g)].map(match => match[1]);

test('after a chat message the drawer tabs are Overview, Chat, Git and Memory and never Output', () => {
  const output = { ...liveReply('Provisional assistant text'), chunks: ['Raw engine text'] };
  const chat = { ...view(), turns: [turn({ state: 'running', reply: null })] };
  const html = renderToStaticMarkup(createElement(AgentDrawer, { floor: floor(chat, { output, git: true }),
    row, selectedId: A, onClose() {}, leaveFocusTo: 'crew-toggle' }));
  assert.deepEqual(tabNames(html), ['Overview', 'Chat', 'Git', 'Memory']);
  assert.doesNotMatch(html, /agent-detail-tab-output|agent-detail-panel-output/);
  assert.match(html, /<div id="chat-full-output" hidden="">[\s\S]*Raw engine text/, 'the raw output stays collapsed until asked for');
});

test('the latest turn offers Show full output, collapsed by default, and opening it shows the live output', () => {
  const chat = { ...view(), turns: [turn({ state: 'running', reply: null })] };
  const closed = render(chat, { output: liveReply('Provisional assistant text') });
  assert.match(closed, /<button[^>]*aria-expanded="false"[^>]*aria-controls="chat-full-output"[^>]*class="[^"]*full-output-toggle[^"]*"[^>]*>Show full output<\/button><div id="chat-full-output" hidden="">/);
  const output = { ...store.getFloorState().output, view: 'output', targetId: A, status: 'live', chunks: ['Raw engine text'] };
  const open = renderToStaticMarkup(createElement(ChatPanelView, { floor: floor(chat, { output }), row, action, fullOutput: true }));
  assert.match(open, /<button[^>]*aria-expanded="true"[^>]*class="[^"]*full-output-toggle[^"]*"[^>]*>Hide full output<\/button><div id="chat-full-output"><section[^>]*aria-label="Live work"/);
  assert.match(open, /<pre class="live-work-text[^"]*">Raw engine text<\/pre>/);
  assert.doesNotMatch(open, /Read-only|Live work<|TurnId|<h3>Live work/i, 'no jargon inside the toggle');
  assert.equal((render({ ...view(), turns: [turn({ turn_id: B, note_lsn: '6' }), turn()] }).match(/class="[^"]*full-output-toggle/g) ?? []).length, 1,
    'only the latest turn carries the toggle');
  assert.doesNotMatch(render(chat, {}, { ...row, mode: 'attached' }), /full-output/);
  assert.doesNotMatch(render(view()), /full-output/);
});

test('opening Show full output switches the stream to output, closing restores the reply stream, and a new agent starts closed', async () => {
  const { fakePage, press } = await import('./helpers/fake-page.mjs');
  const { createRoot } = await import('react-dom/client');
  const { act } = await import('react');
  const rowB = { ...row, id: B, name: 'Grace' };
  const chat = { ...view(), turns: [turn({ state: 'running', reply: null }),
    turn({ agent_id: B, turn_id: '01ARZ3NDEKTSV4RRFFQ69G5FB0', note_lsn: '8', state: 'running', reply: null })] };
  const page = fakePage();
  try {
    const root = createRoot(page.host);
    const show = (agent) => {
      const output = { ...store.getFloorState().output, status: 'live', chunks: ['Raw engine text'] };
      return act(() => root.render(createElement(AgentDrawer, { floor: floor(chat, { agents: [row, rowB], output }),
        row: agent, selectedId: agent.id, onClose() {}, leaveFocusTo: 'crew-toggle' })));
    };
    store.selectOutputAgent(A, false, 'reply');
    await show(row);
    const toggle = () => page.host.querySelector('[aria-controls="chat-full-output"]');
    assert.equal(store.getFloorState().output.view, 'reply');
    assert.equal(toggle().getAttribute('aria-expanded'), 'false');
    await act(async () => press(toggle()));
    await show(row);
    assert.equal(toggle().getAttribute('aria-expanded'), 'true');
    assert.equal(store.getFloorState().output.view, 'output', 'opening asks for the output stream');
    await act(async () => press(toggle()));
    await show(row);
    assert.equal(toggle().getAttribute('aria-expanded'), 'false');
    assert.equal(store.getFloorState().output.view, 'reply', 'closing returns to the reply stream');
    await act(async () => press(toggle()));
    await show(row);
    store.selectOutputAgent(B, false, 'output');
    await show(rowB);
    assert.equal(toggle().getAttribute('aria-expanded'), 'false', 'a newly selected agent starts closed');
    assert.equal(store.getFloorState().output.view, 'reply');
    await act(() => root.unmount());
  } finally {
    store.selectOutputAgent(null);
    page.restore();
  }
});

test('mounted Clear all asks once, keeps per-note removal and clears only this agent through existing routes', async () => {
  const { fakePage, press } = await import('./helpers/fake-page.mjs');
  const { createRoot } = await import('react-dom/client');
  const { act, useSyncExternalStore } = await import('react');
  const { ChatPanel } = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx');
  const note = (note_lsn, patch = {}) => ({ agent_id: A, note_lsn, text: `Queued ${note_lsn}`, goal_id: null,
    state: 'queued', turn_id: null, cancellable: true, reason: null, ...patch });
  let pending = [note('1'), note('2'), note('3', { agent_id: B }),
    note('4', { state: 'delivery_uncertain', turn_id: T, reason: 'delivery_uncertain' })];
  store.resetFloorStore(); chatModule.resetChat();
  const epoch = store.beginFloorEpoch();
  const snapshot = () => store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: {
    lsn: 10, agents: [row, { ...row, id: B, name: 'Grace' }] }, chat: { ...view(), pending } }), epoch);
  snapshot();
  const calls = [];
  const original = { ...chatModule.browserChatDeps };
  Object.assign(chatModule.browserChatDeps, { floor: store.getFloorState, signedIn: () => true, session: () => 1,
    random(bytes) { bytes.fill(calls.length); }, async fetch(url, init) {
      const payload = JSON.parse(init.body); calls.push({ url, payload });
      pending = pending.filter(note => note.note_lsn !== payload.note_lsn); snapshot();
      return { status: 200, text: async () => JSON.stringify({ ...payload, state: 'cancelled' }) };
    } });
  let selected = row;
  function Page() {
    const current = useSyncExternalStore(store.subscribeToFloor, store.getFloorState, store.getFloorState);
    return createElement(ChatPanel, { floor: current, row: selected });
  }
  const page = fakePage();
  const root = createRoot(page.host);
  const control = name => page.host.querySelector(`.${name}`);
  try {
    await act(() => root.render(createElement(Page)));
    await act(() => press(control('chat-clear-all')));
    assert.equal(calls.length, 0);
    assert.ok(page.host.querySelector('[aria-label="Clear queued notes"]'));
    await act(() => press(control('chat-clear-keep')));
    assert.equal(calls.length, 0);
    assert.equal(control('chat-clear-confirm'), null);
    await act(() => press(control('chat-clear-all')));
    selected = { ...row, id: B, name: 'Grace' };
    await act(() => root.render(createElement(Page)));
    assert.equal(control('chat-clear-confirm'), null, 'switching agents discards the earlier confirmation');
    selected = row; await act(() => root.render(createElement(Page)));
    assert.equal(control('chat-clear-confirm'), null);
    await act(() => press(control('chat-clear-all')));
    pending.push(note('5')); await act(snapshot);
    await act(async () => press(control('chat-clear-confirm')));
    assert.deepEqual(calls.map(call => [call.url, call.payload.agent, call.payload.note_lsn]), [
      ['/floor/chat/cancel', A, '1'], ['/floor/chat/cancel', A, '2'], ['/floor/chat/cancel', A, '5']]);
    assert.deepEqual(pending.map(note => note.note_lsn), ['3', '4']);
    assert.ok(control('chat-cancel'), 'uncertain delivery keeps its individual stop control');
    assert.equal(control('chat-clear-all'), null);
    assert.equal(chatModule.getChatState().attempt, null);
  } finally {
    await act(() => root.unmount());
    Object.assign(chatModule.browserChatDeps, original);
    page.restore(); store.resetFloorStore(); chatModule.resetChat();
  }
});

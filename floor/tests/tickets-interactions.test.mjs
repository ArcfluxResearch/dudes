// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { createServer } from 'vite';

import { fakePage, press } from './helpers/fake-page.mjs';
import { ADA, agent, cardId, subscribe, ticketCard, TWO_PROJECTS } from './helpers/tickets.mjs';

let server, store, ticket, detail, git, Control, browserControlDeps;
const seen = [];
let reply;

before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  ticket = await server.ssrLoadModule('/src/renderer/ticket/ticket.ts');
  detail = await server.ssrLoadModule('/src/renderer/ticket/detail.ts');
  git = await server.ssrLoadModule('/src/renderer/git/git.ts');
  ({ Control } = await server.ssrLoadModule('/src/renderer/Control.tsx'));
  ({ browserControlDeps } = await server.ssrLoadModule('/src/renderer/control/control.ts'));
  Object.assign(browserControlDeps, {
    csrf: () => 'A'.repeat(43), epoch: () => 1, session: () => 1, refresh() {}, random() {},
    fetch: async (url, init) => {
      const body = JSON.parse(init.body);
      seen.push([url, body]);
      return reply(url, body);
    },
  });
});
after(async () => { await server?.close(); });
beforeEach(() => {
  store.resetFloorStore(); ticket.resetTicket(); detail.resetDetail(); git.resetGit();
  seen.length = 0;
  reply = (url, body) => ({ status: 200, text: async () => JSON.stringify(url.endsWith('/read') ? {
    state: 'ok', card: { id: body.card }, ticket: {
      note: 'Keep the change small.', project: 'service', check: { id: 'default', label: 'default passes' },
      round: 1, reason: null, summary: '<b>Agent text</b>', check_result: null, position: null, waiting: null,
    },
  } : { state: url.endsWith('/stop') ? 'stopped' : 'removed', card: body.card, seq: 4, lsn: '4096' }) });
});

function pageForTickets() {
  const page = fakePage();
  const prototype = Object.getPrototypeOf(page.host);
  const queued = [];
  const previousRaf = globalThis.requestAnimationFrame;
  globalThis.requestAnimationFrame = (run) => { queued.push(run); return queued.length; };
  page.flushFocus = () => { for (const run of queued.splice(0)) run(); };
  Object.defineProperties(prototype, {
    isConnected: { get() { return page.document.body.contains(this); } },
    disabled: { get() { return this.getAttribute('disabled') !== null; } },
    options: { get() { return this.querySelectorAll('option'); } },
    value: {
      get() { return this.tagName === 'SELECT' ? (this.options.find((option) => option.selected) ?? this.options[0])?.value ?? ''
        : this.currentValue ?? this.getAttribute('value') ?? ''; },
      set(value) { this.currentValue = String(value); },
    },
  });
  const matches = (node, selector) => {
    if (selector.endsWith(':not(:disabled)')) return !node.disabled && matches(node, selector.slice(0, -15));
    if (selector.startsWith('.')) return (node.getAttribute('class') ?? '').split(' ').includes(selector.slice(1));
    if (selector.startsWith('#')) return node.id === selector.slice(1);
    const [prefix, name, value] = /^(\w*)\[([a-z-]+)(?:="([^"]*)")?\]$/.exec(selector)?.slice(1) ?? [];
    if (name) {
      const recorded = node.getAttribute(name) ?? (typeof node[name] === 'string' ? node[name] : null);
      return (!prefix || node.tagName.toLowerCase() === prefix)
        && (value === undefined ? recorded !== null : recorded === value);
    }
    return node.tagName.toLowerCase() === selector;
  };
  prototype.querySelectorAll = function (selector) {
    const found = [];
    for (const child of this.childNodes) {
      if (child.nodeType !== 1) continue;
      if (selector.split(',').some((part) => matches(child, part.trim()))) found.push(child);
      found.push(...child.querySelectorAll(selector));
    }
    return found;
  };
  prototype.closest = function (selector) {
    for (let node = this; node?.nodeType === 1; node = node.parentNode) if (matches(node, selector)) return node;
    return null;
  };
  prototype.showModal = function () { this.setAttribute('open', ''); };
  prototype.close = function () { this.removeAttribute('open'); };
  page.document.querySelector = (selector) => page.document.body.querySelector(selector);
  const restore = page.restore;
  page.restore = () => { restore(); globalThis.requestAnimationFrame = previousRaf; };
  return page;
}

const propsOf = (element) => element[Object.keys(element).find((key) => key.startsWith('__reactProps$'))];
const textOf = (element) => element.nodeType === 3 ? element.nodeValue : element.childNodes.map(textOf).join('');

async function mount(cards, { agents = [agent()], onHire = () => {}, hireAvailable = false } = {}) {
  subscribe(store, { cards, agents, checks: TWO_PROJECTS });
  const page = pageForTickets();
  const root = createRoot(page.host);
  let floor = { ...store.getFloorState(), onboarding: hireAvailable ? { workspaces: [] } : null };
  const closes = [];
  const render = async (next = floor) => {
    floor = next;
    await act(async () => root.render(createElement(Control, { floor, view: 'board', selectedId: null,
      onClose: () => closes.push('board'), onView() {}, onOpenQuestion() {}, onHire })));
    page.flushFocus();
  };
  await render();
  return { page, root, render, closes, get floor() { return floor; },
    async close() { await act(async () => root.unmount()); page.restore(); } };
}

test('a card opens a modal and Escape closes only that modal and returns focus', async () => {
  const app = await mount([ticketCard(1, 'review', { claimed_by: ADA })]);
  try {
    const button = app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`);
    await act(async () => press(button));
    const dialog = app.page.host.querySelector('.ticket-dialog');
    assert.notEqual(dialog.getAttribute('open'), null);
    assert.equal(app.page.document.activeElement.id, 'card-detail-title');
    assert.ok(textOf(dialog).includes('<b>Agent text</b>'));
    assert.equal(dialog.querySelector('b'), null);
    let prevented = false, stopped = false;
    propsOf(dialog).onKeyDown({ key: 'Escape', stopPropagation() { stopped = true; } });
    await act(async () => propsOf(dialog).onCancel({ preventDefault() { prevented = true; } }));
    app.page.flushFocus();
    assert.ok(prevented && stopped);
    assert.equal(dialog.getAttribute('open'), null);
    assert.deepEqual(app.closes, []);
    assert.equal(app.page.document.activeElement, button);
  } finally { await app.close(); }
});

test('closing a changed card targets its replacement and a removed card targets Board', async () => {
  const app = await mount([ticketCard(1, 'doing', { claimed_by: ADA })]);
  try {
    const first = app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`);
    await act(async () => press(first));
    await app.render({ ...app.floor, cards: [{ ...app.floor.cards[0], status: 'review', seq: 4 }] });
    const replacement = app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`);
    assert.notEqual(replacement, first);
    await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
    app.page.flushFocus();
    assert.equal(app.page.document.activeElement, replacement);
    await act(async () => press(replacement));
    await app.render({ ...app.floor, cards: [] });
    assert.equal(app.page.host.querySelector('.ticket-dialog').getAttribute('open'), null);
    assert.equal(app.page.document.activeElement.id, 'control-title');
  } finally { await app.close(); }
});

test('card arrows move among populated lanes and Enter retains native activation', async () => {
  const app = await mount([ticketCard(1, 'todo'), ticketCard(2, 'todo'), ticketCard(3, 'review')]);
  try {
    const region = app.page.host.querySelector('.ticket-card-regions');
    const button = (n) => app.page.host.querySelector(`[data-card-id="${cardId(n)}"]`);
    const key = (n, value) => propsOf(region).onKeyDown({ key: value, target: button(n), currentTarget: region,
      nativeEvent: { isComposing: false }, preventDefault() {} });
    button(1).focus();
    key(1, 'ArrowDown');
    assert.equal(app.page.document.activeElement, button(2));
    key(2, 'ArrowRight');
    assert.equal(app.page.document.activeElement, button(3));
    key(3, 'ArrowLeft');
    assert.equal(app.page.document.activeElement, button(1));
    key(1, 'Enter');
    assert.equal(app.page.host.querySelector('.ticket-dialog').getAttribute('open'), null);
    await act(async () => press(button(1)));
    assert.notEqual(app.page.host.querySelector('.ticket-dialog').getAttribute('open'), null);
  } finally { await app.close(); }
});

test('Stop sends nothing until its in-page confirmation and stale views hide it', async () => {
  const app = await mount([ticketCard(1, 'doing', { claimed_by: ADA })]);
  try {
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
    seen.length = 0;
    await act(async () => press(app.page.host.querySelector('[data-action="stop"]')));
    assert.deepEqual(seen, []);
    assert.ok(textOf(app.page.host).includes('Stop this ticket?'));
    await app.render({ ...app.floor, stale: true });
    assert.notEqual(app.page.host.querySelector('.ticket-remove').closest('[hidden]'), null);
    assert.equal(app.page.host.querySelector('.ticket-remove').disabled, true);
    assert.equal(app.page.host.querySelector('[data-action="stop"]'), null);
    await app.render({ ...app.floor, stale: false });
    await act(async () => press(app.page.host.querySelector('.ticket-remove')));
    assert.deepEqual(seen, [['/floor/ticket/stop', { card: cardId(1), expected_seq: 3 }]]);
  } finally { await app.close(); }
});

test('the composer keeps its draft while Hire is opened and the roster changes', async () => {
  const hiredFrom = [];
  const app = await mount([], { agents: [], hireAvailable: true, onHire: (opener) => hiredFrom.push(opener) });
  try {
    const newTicket = app.page.host.querySelector('.ticket-new-toggle');
    await act(async () => press(newTicket));
    const title = app.page.host.querySelector('[name="title"]');
    const note = app.page.host.querySelector('[name="note"]');
    await act(async () => {
      propsOf(title).onChange({ target: { value: 'Preserve filters' } });
      propsOf(note).onChange({ target: { value: 'Keep <b>text</b> literal.' } });
    });
    const hire = app.page.host.querySelector('.ticket-hire');
    await act(async () => press(hire));
    assert.deepEqual(hiredFrom, [hire]);
    await app.render({ ...app.floor, agents: [agent()] });
    assert.equal(app.page.host.querySelector('[name="title"]').value, 'Preserve filters');
    assert.equal(app.page.host.querySelector('[name="note"]').value, 'Keep <b>text</b> literal.');
    assert.notEqual(app.page.host.querySelector('.ticket-dialog').getAttribute('open'), null);
    await act(async () => press(app.page.host.querySelector('.ticket-form-cancel')));
    app.page.flushFocus();
    assert.equal(app.page.document.activeElement, newTicket);
  } finally { await app.close(); }
});

test('the drawer shows pending and an unconfirmed answer without retrying it', async () => {
  const app = await mount([ticketCard(1, 'review', { claimed_by: ADA })]);
  try {
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
    seen.length = 0;
    let finish;
    reply = () => new Promise((resolve) => { finish = resolve; });
    await act(async () => press(app.page.host.querySelector('[data-action="accept"]')));
    assert.ok(textOf(app.page.host).includes('Sending…'));
    assert.equal(app.page.host.querySelector('[data-action="accept"]').disabled, true);
    await act(async () => finish({ status: 200, text: async () => '{}' }));
    assert.ok(textOf(app.page.host).includes('We could not confirm that saved. Look at the board before you try again.'));
    await app.render();
    assert.deepEqual(seen, [['/floor/ticket/accept', { card: cardId(1), expected_seq: 3 }]]);
    assert.notEqual(app.page.host.querySelector('.ticket-dialog').getAttribute('open'), null);
  } finally { await app.close(); }
});

test('a Working ticket becoming Ready revokes its open Stop confirmation', async () => {
  const app = await mount([ticketCard(1, 'doing', { claimed_by: ADA })]);
  try {
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
    await act(async () => press(app.page.host.querySelector('[data-action="stop"]')));
    seen.length = 0;
    await app.render({ ...app.floor, cards: [{ ...app.floor.cards[0], status: 'review', seq: 4 }] });
    assert.equal(app.page.host.querySelector('.ticket-confirm'), null);
    assert.equal(app.page.host.querySelector('[data-action="stop"]'), null);
    assert.deepEqual(app.page.host.querySelectorAll('.ticket-action').map((node) => node.getAttribute('data-action')), ['accept', 'changes', 'remove']);
    assert.ok(!seen.some(([url]) => url.endsWith('/stop')));
  } finally { await app.close(); }
});

test('Give, Edit and changes retain unsent fields while authority is unavailable', async () => {
  for (const [action, status] of [['give', 'todo'], ['edit', 'todo'], ['changes', 'review']]) {
    for (const closed of [{ stale: true }, { ticket: false }]) {
      const app = await mount([ticketCard(1, status, { claimed_by: ADA })]);
      try {
        await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
        await act(async () => press(app.page.host.querySelector(`[data-action="${action}"]`)));
        const mode = app.page.host.querySelector('.ticket-mode');
        const field = mode.querySelector(action === 'edit' ? 'input' : action === 'changes' ? 'textarea' : 'select');
        const value = action === 'give' ? ADA : 'Keep this unsent field.';
        await act(async () => propsOf(field).onChange({ target: { value } }));
        seen.length = 0;
        await app.render({ ...app.floor, ...closed });
        assert.notEqual(mode.getAttribute('hidden'), null);
        assert.equal(app.page.host.querySelector('.ticket-action'), null);
        const form = mode.querySelector('form');
        await act(async () => propsOf(form).onSubmit({ preventDefault() {} }));
        assert.deepEqual(seen, [], `${action} sends nothing while authority is unavailable`);
        await app.render({ ...app.floor, stale: false, ticket: true });
        assert.equal(mode.getAttribute('hidden'), null);
        assert.equal(field.value, value);
      } finally { await app.close(); }
    }
  }
});

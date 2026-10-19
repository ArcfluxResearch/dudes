// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { act, createElement, StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { createServer } from 'vite';
import ts from 'typescript';

import { restoreFocusAfterDialog } from '../src/renderer/focus.ts';
import { fakePage, press } from './helpers/fake-page.mjs';
import { ADA, agent, cardId, subscribe, ticketCard, TWO_PROJECTS, instructionReview, instructionSummary, INSTRUCTION_WATERMARK } from './helpers/tickets.mjs';

let server, store, ticket, detail, git, Control, browserControlDeps;
const seen = [];
let reply;

before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), configFile: false,
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
    state: 'ok', card: { id: body.card, seq: store.getFloorState().cards.find((row) => row.id === body.card)?.seq }, ticket: {
      note: 'Keep the change small.', project: 'service', check: { id: 'default', label: 'default passes' },
      round: 1, reason: null, summary: '<b>Agent text</b>', check_result: null, position: null, waiting: null,
    },
  } : { state: url.endsWith('/stop') ? 'stopped' : 'removed', card: body.card, seq: 4, lsn: '4096' }) });
});

function pageForTickets() {
  const page = fakePage();
  const prototype = Object.getPrototypeOf(page.host);
  const queued = [];
  const modals = [];
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
  prototype.showModal = function () {
    this.setAttribute('open', '');
    if (!modals.includes(this)) modals.push(this);
  };
  prototype.close = function () {
    this.removeAttribute('open');
    const at = modals.indexOf(this);
    if (at !== -1) modals.splice(at, 1);
  };
  prototype.focus = function () {
    if (!this.isConnected || this.disabled || this.closest('[hidden]') !== null) return;
    const dialog = this.closest('dialog');
    if (dialog !== null && dialog.getAttribute('open') === null) return;
    if (modals.length > 0 && !modals.at(-1).contains(this)) return;
    page.document.activeElement = this;
  };
  page.document.querySelector = (selector) => page.document.body.querySelector(selector);
  const restore = page.restore;
  page.restore = () => { restore(); globalThis.requestAnimationFrame = previousRaf; };
  return page;
}

const propsOf = (element) => element[Object.keys(element).find((key) => key.startsWith('__reactProps$'))];
const textOf = (element) => element.nodeType === 3 ? element.nodeValue : element.childNodes.map(textOf).join('');

async function closeHireCallback(page, dialog, opener) {
  const source = await readFile(resolve(import.meta.dirname, '../src/shell.tsx'), 'utf8');
  const syntax = ts.createSourceFile('shell.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let callback, completion, cancellation;
  (function visit(node) {
    if (ts.isFunctionDeclaration(node) && node.name?.text === 'closeHire') callback = node.getText(syntax);
    if (ts.isCallExpression(node) && node.expression.getText(syntax) === 'useEffect'
      && node.arguments[0]?.getText(syntax).includes('markRevealed(reveal)')) completion = node.arguments[0].getText(syntax);
    if (ts.isJsxSelfClosingElement(node) && node.tagName.getText(syntax) === 'HireWizard') {
      cancellation = node.attributes.properties.find((prop) => ts.isJsxAttribute(prop)
        && prop.name.getText(syntax) === 'onClose')?.initializer.expression.getText(syntax);
    }
    ts.forEachChild(node, visit);
  })(syntax);
  assert.ok(callback && completion && cancellation);
  const code = ts.transpileModule(callback, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
  const closeHire = new Function('backdropPress', 'hireDialog', 'hireOpener', 'document', 'setHiring', 'restoreFocusAfterDialog',
    `${code}\nreturn closeHire;`)({ current: null }, { current: dialog }, { current: opener }, page.document,
    () => {}, restoreFocusAfterDialog);
  const selections = [];
  const completed = ts.transpileModule(`const complete = ${completion};`,
    { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
  const completeHire = new Function('reveal', 'hire', 'markRevealed', 'selectAgent', 'closeHire', 'hireDialog',
    'setHiring', 'setGuide', 'setFocused', 'setArchiveHireKept', `${completed}\nreturn complete;`)(ADA,
    { outcome: { kind: 'complete' } }, (id) => selections.push(['revealed', id]), (id) => selections.push(['selected', id]),
    closeHire, { current: dialog }, () => {}, () => {}, () => {}, () => {});
  const cancelHire = new Function('closeHire', `return ${cancellation};`)(closeHire);
  return { closeHire, completeHire, cancelHire, selections };
}

function hireLayer(page) {
  const topbar = page.document.body.appendChild(page.document.createElement('button'));
  topbar.setAttribute('id', 'hire-open');
  const dialog = page.document.body.appendChild(page.document.createElement('dialog'));
  const field = dialog.appendChild(page.document.createElement('input'));
  dialog.showModal();
  field.focus();
  topbar.focus();
  assert.equal(page.document.activeElement, field, 'controls outside the top modal are inert');
  return { topbar, dialog, field };
}

test('Hire Close and Cancel ignore the click event and restore a removed opener fallback', async () => {
  const page = pageForTickets();
  try {
    const opener = page.document.body.appendChild(page.document.createElement('button'));
    const hire = hireLayer(page);
    page.document.body.removeChild(opener);
    const { cancelHire } = await closeHireCallback(page, hire.dialog, opener);
    cancelHire({ type: 'click', currentTarget: hire.field });
    page.flushFocus();
    assert.equal(hire.dialog.getAttribute('open'), null);
    assert.equal(page.document.activeElement.id, 'hire-open');
  } finally { page.restore(); }
});

async function mount(cards, { agents = [agent()], onHire = () => {}, hireAvailable = false, strict = false } = {}) {
  subscribe(store, { cards, agents, checks: TWO_PROJECTS });
  const page = pageForTickets();
  const root = createRoot(page.host);
  let floor = { ...store.getFloorState(), onboarding: hireAvailable ? { workspaces: [] } : null };
  const closes = [];
  const render = async (next = floor) => {
    floor = next;
    const control = createElement(Control, { floor, view: 'board', selectedId: null,
      onClose: () => closes.push('board'), onView() {}, onOpenQuestion() {}, onHire });
    await act(async () => root.render(strict ? createElement(StrictMode, null, control) : control));
    page.flushFocus();
  };
  await render();
  return { page, root, render, closes, get floor() { return floor; },
    async close() { await act(async () => root.unmount()); page.restore(); } };
}

for (const replacement of ['closed', 'another card', 'new draft']) {
  test(`a delayed create cannot replace ${replacement} after the composer is dismissed`, async () => {
    const app = await mount([ticketCard(1, 'todo')], { strict: true });
    try {
      await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
      const title = app.page.host.querySelector('[name="title"]');
      await act(async () => propsOf(title).onChange({ target: { value: 'The submitted ticket' } }));
      let finish;
      const ordinaryReply = reply;
      reply = (url, body) => url === '/floor/ticket' ? new Promise((resolve) => { finish = resolve; }) : ordinaryReply(url, body);
      await act(async () => propsOf(app.page.host.querySelector('.ticket-composer').querySelector('form')).onSubmit({ preventDefault() {} }));
      assert.equal(ticket.getTicketState().sending, true);
      const dialog = app.page.host.querySelector('.ticket-dialog');
      if (replacement === 'closed') {
        await act(async () => propsOf(dialog).onCancel({ preventDefault() {} }));
      } else {
        await act(async () => press(app.page.host.querySelector('.ticket-form-cancel')));
      }
      app.page.flushFocus();
      if (replacement === 'another card') {
        await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
      } else if (replacement === 'new draft') {
        await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
        await act(async () => propsOf(app.page.host.querySelector('[name="title"]')).onChange({ target: { value: 'A newer unsent draft' } }));
      }
      await act(async () => finish({ status: 200, text: async () => JSON.stringify({
        state: 'created', card: cardId(2), seq: 0, lsn: '4097',
      }) }));
      await app.render({ ...app.floor, cards: [...app.floor.cards, { ...ticketCard(2, 'todo'), seq: 0 }] });
      assert.equal(ticket.getTicketState().notice.text, 'Ticket created. Choose Give to agent when it is ready.');
      assert.ok(app.page.host.querySelector(`[data-card-id="${cardId(2)}"]`), 'the saved card remains on the board');
      assert.equal(seen.filter(([url]) => url === '/floor/ticket').length, 1);
      if (replacement === 'closed') {
        assert.equal(dialog.getAttribute('open'), null);
      } else if (replacement === 'another card') {
        assert.equal(dialog.querySelector('.ticket-panel').getAttribute('data-selected-card-id'), cardId(1));
        assert.ok(dialog.querySelector('.ticket-notice') === null, 'create feedback stays outside another card');
      } else {
        assert.equal(dialog.getAttribute('data-composer'), 'true');
        assert.equal(dialog.querySelector('[name="title"]').value, 'A newer unsent draft');
      }
    } finally { await app.close(); }
  });
}

test('a live composer opens its saved card under StrictMode', async () => {
  const app = await mount([], { strict: true });
  try {
    await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
    await act(async () => propsOf(app.page.host.querySelector('[name="title"]')).onChange({ target: { value: 'Open the saved ticket' } }));
    reply = () => ({ status: 200, text: async () => JSON.stringify({ state: 'created', card: cardId(2), seq: 0, lsn: '4097' }) });
    await act(async () => propsOf(app.page.host.querySelector('.ticket-composer').querySelector('form')).onSubmit({ preventDefault() {} }));
    await app.render({ ...app.floor, cards: [{ ...ticketCard(2, 'todo'), seq: 0 }] });
    const dialog = app.page.host.querySelector('.ticket-dialog');
    assert.notEqual(dialog.getAttribute('open'), null);
    assert.equal(dialog.querySelector('.ticket-panel').getAttribute('data-selected-card-id'), cardId(2));
    const notice = dialog.querySelector('.ticket-notice');
    assert.ok(notice !== null, 'the saved card keeps its creation result');
    assert.ok(textOf(notice).includes('Ticket created. Choose Give to agent when it is ready.'));
  } finally { await app.close(); }
});

test('a refused optional assignment stays visible on the auto-opened created card', async () => {
  const app = await mount([], { strict: true });
  try {
    await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
    await act(async () => propsOf(app.page.host.querySelector('[name="title"]')).onChange({ target: { value: 'Keep the saved ticket' } }));
    await act(async () => propsOf(app.page.host.querySelector('[name="agent"]')).onChange({ target: { value: ADA } }));
    const ordinaryReply = reply;
    reply = (url, body) => url === '/floor/ticket'
      ? { status: 200, text: async () => JSON.stringify({ state: 'created', card: cardId(2), seq: 0, lsn: '4097' }) }
      : url.endsWith('/assign')
        ? { status: 409, text: async () => JSON.stringify({ state: 'refused', reason: 'agent_unavailable' }) }
        : ordinaryReply(url, body);
    await act(async () => propsOf(app.page.host.querySelector('.ticket-composer').querySelector('form')).onSubmit({ preventDefault() {} }));
    await app.render({ ...app.floor, cards: [{ ...ticketCard(2, 'todo'), seq: 0 }] });
    const panel = app.page.host.querySelector('.ticket-panel');
    assert.equal(panel.getAttribute('data-selected-card-id'), cardId(2));
    const notice = panel.querySelector('.ticket-notice');
    assert.ok(notice !== null, 'a refused assignment does not hide the created ticket or its result');
    assert.equal(notice.getAttribute('data-tone'), 'problem');
    assert.ok(textOf(notice).includes('Ticket created, but it was not given to Ada. That agent is not available for this project.'));
    assert.deepEqual(seen.filter(([url]) => url === '/floor/ticket' || url.endsWith('/assign')), [
      ['/floor/ticket', { title: 'Keep the saved ticket', project: 'service', done_when: 'default', done_when_revision: 'a'.repeat(64) }],
      ['/floor/ticket/assign', { card: cardId(2), agent: ADA, expected_seq: 0 }],
    ]);
  } finally { await app.close(); }
});

test('the working notice goes once the ticket is ready for review', async () => {
  const app = await mount([], { strict: true });
  try {
    await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
    await act(async () => propsOf(app.page.host.querySelector('[name="title"]')).onChange({ target: { value: 'Say who is working' } }));
    await act(async () => propsOf(app.page.host.querySelector('[name="agent"]')).onChange({ target: { value: ADA } }));
    const ordinaryReply = reply;
    reply = (url, body) => url === '/floor/ticket'
      ? { status: 200, text: async () => JSON.stringify({ state: 'created', card: cardId(2), seq: 0, lsn: '4097' }) }
      : url.endsWith('/assign')
        ? { status: 200, text: async () => JSON.stringify({ state: 'assigned', card: cardId(2), lsn: '4098', seq: 1, start: { state: 'started' } }) }
        : ordinaryReply(url, body);
    await act(async () => propsOf(app.page.host.querySelector('.ticket-composer').querySelector('form')).onSubmit({ preventDefault() {} }));
    await app.render({ ...app.floor, cards: [{ ...ticketCard(2, 'doing', { claimed_by: ADA }), seq: 1 }] });
    assert.ok(textOf(app.page.host.querySelector('.ticket-notice')).includes('Ticket created. Ada is working on it.'));
    await app.render({ ...app.floor, cards: [{ ...ticketCard(2, 'review', { claimed_by: ADA }), seq: 2 }] });
    assert.equal(app.page.host.querySelector('.ticket-notice'), null, 'a ready ticket is not still being worked on');
  } finally { await app.close(); }
});

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

test('closing a stale card skips its disabled opener and replacement for Board', async () => {
  const app = await mount([ticketCard(1, 'review', { claimed_by: ADA })]);
  try {
    const opener = app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`);
    await act(async () => press(opener));
    await app.render({ ...app.floor, stale: true });
    assert.equal(opener.disabled, true);
    await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
    app.page.flushFocus();
    assert.equal(app.page.document.activeElement.id, 'control-title');
    opener.focus();
    assert.equal(app.page.document.activeElement.id, 'control-title', 'disabled cards cannot take focus');
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

test('closing nested Hire stays inside the composer after its opener disappears', async () => {
  const app = await mount([], { agents: [], hireAvailable: true });
  try {
    await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
    const opener = app.page.host.querySelector('.ticket-hire');
    const hire = hireLayer(app.page);
    await app.render({ ...app.floor, agents: [agent()] });
    assert.equal(opener.isConnected, false);
    const { closeHire } = await closeHireCallback(app.page, hire.dialog, opener);
    closeHire();
    app.page.flushFocus();
    assert.equal(app.page.document.activeElement.getAttribute('class'), 'ticket-dialog-close');
    hire.topbar.focus();
    assert.notEqual(app.page.document.activeElement, hire.topbar, 'underlying ticket keeps the top bar inert');
    await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
    app.page.flushFocus();
    hire.dialog.showModal();
    hire.field.focus();
    closeHire();
    app.page.flushFocus();
    assert.equal(app.page.document.activeElement, hire.topbar, 'standalone Hire keeps its normal fallback');
  } finally { await app.close(); }
});

test('closing nested Hire skips a connected Give opener hidden by revoked authority', async () => {
  const app = await mount([ticketCard(1, 'todo')], { agents: [], hireAvailable: true });
  try {
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
    await act(async () => press(app.page.host.querySelector('[data-action="give"]')));
    const opener = app.page.host.querySelector('.ticket-hire');
    const hire = hireLayer(app.page);
    await app.render({ ...app.floor, stale: true });
    assert.equal(opener.isConnected, true);
    assert.equal(opener.disabled, false);
    assert.notEqual(opener.closest('[hidden]'), null);
    (await closeHireCallback(app.page, hire.dialog, opener)).closeHire();
    app.page.flushFocus();
    assert.equal(app.page.document.activeElement.getAttribute('class'), 'ticket-dialog-close');
  } finally { await app.close(); }
});

test('successful Hire preserves selection and returns to the underlying composer', async () => {
  const app = await mount([], { agents: [], hireAvailable: true });
  try {
    await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
    const opener = app.page.host.querySelector('.ticket-hire');
    const hire = hireLayer(app.page);
    await app.render({ ...app.floor, agents: [agent()] });
    const { completeHire, closeHire, selections } = await closeHireCallback(app.page, hire.dialog, opener);
    completeHire();
    app.page.flushFocus();
    assert.deepEqual(selections, [['revealed', ADA], ['selected', ADA]]);
    assert.equal(app.page.document.activeElement.getAttribute('class'), 'ticket-dialog-close');
    await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
    app.page.flushFocus();
    hire.dialog.showModal();
    hire.field.focus();
    closeHire(true);
    const selected = app.page.document.body.appendChild(app.page.document.createElement('h2'));
    selected.focus();
    app.page.flushFocus();
    assert.equal(app.page.document.activeElement, selected, 'standalone completion does not override newly revealed focus');
  } finally { await app.close(); }
});

test('the composer explains revoked authority inside its modal and keeps the draft', async () => {
  for (const closed of [{ stale: true }, { ticket: false }]) {
    const app = await mount([]);
    try {
      await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
      const title = app.page.host.querySelector('[name="title"]');
      await act(async () => propsOf(title).onChange({ target: { value: 'Keep the ticket draft.' } }));
      seen.length = 0;
      await app.render({ ...app.floor, ...closed });
      const dialog = app.page.host.querySelector('.ticket-dialog');
      assert.ok(textOf(dialog).includes(closed.stale ? 'View unconfirmed — waiting for the stream'
        : 'Tickets are off. Turn them on with dudes config set floor.board on, then restart the daemon.'));
      assert.equal(dialog.querySelector('.ticket-create').disabled, true);
      await act(async () => propsOf(dialog.querySelector('form')).onSubmit({ preventDefault() {} }));
      assert.deepEqual(seen, []);
      await app.render({ ...app.floor, stale: false, ticket: true });
      assert.equal(title.value, 'Keep the ticket draft.');
    } finally { await app.close(); }
  }
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
        assert.ok(textOf(app.page.host.querySelector('.ticket-dialog')).includes(closed.stale
          ? 'View unconfirmed — waiting for the stream' : 'Tickets are off. Turn them on with'));
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

test('the instruction drawer accepts continuing work and sends reviewed rework atomically', async () => {
  for (const action of ['accept', 'changes']) {
    reply = (url, body) => ({ status: 200, text: async () => JSON.stringify(url.endsWith('/read') ? {
      state: 'ok', card: { id: body.card, seq: 3 }, ticket: {
        note: 'Read the instructions.', project: 'service', check: { id: 'default', label: 'passes' }, round: 1,
        reason: 'shared_instructions_unknown', instruction_review: instructionReview(),
      },
    } : url.endsWith('/accept') ? { state: 'accepted', card: body.card, seq: 4, lsn: '14', outcome: 'instructions_acknowledged' }
      : { state: 'assigned', card: body.card, seq: 4, lsn: '14', start: { state: 'started' } }) });
    const app = await mount([ticketCard(1, 'blocked', { claimed_by: ADA }, {
      reason: 'shared_instructions_unknown', instruction_review: instructionSummary(),
    })]);
    try {
      await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
      assert.equal(app.page.host.querySelector('[data-action="accept"]').disabled, false);
      assert.ok(textOf(app.page.host).includes('Known changed paths'));
      assert.ok(textOf(app.page.host).includes('read failed'));
      seen.length = 0;
      await act(async () => press(app.page.host.querySelector(`[data-action="${action}"]`)));
      if (action === 'changes') {
        const form = app.page.host.querySelector('.ticket-mode').querySelector('form');
        await act(async () => propsOf(form.querySelector('textarea')).onChange({ target: { value: 'Revise the shared instructions.' } }));
        await act(async () => propsOf(form).onSubmit({ preventDefault() {} }));
      }
      assert.deepEqual(seen, [[action === 'accept' ? '/floor/ticket/accept' : '/floor/ticket/assign', {
        card: cardId(1), expected_seq: 3, review_watermark: INSTRUCTION_WATERMARK,
        ...(action === 'changes' ? { agent: ADA, message: 'Revise the shared instructions.' } : {}),
      }]]);
      assert.ok(textOf(app.page.host).includes(action === 'accept'
        ? 'Instruction review acknowledged. The ticket remains open.' : 'Ada is working on it.'));
    } finally { await app.close(); }
  }
});

for (const timing of ['before selection', 'after selection']) {
  test(`instruction acknowledgement belongs to its card when it arrives ${timing}`, async () => {
    let finish;
    reply = (url, body) => url.endsWith('/read') ? { status: 200, text: async () => JSON.stringify({
      state: 'ok', card: { id: body.card, seq: 3 }, ticket: {
        note: 'Read the instructions.', project: 'service', check: { id: 'default', label: 'passes' }, round: 1,
        reason: 'shared_instructions_unknown', instruction_review: instructionReview(),
      },
    }) } : new Promise((resolve) => { finish = resolve; });
    const app = await mount([1, 2].map((id) => ticketCard(id, 'blocked', { claimed_by: ADA }, {
      reason: 'shared_instructions_unknown', instruction_review: instructionSummary(),
    })), { strict: true });
    try {
      await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
      await act(async () => press(app.page.host.querySelector('[data-action="accept"]')));
      const acknowledge = () => finish({ status: 200, text: async () => JSON.stringify({
        state: 'accepted', card: cardId(1), seq: 4, lsn: '14', outcome: 'instructions_acknowledged',
      }) });
      if (timing === 'before selection') {
        await act(async () => acknowledge());
        assert.ok(textOf(app.page.host.querySelector('.ticket-notice')).includes('Instruction review acknowledged. The ticket remains open.'));
      }
      await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
      await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(2)}"]`)));
      if (timing === 'after selection') await act(async () => acknowledge());
      const panel = app.page.host.querySelector('.ticket-panel');
      assert.equal(panel.getAttribute('data-selected-card-id'), cardId(2));
      assert.ok(panel.querySelector('.ticket-notice') === null, 'the acknowledgement is not evidence about this card');
      assert.ok(textOf(panel).includes('Instruction review'));
      assert.equal(panel.querySelector('[data-action="accept"]').disabled, false, 'the second card still needs its own review');
      await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
      await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
      assert.ok(textOf(app.page.host.querySelector('.ticket-notice')).includes('Instruction review acknowledged. The ticket remains open.'));
      await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
      await act(async () => press(app.page.host.querySelector('.ticket-new-toggle')));
      assert.ok(app.page.host.querySelector('.ticket-composer').querySelector('.ticket-notice') === null, 'card feedback stays outside the composer');
    } finally { await app.close(); }
  });
}

test('another card\'s feedback survives an obsolete timer and dismiss handler', async () => {
  const timeout = globalThis.setTimeout;
  const notices = [];
  globalThis.setTimeout = (run, delay, ...args) => {
    if (delay === 8000) notices.push(run);
    return timeout(run, delay, ...args);
  };
  reply = (url, body) => ({ status: 200, text: async () => JSON.stringify(url.endsWith('/read') ? {
    state: 'ok', card: { id: body.card, seq: 3 }, ticket: {
      note: 'Keep this note.', project: 'service', check: { id: 'default', label: 'passes' }, round: 1,
    },
  } : { state: 'accepted', card: body.card, seq: 4, lsn: '14' }) });
  let app;
  try {
    app = await mount([1, 2].map((id) => ticketCard(id, 'review', { claimed_by: ADA })), { strict: true });
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
    await act(async () => press(app.page.host.querySelector('[data-action="accept"]')));
    const expire = notices.at(-1);
    const dismiss = propsOf(app.page.host.querySelector('.ticket-dismiss')).onClick;
    assert.equal(typeof expire, 'function');
    await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(2)}"]`)));
    await act(async () => press(app.page.host.querySelector('[data-action="accept"]')));
    const newer = ticket.getTicketState().notice;
    await act(async () => { expire(); dismiss(); });
    assert.equal(ticket.getTicketState().notice, newer);
    assert.ok(app.page.host.querySelector('.ticket-panel').querySelector('.ticket-notice') !== null);
    await act(async () => press(app.page.host.querySelector('.ticket-dismiss')));
    assert.ok(app.page.host.querySelector('.ticket-notice') === null, 'the current dismiss still works');
  } finally {
    if (app !== undefined) await app.close();
    globalThis.setTimeout = timeout;
  }
});

test('an old form completion cannot close another ticket\'s form', async () => {
  const app = await mount([1, 2].map((id) => ticketCard(id, 'review', { claimed_by: ADA })), { strict: true });
  try {
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
    await act(async () => press(app.page.host.querySelector('[data-action="changes"]')));
    const firstForm = app.page.host.querySelector('.ticket-mode').querySelector('form');
    const complete = propsOf(firstForm.querySelectorAll('button').find((node) => textOf(node) === 'Cancel')).onClick;
    await act(async () => press(app.page.host.querySelector('.ticket-dialog-close')));
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(2)}"]`)));
    await act(async () => press(app.page.host.querySelector('[data-action="changes"]')));
    const secondForm = app.page.host.querySelector('.ticket-mode').querySelector('form');
    await act(async () => propsOf(secondForm.querySelector('textarea')).onChange({ target: { value: 'Keep this newer draft.' } }));
    await act(async () => complete());
    assert.ok(app.page.host.querySelector('.ticket-mode').querySelector('form') === secondForm);
    assert.equal(secondForm.querySelector('textarea').value, 'Keep this newer draft.');
  } finally { await app.close(); }
});

test('reviewed rework cannot edit a retained shortened note during a loading or failed refresh', async () => {
  const fullNote = 'The complete original note\nKeep every byte after the review.';
  const readReply = (seq, note, fields = {}) => ({ status: 200, text: async () => JSON.stringify({
    state: 'ok', card: { id: cardId(1), seq }, ticket: {
      note, project: 'service', check: { id: 'default', label: 'passes' }, round: 1, ...fields,
    },
  }) });
  reply = (url) => url.endsWith('/read') ? readReply(3, 'The complete original', {
    instruction_review: instructionReview(), truncated: ['note'],
  }) : { status: 200, text: async () => JSON.stringify({
    state: 'assigned', card: cardId(1), seq: 4, lsn: '14', start: { state: 'queued', position: 1, wait: 'agent_paused' },
  }) };
  const app = await mount([ticketCard(1, 'blocked', { claimed_by: ADA }, {
    reason: 'shared_instructions_unknown', instruction_review: instructionSummary(),
  })]);
  try {
    await act(async () => press(app.page.host.querySelector(`[data-card-id="${cardId(1)}"]`)));
    await act(async () => press(app.page.host.querySelector('[data-action="changes"]')));
    const rework = app.page.host.querySelector('.ticket-mode').querySelector('form');
    await act(async () => propsOf(rework.querySelector('textarea')).onChange({ target: { value: 'Review the instructions.' } }));
    await act(async () => propsOf(rework).onSubmit({ preventDefault() {} }));
    assert.deepEqual(seen.find(([url]) => url.endsWith('/assign'))[1], {
      card: cardId(1), expected_seq: 3, agent: ADA, message: 'Review the instructions.', review_watermark: INSTRUCTION_WATERMARK,
    });
    let finish;
    reply = () => new Promise((resolve) => { finish = resolve; });
    subscribe(store, { agents: [agent()], cards: [ticketCard(1, 'claimed', { claimed_by: ADA, seq: 4 }, { position: 1 })], checks: TWO_PROJECTS });
    await app.render(store.getFloorState());
    assert.equal(detail.getDetailState().phase, 'loading');
    assert.deepEqual(detail.getDetailState().detail.truncated, ['note']);
    const edit = app.page.host.querySelector('[data-action="edit"]');
    assert.equal(edit.disabled, true);
    await act(async () => propsOf(edit).onClick());
    assert.equal(app.page.host.querySelector('.ticket-mode').querySelector('form'), null);
    const unsafeChange = { verb: 'edit', title: 'Ticket 1', note: 'The complete original', check: null };
    await act(async () => { assert.equal(await ticket.changeTicket(store.getFloorState().cards[0], unsafeChange), false); });
    await act(async () => finish({ status: 503, text: async () => '' }));
    assert.equal(detail.getDetailState().phase, 'failed');
    assert.equal(app.page.host.querySelector('[data-action="edit"]').disabled, true);
    await act(async () => { assert.equal(await ticket.changeTicket(store.getFloorState().cards[0], unsafeChange), false); });
    assert.deepEqual(seen.filter(([url]) => url.endsWith('/edit')), []);
    const reread = app.page.host.querySelectorAll('button').find((node) => textOf(node) === 'Read again');
    assert.ok(reread);
    reply = () => readReply(4, 'The complete original', { truncated: ['note'] });
    await act(async () => press(reread));
    assert.equal(detail.getDetailState().phase, 'ready');
    assert.equal(detail.getDetailState().seq, 4);
    assert.equal(app.page.host.querySelector('[data-action="edit"]').disabled, true);
    await act(async () => { assert.equal(await ticket.changeTicket(store.getFloorState().cards[0], unsafeChange), false); });
    reply = (url) => url.endsWith('/read') ? readReply(4, fullNote)
      : { status: 200, text: async () => JSON.stringify({ state: 'edited', card: cardId(1), seq: 5, lsn: '15' }) };
    const freshRead = app.page.host.querySelectorAll('button').find((node) => textOf(node) === 'Read again');
    await act(async () => press(freshRead));
    assert.equal(detail.getDetailState().phase, 'ready');
    assert.equal(app.page.host.querySelector('[data-action="edit"]').disabled, false);
    await act(async () => press(app.page.host.querySelector('[data-action="edit"]')));
    const form = app.page.host.querySelector('.ticket-mode').querySelector('form');
    assert.equal(propsOf(form.querySelector('textarea')).value, fullNote);
    await act(async () => propsOf(form).onSubmit({ preventDefault() {} }));
    const edits = seen.filter(([url]) => url.endsWith('/edit'));
    assert.equal(edits.length, 1);
    assert.equal(edits[0][1].expected_seq, 4);
    assert.equal(edits[0][1].description, fullNote);
  } finally { await app.close(); }
});

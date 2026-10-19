// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import {
  createNotificationPreference, deliverDesktopNotification, notificationArrivals,
  notificationDestination, seedNotificationHistory,
} from '../src/renderer/desktopNotifications.ts';
import * as store from '../src/renderer/floor/store.ts';
import { readDesktopNotificationChoice, writeDesktopNotificationChoice } from '../src/renderer/view.ts';
import { ADA, GRACE, agent, cardId, plainCard, subscribe, ticketCard } from './helpers/tickets.mjs';

beforeEach(() => { store.resetFloorStore(); });

function floor(cards = [], agents = [agent()]) {
  subscribe(store, { cards, agents });
  return store.getFloorState();
}
const event = (floor, kind = 'delta') => ({ kind, floor, snapshotGeneration: 1, previousMessagesLsn: null });

test('only confirmed ticket arrivals and durable questions produce notices', () => {
  const start = floor([ticketCard(1, 'doing', { claimed_by: ADA })]);
  const seen = seedNotificationHistory(start);
  const next = { ...start, cards: [
    ticketCard(1, 'review', { claimed_by: ADA }),
    ticketCard(2, 'blocked', {}, { reason: 'check_cannot_run' }),
    ticketCard(3, 'doing', { claimed_by: GRACE }), { ...plainCard(4, 'review'), ticket: null },
  ], agents: [agent(), agent(GRACE, 'Grace', { status: 'waiting', question: 'private question', question_lsn: '9007199254740993' })] };
  const arrivals = notificationArrivals(seen, event(next));
  assert.deepEqual(arrivals.notices.map(({ title, body }) => ({ title, body })), [
    { title: 'Ticket 1', body: 'Ada finished. Ready for you.' },
    { title: 'Ticket 2', body: "Needs you. The check cannot run: its program is missing, or it is a file inside the project that an agent could change. Install the program, or use one outside the project's repository. To use a script from the repository, run it through its interpreter, for example `sh test.sh` for a shell script; an agent can still edit that script, so review its changes before you accept the work. Then Give to agent. If the check itself is wrong, Remove this ticket and create it again." },
    { title: 'Ticket 3', body: 'Grace is asking a question.' },
  ]);
  assert.deepEqual(arrivals.notices.map((notice) => notificationDestination(notice, next)), [
    { kind: 'ticket', id: cardId(1) }, { kind: 'ticket', id: cardId(2) }, { kind: 'ticket', id: cardId(3) },
  ]);
  assert.deepEqual(notificationArrivals(arrivals.history, event(next)).notices, []);
  const ended = { ...start, agents: [agent(ADA, 'Ada', { status: 'idle', turns: 1 })] };
  assert.deepEqual(notificationArrivals(seen, event(ended)).notices, []);
  for (const unconfirmed of [{ ...next, stale: true }, { ...next, phase: 'closed' }, { ...next, snapshotEpoch: next.epoch - 1 }]) {
    assert.deepEqual(notificationArrivals(seen, event(unconfirmed)).notices, []);
  }
});

test('initial, refreshed and reconnected snapshots silently baseline waiting items', () => {
  const waiting = floor([ticketCard(1, 'review')]);
  let step = notificationArrivals(null, event(waiting));
  assert.deepEqual(step.notices, []);
  const more = { ...waiting, cards: [...waiting.cards, ticketCard(2, 'blocked')] };
  step = notificationArrivals(step.history, event(more, 'snapshot'));
  assert.deepEqual(step.notices, []);
  assert.deepEqual(notificationArrivals(step.history, event(more)).notices, []);
  const reconnected = { ...more, epoch: more.epoch + 1, snapshotEpoch: more.epoch + 1 };
  step = notificationArrivals(step.history, event(reconnected));
  assert.deepEqual(step.notices, []);
  const ready = { ...reconnected, cards: [...more.cards, ticketCard(3, 'review')] };
  assert.deepEqual(notificationArrivals(step.history, event(ready)).notices.map(({ title, body }) => ({ title, body })),
    [{ title: 'Ticket 3', body: 'Ready for you.' }]);
});

test('questions retain exact sequence identity, never carry text and use an agent drawer without a ticket', () => {
  const start = floor();
  const question = { ...start, agents: [agent(ADA, '<Ada>', { status: 'waiting', question: 'same secret', question_lsn: '18446744073709551614' })] };
  let step = notificationArrivals(seedNotificationHistory(start), event(question));
  assert.equal(step.notices[0].title, '<Ada>');
  assert.equal(step.notices[0].body, '<Ada> is asking a question.');
  assert.deepEqual(notificationDestination(step.notices[0], question), { kind: 'question', id: ADA });
  const replaced = { ...question, agents: [{ ...question.agents[0], question_lsn: '18446744073709551615' }] };
  assert.equal(notificationDestination(step.notices[0], replaced), null);
  step = notificationArrivals(step.history, event(replaced));
  assert.deepEqual(step.notices.map(({ title, body }) => ({ title, body })), [{ title: '<Ada>', body: '<Ada> is asking a question.' }]);
  assert.doesNotMatch(JSON.stringify(step.notices), /same secret/);
  for (const fields of [{ status: 'idle' }, { question_lsn: '0' }, { question_lsn: '18446744073709551616' }, { question: '' }]) {
    assert.deepEqual(notificationArrivals(seedNotificationHistory(start), event({ ...question,
      agents: [{ ...question.agents[0], ...fields }] })).notices, []);
  }
});

test('ticket transitions can return after work, and notification text excludes other row fields', () => {
  const start = floor();
  const blocked = { ...start, cards: [ticketCard(1, 'blocked', {
    title: '<Fix login>\u202E', summary: 'private summary', notes: 'private notes', claimed_by: ADA,
  }, { reason: 'no_turns' })], agents: [agent(ADA, 'Ada\u2066', { goal: { text: 'private goal' } })] };
  let step = notificationArrivals(seedNotificationHistory(start), event(blocked));
  assert.deepEqual(step.notices.map(({ title, body }) => ({ title, body })), [{ title: '<Fix login>',
    body: 'Needs you. The agent used all its turns on this ticket. Give to agent to try again, or Stop.' }]);
  assert.doesNotMatch(JSON.stringify(step.notices), /private/);
  step = notificationArrivals(step.history, event(start));
  step = notificationArrivals(step.history, event({ ...blocked, cards: [{ ...blocked.cards[0], status: 'review' }] }));
  assert.deepEqual(step.notices.map(({ title, body }) => ({ title, body })), [{ title: '<Fix login>', body: 'Ada finished. Ready for you.' }]);
});

function preferencePorts(permission = 'default') {
  const values = new Map();
  return {
    values, requests: 0, currentPermission: permission,
    storage: () => ({ getItem: (key) => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) }),
    readChoice() { return readDesktopNotificationChoice(this.storage); },
    writeChoice(enabled) { return writeDesktopNotificationChoice(enabled, this.storage); },
    permission() { return this.currentPermission; },
    async requestPermission() { this.requests += 1; this.currentPermission = 'granted'; return 'granted'; },
  };
}

test('permission is requested only by opt-in, remembers this browser and can be turned off', async () => {
  const ports = preferencePorts();
  const preference = createNotificationPreference(ports);
  assert.deepEqual(preference.getState(), { kind: 'off', remembered: true });
  assert.equal(ports.requests, 0);
  await preference.setEnabled(true);
  assert.deepEqual(preference.getState(), { kind: 'on', remembered: true });
  assert.equal(ports.requests, 1);
  assert.deepEqual(createNotificationPreference(ports).getState(), { kind: 'on', remembered: true });
  await preference.setEnabled(false);
  assert.deepEqual(preference.getState(), { kind: 'off', remembered: true });
  assert.deepEqual(createNotificationPreference(ports).getState(), { kind: 'off', remembered: true });
});

test('permission denial and unavailable storage keep a usable banner fallback', async () => {
  const denied = preferencePorts('denied');
  const preference = createNotificationPreference(denied);
  await preference.setEnabled(true);
  assert.deepEqual(preference.getState(), { kind: 'denied', remembered: true });
  assert.equal(denied.requests, 0);
  for (const unavailable of [() => { throw Error('storage getter'); }, () => ({ getItem() { throw Error('read'); }, setItem() { throw Error('write'); } })]) {
    const ports = preferencePorts('granted');
    ports.storage = unavailable;
    const local = createNotificationPreference(ports);
    assert.equal(local.getState().kind, 'off');
    await local.setEnabled(true);
    assert.deepEqual(local.getState(), { kind: 'on', remembered: false });
    await local.setEnabled(false);
    assert.deepEqual(local.getState(), { kind: 'off', remembered: false });
  }
  const unsupported = createNotificationPreference(preferencePorts('unsupported'));
  await unsupported.setEnabled(true);
  assert.equal(unsupported.getState().kind, 'unsupported');
  const rejected = preferencePorts();
  rejected.requestPermission = async () => { throw Error('permission unavailable'); };
  const unavailable = createNotificationPreference(rejected);
  await unavailable.setEnabled(true);
  assert.equal(unavailable.getState().kind, 'unavailable');
});

test('turning off during a permission prompt cannot turn notifications back on', async () => {
  const ports = preferencePorts();
  let finish;
  ports.requestPermission = () => new Promise((resolve) => { finish = resolve; });
  const preference = createNotificationPreference(ports);
  const pending = preference.setEnabled(true);
  assert.equal(preference.getState().kind, 'asking');
  await preference.setEnabled(false);
  finish('granted');
  await pending;
  assert.deepEqual(preference.getState(), { kind: 'off', remembered: true });
});

test('Settings offers opt-in before a daemon report, renders denial guidance and reports unsaved browser choice', async () => {
  let permission = 'default';
  let requests = 0;
  let saved = false;
  const writeChoice = (_key, value) => { saved = value === 'true'; };
  const previousNotification = globalThis.Notification;
  const previousWindow = globalThis.window;
  globalThis.Notification = class {
    static get permission() { return permission; }
    static async requestPermission() { requests += 1; permission = 'granted'; return permission; }
  };
  globalThis.window = { localStorage: {
    getItem: () => saved ? 'true' : null,
    setItem: writeChoice,
  } };
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { SettingsView } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const { desktopNotificationPreference, createNotificationPreference: reloadPreference }
      = await server.ssrLoadModule('/src/renderer/desktopNotifications.ts');
    const html = () => renderToStaticMarkup(createElement(SettingsView, { floor: store.getFloorState(), onSetup() {} }));
    assert.match(html(), /Desktop notifications/);
    assert.match(html(), /aria-pressed="false"[^>]*>Turn notifications on/);
    assert.equal(requests, 0);
    await desktopNotificationPreference.setEnabled(true);
    assert.match(html(), /aria-pressed="true"[^>]*>Turn notifications off/);
    assert.equal(requests, 1);
    globalThis.window.localStorage.setItem = () => { throw Error('storage write'); };
    await desktopNotificationPreference.setEnabled(false);
    assert.deepEqual(desktopNotificationPreference.getState(), { kind: 'off', remembered: false });
    assert.equal(saved, true);
    assert.match(html(), /aria-pressed="false"[^>]*>Turn notifications on/);
    assert.match(html(), /choice lasts only for this page/);
    assert.deepEqual(reloadPreference().getState(), { kind: 'on', remembered: true });
    globalThis.window.localStorage.setItem = writeChoice;
    await desktopNotificationPreference.setEnabled(false);
    permission = 'denied';
    await desktopNotificationPreference.setEnabled(true);
    assert.match(html(), /Notifications are blocked/);
    assert.match(html(), /browser&#x27;s site settings/);
    assert.match(html(), /banner still shows/);
    permission = 'granted';
    globalThis.window.localStorage.setItem = () => { throw Error('storage write'); };
    await desktopNotificationPreference.setEnabled(true);
    assert.match(html(), /aria-pressed="true"/);
    assert.match(html(), /choice lasts only for this page/);
  } finally {
    await server.close();
    globalThis.Notification = previousNotification;
    globalThis.window = previousWindow;
  }
});

test('events observed while off do not become a backlog on opt-in, and a refused constructor falls back', async () => {
  const previousNotification = globalThis.Notification;
  globalThis.Notification = class {
    static permission = 'granted';
    constructor() { throw Error('browser could not show notification'); }
  };
  try {
    const start = floor();
    const ready = { ...start, cards: [ticketCard(1, 'review')] };
    const preference = createNotificationPreference(preferencePorts('granted'));
    const observed = notificationArrivals(seedNotificationHistory(start), event(ready));
    assert.equal(deliverDesktopNotification(observed.notices[0], preference, () => {}, () => ready), false);
    await preference.setEnabled(true);
    assert.deepEqual(notificationArrivals(observed.history, event(ready)).notices, []);
    const next = { ...ready, cards: [...ready.cards, ticketCard(2, 'blocked')] };
    const arrivals = notificationArrivals(observed.history, event(next));
    assert.equal(arrivals.notices[0].title, 'Ticket 2');
    assert.equal(deliverDesktopNotification(arrivals.notices[0], preference, () => {}, () => next), false);
    assert.equal(preference.getState().kind, 'unavailable');
  } finally { globalThis.Notification = previousNotification; }
});

test('delivery requires opt-in and granted permission, focuses the tab and opens a current target on click', async () => {
  const seen = [];
  let focus = 0;
  const previousNotification = globalThis.Notification;
  const previousWindow = globalThis.window;
  globalThis.Notification = class {
    static permission = 'granted';
    constructor(title, options) { this.title = title; this.options = options; seen.push(this); }
    addEventListener(kind, listener) { assert.equal(kind, 'click'); this.listener = listener; }
    close() { this.closed = true; }
  };
  globalThis.window = { focus() { focus += 1; } };
  try {
    const start = floor();
    const ready = { ...start, cards: [ticketCard(1, 'review', { title: '<Fix login>', claimed_by: ADA })] };
    const notice = notificationArrivals(seedNotificationHistory(start), event(ready)).notices[0];
    const ports = preferencePorts('granted');
    const preference = createNotificationPreference(ports);
    const opened = [];
    let current = ready;
    assert.equal(deliverDesktopNotification(notice, preference, (target) => opened.push(target), () => current), false);
    await preference.setEnabled(true);
    assert.equal(deliverDesktopNotification(notice, preference, (target) => opened.push(target), () => current), true);
    assert.deepEqual({ title: seen[0].title, body: seen[0].options.body }, { title: '<Fix login>', body: 'Ada finished. Ready for you.' });
    seen[0].listener();
    assert.equal(focus, 1);
    assert.deepEqual(opened, [{ kind: 'ticket', id: cardId(1) }]);
    assert.equal(seen[0].closed, true);
    deliverDesktopNotification(notice, preference, (target) => opened.push(target), () => current);
    current = { ...ready, stale: true };
    seen[1].listener();
    assert.deepEqual(opened, [{ kind: 'ticket', id: cardId(1) }]);
    globalThis.Notification.permission = 'denied';
    ports.currentPermission = 'denied';
    assert.equal(deliverDesktopNotification(notice, preference, (target) => opened.push(target), () => ready), false);
    assert.equal(preference.getState().kind, 'denied');
  } finally {
    globalThis.Notification = previousNotification;
    globalThis.window = previousWindow;
  }
});

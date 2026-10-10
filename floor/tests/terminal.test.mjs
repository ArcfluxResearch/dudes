// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The terminal channel, driven through a fake socket and a fake screen, and
// its views rendered to markup. What is asserted is what crosses the wire and
// what an operator reads: which frames the page sends, which bytes reach the
// screen, and how each way a channel ends is said.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import {
  MAX_CHUNK_BYTES, abandonTerminal, attachTerminal, bindTerminalScreen, detachTerminal, dismissTerminal, fitGrid,
  getTerminalState, leavesTerminal, resetTerminal, resizeTerminal, terminalOffer, terminalUrl, writeTerminal,
} from '../src/renderer/terminal/terminal.ts';

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const GRACE = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';
const CSRF = 'p8Zt3Qd1Kx9Lm2Nv5Rb7Yc0Ae4Gh6Jk8Sw1Uz3Xq0';
const HANDLE = 'NVXX_l8kLbkVqyiQYpmOw9dabOgXjcb6SaghVuoDpNw';
const HERE = { protocol: 'http:', host: '127.0.0.1:7717' };
const SESSION = '5f0c1d2e-0000-4000-8000-000000000608';

function fakeSocket() {
  const listeners = new Map();
  const socket = {
    sent: [],
    closed: 0,
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    send(data) {
      socket.sent.push(data);
    },
    close() {
      socket.closed += 1;
    },
    emit(type, event) {
      listeners.get(type)?.(event);
    },
  };
  return socket;
}

function portsFor(socket, csrf = CSRF) {
  const calls = [];
  return {
    calls,
    location: HERE,
    socket: (url, protocols) => {
      calls.push({ url, protocols: [...protocols] });
      return socket;
    },
    csrf: () => csrf,
  };
}

function fakeScreen(cols = 100, rows = 30) {
  const screen = {
    shown: { cols, rows },
    written: [],
    size: () => screen.shown,
    write: (bytes) => screen.written.push(...bytes),
  };
  return screen;
}

const frames = (socket) => socket.sent.map((text) => JSON.parse(text));
const frame = (value) => ({ data: JSON.stringify(value) });
const ATTACHED = frame({ type: 'terminal.attached', handle: HANDLE });

/** A channel for Ada that has opened, been drawn and been granted. */
function attached(screen = fakeScreen()) {
  const socket = fakeSocket();
  attachTerminal(ADA, portsFor(socket));
  socket.emit('open');
  bindTerminalScreen(screen);
  socket.emit('message', ATTACHED);
  return { socket, screen };
}

/** How a channel ended, or its phase if it has not. */
function ending() {
  const state = getTerminalState();
  return state.phase === 'ended' ? state.ending : state.phase;
}

beforeEach(() => {
  resetTerminal();
});

// --- the wire ---------------------------------------------------------------------------

test('the channel opens at the terminal route with the two offers this session authenticated with', () => {
  const socket = fakeSocket();
  const ports = portsFor(socket);
  attachTerminal(ADA, ports);
  assert.deepEqual(ports.calls, [{
    url: 'ws://127.0.0.1:7717/floor/terminal', protocols: ['dudes-terminal-v1', `dudes-csrf.${CSRF}`],
  }]);
  assert.equal(terminalUrl({ protocol: 'https:', host: 'floor.test:9' }), 'wss://floor.test:9/floor/terminal');
  assert.deepEqual({ ...getTerminalState(), id: 0 }, { phase: 'opening', id: 0, agent: ADA });
  // A second Attach while one channel is open opens nothing.
  attachTerminal(GRACE, ports);
  assert.equal(ports.calls.length, 1);
});

test('a page with no session opens no channel, and says so', () => {
  const socket = fakeSocket();
  const ports = portsFor(socket, null);
  attachTerminal(ADA, ports);
  assert.deepEqual(ports.calls, []);
  assert.equal(ending(), 'unopened');
});

test('attach waits for both the socket and a screen, then asks once with the screen\'s size', () => {
  for (const order of [['screen', 'open'], ['open', 'screen']]) {
    resetTerminal();
    const socket = fakeSocket();
    attachTerminal(ADA, portsFor(socket));
    const screen = fakeScreen(132, 41);
    for (const step of order) {
      assert.deepEqual(socket.sent, [], `${order.join(' then ')}: sent before both were ready`);
      if (step === 'open') socket.emit('open');
      else bindTerminalScreen(screen);
    }
    assert.deepEqual(frames(socket), [{ type: 'terminal.attach', agent: ADA, cols: 132, rows: 41 }], order.join(' then '));
    assert.equal(getTerminalState().phase, 'attaching');
    bindTerminalScreen(screen);
    socket.emit('open');
    assert.equal(socket.sent.length, 1, 'a second open or bind asked again');
  }
});

test('output reaches the screen byte for byte, and only from an attached channel', () => {
  const { socket, screen } = attached();
  assert.equal(getTerminalState().phase, 'attached');
  socket.emit('message', frame({ type: 'terminal.data', data: 'G1szODsyOzI1NTs2NDswbWZnG1s0ODsyOzA7NjQ7MjU1bWJnG1swbSDinJM=' }));
  assert.deepEqual(screen.written, [27, 91, 51, 56, 59, 50, 59, 50, 53, 53, 59, 54, 52, 59, 48, 109, 102, 103, 27, 91,
    52, 56, 59, 50, 59, 48, 59, 54, 52, 59, 50, 53, 53, 109, 98, 103, 27, 91, 48, 109, 32, 226, 156, 147]);

  resetTerminal();
  const early = fakeSocket();
  attachTerminal(ADA, portsFor(early));
  early.emit('open');
  const blank = fakeScreen();
  bindTerminalScreen(blank);
  early.emit('message', frame({ type: 'terminal.data', data: 'aGk=' }));
  assert.deepEqual(blank.written, [], 'output drawn before the grant');
  assert.equal(ending(), 'lost');
});

test('the operator\'s typing goes out as terminal.write under the grant, split at 65,536 bytes', () => {
  const { socket } = attached();
  assert.deepEqual(frames(socket).map(({ type }) => type), ['terminal.attach'], 'something but attach was sent');
  writeTerminal('ls -la\r');
  writeTerminal('é');
  writeTerminal('');
  assert.deepEqual(frames(socket).slice(1), [
    { type: 'terminal.write', handle: HANDLE, data: 'bHMgLWxhDQ==' },
    { type: 'terminal.write', handle: HANDLE, data: 'w6k=' },
  ]);
  writeTerminal('x'.repeat(MAX_CHUNK_BYTES + 3));
  const pasted = frames(socket).slice(3);
  assert.deepEqual(pasted.map(({ data }) => atob(data).length), [65_536, 3]);
  assert.equal(pasted.map(({ data }) => atob(data)).join(''), 'x'.repeat(65_539));
});

test('a write is dropped unless the channel is attached', () => {
  const socket = fakeSocket();
  attachTerminal(ADA, portsFor(socket));
  writeTerminal('opening');
  socket.emit('open');
  bindTerminalScreen(fakeScreen());
  writeTerminal('attaching');
  socket.emit('message', ATTACHED);
  detachTerminal();
  writeTerminal('detaching');
  socket.emit('message', frame({ type: 'terminal.detached' }));
  writeTerminal('ended');
  assert.deepEqual(frames(socket).map(({ type }) => type), ['terminal.attach', 'terminal.detach']);
});

test('resize tells the daemon a changed size, once, and only while attached', () => {
  const socket = fakeSocket();
  attachTerminal(ADA, portsFor(socket));
  const screen = fakeScreen(100, 30);
  bindTerminalScreen(screen);
  socket.emit('open');
  // The view refits while the daemon is still attaching: it is told once the grant arrives.
  screen.shown = { cols: 120, rows: 40 };
  resizeTerminal();
  socket.emit('message', ATTACHED);
  resizeTerminal();
  screen.shown = { cols: 90, rows: 20 };
  resizeTerminal();
  resizeTerminal();
  assert.deepEqual(frames(socket).slice(1), [
    { type: 'terminal.resize', handle: HANDLE, cols: 120, rows: 40 },
    { type: 'terminal.resize', handle: HANDLE, cols: 90, rows: 20 },
  ]);
});

test('detach waits for the daemon\'s confirmation, and output before it still draws', () => {
  const { socket, screen } = attached();
  detachTerminal();
  assert.deepEqual(frames(socket).at(-1), { type: 'terminal.detach', handle: HANDLE });
  assert.equal(getTerminalState().phase, 'detaching');
  assert.equal(socket.closed, 0);
  socket.emit('message', frame({ type: 'terminal.data', data: 'b2s=' }));
  assert.deepEqual(screen.written, [111, 107]);
  socket.emit('message', frame({ type: 'terminal.detached' }));
  assert.equal(ending(), 'detached');
  assert.equal(socket.closed, 1, 'the page keeps a channel it no longer needs');
  // A late event from the closed channel changes nothing.
  const ended = getTerminalState();
  socket.emit('close', { code: 1006 });
  assert.equal(getTerminalState(), ended);
  dismissTerminal();
  assert.deepEqual(getTerminalState(), { phase: 'idle' });
});

test('each way a channel closes is one ending', () => {
  const opened = () => {
    const socket = fakeSocket();
    attachTerminal(ADA, portsFor(socket));
    socket.emit('open');
    bindTerminalScreen(fakeScreen());
    return socket;
  };
  const cases = {
    'closed while attaching': [() => opened().emit('message', frame({ type: 'terminal.closed' })), 'refused'],
    'closed while attached': [() => attached().socket.emit('message', frame({ type: 'terminal.closed' })), 'closed'],
    'closed while detaching': [() => {
      const { socket } = attached();
      detachTerminal();
      socket.emit('message', frame({ type: 'terminal.closed' }));
    }, 'unconfirmed'],
    'a policy close': [() => attached().socket.emit('close', { code: 1008 }), 'policy'],
    'a plain close': [() => attached().socket.emit('close', { code: 1000 }), 'lost'],
    'an abnormal close': [() => attached().socket.emit('close', { code: 1006 }), 'lost'],
    'a close before the socket opened': [() => {
      const socket = fakeSocket();
      attachTerminal(ADA, portsFor(socket));
      socket.emit('close', { code: 1006 });
    }, 'unopened'],
    'a frame that is not JSON': [() => attached().socket.emit('message', { data: '{' }), 'lost'],
    'an unlisted type': [() => attached().socket.emit('message', frame({ type: 'terminal.hire' })), 'lost'],
    'an extra field': [() => attached().socket.emit('message', frame({ type: 'terminal.closed', reason: 'x' })), 'lost'],
    'data that is not canonical base64': [
      () => attached().socket.emit('message', frame({ type: 'terminal.data', data: 'aGk' })), 'lost'],
    'a handle that is not canonical': [() => opened().emit('message', frame({ type: 'terminal.attached', handle: 'x' })),
      'lost'],
    'detached without a detach': [() => attached().socket.emit('message', frame({ type: 'terminal.detached' })), 'lost'],
    'a page that could not load its terminal': [() => {
      attachTerminal(ADA, portsFor(fakeSocket()));
      abandonTerminal();
    }, 'unloaded'],
  };
  for (const [label, [act, expected]] of Object.entries(cases)) {
    resetTerminal();
    act();
    assert.equal(ending(), expected, label);
  }
});

test('the handle stays out of the state a view reads', () => {
  const { socket } = attached();
  assert.doesNotMatch(JSON.stringify(getTerminalState()), new RegExp(HANDLE));
  detachTerminal();
  assert.doesNotMatch(JSON.stringify(getTerminalState()), new RegExp(HANDLE));
  // The control: the same search finds the handle in what the channel sent.
  assert.match(JSON.stringify(frames(socket)), new RegExp(HANDLE));
});

// --- fitting the grid ---------------------------------------------------------------------

test('the grid fits its box in whole cells, beside the scrollbar, within the daemon\'s bounds', () => {
  // 80 by 24 cells drawn in 640 by 384 pixels: each cell is 8 by 16.
  const shown = { cols: 80, rows: 24, width: 640, height: 384 };
  assert.deepEqual(fitGrid({ width: 814, height: 420 }, shown), { cols: 100, rows: 26 });
  assert.deepEqual(fitGrid({ width: 10, height: 3 }, shown), { cols: 2, rows: 1 });
  assert.deepEqual(fitGrid({ width: 1e9, height: 1e9 }, shown), { cols: 32_767, rows: 32_767 });
  assert.equal(fitGrid({ width: 814, height: 420 }, { ...shown, width: 0 }), null);
});

// --- leaving the grid by keyboard ----------------------------------------------------------

test('Ctrl+] alone leaves the grid, and Tab and Shift+Tab stay with the CLI', () => {
  const key = (fields) => leavesTerminal({ ctrlKey: false, altKey: false, metaKey: false, shiftKey: false,
    code: 'BracketRight', ...fields });
  assert.equal(key({ ctrlKey: true }), true);
  for (const [label, fields] of Object.entries({
    ']': {}, 'Ctrl+Shift+]': { ctrlKey: true, shiftKey: true }, 'Ctrl+Alt+]': { ctrlKey: true, altKey: true },
    'Cmd+Ctrl+]': { ctrlKey: true, metaKey: true }, 'Ctrl+[': { ctrlKey: true, code: 'BracketLeft' },
    Tab: { code: 'Tab' }, 'Shift+Tab': { code: 'Tab', shiftKey: true }, 'Ctrl+Tab': { code: 'Tab', ctrlKey: true },
  })) {
    assert.equal(key(fields), false, `${label} left the grid`);
  }
});

// --- what the detail offers ----------------------------------------------------------------

function floorOf(rows, fields = {}) {
  return {
    phase: 'live', stale: false, epoch: 4, snapshotEpoch: 4, lsn: 9, agents: rows, terminal: true, control: false,
    answering: null, onboarding: null, rejected: 0, discarded: 0, doneNotices: [], ...fields,
  };
}

function row(fields = {}) {
  return { id: ADA, name: 'Ada', status: 'idle', mode: 'managed', halted: false, session: SESSION, ...fields };
}

test('Attach is offered for a Managed agent between turns with a confirmed session, and why not otherwise', () => {
  const idle = { phase: 'idle' };
  const offer = (fields, channel = idle) => terminalOffer(floorOf([row(fields)]), row(fields), channel);
  assert.deepEqual(offer({}), { kind: 'attach', agent: ADA });
  assert.deepEqual(offer({ status: 'running' }), { kind: 'unavailable', reason: 'running' });
  assert.deepEqual(offer({ mode: 'attached', status: 'attached' }), { kind: 'unavailable', reason: 'attached' });
  assert.deepEqual(offer({ halted: true }), { kind: 'unavailable', reason: 'halted' });
  assert.deepEqual(offer({ session: undefined }), { kind: 'unavailable', reason: 'no-session' });
  assert.deepEqual(offer({ paused: true }), { kind: 'attach', agent: ADA });
  assert.deepEqual(offer({ mode: 'attached' }, { phase: 'attached', id: 3, agent: ADA }), { kind: 'open-here' });
  assert.deepEqual(offer({}, { phase: 'opening', id: 3, agent: GRACE }), { kind: 'open-elsewhere', agent: GRACE });
  assert.deepEqual(offer({}, { phase: 'ended', id: 3, agent: GRACE, ending: 'detached' }), { kind: 'attach', agent: ADA });
  assert.deepEqual(terminalOffer(floorOf([row()], { terminal: false }), row(), idle), { kind: 'none' });
});

// --- what the views say ------------------------------------------------------------------

let server;
let TerminalAttachView;
let AttachConfirmation;
let TerminalPanelView;

before(async () => {
  server = await createServer({
    root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-terminal'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  ({ TerminalAttachView, TerminalPanelView, AttachConfirmation } = await server.ssrLoadModule('/src/renderer/terminal/TerminalPanel.tsx'));
});

after(async () => {
  await server?.close();
});

const panel = (channel, floor) => renderToStaticMarkup(createElement(TerminalPanelView, { channel, floor }));
const status = (html) => /<p class="terminal-state[^"]*" role="(?:status|alert)">([^<]*)<\/p>/.exec(html)?.[1];
const buttons = (html) => [...html.matchAll(/<button\b[^>]*>([^<]*)<\/button>/g)].map(([, label]) => label)
  .filter((label) => ['Detach', 'Detaching…', 'Close'].includes(label));

test('Attach carries its explanation as a tooltip and description, and a disabled Attach says why', () => {
  const floor = floorOf([row({ provider: 'local' }), row({ id: GRACE, name: 'Grace', provider: 'local' })]);
  const view = (offer) => renderToStaticMarkup(createElement(TerminalAttachView, { floor, offer }));
  const attachButton = (html) => /<button[^>]*class="terminal-attach"[^>]*>/.exec(html)?.[0] ?? '';
  const described = (html, words) => {
    const button = attachButton(html);
    assert.match(button, new RegExp(`title="${words}"`));
    assert.match(button, /aria-describedby="terminal-hint"/);
    assert.match(html, new RegExp(`<span class="visually-hidden" id="terminal-hint">${words}</span>`));
    assert.doesNotMatch(html, /<p\b/, 'the words are a tooltip, not a paragraph');
  };
  assert.equal(view({ kind: 'none' }), '');
  const open = view({ kind: 'attach', agent: ADA });
  described(open, 'Attach hands you this agent’s CLI\\. Its loop holds while you type, and Detach hands it back\\.');
  assert.doesNotMatch(attachButton(open), /aria-disabled/);
  for (const [offer, words] of [
    [{ kind: 'unavailable', reason: 'running' }, 'Attach waits for this agent’s running turn to end\\.'],
    [{ kind: 'unavailable', reason: 'attached' }, 'A person already holds this agent’s keyboard\\.'],
    [{ kind: 'unavailable', reason: 'halted' }, 'A halt stands on this agent, so the Floor cannot attach it\\.'],
    [{ kind: 'unavailable', reason: 'no-session' }, 'Attach waits for this agent’s first confirmed session\\.'],
    [{ kind: 'open-here' }, 'This agent’s terminal is open\\.'],
    [{ kind: 'open-elsewhere', agent: GRACE }, 'The terminal is open for Grace\\. Detach it before attaching this agent\\.'],
  ]) {
    const html = view(offer);
    described(html, words);
    assert.match(attachButton(html), /aria-disabled="true"/, offer.kind + ' ' + (offer.reason ?? ''));
  }
});

test('Claude Attach shows the workspace trust warning in a confirmation before opening the terminal', () => {
  const floor = floorOf([row({ provider: 'claude' }), row({ id: GRACE, provider: 'local' })]);
  const view = (agent) => renderToStaticMarkup(createElement(TerminalAttachView, { floor, offer: { kind: 'attach', agent } }));
  const claude = view(ADA);
  assert.doesNotMatch(claude, /trust this workspace/);
  assert.match(claude, /<button type="button" class="terminal-attach" title="[^"]*" aria-describedby="terminal-hint" aria-haspopup="dialog">Attach<\/button>/);
  assert.doesNotMatch(view(GRACE), /aria-haspopup/);
  const dialog = renderToStaticMarkup(createElement(AttachConfirmation, { name: 'Ada', onConfirm() {}, onCancel() {} }));
  assert.match(dialog, /Claude Code may ask you to trust this workspace/);
  assert.match(dialog, /No, exit/);
  assert.ok(dialog.indexOf('trust this workspace') < dialog.indexOf('>Attach<'));
  assert.match(dialog, /class="retire-cancel"[^>]*>Cancel</);
});

test('the panel offers Detach only while attached, and Close only once ended', () => {
  const floor = floorOf([row({ mode: 'attached', status: 'attached' })]);
  const open = (phase) => ({ phase, id: 1, agent: ADA });
  assert.deepEqual(buttons(panel(open('opening'), floor)), []);
  assert.deepEqual(buttons(panel(open('attaching'), floor)), []);
  assert.deepEqual(buttons(panel(open('attached'), floor)), ['Detach']);
  assert.deepEqual(buttons(panel(open('detaching'), floor)), ['Detaching…']);
  assert.match(panel(open('detaching'), floor), /aria-disabled="true"/);
  assert.deepEqual(buttons(panel({ ...open('ended'), ending: 'detached' }, floor)), ['Close']);
  assert.equal(status(panel(open('attached'), floor)),
    'Attached — what you type goes to this agent’s CLI. Its loop holds until you detach.');
  // The way out of the grid is on screen in every phase, the ended one included.
  for (const channel of [open('attached'), { ...open('ended'), ending: 'lost' }]) {
    assert.match(panel(channel, floor), /<p class="hire-note terminal-keys">Tab and Shift\+Tab go to the CLI\. Press Ctrl\+\] to move the keyboard from the terminal to this panel’s button\.<\/p>/);
  }
});

test('a daemon close is explained from the projection, which may arrive after it', () => {
  const closed = { phase: 'ended', id: 1, agent: ADA, ending: 'closed' };
  assert.equal(status(panel(closed, floorOf([]))), 'The agent was retired, so the daemon closed its terminal.');
  assert.equal(status(panel(closed, floorOf([row({ halted: true })]))),
    'A halt now stands on this agent, so the daemon closed its terminal.');
  assert.equal(status(panel(closed, floorOf([row()], { phase: 'closed' }))),
    'The Floor session ended or the daemon stopped, so the terminal closed.');
  assert.equal(status(panel(closed, floorOf([row()]))),
    'The daemon closed the terminal: the Floor session may have expired, or the agent’s CLI exited. '
    + 'The daemon closes the agent’s terminal and returns the agent to its loop.');
  // Between subscriptions an absent row proves nothing about retirement.
  assert.match(status(panel(closed, floorOf([], { phase: 'resyncing', snapshotEpoch: 3 }))), /^The daemon closed/);
  assert.equal(status(panel({ ...closed, ending: 'detached' }, floorOf([row()]))),
    'Detached. The daemon confirmed the close, and the agent is back under its loop.');
  // A confirmed detach is the one ending that is not an alert.
  assert.match(panel({ ...closed, ending: 'detached' }, floorOf([row()])),
    /<p class="terminal-state terminal-ended" role="status">Detached\./);
  assert.match(panel(closed, floorOf([])), /<p class="terminal-state terminal-ended terminal-failed" role="alert">/);
});
test('the terminal header offers bounded zoom, selection copy and reversible expansion', () => {
  const html = panel({ phase: 'attached', id: 1, agent: ADA }, floorOf([row()]));
  assert.match(html, /aria-label="Zoom out"/);
  assert.match(html, /aria-label="Reset terminal zoom"/);
  assert.match(html, /aria-label="Zoom in"/);
  assert.match(html, /aria-label="Copy selection"/);
  assert.match(html, /aria-expanded="false"[^>]*>Expand<\/button>/);
  assert.match(html, /data-terminal-expanded="false"/);
  assert.match(html, /Cmd\+= zooms in|Ctrl\+\] first/);
});

test('pressing a waiting Attach neither opens the confirmation nor attaches, and an available one does open it', async () => {
  const { fakePage, press } = await import('./helpers/fake-page.mjs');
  const { createRoot } = await import('react-dom/client');
  const { act } = await import('react');
  const terminal = await server.ssrLoadModule('/src/renderer/terminal/terminal.ts');
  terminal.resetTerminal();
  const floor = floorOf([row({ provider: 'claude' })]);
  const page = fakePage();
  const prototype = page.host.constructor.prototype;
  prototype.showModal = () => {};
  prototype.close = () => {};
  try {
    const root = createRoot(page.host);
    const mount = (offer) => act(() => root.render(createElement(TerminalAttachView, { floor, offer })));
    const attach = () => page.host.querySelector('[aria-describedby="terminal-hint"]');
    const dialog = () => page.host.querySelector('[data-attach-dialog="true"]');
    for (const offer of [{ kind: 'unavailable', reason: 'running' }, { kind: 'unavailable', reason: 'attached' },
      { kind: 'unavailable', reason: 'halted' }, { kind: 'unavailable', reason: 'no-session' }, { kind: 'open-here' },
      { kind: 'open-elsewhere', agent: GRACE }]) {
      await mount(offer);
      assert.equal(attach().getAttribute('aria-disabled'), 'true');
      await act(async () => press(attach()));
      assert.equal(dialog(), null, `${offer.kind} ${offer.reason ?? ''} opened the confirmation`);
      assert.equal(terminal.getTerminalState().phase, 'idle', `${offer.kind} ${offer.reason ?? ''} attached`);
    }
    await mount({ kind: 'attach', agent: ADA });
    await act(async () => press(attach()));
    assert.notEqual(dialog(), null, 'an available Claude Attach opens its confirmation');
    await act(() => root.unmount());
  } finally {
    delete prototype.showModal;
    delete prototype.close;
    terminal.resetTerminal();
    page.restore();
  }
});

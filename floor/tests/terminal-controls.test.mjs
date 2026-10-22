// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';

import {
  copyTerminalSelection, fitTerminal, readFontSize, terminalShortcut, writeFontSize, zoomFontSize,
} from '../src/renderer/terminal/controls.ts';
import {
  attachTerminal, bindTerminalScreen, getTerminalState, resetTerminal,
} from '../src/renderer/terminal/terminal.ts';

const key = (fields = {}) => ({
  ctrlKey: false, altKey: false, metaKey: false, shiftKey: false, code: 'Equal', ...fields,
});

test('zoom bounds and reset keep the font readable', () => {
  assert.equal(zoomFontSize(14, 'zoom-in'), 15);
  assert.equal(zoomFontSize(14, 'zoom-out'), 13);
  assert.equal(zoomFontSize(32, 'zoom-in'), 32);
  assert.equal(zoomFontSize(10, 'zoom-out'), 10);
  assert.equal(zoomFontSize(27, 'zoom-reset'), 14);
});

test('font storage accepts only a bounded integer and survives missing or throwing storage', () => {
  const saved = new Map([['dudes.terminal.font-size', '19']]);
  const storage = () => ({ getItem: (key) => saved.get(key) ?? null,
    setItem: (key, value) => saved.set(key, value) });
  assert.equal(readFontSize(storage), 19);
  writeFontSize(23, storage);
  assert.equal(readFontSize(storage), 23);
  for (const value of ['9', '33', '14px', '14.5', '0x10', '1.4e1', ' 14 ', 'NaN', '', 'Infinity']) {
    saved.set('dudes.terminal.font-size', value);
    assert.equal(readFontSize(storage), 14, value);
  }
  assert.equal(readFontSize(() => null), 14);
  assert.equal(readFontSize(() => { throw new Error('denied'); }), 14);
  writeFontSize(20, () => { throw new Error('denied'); });
  assert.equal(zoomFontSize(20, 'zoom-in'), 21);
});

test('font writes preserve the last valid size when runtime input is not a bounded integer', () => {
  const saved = new Map([['dudes.terminal.font-size', '23']]);
  const storage = () => ({ getItem: (key) => saved.get(key) ?? null,
    setItem: (key, value) => saved.set(key, value) });
  writeFontSize(14, storage);
  assert.equal(saved.get('dudes.terminal.font-size'), '14');
  for (const value of ['20', NaN, Infinity, -Infinity, 14.5, 9, 33, null, undefined, {}]) {
    writeFontSize(value, storage);
    assert.equal(saved.get('dudes.terminal.font-size'), '14', String(value));
    assert.equal(readFontSize(storage), 14);
  }
});

test('shortcuts reserve Cmd on Mac and leave every other platform grid chord to the CLI', () => {
  const commands = { Equal: 'zoom-in', Minus: 'zoom-out', Digit0: 'zoom-reset', KeyC: 'copy' };
  for (const [code, command] of Object.entries(commands)) {
    assert.equal(terminalShortcut(key({ code, metaKey: true }), 'mac', 'grid'), command);
    assert.equal(terminalShortcut(key({ code, ctrlKey: true, shiftKey: true }), 'other', 'header'), command);
    assert.equal(terminalShortcut(key({ code, ctrlKey: true, shiftKey: true }), 'other', 'grid'), null);
  }
  for (const fields of [{ ctrlKey: true, code: 'KeyC' }, { ctrlKey: true, code: 'Minus' },
    { ctrlKey: true, shiftKey: true, code: 'Minus' }, { code: 'Escape' }, { code: 'Tab' },
    { ctrlKey: true, code: 'BracketRight' }, { metaKey: true, altKey: true },
    { metaKey: true, ctrlKey: true }]) {
    assert.equal(terminalShortcut(key(fields), 'mac', 'grid'), null);
    assert.equal(terminalShortcut(key(fields), 'other', 'grid'), null);
  }
  assert.equal(terminalShortcut(key({ code: 'KeyC', ctrlKey: true }), 'other', 'header'), null);
  assert.equal(terminalShortcut(key({ metaKey: true }), 'other', 'header'), null);
});

test('copy starts the exact selection write inside the gesture and reports empty or refused text', async () => {
  const writes = [];
  let resolveWrite;
  const pending = copyTerminalSelection({ getSelection: () => 'chosen \u001b[31m text\n' }, () => ({
    writeText: (text) => { writes.push(text); return new Promise((resolve) => { resolveWrite = resolve; }); },
  }));
  assert.deepEqual(writes, ['chosen \u001b[31m text\n']);
  resolveWrite();
  assert.deepEqual(await pending, { kind: 'copied' });
  assert.deepEqual(await copyTerminalSelection({ getSelection: () => '' }, () => ({
    writeText: (text) => { writes.push(text); return Promise.resolve(); },
  })), { kind: 'empty' });
  assert.deepEqual(writes, ['chosen \u001b[31m text\n']);
  for (const clipboard of [() => null, () => { throw new Error('unavailable'); },
    () => ({ writeText: () => Promise.reject(new Error('denied')) }),
    () => ({ writeText: () => { throw new Error('denied'); } })]) {
    assert.deepEqual(await copyTerminalSelection({ getSelection: () => 'keep me' }, clipboard),
      { kind: 'refused', selection: 'keep me' });
  }
});

test('a zoom refit sends the changed dimensions once through the existing channel without PTY input', () => {
  resetTerminal();
  const listeners = new Map();
  const sent = [];
  const socket = { addEventListener: (kind, listener) => listeners.set(kind, listener),
    send: (data) => sent.push(JSON.parse(data)), close() {} };
  const grid = { cols: 80, rows: 24, options: { fontSize: 14 },
    element: { querySelector: () => ({ getBoundingClientRect: () => ({
      width: grid.cols * grid.options.fontSize / 2, height: grid.rows * grid.options.fontSize,
    }) }) }, resize: (cols, rows) => { grid.cols = cols; grid.rows = rows; } };
  attachTerminal('01JBQ8XK2M3N4P5Q6R7S8T9VWX', {
    location: { protocol: 'http:', host: '127.0.0.1:7717' }, socket: () => socket,
    signedIn: () => true,
  });
  bindTerminalScreen({ size: () => ({ cols: grid.cols, rows: grid.rows }), write() {} });
  listeners.get('open')();
  listeners.get('message')({ data: JSON.stringify({ type: 'terminal.attached',
    handle: 'NVXX_l8kLbkVqyiQYpmOw9dabOgXjcb6SaghVuoDpNw' }) });
  assert.equal(getTerminalState().phase, 'attached');
  grid.options.fontSize = zoomFontSize(14, 'zoom-in');
  fitTerminal(grid, { getBoundingClientRect: () => ({ width: 814, height: 420 }) });
  fitTerminal(grid, { getBoundingClientRect: () => ({ width: 814, height: 420 }) });
  assert.deepEqual(sent, [
    { type: 'terminal.attach', agent: '01JBQ8XK2M3N4P5Q6R7S8T9VWX', cols: 80, rows: 24 },
    { type: 'terminal.resize', handle: 'NVXX_l8kLbkVqyiQYpmOw9dabOgXjcb6SaghVuoDpNw', cols: 106, rows: 28 },
  ]);
  resetTerminal();
});

// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Board's keyboard shortcuts, the command menu's rows, the copy-link fallback and the ticket address.

import assert from 'node:assert/strict';
import test from 'node:test';

import { listCommands } from '../src/renderer/commands/commands.ts';
import { copyText } from '../src/renderer/ticket/copyLink.ts';
import { boardCommand, isTypingTarget, opensCommandMenu, shortcutRows } from '../src/renderer/ticket/shortcuts.ts';
import { isOpenableAddress, ticketAddress, ticketFromAddress, ticketLink } from '../src/navigation.ts';

const key = (name, extra = {}) => ({ key: name, ctrlKey: false, altKey: false, metaKey: false, ...extra });
const body = { tagName: 'BODY', closest: () => null };
const button = { tagName: 'BUTTON', closest: () => null };

test('each Board key picks one command', () => {
  const picked = Object.fromEntries(['/', 'c', 'j', 'k', 'b', 'l', '?', 'x', 'Enter', 'Escape', 'J'].map((name) => [name, boardCommand(key(name), button)]));
  assert.deepEqual(picked, {
    '/': 'search', c: 'new', j: 'next', k: 'previous', b: 'board', l: 'list', '?': 'help',
    x: null, Enter: null, Escape: null, J: null,
  });
});

test('no Board key fires with Ctrl, Alt or Meta held, or while an input method is composing', () => {
  for (const held of ['ctrlKey', 'altKey', 'metaKey']) assert.equal(boardCommand(key('c', { [held]: true }), button), null, held);
  assert.equal(boardCommand(key('c', { isComposing: true }), button), null);
});

test('no Board key fires while typing in a field, a menu or a list that reads letters', () => {
  const fields = ['INPUT', 'TEXTAREA', 'SELECT'].map((tagName) => ({ tagName, closest: () => null }));
  const editable = { tagName: 'DIV', isContentEditable: true, closest: () => null };
  const widget = { tagName: 'BUTTON', closest: (selector) => (selector.includes('[role="combobox"]') ? {} : null) };
  for (const target of [...fields, editable, widget]) {
    assert.equal(isTypingTarget(target), true);
    assert.equal(boardCommand(key('c'), target), null);
  }
  assert.equal(isTypingTarget(button), false);
  assert.equal(isTypingTarget(null), false);
});

test('the arrow keys move the selection only when nothing has focus yet', () => {
  assert.deepEqual([boardCommand(key('ArrowDown'), body), boardCommand(key('ArrowUp'), body)], ['next', 'previous']);
  assert.deepEqual([boardCommand(key('ArrowDown'), button), boardCommand(key('ArrowUp'), button)], [null, null]);
  assert.equal(boardCommand(key('ArrowDown', { ctrlKey: true }), body), null);
});

test('the command menu opens with Cmd+K on a Mac and Ctrl+K elsewhere, and with no other modifier', () => {
  const press = (extra) => ({ ...key('k'), shiftKey: false, ...extra });
  assert.deepEqual([true, false].map((mac) => opensCommandMenu(press({ metaKey: true }), mac)), [true, false]);
  assert.deepEqual([true, false].map((mac) => opensCommandMenu(press({ ctrlKey: true }), mac)), [false, true]);
  assert.equal(opensCommandMenu(press({ metaKey: true, key: 'K' }), true), true);
  assert.equal(opensCommandMenu(press({ metaKey: true, shiftKey: true }), true), false);
  assert.equal(opensCommandMenu(press({ metaKey: true, altKey: true }), true), false);
  assert.equal(opensCommandMenu(press({ metaKey: true, key: 'j' }), true), false);
  assert.equal(opensCommandMenu(press({ metaKey: true, repeat: true }), true), false);
  assert.equal(opensCommandMenu(press({}), true), false);
});

test('the help sheet lists every shortcut and names the menu key for the platform', () => {
  assert.deepEqual(shortcutRows(true).map((row) => `${row.keys.join(' ')}: ${row.label}`), [
    '/: Search tickets', 'c: New ticket', 'j ↓: Next ticket', 'k ↑: Previous ticket', 'Enter: Open the selected ticket',
    'Esc: Close the top layer', 'b: Show the Board', 'l: Show the List', '⌘K: Command menu', '?: Show this sheet',
  ]);
  assert.deepEqual(shortcutRows(false).find((row) => row.label === 'Command menu').keys, ['Ctrl+K']);
});

const tickets = [
  { id: 'A', title: 'Fix the login page', detail: 'To do' },
  { id: 'B', title: 'Add a login log', detail: 'Working' },
  { id: 'C', title: 'Write release notes', detail: 'Done' },
];
const plain = (rows) => rows.map((row) => `${row.kind}:${row.id}:${row.label}`);

test('with nothing typed the menu offers New ticket, the two views and the tickets', () => {
  assert.deepEqual(plain(listCommands('', tickets, true)), [
    'new:new:New ticket', 'board:board:Open Board', 'list:list:Open List',
    'ticket:A:Fix the login page', 'ticket:B:Add a login log', 'ticket:C:Write release notes',
  ]);
});

test('typed words become the new ticket title and filter the tickets by title', () => {
  const rows = listCommands('  login ', tickets, true);
  assert.deepEqual(plain(rows), ['new:new:New ticket: login', 'ticket:A:Fix the login page', 'ticket:B:Add a login log']);
  assert.equal(rows[0].title, 'login');
  assert.deepEqual(plain(listCommands('add login', tickets, true)), ['new:new:New ticket: add login', 'ticket:B:Add a login log']);
});

test('tickets whose title starts with the query come first, and the command words do not become a title', () => {
  assert.deepEqual(plain(listCommands('add', tickets, false)), ['ticket:B:Add a login log']);
  assert.deepEqual(plain(listCommands('lo', [{ id: 'X', title: 'Close it', detail: '' }, { id: 'Y', title: 'Login', detail: '' }], false)),
    ['ticket:Y:Login', 'ticket:X:Close it']);
  assert.equal(listCommands('new t', tickets, true)[0].title, '');
  assert.equal(listCommands('new t', tickets, true)[0].label, 'New ticket');
  assert.deepEqual(plain(listCommands('open l', tickets, true)), ['new:new:New ticket: open l', 'list:list:Open List']);
  assert.deepEqual(plain(listCommands('board', tickets, true)), ['new:new:New ticket: board', 'board:board:Open Board']);
});

test('no ticket can be made while tickets are off or the view is stale, and a title is cut at 256 characters', () => {
  assert.deepEqual(plain(listCommands('zzz', tickets, false)), []);
  assert.equal(listCommands('zzz', tickets, true)[0].label, 'New ticket: zzz');
  assert.equal(listCommands('x'.repeat(300), [], true)[0].title.length, 256);
});

test('only eight tickets are shown', () => {
  const many = Array.from({ length: 12 }, (_, at) => ({ id: `T${at}`, title: `Ticket ${at}`, detail: '' }));
  assert.equal(listCommands('', many, true).filter((row) => row.kind === 'ticket').length, 8);
});

test('copying uses the clipboard inside the click and falls back to selecting when it is missing or refuses', async () => {
  const written = [];
  assert.equal(await copyText({ writeText: async (text) => { written.push(text); } }, 'https://x/#/ticket/A'), 'copied');
  assert.deepEqual(written, ['https://x/#/ticket/A']);
  assert.equal(await copyText({ writeText: async () => { throw new DOMException('denied', 'NotAllowedError'); } }, 'a'), 'select');
  assert.equal(await copyText({ writeText: () => { throw new Error('sync'); } }, 'a'), 'select');
  assert.equal(await copyText({}, 'a'), 'select');
  assert.equal(await copyText(undefined, 'a'), 'select');
});

test('the ticket address is a plain token the shell reads once, and the copied link holds no sign-in fragment', () => {
  const id = '01JBQ8XK2M3N4P5Q6R7S8T9V0W';
  assert.equal(ticketLink(id), '#/ticket/01JBQ8XK2M3N4P5Q6R7S8T9V0W');
  assert.equal(ticketFromAddress(ticketLink(id)), id);
  assert.equal(ticketFromAddress('#/ticket/My_card-2'), 'My_card-2');
  for (const address of ['', '#', '#/ticket', '#/ticket/', '#/tickets/A', '#/ticket/a/b', '#/ticket/a b', '#/ticket/%2e', `#/ticket/${'x'.repeat(129)}`, '#/board/A', '#bootstrap=link']) {
    assert.equal(ticketFromAddress(address), null, address);
  }
  assert.equal(ticketAddress({ origin: 'http://127.0.0.1:7433', pathname: '/floor/' }, id), 'http://127.0.0.1:7433/floor/#/ticket/01JBQ8XK2M3N4P5Q6R7S8T9V0W');
});

test('a signed-in tab keeps a page, Board or ticket address on load and nothing else', () => {
  for (const address of ['#/log', '#/board', '#/board/service', '#/ticket/A']) assert.equal(isOpenableAddress(address), true, address);
  for (const address of ['#/office', '#/ticket/', '#section', '#bootstrap=link']) assert.equal(isOpenableAddress(address), false, address);
});

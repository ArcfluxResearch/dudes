// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What each owned component puts in the page: the data attributes a test can hook on, and the roles and ARIA a screen
// reader and the keyboard rely on. The keys, the focus rings and the top layer are checked in a real browser by
// `npm run test:browser`, and the pure part of the keys in ui-nav.test.mjs.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement as h } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

let server;
let ui;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-ui'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ui = await server.ssrLoadModule('/src/ui/index.ts');
});
after(async () => { await server?.close(); });
const html = (element) => renderToStaticMarkup(element);
const attr = (markup, name) => new RegExp(`${name}="([^"]*)"`).exec(markup)?.[1];

test('a button wears data-ui, data-variant and data-size, keeps its caller\'s class and merges it over the base', () => {
  const page = html(h(ui.Button, { variant: 'primary', size: 'sm', className: 'control-pause h-9' }, 'Pause'));
  assert.match(page, /^<button type="button" data-ui="button" data-variant="primary" data-size="sm"/);
  assert.match(page, /class="[^"]*\bcontrol-pause\b/);
  assert.doesNotMatch(page, /\bh-7\b/, 'the caller\'s height replaced the size\'s');
  assert.match(page, /\bh-9\b/);
  const kinds = ['primary', 'secondary', 'ghost', 'danger'].map((variant) => attr(html(h(ui.Button, { variant }, 'x')), 'class'));
  assert.equal(new Set(kinds).size, 4, 'each variant looks different');
  assert.deepEqual(kinds.filter((value) => !/border-/.test(value)), [], 'every variant sets its own border colour');
});

test('a secondary button keeps its edge: the variant\'s border colour replaces the base\'s, never cancels with it', () => {
  const classes = attr(html(h(ui.Button, { variant: 'secondary' }, 'x')), 'class').split(' ');
  assert.ok(classes.includes('border-line-strong'));
  assert.ok(!classes.includes('border-transparent'));
});

test('a loading button says it is busy, ignores a press and stays focusable; a disabled one is a real disabled button', () => {
  const busy = html(h(ui.Button, { loading: true }, 'Save'));
  assert.match(busy, /aria-busy="true"/);
  assert.match(busy, /aria-disabled="true"/);
  assert.match(busy, /data-loading="true"/);
  assert.match(busy, /<svg[^>]*animate-spin/);
  assert.match(busy, />Save<\/button>/);
  assert.doesNotMatch(busy, / disabled=""/);
  assert.match(html(h(ui.Button, { disabled: true }, 'Save')), / disabled=""/);
  assert.match(html(h(ui.Button, { icon: h('i', { id: 'lead' }) }, 'Save')), /<i id="lead"><\/i>Save/);
});

test('an icon button needs its label by type and carries it as its accessible name', () => {
  const page = html(h(ui.IconButton, { label: 'Close', icon: h('i') }));
  assert.match(page, /aria-label="Close"[^>]*data-ui="icon-button"/);
  assert.match(page, /\bsize-8\b/);
  assert.match(html(h(ui.IconButton, { label: 'Close', icon: h('i'), size: 'sm' })), /\bsize-7\b/);
});

test('a field ties its label, hint and error to its control through context, and marks the control invalid', () => {
  const page = html(h(ui.Field, { label: 'Name', hint: 'Shown on the Floor', error: 'Pick a name', id: 'hire-name' }, h(ui.Input, { type: 'text' })));
  assert.match(page, /<label for="hire-name"[^>]*>Name<\/label>/);
  assert.match(page, /<input data-ui="input" type="text" id="hire-name" aria-describedby="hire-name-hint hire-name-error" aria-invalid="true"/);
  assert.match(page, /<p id="hire-name-hint"[^>]*>Shown on the Floor<\/p>/);
  assert.match(page, /<p id="hire-name-error" role="alert"[^>]*>Pick a name<\/p>/);
  assert.match(page, /data-ui="field" data-invalid="true"/);
  const quiet = html(h(ui.Field, { label: 'Name', id: 'n' }, h(ui.Textarea, {})));
  assert.match(quiet, /<textarea data-ui="textarea" id="n"/);
  assert.doesNotMatch(quiet, /aria-describedby|aria-invalid|role="alert"/);
  assert.match(html(h(ui.Input, { id: 'alone' })), /^<input data-ui="input" id="alone"/, 'outside a field an input is just an input');
  const own = html(h(ui.Field, { label: 'x', hint: 'h', id: 'f' }, h(ui.Input, { 'aria-describedby': 'mine' })));
  assert.match(own, /aria-describedby="mine f-hint"/);
});

test('a select is a combobox button that names its listbox, with the chosen option marked and the list a popover', () => {
  const options = [{ value: '', label: 'Automatic' }, { value: 'c', label: 'Character C' }, { value: 'd', label: 'Character D', disabled: true }];
  const page = html(h(ui.Field, { label: 'Character', id: 'who', hint: 'Looks only' }, h(ui.Select, { options, value: 'c', onValueChange() {}, name: 'character' })));
  assert.match(page, /<button type="button" id="who" data-ui="select" data-field="character" role="combobox" aria-haspopup="listbox" aria-expanded="false" aria-controls="who-list"[^>]*aria-describedby="who-hint"/);
  assert.match(page, /<div id="who-list" role="listbox" popover="manual" tabindex="-1" data-ui="select-list"/);
  assert.deepEqual([...page.matchAll(/role="option" data-value="([^"]*)" aria-selected="(true|false)"/g)].map(([, value, on]) => `${value}:${on}`),
    [':false', 'c:true', 'd:false']);
  assert.match(page, /aria-disabled="true"[^>]*>(?:<span[^>]*>)?Character D/);
  assert.match(page, /<span class="truncate">Character C<\/span>/);
  assert.match(html(h(ui.Select, { options, value: 'zzz', onValueChange() {}, placeholder: 'Pick one' })), /text-subtle[^>]*>Pick one</);
});

test('a menu is a labelled button over a role=menu popover, and its items are menu items that the keyboard can reach', () => {
  const page = html(h(ui.Menu, { label: 'More actions' },
    h(ui.MenuItem, { onSelect() {} }, 'Change model'), h(ui.MenuSeparator, {}), h(ui.MenuItem, { onSelect() {}, danger: true }, 'Retire'),
    h(ui.MenuItem, { onSelect() {}, disabled: true }, 'Off')));
  assert.match(page, /<button type="button" aria-label="More actions" title="More actions" aria-haspopup="menu" aria-expanded="false" aria-controls="([^"]+)" data-ui="menu-button" data-variant="ghost" data-size="md"/);
  assert.match(page, /<div id="[^"]+" role="menu" aria-label="More actions" popover="manual" tabindex="-1" data-ui="menu"/);
  assert.equal([...page.matchAll(/role="menuitem" tabindex="-1" data-ui="menu-item"/g)].length, 3);
  assert.match(page, /data-variant="danger"[^>]*>(?:<[^>]*>)*Retire/);
  assert.match(page, /role="separator"/);
  assert.match(page, /aria-disabled="true"[^>]*disabled=""[^>]*>Off|disabled=""[^>]*aria-disabled="true"[^>]*>Off/);
});

test('a switch is role=switch, named by its label, described by its description, and shows the value it is handed', () => {
  const on = html(h(ui.Switch, { checked: true, onCheckedChange() {}, label: 'Hire', description: 'Lets you hire from the page' }));
  assert.match(on, /role="switch" aria-checked="true" aria-labelledby="([^"]+)-label" aria-describedby="\1-description"/);
  assert.match(on, /data-ui="switch" data-state="on"/);
  assert.match(on, /<span id="[^"]+-label"[^>]*>Hire<\/span>/);
  assert.match(on, /data-part="knob"/);
  const off = html(h(ui.Switch, { checked: false, onCheckedChange() {}, label: 'Hire' }));
  assert.match(off, /aria-checked="false"/);
  assert.match(off, /data-state="off"/);
  assert.doesNotMatch(off, /aria-describedby/);
  const busy = html(h(ui.Switch, { checked: false, onCheckedChange() {}, label: 'Hire', busy: true }));
  assert.match(busy, /aria-busy="true"/);
  assert.match(busy, /aria-disabled="true"/);
  assert.match(busy, /aria-checked="false"/, 'busy shows the held value, not the pending one');
});

test('tabs are a labelled tablist whose selected tab alone is in the tab order', () => {
  const page = html(h(ui.TabList, { label: 'Agent' }, h(ui.Tab, { selected: true }, 'Overview'), h(ui.Tab, { selected: false }, 'Memory')));
  assert.match(page, /role="tablist" aria-label="Agent" data-ui="tab-list"/);
  assert.match(page, /role="tab" aria-selected="true" tabindex="0" data-ui="tab" data-state="selected"[^>]*>Overview/);
  assert.match(page, /role="tab" aria-selected="false" tabindex="-1" data-ui="tab" data-state="idle"[^>]*>Memory/);
});

test('a badge says one of the six state words, and the word is text and not only a colour', () => {
  const words = { 'needs-you': 'Needs you', ready: 'Ready for you', working: 'Working', waiting: 'Waiting', done: 'Done', idle: 'Idle' };
  for (const [state, word] of Object.entries(words)) {
    const page = html(h(ui.Badge, { state }));
    assert.match(page, new RegExp(`data-ui="badge" data-state="${state}"`));
    assert.match(page, new RegExp(`>${word}</span>$`));
    assert.match(page, /aria-hidden="true"/);
  }
  assert.equal(new Set(Object.keys(words).map((state) => attr(html(h(ui.Badge, { state })), 'class'))).size, 5, 'the neutral states share a chip, the others differ');
});

test('a card and a panel are surfaces, and a panel names itself by its heading', () => {
  assert.match(html(h(ui.Card, { className: 'chat-card' }, 'x')), /^<div data-ui="card" class="[^"]*\bchat-card\b/);
  const panel = html(h(ui.Panel, { title: 'Log', headingId: 'log-title', actions: h('b', {}, 'act') }, 'body'));
  assert.match(panel, /<section data-ui="panel" aria-labelledby="log-title"/);
  assert.match(panel, /<h2 id="log-title"[^>]*>Log<\/h2>/);
  assert.match(panel, /<b>act<\/b>/);
});

test('a dialog is the native dialog, which the caller opens with showModal, with a size', () => {
  const page = html(h(ui.Dialog, { size: 'lg', 'aria-labelledby': 't', className: 'hire-dialog' }, h('h2', { id: 't' }, 'Hire')));
  assert.match(page, /^<dialog data-ui="dialog" data-size="lg" aria-labelledby="t" class="[^"]*\bhire-dialog\b/);
  assert.doesNotMatch(page, / open/);
  assert.match(page, /backdrop:bg-scrim\/50/);
});

test('a toast is a status region with its message, an optional action and Dismiss', () => {
  const page = html(h(ui.Toast, { tone: 'ok', onDismiss() {}, action: { label: 'Undo', onAction() {} } }, 'Accepted.'));
  assert.match(page, /^<div role="status" data-ui="toast" data-tone="ok"/);
  assert.match(page, /<p[^>]*>Accepted\.<\/p>/);
  assert.match(page, />Undo<\/button>/);
  assert.match(page, /aria-label="Dismiss"/);
  assert.doesNotMatch(html(h(ui.Toast, { onDismiss() {} }, 'Hi')), />Undo</);
  for (const tone of ['info', 'warn', 'danger']) assert.match(html(h(ui.Toast, { tone, onDismiss() {} }, 'x')), new RegExp(`data-tone="${tone}"`));
});

test('a disclosure is a details whose summary carries a Lucide chevron that turns when it opens', () => {
  const page = html(h(ui.Disclosure, { summary: 'Details', open: true }, 'More'));
  assert.match(page, /^<details data-ui="disclosure" open=""/);
  assert.match(page, /<summary data-part="summary"[^>]*><svg[^>]*lucide-chevron-right[^>]*group-open:rotate-90[^>]*>/);
  assert.match(page, /<\/svg>Details<\/summary>/);
});

// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What each owned component puts in the page: the roles and ARIA a screen reader and the keyboard rely on. The keys
// themselves are checked in a real browser, and the pure parts of them in ui-nav.test.mjs.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { createElement as h } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

let server;
let ui;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ui = await server.ssrLoadModule('/src/ui/index.ts');
});
after(async () => { await server?.close(); });
const html = (element) => renderToStaticMarkup(element);

test('an icon-only button carries its label, and a loading button says it is busy and waiting', () => {
  const icon = html(h(ui.Button, { iconOnly: h('i'), label: 'More actions' }));
  assert.match(icon, /<button type="button"[^>]*aria-label="More actions"/);
  const busy = html(h(ui.Button, { loading: true }, 'Save'));
  assert.match(busy, /aria-busy="true"/);
  assert.match(busy, /aria-disabled="true"/);
  assert.match(busy, />Save<\/button>/);
  const kinds = ['primary', 'secondary', 'ghost', 'danger'].map((variant) => html(h(ui.Button, { variant }, 'x')));
  assert.equal(new Set(kinds).size, 4, 'each variant looks different');
});

test('a select is a combobox button that names its listbox, with the chosen option marked', () => {
  const options = [{ value: '', label: 'Automatic' }, { value: 'c', label: 'Character C' }, { value: 'd', label: 'Character D' }];
  const page = html(h(ui.Select, { id: 'who', options, value: 'c', onValueChange() {}, 'aria-describedby': 'why' }));
  assert.match(page, /<button type="button" id="who"[^>]*role="combobox" aria-haspopup="listbox" aria-expanded="false" aria-controls="([^"]+)"/);
  const listId = /aria-controls="([^"]+)"/.exec(page)[1];
  assert.match(page, new RegExp(`<div id="${listId}" role="listbox" popover="manual"`));
  assert.deepEqual([...page.matchAll(/role="option" data-value="([^"]*)" aria-selected="(true|false)"/g)].map(([, value, on]) => `${value}:${on}`),
    [':false', 'c:true', 'd:false']);
  assert.match(page, /Character C<\/span><svg/, 'the chosen label is on the button and the option carries a check');
  assert.match(html(h(ui.Select, { options, value: 'zzz', onValueChange() {}, placeholder: 'Pick' })), />Pick</);
});

test('a menu is a button that names a role menu list of menuitems, with a separator and a danger item', () => {
  const page = html(h(ui.Menu, { label: 'More actions', trigger: 'x' },
    h(ui.MenuItem, { onSelect() {} }, 'Edit look'), h(ui.MenuSeparator), h(ui.MenuItem, { onSelect() {}, danger: true }, 'Retire agent')));
  assert.match(page, /aria-label="More actions" aria-haspopup="menu" aria-expanded="false" aria-controls="([^"]+)"/);
  assert.match(page, /role="menu" aria-label="More actions" popover="manual"/);
  assert.equal([...page.matchAll(/role="menuitem" tabindex="-1"/g)].length, 2);
  assert.match(page, /role="separator"/);
  assert.match(page, /text-danger[^"]*"[^>]*>Retire agent|Retire agent<\/button>/);
});

test('a switch is a labelled role switch with its description tied to it, and the value is the caller’s', () => {
  const on = html(h(ui.Switch, { checked: true, onCheckedChange() {}, label: 'Hire agents', description: 'Lets this page hire.' }));
  assert.match(on, /role="switch" aria-checked="true" aria-labelledby="([^"]+)-label" aria-describedby="\1-description"/);
  assert.match(on, /Hire agents<\/span>/);
  assert.match(html(h(ui.Switch, { checked: false, onCheckedChange() {}, label: 'x' })), /aria-checked="false"/);
});

test('a field ties its label, hint and error to the control, and an error is announced', () => {
  const page = html(h(ui.Field, { label: 'Name', hint: 'Up to 40.', error: 'Name is needed.' },
    (control) => h(ui.Input, { id: control.id, 'aria-describedby': control['aria-describedby'], 'aria-invalid': control['aria-invalid'] })));
  const id = /<label for="([^"]+)"/.exec(page)[1];
  assert.match(page, new RegExp(`<input id="${id}" aria-describedby="${id}-hint ${id}-error" aria-invalid="true"`));
  assert.match(page, new RegExp(`<p id="${id}-error" role="alert"`));
  assert.doesNotMatch(html(h(ui.Field, { label: 'Name' }, (control) => h(ui.Input, { id: control.id }))), /aria-invalid|role="alert"/);
});

test('a badge says one of the six state words', () => {
  const words = Object.keys(ui.BADGE_WORDS).map((state) => html(h(ui.Badge, { state })).replace(/<[^>]+>/g, ''));
  assert.deepEqual(words, ['Needs you', 'Ready for you', 'Working', 'Waiting', 'Done', 'Idle']);
});

test('tabs, a toast, a disclosure and a dialog draw their native elements with the right roles', () => {
  const tabs = html(h(ui.TabList, { 'aria-label': 'Sections' }, h(ui.Tab, { 'aria-selected': true, tabIndex: 0 }, 'Chat'), h(ui.Tab, { 'aria-selected': false, tabIndex: -1 }, 'Git')));
  assert.match(tabs, /role="tablist" aria-label="Sections"/);
  assert.deepEqual([...tabs.matchAll(/role="tab" aria-selected="(true|false)" tabindex="(-?\d)"/g)].map(([, a, b]) => `${a}${b}`), ['true0', 'false-1']);
  const toast = html(h(ui.Toast, { tone: 'success', onDismiss() {} }, 'Accepted.'));
  assert.match(toast, /<div role="status"/);
  assert.match(toast, /<p class="[^"]*">Accepted\.<\/p>/);
  assert.match(toast, />Dismiss<\/button>/);
  const closed = html(h(ui.Disclosure, { summary: 'Details' }, h('p', null, 'More')));
  assert.match(closed, /<details[^>]*><summary[^>]*>.*<svg.*Details<\/summary>/);
  assert.match(html(h(ui.Dialog, { 'aria-label': 'x' }, 'body')), /^<dialog aria-label="x" class="[^"]*">body<\/dialog>$/);
});

// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The owned components in src/ui: the markup and ARIA each one draws, the select's keyboard rules, and the colour
// tokens they sit on. A real browser covers the popover and focus behaviour, which the page stand-in cannot draw.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { fakePage } from './helpers/fake-page.mjs';
import { themeTokens } from './helpers/theme-colour.mjs';

const floor = resolve(import.meta.dirname, '..');
let server, ui;
before(async () => {
  server = await createServer({ root: floor, cacheDir: resolve(floor, 'node_modules/.vite-ui-components'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ui = await server.ssrLoadModule('/src/ui/index.ts');
});
after(async () => { await server?.close(); });

const html = (element) => renderToStaticMarkup(element);

test('a button is a native button with its look carried by attributes, so call sites keep their own class', () => {
  const { Button, IconButton } = ui;
  assert.equal(html(createElement(Button, { className: 'control-pause' }, 'Pause')),
    '<button type="button" class="control-pause" data-ui="button" data-variant="secondary" data-size="md">Pause</button>');
  assert.match(html(createElement(Button, { variant: 'primary', size: 'sm', 'aria-disabled': true }, 'Next')),
    /aria-disabled="true"[^>]*data-variant="primary" data-size="sm"/);
  const busy = html(createElement(Button, { loading: true }, 'Hire'));
  assert.match(busy, /aria-busy="true"/);
  assert.doesNotMatch(busy, /disabled=/, 'a loading button stays focusable');
  const icon = html(createElement(IconButton, { label: 'Close', icon: createElement('i') }));
  assert.match(icon, /aria-label="Close" title="Close"[^>]*data-icon-only/);
});

test('a field ties its label, hint and error to the control and marks the control invalid', () => {
  const { Field, Input } = ui;
  const plain = html(createElement(Field, { label: 'Name', id: 'who' }, createElement(Input)));
  assert.match(plain, /<label for="who">Name<\/label><input id="who" data-ui="input"\/>/);
  const bad = html(createElement(Field, { label: 'Name', id: 'who', hint: 'Short', error: 'Too long' },
    createElement(Input, { 'aria-describedby': 'extra' })));
  assert.match(bad, /<input aria-describedby="extra who-hint who-error" id="who" aria-invalid="true"/);
  assert.match(bad, /<p class="ui-field-error" id="who-error" role="alert">Too long<\/p>/);
  assert.match(bad, /data-invalid="true"/);
});

test('a switch is a labelled button with aria-checked, and a busy one keeps focus and ignores presses', () => {
  const { Switch } = ui;
  const markup = html(createElement(Switch, { label: 'Hire agents', description: 'Lets this page hire.', checked: true,
    onCheckedChange() {} }));
  assert.match(markup, /role="switch" class="ui-switch" aria-checked="true" aria-labelledby="([^"]+)-label" aria-describedby="\1-desc"/);
  const busy = html(createElement(Switch, { label: 'Hire agents', checked: false, busy: true, busyTitle: 'Wait.', onCheckedChange() {} }));
  assert.match(busy, /aria-checked="false"[^>]*aria-disabled="true" title="Wait\."/);
  assert.doesNotMatch(busy, / disabled=/);
  const page = fakePage();
  try {
    const calls = [];
    const root = createRoot(page.host);
    act(() => root.render(createElement(Switch, { label: 'A', checked: false, onCheckedChange: (next) => calls.push(next) })));
    const button = page.host.querySelectorAll('[role="switch"]')[0];
    const props = button[Object.keys(button).find((key) => key.startsWith('__reactProps$'))];
    act(() => props.onClick());
    assert.deepEqual(calls, [true]);
    act(() => root.render(createElement(Switch, { label: 'A', checked: false, busy: true, onCheckedChange: (next) => calls.push(next) })));
    act(() => button[Object.keys(button).find((key) => key.startsWith('__reactProps$'))].onClick());
    assert.deepEqual(calls, [true], 'a busy switch sent nothing');
    act(() => root.unmount());
  } finally { page.restore(); }
});

test('tabs put only the selected tab in the tab order', () => {
  const { TabList, Tab } = ui;
  const markup = html(createElement(TabList, { 'aria-label': 'Sections' },
    createElement(Tab, { selected: true, id: 'a' }, 'One'), createElement(Tab, { selected: false, id: 'b' }, 'Two')));
  assert.match(markup, /^<div role="tablist" aria-label="Sections" data-ui="tabs">/);
  assert.match(markup, /<button type="button" role="tab" id="a" aria-selected="true" tabindex="0">One<\/button>/);
  assert.match(markup, /<button type="button" role="tab" id="b" aria-selected="false" tabindex="-1">Two<\/button>/);
});

test('a badge draws its word with the state as an attribute, and a disclosure is a bare details', () => {
  const { Badge, Disclosure } = ui;
  assert.equal(html(createElement(Badge, { state: 'needs-you', className: 'crew-chip' }, 'Needs you')),
    '<span class="crew-chip" data-state="needs-you" data-ui="badge">Needs you</span>');
  assert.equal(html(createElement(Disclosure, { summary: 'Details' }, createElement('p', null, 'More'))),
    '<details data-ui="disclosure"><summary>Details</summary><p>More</p></details>');
});

test('a toast is a status region with one Dismiss, and the dialog keeps the native element', () => {
  const { Toast, Dialog } = ui;
  const markup = html(createElement(Toast, { tone: 'done', onDismiss() {}, dismissClassName: 'ticket-dismiss' }, 'Accepted.'));
  assert.match(markup, /^<p role="status" data-tone="done" data-ui="toast" data-ui-scope="">/);
  assert.match(markup, /<span class="ui-toast-message">Accepted\.<\/span>/);
  assert.match(markup, /<button type="button" class="ticket-dismiss"[^>]*>Dismiss<\/button>/);
  assert.match(html(createElement(Dialog, { className: 'hire-dialog', 'aria-labelledby': 't' })),
    /^<dialog class="hire-dialog" aria-labelledby="t" data-ui="dialog" data-ui-scope=""><\/dialog>$/);
});

test('a menu is a named menu button over a popover of items, with a danger item and a disabled one', () => {
  const { Menu, MenuItem, MenuSeparator } = ui;
  const markup = html(createElement(Menu, { label: 'More actions' },
    createElement(MenuItem, { onSelect() {} }, 'Edit look'), createElement(MenuSeparator),
    createElement(MenuItem, { onSelect() {}, danger: true, disabled: true }, 'Retire')));
  assert.match(markup, /aria-label="More actions" title="More actions" aria-haspopup="menu" aria-expanded="false" aria-controls="([^"]+)-menu"/);
  assert.match(markup, /<div class="ui-pop ui-menu-panel" id="[^"]+-menu" role="menu" aria-label="More actions" popover="manual">/);
  assert.match(markup, /role="menuitem" tabindex="-1" class="ui-pop-item">Edit look/);
  assert.match(markup, /role="menuitem" tabindex="-1" aria-disabled="true" class="ui-pop-item ui-pop-item--danger">Retire/);
});

const OPTIONS = [{ value: '', label: 'Automatic' }, { value: 'c', label: 'Character C' }, { value: 'd', label: 'Character D' },
  { value: 'x', label: 'Hidden', disabled: true }, { value: 'e', label: 'Character E' }];

test('a select is a combobox over a listbox that names its selected option', () => {
  const { Select, Field } = ui;
  const markup = html(createElement(Field, { label: 'Character', id: 'pick' },
    createElement(Select, { value: 'c', options: OPTIONS, onValueChange() {} })));
  assert.match(markup, /<label for="pick">Character<\/label><div class="ui-select"><button type="button" id="pick" class="ui-select-trigger" role="combobox"/);
  assert.match(markup, /aria-haspopup="listbox" aria-expanded="false" aria-controls="pick-list"/);
  assert.match(markup, /<div id="pick-opt-1" role="option" class="ui-pop-item ui-select-option" aria-selected="true">/);
  assert.match(markup, /id="pick-opt-3" role="option"[^>]*aria-selected="false" aria-disabled="true"/);
  assert.equal((markup.match(/role="option"/g) ?? []).length, OPTIONS.length);
});

test('the select opens on a key, moves over enabled options, types ahead, selects on Enter and closes on Escape', () => {
  const { Select } = ui;
  const page = fakePage({ rect: () => ({ left: 0, right: 100, width: 100, top: 0, bottom: 30, height: 30 }) });
  Object.assign(globalThis.window, { addEventListener() {}, removeEventListener() {}, innerWidth: 1000, innerHeight: 800 });
  try {
    const picked = [];
    const root = createRoot(page.host);
    act(() => root.render(createElement(Select, { id: 's', value: 'c', options: OPTIONS, onValueChange: (value) => picked.push(value) })));
    const button = page.host.querySelectorAll('[id="s"]')[0];
    const props = () => button[Object.keys(button).find((key) => key.startsWith('__reactProps$'))];
    const keys = [];
    const press = (key) => act(() => props().onKeyDown({ key, altKey: false, ctrlKey: false, metaKey: false,
      preventDefault() { keys.push(`${key} default prevented`); }, stopPropagation() {} }));
    const state = () => [button.getAttribute('aria-expanded'), button.getAttribute('aria-activedescendant')];
    assert.deepEqual(state(), ['false', null]);
    press('ArrowDown');
    assert.deepEqual(state(), ['true', 's-opt-1'], 'opens on the selected option');
    press('ArrowDown');
    assert.deepEqual(state(), ['true', 's-opt-2']);
    press('ArrowDown');
    assert.deepEqual(state(), ['true', 's-opt-4'], 'the disabled option is skipped');
    press('Home');
    assert.deepEqual(state(), ['true', 's-opt-0']);
    press('End');
    assert.deepEqual(state(), ['true', 's-opt-4']);
    press('c');
    assert.deepEqual(state(), ['true', 's-opt-1'], 'type-ahead finds the next label that starts with the letter');
    press('Enter');
    assert.deepEqual(picked, [], 'choosing the value it already has changes nothing');
    assert.deepEqual(state(), ['false', null]);
    press('ArrowDown'); press('ArrowDown'); press('Enter');
    assert.deepEqual(picked, ['d']);
    press('ArrowUp');
    assert.equal(state()[0], 'true');
    press('Escape');
    assert.deepEqual(state(), ['false', null]);
    assert.ok(keys.includes('Escape default prevented'), 'Escape is consumed, so a dialog around the select stays open');
    act(() => root.unmount());
  } finally { page.restore(); }
});

test('the colour tokens define every spec name in day and night, and text keeps the contrast the spec asks for', async () => {
  const css = await readFile(resolve(floor, 'src/ui/tokens.css'), 'utf8');
  const luminance = (hex) => {
    const n = Number.parseInt(hex.slice(1), 16);
    const channel = (value) => { const c = value / 255; return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; };
    return 0.2126 * channel((n >> 16) & 255) + 0.7152 * channel((n >> 8) & 255) + 0.0722 * channel(n & 255);
  };
  const ratio = (a, b) => { const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x); return (hi + 0.05) / (lo + 0.05); };
  const names = ['bg', 'surface', 'surface-2', 'border', 'border-strong', 'text', 'text-muted', 'text-subtle', 'accent',
    'accent-hover', 'accent-fg', 'accent-soft', 'focus', 'ok', 'ok-soft', 'warn', 'warn-soft', 'danger', 'danger-soft',
    'info', 'info-soft'];
  // Spec pairs that sit under 4.5:1 as written, with the ratio they have, so a change to either colour is noticed.
  const BELOW = { 'light ok on ok-soft': 4.33, 'light warn on warn-soft': 3.98 };
  const low = {};
  for (const theme of ['light', 'dark']) {
    const tokens = themeTokens(css, theme);
    for (const name of names) assert.match(tokens[name] ?? '', /^#[0-9a-f]{6}$/i, `${theme} ${name}`);
    const pairs = [['text', 'bg'], ['text', 'surface'], ['text', 'surface-2'], ['text-muted', 'bg'], ['text-muted', 'surface'],
      ['text-muted', 'surface-2'], ['accent-fg', 'accent'], ['accent', 'surface'], ['accent', 'accent-soft'], ['ok', 'surface'],
      ['ok', 'ok-soft'], ['warn', 'surface'], ['warn', 'warn-soft'], ['danger', 'surface'], ['danger', 'danger-soft'],
      ['info', 'surface'], ['info', 'info-soft']];
    for (const [ink, ground] of pairs) {
      const got = ratio(tokens[ink], tokens[ground]);
      if (got < 4.5) low[`${theme} ${ink} on ${ground}`] = Math.round(got * 100) / 100;
    }
  }
  assert.deepEqual(low, BELOW);
});

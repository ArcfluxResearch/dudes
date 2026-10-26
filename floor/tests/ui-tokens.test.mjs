// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The design tokens: both themes define every one with the specified value, the night set that follows the system matches
// the pinned one, and the pairs a person reads keep their contrast.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

const css = await readFile(resolve(import.meta.dirname, '../src/tailwind.css'), 'utf8');

const SPEC = {
  bg: ['#f7f8f7', '#0f1312'], 'ui-surface': ['#ffffff', '#151a19'], 'surface-2': ['#f1f4f3', '#1b2120'],
  border: ['#e1e6e4', '#262e2c'], 'border-strong': ['#8c9793', '#5c6b66'],
  text: ['#16201d', '#e6ecea'], 'text-muted': ['#5b6863', '#9ba9a4'], 'text-subtle': ['#6b7672', '#8a9893'],
  accent: ['#1f6f66', '#4fb3a6'], 'accent-hover': ['#185b53', '#66c4b8'], 'accent-fg': ['#ffffff', '#0b1110'],
  'accent-soft': ['#e3f1ee', '#163029'], focus: ['#2a8a7f', '#6fd0c3'],
  ok: ['#256a40', '#5cbf83'], 'ok-soft': ['#e4f1e8', '#1a3326'], warn: ['#7a560c', '#e0ad4b'], 'warn-soft': ['#f7eed8', '#3a2f16'],
  danger: ['#b23b2e', '#ec7d6d'], 'danger-soft': ['#f9e4e0', '#3a1f1b'], info: ['#3b6aa0', '#7ea6d8'], 'info-soft': ['#e5edf7', '#1c2836'],
  'tok-keyword': ['#0000ff', '#569cd6'], 'tok-string': ['#a31515', '#ce9178'], 'tok-number': ['#076c49', '#b5cea8'],
  'tok-comment': ['#007000', '#6a9955'], 'tok-literal': ['#0000ff', '#569cd6'], 'tok-key': ['#0451a5', '#9cdcfe'],
  'tok-add': ['#076c49', '#b5cea8'], 'tok-remove': ['#a31515', '#f48771'], 'tok-meta': ['#795e26', '#c586c0'],
  'surface-muted': ['#efe8d8', '#1b1814'], 'studio-background': ['#efe8d8', '#1b1814'], 'portrait-border': ['#ddd2bb', '#40382d'],
  'terminal-background': ['#fffcf4', '#201c16'], 'terminal-foreground': ['#2a2520', '#eee6d6'], 'terminal-cursor': ['#2a6150', '#86cdb5'],
  'terminal-selection': ['#ddd2bb', '#4a4136'],
};

function block(head, from = 0) {
  const at = css.indexOf(`${head} {`, from);
  assert.ok(at >= 0, `${head} is missing`);
  let depth = 0;
  for (let i = css.indexOf('{', at); i < css.length; i += 1) {
    if (css[i] === '{') depth += 1;
    else if (css[i] === '}' && (depth -= 1) === 0) return css.slice(css.indexOf('{', at) + 1, i);
  }
  throw new Error(`${head} never closes`);
}

const colours = (text) => Object.fromEntries([...text.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})/gi)].map(([, name, hex]) => [name, hex]));
const themeLayer = block('@layer theme');
const LIGHT = colours(block(':root', css.indexOf('@layer theme')));
const NIGHT = colours(block(":root:has(.shell[data-theme='dark'])"));
const SYSTEM_NIGHT = colours(block(":root:not(:has(.shell[data-theme='light']))"));

function luminance(hex) {
  const n = Number.parseInt(hex.slice(1), 16);
  const channel = (value) => {
    const c = value / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * channel((n >> 16) & 255) + 0.7152 * channel((n >> 8) & 255) + 0.0722 * channel(n & 255);
}

function contrast(a, b) {
  const [lighter, darker] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (lighter + 0.05) / (darker + 0.05);
}

test('every token has the specified day and night value, and the system night set is the pinned one', () => {
  for (const [name, [day, night]] of Object.entries(SPEC)) {
    assert.equal(LIGHT[name], day, `day ${name}`);
    assert.equal(NIGHT[name], night, `night ${name}`);
  }
  assert.deepEqual(SYSTEM_NIGHT, NIGHT);
  assert.deepEqual(Object.keys(LIGHT).sort(), Object.keys(SPEC).sort());
  assert.match(themeLayer, /@media \(prefers-color-scheme: dark\)/);
});

test('text reads at 4.5 to 1 on every surface it is drawn on, day and night', () => {
  const low = [];
  for (const [theme, t] of Object.entries({ day: LIGHT, night: NIGHT })) {
    const pairs = [
      ...['text', 'text-muted'].flatMap((ink) => ['bg', 'ui-surface', 'surface-2'].map((surface) => [ink, surface])),
      // Hints and placeholders sit on a panel or in a field, and a field is the panel's colour by day and the inset one by night.
      ['text-subtle', 'ui-surface'], ['text-subtle', theme === 'day' ? 'ui-surface' : 'surface-2'],
      ['accent-fg', 'accent'], ['accent-fg', 'accent-hover'], ['accent-fg', 'danger'],
      ['accent', 'ui-surface'], ['accent', 'accent-soft'], ['accent', 'bg'],
      ['text', 'warn-soft'], ['text', 'ok-soft'], ['text', 'danger-soft'], ['text', 'info-soft'], ['text', 'accent-soft'],
      ['ok', 'ok-soft'], ['warn', 'warn-soft'], ['danger', 'danger-soft'], ['info', 'info-soft'],
      ['ok', 'ui-surface'], ['warn', 'ui-surface'], ['danger', 'ui-surface'], ['info', 'ui-surface'],
      // The code colours are drawn on the inset colour a code block has.
      ...Object.keys(SPEC).filter((name) => name.startsWith('tok-')).map((name) => [name, 'surface-2']),
    ];
    for (const [ink, surface] of pairs) {
      const ratio = contrast(t[ink], t[surface]);
      if (ratio < 4.5) low.push(`${theme} ${ink} on ${surface}: ${ratio.toFixed(2)}:1`);
    }
  }
  assert.deepEqual(low, [], 'under the 4.5:1 bar');
});

test('input edges read at 3 to 1 on the panel colour, and the focus ring on the page and its panels, day and night', () => {
  const low = [];
  for (const [theme, t] of Object.entries({ day: LIGHT, night: NIGHT })) {
    const ratio = contrast(t['border-strong'], t['ui-surface']);
    if (ratio < 3) low.push(`${theme} border-strong on ui-surface: ${ratio.toFixed(2)}:1`);
    for (const surface of ['bg', 'ui-surface', 'surface-2']) {
      const ring = contrast(t.focus, t[surface]);
      if (ring < 3) low.push(`${theme} focus on ${surface}: ${ring.toFixed(2)}:1`);
    }
  }
  assert.deepEqual(low, []);
});

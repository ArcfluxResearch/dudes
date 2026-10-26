// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The design tokens: both themes define every one, the night set follows the shell the way the older sheet does,
// and the pairs a person reads text on keep at least 4.5 to 1, and the focus ring at least 3 to 1.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

const css = await readFile(resolve(import.meta.dirname, '../src/ui/tokens.css'), 'utf8');
const NAMES = ['bg', 'ui-surface', 'surface-2', 'border', 'border-strong', 'text', 'text-muted', 'text-subtle', 'accent',
  'accent-hover', 'accent-fg', 'accent-soft', 'focus', 'ok', 'ok-soft', 'warn', 'warn-soft', 'danger', 'danger-soft', 'info',
  'info-soft'];

function block(selector) {
  const at = css.indexOf(`${selector} {`);
  assert.ok(at >= 0, `${selector} is missing`);
  return css.slice(at, css.indexOf('}', at));
}

function tokens(selector) {
  return Object.fromEntries([...block(selector).matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})/gi)].map(([, name, hex]) => [name, hex]));
}

const THEMES = {
  light: tokens(':root'),
  // The media block nests the same declarations one level deeper; its own selector line names the shell's light pin.
  dark: tokens(":root:has(.shell[data-theme='dark'])"),
};

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

test('every token is defined for day and night, and the system night set matches the pinned one', () => {
  for (const [theme, set] of Object.entries(THEMES)) {
    for (const name of NAMES) assert.match(set[name] ?? '', /^#[0-9a-f]{6}$/i, `${theme} ${name}`);
  }
  const media = css.slice(css.indexOf('@media (prefers-color-scheme: dark)'), css.indexOf(":root:has(.shell[data-theme='dark'])"));
  assert.match(media, /:root:not\(:has\(\.shell\[data-theme='light'\]\)\)/);
  const system = Object.fromEntries([...media.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})/gi)].map(([, name, hex]) => [name, hex]));
  assert.deepEqual(system, THEMES.dark, 'the system night set differs from the pinned night set');
});

test('text reads at 4.5 to 1 on every surface it is drawn on, day and night', () => {
  const low = [];
  for (const [theme, t] of Object.entries(THEMES)) {
    const pairs = [
      ...['text', 'text-muted'].flatMap((ink) => ['bg', 'ui-surface', 'surface-2'].map((surface) => [ink, surface])),
      ['accent-fg', 'accent'], ['accent-fg', 'accent-hover'], ['accent-fg', 'danger'],
      ['accent', 'ui-surface'], ['accent', 'accent-soft'], ['accent', 'bg'],
      ['text', 'warn-soft'], ['text', 'ok-soft'], ['text', 'danger-soft'], ['text', 'info-soft'], ['text', 'accent-soft'],
      ['danger', 'ui-surface'], ['danger', 'danger-soft'], ['info', 'info-soft'],
    ];
    for (const [ink, surface] of pairs) {
      const ratio = contrast(t[ink], t[surface]);
      if (ratio < 4.5) low.push(`${theme} ${ink} on ${surface}: ${ratio.toFixed(2)}:1`);
    }
  }
  assert.deepEqual(low, [], 'under the 4.5:1 bar');
});

test('the focus ring reads at 3 to 1 on the page and its panels, day and night', () => {
  const low = [];
  for (const [theme, t] of Object.entries(THEMES)) {
    for (const surface of ['bg', 'ui-surface', 'surface-2']) {
      const ratio = contrast(t.focus, t[surface]);
      if (ratio < 3) low.push(`${theme} focus on ${surface}: ${ratio.toFixed(2)}:1`);
    }
  }
  assert.deepEqual(low, []);
});

test('hints and placeholders may fall under 4.5 to 1, so no essential text uses text-subtle', async () => {
  const { readdir } = await import('node:fs/promises');
  const files = (await readdir(resolve(import.meta.dirname, '../src/ui'))).filter((name) => name.endsWith('.tsx'));
  for (const name of files) {
    const text = await readFile(resolve(import.meta.dirname, '../src/ui', name), 'utf8');
    for (const [line] of text.matchAll(/^.*text-subtle.*$/gm)) {
      assert.match(line, /placeholder:text-subtle|selected === -1|bg-subtle|text-caption/, `${name}: ${line.trim()}`);
    }
  }
});

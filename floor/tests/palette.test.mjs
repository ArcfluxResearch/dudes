// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The day and night palettes and the type: what the stylesheet itself promises.
//
// Mutation controls, each named by the test it turns red:
//   PALETTE_CONTRAST  day --muted-ink to #9a9087 ................ "every text colour reads at 4.5:1…"
//   MUTED_ON_FILLS    night --muted-ink back to #b0a591 .......... "every text colour reads at 4.5:1…"
//   NIGHT_FOLLOWS     drop --surface from the dark theme block ... "night redefines every day colour…"
//   UI_SANS           the root font back to the monospace stack .. "the chrome is set in a sans…"
//   DATA_MONO         drop `code` from the monospace data rule ... "the chrome is set in a sans…"
//   RING_THIN         Crew rows' ring back to 1px solid var(--line)  "every focus ring is at least 2 pixels…"
//   AVATAR_LIGHT      the avatar fill back to the bare Crew colour ... "white initials read at 4.5:1 on every Crew colour…"
//   BADGE_LIGHT       the portrait badge back to #fbf6ea on the bare shirt  "white initials read at 4.5:1 on every Crew colour…"
//   BADGE_COVERED     the drawer portrait's paper fill back on the badge .. "no rule that outranks the portrait badge…"

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import { ACCENT_COLOURS } from '../src/renderer/appearance.ts';
import { themeTokens } from './helpers/theme-colour.mjs';
import { legacyStyles } from './helpers/legacy-css.mjs';

const floor = resolve(import.meta.dirname, '..');
const css = await legacyStyles();
const READABLE = 4.5;

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

test('every text colour reads at 4.5:1 on every surface it is drawn on, day and night', () => {
  const text = ['ink', 'accent-ink', 'muted-ink', 'danger-ink', 'status-exhausted', 'status-idle', 'status-attached',
    'status-running', 'status-done', 'status-waiting'];
  const surfaces = ['surface', 'surface-muted', 'output', 'card-background', 'button-background'];
  const low = [];
  for (const theme of ['light', 'dark']) {
    const tokens = themeTokens(css, theme);
    const pairs = [...text.flatMap((ink) => surfaces.map((surface) => [ink, surface])),
      ['on-accent', 'accent-ink'], ['ink', 'selected'], ['ink', 'question'], ['ink', 'danger-background'],
      ['muted-ink', 'selected'], ['muted-ink', 'question']];
    for (const [ink, surface] of pairs) {
      assert.ok(tokens[ink] && tokens[surface], `${theme} ${ink} or ${surface} is not a colour token`);
      const ratio = contrast(tokens[ink], tokens[surface]);
      if (ratio < READABLE) low.push(`${theme} ${ink} on ${surface}: ${ratio.toFixed(2)}:1`);
    }
  }
  assert.deepEqual(low, [], `under the ${READABLE}:1 bar`);
});

test('every focus ring is at least 2 pixels wide and reads at 3:1 on every panel surface, day and night', async () => {
  const sheets = [css, ...await Promise.all(['src/renderer/terminal/controls.css']
    .map((sheet) => readFile(resolve(floor, sheet), 'utf8')))].join('\n');
  const surfaces = ['surface', 'surface-muted', 'output', 'card-background', 'button-background', 'question'];
  const rings = [];
  for (const [, selector, body] of sheets.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    if (!/:focus/.test(selector) || /\.agent-select|\.board-select/.test(selector)) continue;
    const outline = /(?:^|;|\s)outline:\s*([^;]+);/.exec(body)?.[1].trim();
    if (outline !== undefined && outline !== 'none') rings.push([selector.trim().replace(/\s+/g, ' '), outline]);
  }
  assert.ok(rings.length >= 10, `only ${rings.length} focus rings found; the reader is wrong`);
  const weak = [];
  for (const [selector, outline] of rings) {
    const [, width, colour] = /^(\d+)px solid var\(--([a-z-]+)\)$/.exec(outline) ?? [];
    if (width === undefined || Number(width) < 2) {
      weak.push(`${selector}: ${outline}`);
      continue;
    }
    for (const theme of ['light', 'dark']) {
      const tokens = themeTokens(css, theme);
      for (const surface of surfaces) {
        const ratio = contrast(tokens[colour], tokens[surface]);
        if (ratio < 3) weak.push(`${theme} ${selector} on ${surface}: ${ratio.toFixed(2)}:1`);
      }
    }
  }
  assert.deepEqual(weak, [], 'these focus rings are thin or faint');
});

test('night redefines every day colour the chrome uses, so the theme toggle changes the whole page', () => {
  const day = themeTokens(css, 'light');
  const night = themeTokens(css, 'dark');
  const same = Object.keys(day).filter((name) => name !== 'studio-selected' && day[name] === night[name]);
  assert.deepEqual(same, [], 'these colours stay the same at night');
  const system = /@media \(prefers-color-scheme: dark\) \{\s*:root:not\(:has\(\.shell\[data-theme='light'\]\)\) \{([^}]+)\}/
    .exec(css)?.[1] ?? '';
  const explicit = /:root:has\(\.shell\[data-theme='dark'\]\)\s*\{([^}]+)\}/.exec(css)?.[1] ?? '';
  const names = (block) => [...block.matchAll(/--([a-z-]+):\s*([^;]+);/g)].map(([, name, value]) => `${name}=${value.trim()}`);
  assert.ok(names(explicit).length > 20, 'the dark block was not read');
  assert.deepEqual(names(system), names(explicit), 'the system night and the chosen night differ');
});

test('the chrome is set in a sans, data in monospace, and no font comes from outside the Floor', async () => {
  const root = /:root\s*\{([^}]+)\}/.exec(css)[1];
  const ui = /--font-ui:\s*([^;]+);/.exec(root)?.[1] ?? '';
  assert.match(root, /font-family: var\(--font-ui\);/);
  assert.match(ui, /sans-serif$/);
  assert.doesNotMatch(ui, /mono/i);
  assert.match(/--font-data:\s*([^;]+);/.exec(root)?.[1] ?? '', /monospace$/);
  const data = /\n((?:[^{}\n]+,\n)*[^{}\n]+) \{\n  font-family: var\(--font-data\);/.exec(css)?.[1].split(/,\n/) ?? [];
  for (const selector of ['code', 'pre', '.data', '.live-work-text']) assert.ok(data.includes(selector), `${selector} is not data type`);
  for (const sheet of ['legacy styles', 'src/renderer/terminal/controls.css']) {
    const text = sheet === 'legacy styles' ? css : await readFile(resolve(floor, sheet), 'utf8');
    assert.doesNotMatch(text, /@import|@font-face|url\(|https?:/, `${sheet} reaches for a resource`);
  }
});

test('white initials read at 4.5:1 on every Crew colour a portrait badge can wear', () => {
  const low = [];
  for (const initials of ['.portrait-badge']) {
    const rules = [...css.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/([^{}]+)\{([^{}]*)\}/g)]
      .filter(([, selectors]) => selectors.split(',').map((selector) => selector.trim()).includes(initials))
      .map(([, , body]) => body).join(';');
    const ink = [...rules.matchAll(/(?:^|[;\s])color: (#[0-9a-f]{6});/g)].at(-1)?.[1];
    const fill = [...rules.matchAll(/background: color-mix\(in srgb, var\(--agent\) (\d+)%, (#[0-9a-f]{6})\);/g)].at(-1);
    assert.ok(ink && fill, `the ${initials} rules name its initial colour and its fill`);
    const [, share, toward] = fill;
    const mix = (hex) => `#${[0, 2, 4].map((at) => {
      const channel = (colour) => Number.parseInt(colour.slice(1 + at, 3 + at), 16);
      return Math.round(channel(hex) * share / 100 + channel(toward) * (100 - share) / 100).toString(16).padStart(2, '0');
    }).join('')}`;
    low.push(...Object.entries(ACCENT_COLOURS).map(([name, hex]) => [name, contrast(ink, mix(hex))])
      .filter(([, ratio]) => ratio < READABLE).map(([name, ratio]) => `${initials} on ${name}: ${ratio.toFixed(2)}:1`));
  }
  assert.deepEqual(low, [], `under the ${READABLE}:1 bar`);
});

test('no rule that outranks the portrait badge paints over its Crew colour', () => {
  const over = [...css.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .filter(([, , body]) => /(?:^|[;\s])background(?:-color)?:/.test(body))
    .flatMap(([, selectors]) => selectors.split(',').map((selector) => selector.trim()))
    .filter((selector) => /\.(?:agent-portrait|agent-detail-portrait|hire-portrait)\b/.test(selector)
      && !selector.includes('.portrait-figure') && (selector.match(/[.[]/g) ?? []).length > 1);
  assert.deepEqual(over, [], 'these rules paint over the badge');
});

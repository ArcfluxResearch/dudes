// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The type and the portrait badge: what the sources themselves promise. The colour tokens and their contrast are
// pinned in ui-tokens.test.mjs.
//
// Mutation controls, each named by the test it turns red:
//   UI_SANS       the root font back to the monospace stack ...... "the chrome is set in a sans…"
//   AVATAR_LIGHT  the badge fill back to the bare Crew colour ...... "white initials read at 4.5:1 on every Crew colour…"
//   BADGE_COVERED a caller's plain background on the badge ......... "no caller paints over the portrait badge's Crew colour"

import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import { ACCENT_COLOURS } from '../src/renderer/appearance.ts';

const floor = resolve(import.meta.dirname, '..');
const READABLE = 4.5;
const entry = await readFile(resolve(floor, 'src/tailwind.css'), 'utf8');
const portrait = await readFile(resolve(floor, 'src/renderer/Portrait.tsx'), 'utf8');

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

test('the chrome is set in a sans, data in monospace, and no font comes from outside the Floor', () => {
  const sans = /--font-sans:\s*([^;]+);/.exec(entry)?.[1] ?? '';
  const mono = /--font-mono:\s*([^;]+);/.exec(entry)?.[1] ?? '';
  assert.match(sans, /sans-serif$/);
  assert.doesNotMatch(sans, /mono/i);
  assert.match(mono, /monospace$/);
  const base = /@layer base \{\s*:root \{([^}]+)\}/.exec(entry)?.[1] ?? '';
  assert.match(base, /font-family: var\(--font-sans\);/);
  assert.doesNotMatch(entry.replace(/url\('\.\.\/node_modules\/[^']+'\)/g, ''), /url\(|https?:/, 'the entry file reaches for a resource');
});

test('white initials read at 4.5:1 on every Crew colour a portrait badge can wear', () => {
  const badge = /cn\(BOX, '([^']*)', className, 'portrait-badge'\)/.exec(portrait)?.[1] ?? '';
  const fill = /bg-\[color-mix\(in_srgb,var\(--agent\)_(\d+)%,(#[0-9a-f]{6})\)\]/.exec(badge);
  assert.ok(fill && /(?:^|\s)text-white(?:\s|$)/.test(badge), 'the badge names its initial colour and its fill');
  const [, share, toward] = fill;
  const mix = (hex) => `#${[0, 2, 4].map((at) => {
    const channel = (colour) => Number.parseInt(colour.slice(1 + at, 3 + at), 16);
    return Math.round(channel(hex) * share / 100 + channel(toward) * (100 - share) / 100).toString(16).padStart(2, '0');
  }).join('')}`;
  const low = Object.entries(ACCENT_COLOURS).map(([name, hex]) => [name, contrast('#ffffff', mix(hex))])
    .filter(([, ratio]) => ratio < READABLE).map(([name, ratio]) => `badge on ${name}: ${ratio.toFixed(2)}:1`);
  assert.deepEqual(low, [], `under the ${READABLE}:1 bar`);
});

test("no caller paints over the portrait badge's Crew colour", async () => {
  const over = [];
  async function walk(dir) {
    for (const item of await readdir(dir, { withFileTypes: true })) {
      const path = resolve(dir, item.name);
      if (item.isDirectory()) await walk(path);
      else if (item.name.endsWith('.tsx')) {
        for (const [, classes] of (await readFile(path, 'utf8')).matchAll(/<Portrait\s+className=\{?[`"']([^`"']*)[`"']/g)) {
          for (const word of classes.split(/\s+/)) if (/^bg-/.test(word)) over.push(`${path.slice(floor.length + 1)}: ${word}`);
        }
      }
    }
  }
  await walk(resolve(floor, 'src/renderer'));
  assert.deepEqual(over, [], 'these callers paint over the badge');
});

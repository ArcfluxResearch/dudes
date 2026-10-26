// SPDX-License-Identifier: MIT OR Apache-2.0
//
// How the utility classes stay in their lane: only files that use them are read for class names, no class a
// screen uses is also the name of an older rule, and spacing keeps to the scale.

import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

const src = resolve(import.meta.dirname, '../src');

async function walk(dir, extension, found = []) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = resolve(dir, entry.name);
    if (entry.isDirectory()) await walk(path, extension, found);
    else if (entry.name.endsWith(extension)) found.push(path);
  }
  return found;
}

const sheet = await readFile(resolve(src, 'ui/ui.css'), 'utf8');
const listed = new Set([...sheet.matchAll(/@source '([^']+)'/g)].map(([, path]) => resolve(src, 'ui', path)).filter((path) => !path.includes('*')));
const usesUtilities = async (path) => /from '(?:\.\.?\/)+ui\/|from '\.\/(?:cx|Button|Field|Menu|Select)/.test(await readFile(path, 'utf8'));

test('every file outside ui that imports the owned components is named as a class source', async () => {
  const importers = [];
  for (const path of await walk(src, '.tsx')) {
    if (path.startsWith(resolve(src, 'ui') + '/')) continue;
    if (await usesUtilities(path)) importers.push(path);
  }
  assert.ok(importers.length >= 10, `only ${importers.length} importers found`);
  const unlisted = importers.filter((path) => !listed.has(path));
  assert.deepEqual(unlisted.map((path) => path.slice(src.length + 1)), [], 'add each to a @source line in ui/ui.css');
  for (const path of listed) assert.ok((await readFile(path, 'utf8')).length > 0, `${path} is listed but missing`);
});

function classTokens(text) {
  const tokens = new Set();
  for (const [, value] of text.matchAll(/(?:className|summaryClassName|dismissClassName)=(?:"([^"]*)"|\{([^}]*)\})/g)) {
    for (const part of [value, ...(value ?? '').matchAll(/'([^']*)'/g)].filter(Boolean)) {
      for (const token of String(typeof part === 'string' ? part : part[1]).split(/\s+/)) if (token !== '') tokens.add(token);
    }
  }
  for (const [, value] of text.matchAll(/'([a-z0-9 :\[\]\-.\/_()%,=*&>]{6,})'/g)) {
    for (const token of value.split(/\s+/)) if (/^[a-z0-9:\[\]\-.\/_()%,=*&>]+$/.test(token)) tokens.add(token);
  }
  return tokens;
}

test('no utility class a screen uses is the name of a rule in the older sheets', async () => {
  const legacy = new Set();
  for (const path of [...await walk(src, '.css')].filter((file) => !file.includes('/ui/'))) {
    for (const [, name] of (await readFile(path, 'utf8')).matchAll(/\.(-?[_a-zA-Z][\w-]*)/g)) legacy.add(name);
  }
  const used = new Set();
  for (const path of listed) {
    if (!path.endsWith('.tsx')) continue;
    for (const token of classTokens(await readFile(path, 'utf8'))) used.add(token);
  }
  for (const path of await walk(resolve(src, 'ui'), '.tsx')) for (const token of classTokens(await readFile(path, 'utf8'))) used.add(token);
  assert.ok(used.size > 200, `only ${used.size} class tokens read`);
  // Compile the same sheet for exactly these tokens, so what comes out is what Tailwind took for a utility.
  const { compile } = await import('@tailwindcss/node');
  const { build } = await compile(sheet, { base: resolve(src, 'ui'), onDependency() {} });
  const generated = new Set([...build([...used]).matchAll(/\.((?:\\.|[\w-])+)/g)].map(([, name]) => name.replace(/\\(.)/g, '$1')));
  assert.ok(generated.size > 150, `only ${generated.size} utilities were generated`);
  // `ui` marks the owned components and `shell` appears in the token selectors; both are meant to be in the older sheets too.
  assert.deepEqual([...generated].filter((name) => legacy.has(name) && name !== 'ui' && name !== 'shell'), [], 'a utility name is also an older rule');
});

test('spacing, gaps and offsets use the scale: 2, 4, 6, 8, 12, 16, 20, 24, 32, 40 and 48 pixels', async () => {
  const SCALE = new Set(['0', '0.5', '1', '1.5', '2', '3', '4', '5', '6', '8', '10', '12', 'px', 'auto']);
  const off = [];
  for (const path of [...listed, ...await walk(resolve(src, 'ui'), '.tsx')]) {
    if (!path.endsWith('.tsx')) continue;
    for (const token of classTokens(await readFile(path, 'utf8'))) {
      const [, , value] = /^(?:[a-z-]+:)*-?(p|px|py|pt|pr|pb|pl|m|mx|my|mt|mr|mb|ml|gap|gap-x|gap-y|space-x|space-y)-(\d+(?:\.\d+)?|px|auto)$/.exec(token) ?? [];
      if (value !== undefined && !SCALE.has(value)) off.push(`${path.slice(src.length + 1)}: ${token}`);
    }
  }
  assert.deepEqual(off, []);
});

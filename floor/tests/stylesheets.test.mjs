// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Every stylesheet in the Floor. New styling is Tailwind utilities on components and screens, so the only hand-written
// CSS that may exist is the Tailwind entry file, the component lab's entry, the third-party wrapper, and the Messages
// tab's rules, which sit in the legacy layer until that tab is rebuilt and can only shrink.

import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import test from 'node:test';

const floor = resolve(import.meta.dirname, '..');
const LAYERS = '@layer legacy, theme, base, components, utilities;';
const ENTRY = 'src/tailwind.css';
/** The component lab's own entry only imports the Tailwind entry file and names the lab's folder as a class source. */
const LAB = 'lab/lab.css';
const LAB_TEXT = "@import '../src/tailwind.css';\n@source '.';\n";
const THIRD_PARTY = new Map([['src/renderer/terminal/xterm.css', "@import '@xterm/xterm/css/xterm.css' layer(legacy);"]]);

/**
 * The Messages tab's stylesheet and what it holds now: its rule blocks and its declarations, counted with comments
 * removed. It may not grow, and when it shrinks its line is lowered to the new count. It is deleted, with this entry
 * and the layer, when the tab is rebuilt on the owned components.
 */
const LEGACY = new Map([
  ['src/legacy/messages.css', { blocks: 10, declarations: 20 }],
]);

async function stylesheets(dirs = [resolve(floor, 'src'), resolve(floor, 'lab')]) {
  const found = [];
  for (const dir of dirs) {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = resolve(dir, entry.name);
      if (entry.isDirectory()) found.push(...await stylesheets([path]));
      else if (entry.name.endsWith('.css')) found.push(relative(floor, path));
    }
  }
  return found.sort();
}

const bare = (text) => text.replace(/\/\*[\s\S]*?\*\//g, '');

/** The top-level statements of a stylesheet: each at-rule or rule head, and what it ends with (`{` for a block). */
function topLevel(text) {
  const found = [];
  let depth = 0;
  let head = '';
  for (const char of bare(text)) {
    if (char === '{') {
      if (depth === 0) found.push({ head: head.trim(), block: true });
      depth += 1;
      if (depth === 1) head = '';
    } else if (char === '}') {
      depth -= 1;
      if (depth === 0) head = '';
    } else if (depth === 0) {
      head += char;
      if (char === ';') {
        found.push({ head: head.trim(), block: false });
        head = '';
      }
    }
  }
  assert.equal(depth, 0, 'unbalanced braces');
  assert.equal(head.trim(), '', 'text after the last statement');
  return found;
}

test('the stylesheets are the Tailwind entry file, the third-party wrapper and the Messages file, and no others', async () => {
  const actual = await stylesheets();
  const expected = [ENTRY, LAB, ...THIRD_PARTY.keys(), ...LEGACY.keys()].sort();
  assert.deepEqual(actual, expected,
    'a new CSS file is not allowed: style with Tailwind utilities, and put a token or keyframe in src/tailwind.css. '
    + 'The Messages file leaves the LEGACY list in this test when it is deleted.');
});

test('the Messages file never grows, and a shrunken one has its count lowered', async () => {
  const off = [];
  for (const [file, pinned] of LEGACY) {
    const text = bare(await readFile(resolve(floor, file), 'utf8'));
    const actual = { blocks: text.split('{').length - 1, declarations: text.split(';').length - 1 };
    if (actual.blocks > pinned.blocks || actual.declarations > pinned.declarations) {
      off.push(`${file} grew from ${JSON.stringify(pinned)} to ${JSON.stringify(actual)}: new styling is Tailwind, not a rule here`);
    } else if (actual.blocks < pinned.blocks || actual.declarations < pinned.declarations) {
      off.push(`${file} shrank: set its line to ${JSON.stringify(actual)}`);
    }
  }
  assert.deepEqual(off, []);
});

test('every stylesheet starts with the one layer order, and its rules are inside a layer', async () => {
  const bad = [];
  for (const file of await stylesheets()) {
    const text = await readFile(resolve(floor, file), 'utf8');
    if (file === LAB) {
      if (text !== LAB_TEXT) bad.push(`${file}: it imports the entry file and names its folder, and holds nothing else`);
      continue;
    }
    const statements = topLevel(text);
    if (statements[0]?.head !== LAYERS) bad.push(`${file}: the first statement is not \`${LAYERS}\``);
    if (!bare(text).trimStart().startsWith(LAYERS)) bad.push(`${file}: something comes before the layer order`);
    if (file === ENTRY) continue;
    if (THIRD_PARTY.has(file)) {
      if (JSON.stringify(statements.slice(1)) !== JSON.stringify([{ head: THIRD_PARTY.get(file), block: false }])) {
        bad.push(`${file}: a third-party wrapper holds one import into the legacy layer and nothing else`);
      }
      continue;
    }
    const rest = statements.slice(1);
    if (rest.length !== 1 || rest[0].head !== '@layer legacy' || !rest[0].block) bad.push(`${file}: rules must all sit in one \`@layer legacy { }\` block`);
  }
  assert.deepEqual(bad, []);
});

test('the Tailwind entry file holds imports, layers, theme and keyframes and no style rule', async () => {
  const statements = topLevel(await readFile(resolve(floor, ENTRY), 'utf8'));
  const allowed = [/^@import /, /^@layer (?:theme|base|components|utilities)$/, /^@layer [a-z, ]+;$/, /^@font-face$/, /^@custom-variant /,
    /^@theme (?:static|inline)?$/, /^@theme$/];
  const rules = statements.filter(({ head }) => !allowed.some((pattern) => pattern.test(head)));
  assert.deepEqual(rules, [], 'a style rule belongs on a component as utilities, not in the entry file');
});

test('the entry file imports the Messages file once, before the utilities', async () => {
  const entry = await readFile(resolve(floor, ENTRY), 'utf8');
  const imports = [...entry.matchAll(/@import '([^']+)'/g)].map(([, spec]) => spec);
  assert.deepEqual(imports.slice(0, 2), ['./legacy/messages.css', 'tailwindcss/utilities.css']);
});

test('a module imports only the entry file or the third-party wrapper', async () => {
  const sources = [];
  async function walk(dir) {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = resolve(dir, entry.name);
      if (entry.isDirectory()) await walk(path);
      else if (/\.(?:ts|tsx)$/.test(entry.name)) sources.push(path);
    }
  }
  await walk(resolve(floor, 'src'));
  const imported = [];
  for (const path of sources) {
    for (const [, spec] of (await readFile(path, 'utf8')).matchAll(/^import ['"]([^'"]+\.css)['"];/gm)) {
      imported.push({ from: relative(floor, path), file: relative(floor, resolve(path, '..', spec)), spec });
    }
  }
  const unlisted = imported.filter(({ file, spec }) => !spec.startsWith('.') || ![ENTRY, ...THIRD_PARTY.keys()].includes(file));
  assert.deepEqual(unlisted, [], 'a module imports a stylesheet that is not listed');
  assert.ok(imported.some(({ from, file }) => from === 'src/main.tsx' && file === ENTRY), 'the entry file is loaded from main.tsx');
});

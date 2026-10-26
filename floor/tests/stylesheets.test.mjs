// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Every stylesheet in the Floor. New styling is Tailwind utilities on components and screens, so the only hand-written
// CSS that may exist is the Tailwind entry file, the third-party wrapper, and the older screens' rules, which sit in the
// legacy layer until each screen moves and can only shrink.

import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import test from 'node:test';

import { legacyFileNames } from './helpers/legacy-css.mjs';

const floor = resolve(import.meta.dirname, '..');
const LAYERS = '@layer legacy, theme, base, components, utilities;';
const ENTRY = 'src/tailwind.css';
/** The component lab's own entry only imports the Tailwind entry file and names the lab's folder as a class source. */
const LAB = 'lab/lab.css';
const LAB_TEXT = "@import '../src/tailwind.css';\n@source '.';\n";
const THIRD_PARTY = new Map([['src/renderer/terminal/xterm.css', "@import '@xterm/xterm/css/xterm.css' layer(legacy);"]]);

/**
 * The older screens' stylesheets and what each holds now: its rule blocks and its declarations, counted with comments
 * removed. A screen lane deletes its files and removes their lines here. A file may not grow, and when one shrinks its line
 * is lowered to the new count, so the set only ever gets smaller. The last lane deletes the legacy layer.
 */
const LEGACY = new Map([
  ['src/failure.css', { blocks: 4, declarations: 4 }],
  ['src/legacy/activity-log.css', { blocks: 32, declarations: 80 }],
  ['src/legacy/agent-actions.css', { blocks: 26, declarations: 66 }],
  ['src/legacy/agent-detail.css', { blocks: 54, declarations: 168 }],
  ['src/legacy/agent-panel.css', { blocks: 71, declarations: 164 }],
  ['src/legacy/base.css', { blocks: 7, declarations: 114 }],
  ['src/legacy/chat.css', { blocks: 106, declarations: 306 }],
  ['src/legacy/crew.css', { blocks: 38, declarations: 128 }],
  ['src/legacy/engine-sign-in.css', { blocks: 12, declarations: 41 }],
  ['src/legacy/hire-entry.css', { blocks: 13, declarations: 50 }],
  ['src/legacy/hire-wizard.css', { blocks: 55, declarations: 181 }],
  ['src/legacy/live-work.css', { blocks: 10, declarations: 28 }],
  ['src/legacy/memory.css', { blocks: 16, declarations: 39 }],
  ['src/legacy/office-controls.css', { blocks: 6, declarations: 13 }],
  ['src/legacy/office.css', { blocks: 56, declarations: 177 }],
  ['src/legacy/retire-terminal.css', { blocks: 28, declarations: 88 }],
  ['src/legacy/roster.css', { blocks: 49, declarations: 137 }],
  ['src/legacy/settings.css', { blocks: 56, declarations: 161 }],
  ['src/legacy/setup.css', { blocks: 19, declarations: 46 }],
  ['src/legacy/task.css', { blocks: 26, declarations: 71 }],
  ['src/legacy/touch-and-phone.css', { blocks: 25, declarations: 68 }],
  ['src/renderer/cardNotes.css', { blocks: 3, declarations: 16 }],
  ['src/renderer/chat/code.css', { blocks: 14, declarations: 37 }],
  ['src/renderer/chat/engine-said.css', { blocks: 3, declarations: 10 }],
  ['src/renderer/diff/diff.css', { blocks: 37, declarations: 104 }],
  ['src/renderer/model/model-list.css', { blocks: 4, declarations: 9 }],
  ['src/renderer/recovery/recovery.css', { blocks: 23, declarations: 61 }],
  ['src/renderer/terminal/controls.css', { blocks: 5, declarations: 8 }],
  ['src/renderer/ticket/ticket.css', { blocks: 126, declarations: 385 }],
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

test('the stylesheets are the Tailwind entry file, the third-party wrapper and the listed older screens, and no others', async () => {
  const actual = await stylesheets();
  const expected = [ENTRY, LAB, ...THIRD_PARTY.keys(), ...LEGACY.keys()].sort();
  assert.deepEqual(actual, expected,
    'a new CSS file is not allowed: style with Tailwind utilities, and put a token or keyframe in src/tailwind.css. '
    + 'A deleted older file must also leave the LEGACY list in this test.');
});

test('an older screen file never grows, and a shrunken one has its count lowered', async () => {
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

test('the entry file imports every older file once, in the order the cascade had before the split', async () => {
  const names = await legacyFileNames();
  const files = (await stylesheets()).filter((file) => file.startsWith('src/legacy/')).map((file) => file.slice('src/legacy/'.length)).sort();
  assert.deepEqual([...names].sort(), files);
  assert.equal(new Set(names).size, names.length);
  assert.equal(names[0], 'base.css');
});

test('a module imports only the entry file, a listed older sheet or the third-party wrapper', async () => {
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
  const unlisted = imported.filter(({ file, spec }) => !spec.startsWith('.') || ![ENTRY, ...THIRD_PARTY.keys(), ...LEGACY.keys()].includes(file));
  assert.deepEqual(unlisted, [], 'a module imports a stylesheet that is not listed');
  assert.ok(imported.some(({ from, file }) => from === 'src/main.tsx' && file === ENTRY), 'the entry file is loaded from main.tsx');
});

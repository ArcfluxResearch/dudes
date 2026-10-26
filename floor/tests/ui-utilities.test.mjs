// SPDX-License-Identifier: MIT OR Apache-2.0
//
// How the utility classes stay in their lane. Tailwind reads every file under `src` for class names, and its
// utilities sit in a layer above every older rule, so a word in a source file that happens to be a utility name
// must never reach an element that wears the same class from an older rule.

import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';
import { compile } from '@tailwindcss/node';
import { Scanner } from '@tailwindcss/oxide';
import ts from 'typescript';

const src = resolve(import.meta.dirname, '../src');

async function walk(dir, test, found = []) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = resolve(dir, entry.name);
    if (entry.isDirectory()) await walk(path, test, found);
    else if (test(entry.name)) found.push(path);
  }
  return found;
}

const entry = await readFile(resolve(src, 'tailwind.css'), 'utf8');
const compiler = await compile(entry, { base: src, onDependency() {} });
const sources = [...(compiler.root && compiler.root !== 'none' ? [{ ...compiler.root, negated: false }] : []), ...compiler.sources];
const candidates = new Scanner({ sources }).scan();
const css = compiler.build(candidates);
const unescape = (name) => name.replace(/\\(.)/g, '$1');

/** The text inside the utilities layer's block, where Tailwind put the rules it generated. */
function utilitiesLayer(text) {
  const open = text.indexOf('@layer utilities {');
  assert.ok(open >= 0, 'the build has a utilities layer');
  let depth = 0;
  for (let at = text.indexOf('{', open); at < text.length; at += 1) {
    if (text[at] === '{') depth += 1;
    else if (text[at] === '}' && (depth -= 1) === 0) return text.slice(text.indexOf('{', open) + 1, at);
  }
  throw new Error('the utilities layer never closes');
}

const generated = new Set([...utilitiesLayer(css).matchAll(/\.((?:\\.|[\w-])+)/g)].map(([, name]) => unescape(name)));

const uiFiles = await walk(resolve(src, 'ui'), (name) => name.endsWith('.tsx') || name.endsWith('.ts'));
const CLASS_TOKEN = /^[a-z!-][\w:[\]/.()%,&>=*#-]*$/;

/** The class names the owned components write: the strings handed to `cn`, to `className`, and the style tables. */
function classTokensIn(text, file) {
  const syntax = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const found = new Set();
  const add = (value) => {
    const tokens = value.split(/\s+/).filter((token) => token !== '');
    if (tokens.length > 0 && tokens.every((token) => CLASS_TOKEN.test(token))) for (const token of tokens) found.add(token);
  };
  function inClassPosition(node) {
    if (ts.isPropertyAssignment(node.parent) && node.parent.name === node) return false;
    if (ts.isCallExpression(node.parent) && node.parent.expression.getText(syntax) !== 'cn') return false;
    for (let up = node.parent; up !== undefined; up = up.parent) {
      if (ts.isCallExpression(up) && up.expression.getText(syntax) === 'cn') return true;
      if (ts.isJsxAttribute(up)) return up.name.getText(syntax) === 'className';
      if (ts.isVariableDeclaration(up)) return /^[A-Z_]+$/.test(up.name.getText(syntax));
    }
    return false;
  }
  (function visit(node) {
    if ((ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) && inClassPosition(node)) add(node.text);
    ts.forEachChild(node, visit);
  })(syntax);
  return found;
}

const owned = new Set();
for (const file of uiFiles) {
  if (!file.endsWith('.tsx') && !file.endsWith('look.ts')) continue;
  for (const token of classTokensIn(await readFile(file, 'utf8'), file)) owned.add(token);
}

test('the whole of src is read for class names, and the owned components get every utility they write', () => {
  assert.ok(candidates.length > 1000, `only ${candidates.length} candidates were read`);
  assert.ok(owned.size > 150, `only ${owned.size} class names were read from src/ui`);
  const missing = [...owned].filter((token) => !generated.has(token));
  assert.deepEqual(missing, [], 'a class the components write produces no CSS: a typo, or a theme value that is not defined');
});

test('no utility has the name of a class an older rule sets', async () => {
  const legacy = new Set();
  for (const path of await walk(src, (name) => name.endsWith('.css'))) {
    if (path.endsWith('tailwind.css')) continue;
    const text = (await readFile(path, 'utf8')).replace(/\/\*[\s\S]*?\*\//g, '');
    for (const [, name] of text.matchAll(/\.(-?[_a-zA-Z][\w-]*)/g)) legacy.add(name);
  }
  assert.ok(legacy.size > 50, `only ${legacy.size} older class names were read`);
  assert.ok(generated.size > 150, `only ${generated.size} utilities were generated`);
  assert.deepEqual([...generated].filter((name) => legacy.has(name)), [],
    'an older rule shares a name with a utility, so the utility would reach its elements: rename the older class');
});

test('no screen that has not moved to the owned components wears a class a utility would take', async () => {
  const wearing = [];
  for (const path of await walk(src, (name) => name.endsWith('.tsx'))) {
    if (path.startsWith(resolve(src, 'ui') + '/')) continue;
    const text = await readFile(path, 'utf8');
    if (/from '(?:\.\.?\/)+ui\//.test(text)) continue;
    for (const [, plain, braced] of text.matchAll(/className=(?:"([^"]*)"|\{([^}]*)\})/g)) {
      const words = [...(plain ?? '').split(/\s+/), ...[...(braced ?? '').matchAll(/['"`]([^'"`]*)['"`]/g)].flatMap(([, part]) => part.split(/\s+/))];
      for (const word of words) if (word !== '' && generated.has(word)) wearing.push(`${path.slice(src.length + 1)}: ${word}`);
    }
  }
  assert.deepEqual(wearing, []);
});

test('weights are 400, 500 and 600, and spacing keeps to the scale of 2, 4, 6, 8, 12, 16, 20, 24, 32, 40 and 48 pixels', () => {
  const SCALE = new Set(['0', '0.5', '1', '1.5', '2', '3', '4', '5', '6', '8', '10', '12', 'px', 'auto']);
  const off = [];
  for (const token of owned) {
    if (/(?:^|:)font-(?:thin|extralight|light|bold|extrabold|black)$/.test(token)) off.push(token);
    const [, , value] = /^(?:[a-z-]+:)*-?(p|px|py|pt|pr|pb|pl|m|mx|my|mt|mr|mb|ml|gap|gap-x|gap-y|space-x|space-y)-(\d+(?:\.\d+)?|px|auto)$/.exec(token) ?? [];
    if (value !== undefined && !SCALE.has(value)) off.push(token);
  }
  assert.deepEqual(off, []);
});

// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import ts from 'typescript';

import { restoreFocusAfterDialog } from '../src/renderer/focus.ts';

const root = resolve(import.meta.dirname, '..');

test('CLOSE_BUTTON and CANCEL_BUTTON mutants offer both explicit dismissal paths', async () => {
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-hire-dialog'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { HireWizardView } = await server.ssrLoadModule('/src/renderer/onboarding/HireWizard.tsx');
    const { getHireState } = await server.ssrLoadModule('/src/renderer/onboarding/hire.ts');
    const { getFloorState } = await server.ssrLoadModule('/src/renderer/floor/store.ts');
    const html = renderToStaticMarkup(createElement(HireWizardView, {
      hire: getHireState(), floor: getFloorState(), onClose() {},
    }));
    assert.match(html, /<button[^>]*>Close<\/button>/);
    assert.match(html, /<button[^>]*>Cancel<\/button>/);
  } finally { await server.close(); }
});

test('ESCAPE_DISMISS and OUTSIDE_DISMISS mutants route native dialog exits through the same close path', async () => {
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  const syntax = ts.createSourceFile('shell.tsx', shell, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let dialog;
  function visit(node) {
    if (ts.isJsxOpeningElement(node) && node.tagName.getText(syntax) === 'dialog') dialog = node;
    ts.forEachChild(node, visit);
  }
  visit(syntax);
  assert.ok(dialog, 'the Hire wizard needs a native modal dialog');
  const props = Object.fromEntries(dialog.attributes.properties.filter(ts.isJsxAttribute)
    .map((entry) => [entry.name.getText(syntax), entry.initializer?.getText(syntax)]));
  assert.equal(props.onCancel, '{(event) => { event.preventDefault(); closeHire(); }}');
  assert.equal(props.onClick, undefined);
  assert.match(props.onPointerDown, /event\.target === event\.currentTarget/);
  assert.match(props.onPointerUp, /if \(dismiss\) closeHire\(\);/);
  assert.match(shell, /<HireWizard floor=\{floor\} onClose=\{\(\) => closeHire\(\)\}/);
});

test('FOCUS_RETURN mutant restores the opener and falls back if that control left the page', () => {
  let active = '';
  let next;
  const schedule = (callback) => { next = callback; };
  const opener = { isConnected: true, focus() { active = 'opener'; } };
  const fallback = { isConnected: true, focus() { active = 'fallback'; } };
  restoreFocusAfterDialog(opener, fallback, schedule);
  next();
  assert.equal(active, 'opener');
  opener.isConnected = false;
  restoreFocusAfterDialog(opener, fallback, schedule);
  next();
  assert.equal(active, 'fallback');
});

test('GUIDE_FOCUS_RETURN mutant keeps the Guide opener mounted while Hire is open', async () => {
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  assert.match(shell, /\{guide === 'open' \?/);
  const open = shell.match(/function openHire\(opener: HTMLElement \| null\): void \{([\s\S]*?)\n  \}/);
  assert.ok(open);
  assert.doesNotMatch(open[1], /setGuide\('closed'\)/);
});

test('ARCHIVE_FOCUS_RETURN mutant keeps the focus roster opener mounted while Hire is open', async () => {
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  const hireAgain = shell.match(/function hireAgain\(row: RetiredAgentRow, opener: HTMLButtonElement\): void \{([\s\S]*?)\n  \}/);
  assert.ok(hireAgain);
  assert.doesNotMatch(hireAgain[1], /setFocused\(false\)/);
});

test('FALLBACK_FOCUS_RETURN mutant uses a surviving control when Hire availability changes', async () => {
  let active = '';
  let next;
  const opener = { isConnected: true, disabled: true, focus() { active = 'disabled opener'; } };
  const fallback = { focus() { active = 'fallback'; } };
  restoreFocusAfterDialog(opener, fallback, (callback) => { next = callback; });
  next();
  assert.equal(active, 'fallback');
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  assert.match(shell, /document\.getElementById\('hire-open'\) \?\? document\.getElementById\('crew-toggle'\)/);
});


test('GUIDE_POINTER_OPENER mutant passes the clicked Guide button even if another control has focus', async () => {
  const guide = await readFile(resolve(root, 'src/renderer/setup/FirstRunGuide.tsx'), 'utf8');
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  assert.match(guide, /onClick=\{\(event\) => onHire\(event\.currentTarget\)\}>Open Hire wizard/);
  assert.match(shell, /onHire=\{\(opener\) => openHire\(opener\)\}/);
});

test('ARCHIVE_POINTER_OPENER mutant passes the clicked archive button even if another control has focus', async () => {
  const archive = await readFile(resolve(root, 'src/renderer/ArchivedAgents.tsx'), 'utf8');
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  assert.match(archive, /onClick=\{\(event\) => onHireAgain\(row, event\.currentTarget\)\}>Hire again/);
  assert.match(shell, /function hireAgain\(row: RetiredAgentRow, opener: HTMLButtonElement\): void \{/);
  assert.match(shell, /hireOpener\.current = opener;/);
});

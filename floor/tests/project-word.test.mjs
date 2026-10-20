// SPDX-License-Identifier: MIT OR Apache-2.0
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { oldProjectWords } from './helpers/project-word.mjs';

const root = resolve(import.meta.dirname, '../..');

function markdown(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => entry.isDirectory()
    ? markdown(join(dir, entry.name))
    : entry.name.endsWith('.md') ? [join(dir, entry.name)] : []);
}

test('the user docs and the CLI usage say project, never workspace', () => {
  const usage = readFileSync(join(root, 'bins/dudes/src/args.rs'), 'utf8');
  const texts = [
    ...[join(root, 'README.md'), ...markdown(join(root, 'docs'))]
      .map((file) => [file.slice(root.length + 1), readFileSync(file, 'utf8')]),
    ['dudes usage', usage.slice(usage.indexOf('pub const USAGE'), usage.indexOf('";', usage.indexOf('pub const USAGE')))],
  ];
  const found = texts.flatMap(([name, text]) => oldProjectWords(text).map((context) => `${name}: ${context}`));
  assert.deepEqual(found, []);
});

test('the pattern finds the old word and leaves the Cargo flag and the Codex sandbox alone', () => {
  assert.equal(oldProjectWords('Choose the workspace, or Workspaces in Settings').length, 2);
  assert.deepEqual(oldProjectWords('cargo nextest run --workspace in a workspace-write sandbox (workspaceWrite)'), []);
  assert.deepEqual(oldProjectWords('Choose the project and an engine'), []);
});

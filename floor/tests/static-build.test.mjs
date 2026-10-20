import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readdir, readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';
import { servedOutputPath, validateRuntimeNotices } from '../scripts/write-release-manifest.mjs';

const dist = resolve(import.meta.dirname, '../dist');

async function files(dir = dist, prefix = '') {
  const result = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) result.push(...await files(resolve(dir, entry.name), path));
    else if (entry.isFile()) result.push(path);
    else assert.fail(`non-regular build output: ${path}`);
  }
  return result;
}

test('built index has no inline executable or style', async () => {
  const html = await readFile(resolve(dist, 'index.html'), 'utf8');
  assert.doesNotMatch(html, /<style\b/i);
  assert.doesNotMatch(html, /\sstyle\s*=/i);
  assert.doesNotMatch(html, /\son[a-z]+\s*=/i);
  for (const script of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
    assert.match(script[1], /\ssrc\s*=/i);
    assert.equal(script[2].trim(), '');
  }
});

test('release manifest covers the static build and excludes build tools', async () => {
  const manifest = JSON.parse(await readFile(resolve(dist, 'release-manifest.json'), 'utf8'));
  const actual = (await files()).filter((path) => path !== 'release-manifest.json').sort();
  assert.deepEqual(manifest.files.map((file) => file.path).sort(), actual);
  for (const file of manifest.files) {
    const bytes = await readFile(resolve(dist, ...file.path.split('/')));
    assert.equal(file.bytes, bytes.length);
    assert.equal(file.sha256, createHash('sha256').update(bytes).digest('hex'));
    assert.equal(servedOutputPath(file.path), true, file.path);
    assert.doesNotMatch(file.path, /node_modules/i);
  }
  const notice = manifest.files.find((file) => /third-party-licenses-.*\.json$/.test(file.path));
  assert.ok(notice, 'hashed bundled notice is admitted');
  const notices = JSON.parse(await readFile(resolve(dist, notice.path), 'utf8'));
  assert.doesNotThrow(() => validateRuntimeNotices(notices));
  assert.deepEqual(
    notices.map(({ name, version, identifier }) => `${name}@${version}@${identifier}`).sort(),
    ['@xterm/xterm@6.0.0@MIT', 'react-dom@19.3.1@MIT', 'react@19.3.1@MIT', 'scheduler@0.28.0@MIT', 'three@0.186.1@MIT'],
  );
});

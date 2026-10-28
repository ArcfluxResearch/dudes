import assert from 'node:assert/strict';
import test from 'node:test';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { checkLock } from '../scripts/check-dependency-licenses.mjs';
import {
  hashedAsset,
  servedOutputPath,
  treeSha256,
  validManifestPath,
  validateBuildContract,
  validateRuntimeNotices,
  editorNotices,
} from '../scripts/write-release-manifest.mjs';

const contract = JSON.parse(await readFile(resolve(import.meta.dirname, 'fixtures/static-contract.json'), 'utf8'));

test('the release build selects browser generation five with manifest schema one', async () => {
  const build = JSON.parse(await readFile(resolve(import.meta.dirname, '../build-contract.json'), 'utf8'));
  assert.deepEqual(build, { schema_version: 1, base_path: '/floor/', browser_wire_version: 9 });
  assert.doesNotThrow(() => validateBuildContract(build));
  for (const browser_wire_version of [1, 2, 3, 4, 5, 6, 7, 8]) {
    assert.throws(() => validateBuildContract({ ...build, browser_wire_version }), /unsupported Floor build contract/);
  }
  assert.throws(() => validateBuildContract({ ...build, schema_version: 3 }), /unsupported Floor build contract/);
});

test('manifest paths use one portable grammar', () => {
  for (const path of ['index.html', 'assets/index-Abc_1234.js']) assert.equal(validManifestPath(path), true);
  for (const path of [
    '', '../secret', 'assets/../secret', 'assets/%2e%2e/secret', 'assets\\..\\secret',
    'C:secret', 'C:\\secret', '\\\\server\\share', '\\rooted', '/absolute', 'assets//app.js',
  ]) assert.equal(validManifestPath(path), false, path);
});

test('tree digest is byte-sorted and canonical', () => {
  const files = [
    { path: 'index.html', bytes: 3, sha256: 'b'.repeat(64) },
    { path: 'assets/app-12345678.js', bytes: 2, sha256: 'a'.repeat(64) },
  ];
  assert.equal(treeSha256(files), treeSha256([...files].reverse()));
  assert.notEqual(treeSha256(files), treeSha256([{ ...files[0], bytes: 4 }, files[1]]));
});

test('dependency licence check rejects missing and unreviewed MPL entries', async () => {
  const lock = JSON.parse(await readFile(resolve(import.meta.dirname, '../package-lock.json'), 'utf8'));
  assert.equal(checkLock(lock).direct, 23);
  const missing = structuredClone(lock);
  delete missing.packages['node_modules/react'].license;
  assert.throws(() => checkLock(missing), /unreviewed licence/);
  const mpl = structuredClone(lock);
  mpl.packages['node_modules/lightningcss-unreviewed'] = { version: '1.33.0', license: 'MPL-2.0' };
  assert.throws(() => checkLock(mpl), /unreviewed MPL package/);
});

test('the Open Font Licence is accepted for the two font packages and for nothing else', async () => {
  const lock = JSON.parse(await readFile(resolve(import.meta.dirname, '../package-lock.json'), 'utf8'));
  for (const font of ['inter', 'jetbrains-mono']) assert.equal(lock.packages[`node_modules/@fontsource-variable/${font}`].license, 'OFL-1.1');
  const other = structuredClone(lock);
  other.packages['node_modules/some-other-font'] = { version: '1.0.0', license: 'OFL-1.1' };
  assert.throws(() => checkLock(other), /some-other-font: unreviewed licence OFL-1.1/);
});

test('a Tailwind release that pins another lightningcss fails with the way out', async () => {
  const lock = JSON.parse(await readFile(resolve(import.meta.dirname, '../package-lock.json'), 'utf8'));
  assert.equal(lock.packages['node_modules/lightningcss'].version, '1.33.0', 'the override holds one reviewed copy');
  const nested = structuredClone(lock);
  nested.packages['node_modules/@tailwindcss/node/node_modules/lightningcss'] = { version: '1.34.0', license: 'MPL-2.0' };
  assert.throws(() => checkLock(nested), /unreviewed MPL package\. Only lightningcss 1\.33\.0 is reviewed.*"overrides"/);
});

test('hash suffix and served extensions use the shared finite contract', () => {
  for (const path of contract.hashed_valid) assert.equal(hashedAsset(path), true, path);
  for (const path of contract.hashed_invalid) assert.equal(hashedAsset(path), false, path);
  for (const path of contract.served_valid) assert.equal(servedOutputPath(path), true, path);
  for (const path of contract.served_invalid) assert.equal(servedOutputPath(path), false, path);
});

test('runtime notices are exactly the copied code set', () => {
  const notices = [
    { name: '@xterm/xterm', version: '6.0.0', identifier: 'MIT', text: 'notice' },
    { name: 'react', version: '19.3.1', identifier: 'MIT', text: 'notice' },
    { name: 'react-dom', version: '19.3.1', identifier: 'MIT', text: 'notice' },
    { name: 'scheduler', version: '0.28.0', identifier: 'MIT', text: 'notice' },
    { name: 'three', version: '0.186.1', identifier: 'MIT', text: 'notice' },
  ];
  const editor = editorNotices.map((entry) => {
    const [, name, version, identifier] = /^(.+)@([^@]+)@([^@]+)$/.exec(entry);
    return { name, version, identifier, text: 'notice' };
  });
  assert.equal(editor.length, 37);
  assert.doesNotThrow(() => validateRuntimeNotices([...notices, ...editor]));
  assert.throws(() => validateRuntimeNotices(notices), /runtime notice set/, 'the editor chunk lost its notices');
  assert.throws(() => validateRuntimeNotices([...notices, ...editor.slice(1)]), /runtime notice set/, 'one editor package lost its notice');
  notices.push(...editor);
  assert.doesNotThrow(() => validateRuntimeNotices(notices));
  for (const injected of [
    { name: 'vite', version: '8.3.0', identifier: 'MIT', text: 'notice' },
    { name: 'typescript', version: '6.0.2', identifier: 'Apache-2.0', text: 'notice' },
    { name: 'lightningcss', version: '1.33.0', identifier: 'MPL-2.0', text: 'notice' },
  ]) {
    assert.throws(() => validateRuntimeNotices([...notices, injected]), /runtime notice set/);
  }
  assert.throws(() => validateRuntimeNotices([...notices, notices[0]]), /runtime notice set/);
  assert.throws(() => validateRuntimeNotices(notices.slice(1)), /runtime notice set/, 'the bundled xterm lost its notice');
  const libraries = [
    { name: 'lucide-react', version: '1.55.0', identifier: 'ISC', text: 'notice' },
    { name: 'tailwind-merge', version: '3.7.0', identifier: 'MIT', text: 'notice' },
  ];
  assert.doesNotThrow(() => validateRuntimeNotices([...notices, ...libraries]), 'the interface libraries may be bundled');
  assert.doesNotThrow(() => validateRuntimeNotices([...notices, libraries[0]]));
  assert.throws(() => validateRuntimeNotices([...notices, { ...libraries[0], version: '1.56.0' }]), /runtime notice set/, 'an unreviewed icon release');
});

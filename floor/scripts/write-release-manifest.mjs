import { createHash } from 'node:crypto';
import { readdir, readFile, rename, stat, writeFile } from 'node:fs/promises';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';
import { validReleaseVersion } from '../src/renderer/settings/version.ts';

const here = dirname(fileURLToPath(import.meta.url));
const floorRoot = resolve(here, '..');
const repoRoot = resolve(floorRoot, '..');

export function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

export function validManifestPath(path) {
  if (!path || !/^[\x00-\x7f]+$/.test(path) || path.startsWith('/') || path.endsWith('/')) return false;
  if (path.includes('\\') || path.includes(':') || path.includes('%')) return false;
  const segments = path.split('/');
  return segments.every((segment) =>
    segment !== '' && segment !== '.' && segment !== '..' && /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(segment),
  );
}

export function treeSha256(files) {
  const hash = createHash('sha256');
  for (const file of [...files].sort((a, b) => Buffer.from(a.path).compare(Buffer.from(b.path)))) {
    hash.update(file.path);
    hash.update(Buffer.from([0]));
    hash.update(String(file.bytes));
    hash.update(Buffer.from([0]));
    hash.update(file.sha256);
    hash.update('\n');
  }
  return hash.digest('hex');
}

async function regularFiles(root, current = root) {
  const files = [];
  for (const entry of await readdir(current, { withFileTypes: true })) {
    const path = join(current, entry.name);
    if (entry.isSymbolicLink()) throw new Error(`build output contains symlink: ${path}`);
    if (entry.isDirectory()) files.push(...await regularFiles(root, path));
    else if (entry.isFile()) files.push(relative(root, path).split(sep).join('/'));
    else throw new Error(`build output is not a regular file: ${path}`);
  }
  return files;
}

export function daemonVersion() {
  const result = spawnSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {
    cwd: repoRoot,
    encoding: 'utf8',
    shell: false,
  });
  if (result.status !== 0) throw new Error(`cargo metadata failed: ${result.stderr}`);
  const metadata = JSON.parse(result.stdout);
  const pkg = metadata.packages.find((candidate) => candidate.name === 'duded');
  if (!pkg) throw new Error('cargo metadata did not contain duded');
  if (!validReleaseVersion(pkg.version)) throw new Error('invalid release version');
  return pkg.version;
}

export function hashedAsset(path) {
  const file = path.split('/').at(-1) ?? '';
  return path.startsWith('assets/') && /-[A-Za-z0-9_-]{8,}\.[A-Za-z0-9]+$/.test(file);
}

const servedExtensions = new Set(['js', 'css', 'json', 'png', 'svg', 'woff2']);

export function servedOutputPath(path) {
  if (path === 'index.html') return true;
  const extension = path.split('.').at(-1) ?? '';
  return hashedAsset(path) && servedExtensions.has(extension);
}

const runtimeNotices = new Set([
  '@xterm/xterm@6.0.0@MIT',
  'react@19.3.1@MIT',
  'react-dom@19.3.1@MIT',
  'scheduler@0.28.0@MIT',
  'three@0.186.1@MIT',
]);

export function validateRuntimeNotices(notices) {
  if (!Array.isArray(notices)) throw new Error('runtime notice set is not an array');
  const actual = new Set();
  for (const notice of notices) {
    if (!notice || typeof notice.text !== 'string' || notice.text.trim() === '') {
      throw new Error('runtime notice set contains an empty notice');
    }
    actual.add(`${notice.name}@${notice.version}@${notice.identifier}`);
  }
  if (
    notices.length !== runtimeNotices.size
    || actual.size !== runtimeNotices.size
    || [...actual].some((value) => !runtimeNotices.has(value))
  ) {
    throw new Error(`runtime notice set differs: ${[...actual].sort().join(', ')}`);
  }
}

export function validateBuildContract(contract) {
  if (contract.schema_version !== 1 || contract.base_path !== '/floor/' || contract.browser_wire_version !== 3) {
    throw new Error('unsupported Floor build contract');
  }
}

export async function writeReleaseManifest(dist = resolve(floorRoot, 'dist')) {
  const contract = JSON.parse(await readFile(resolve(floorRoot, 'build-contract.json'), 'utf8'));
  validateBuildContract(contract);

  const generatedLicense = resolve(dist, '.vite/third-party-licenses.json');
  const licenseBytes = await readFile(generatedLicense);
  validateRuntimeNotices(JSON.parse(licenseBytes));
  const licensePath = `assets/third-party-licenses-${sha256(licenseBytes)}.json`;
  await rename(generatedLicense, resolve(dist, licensePath));

  const paths = (await regularFiles(dist)).filter((path) => path !== 'release-manifest.json');
  const files = [];
  for (const path of paths) {
    if (!validManifestPath(path)) throw new Error(`invalid manifest path: ${path}`);
    if (!servedOutputPath(path)) throw new Error(`unsupported or unhashed build output: ${path}`);
    const bytes = await readFile(resolve(dist, ...path.split('/')));
    const info = await stat(resolve(dist, ...path.split('/')));
    files.push({
      path,
      bytes: info.size,
      sha256: sha256(bytes),
      cache: path === 'index.html' ? 'no-store' : 'immutable',
    });
  }
  if (files.filter((file) => file.path === 'index.html').length !== 1) {
    throw new Error('build must contain exactly one index.html');
  }
  const manifest = {
    schema_version: contract.schema_version,
    base_path: contract.base_path,
    daemon_version: daemonVersion(),
    browser_wire_version: contract.browser_wire_version,
    asset_tree_sha256: treeSha256(files),
    files: files.sort((a, b) => Buffer.from(a.path).compare(Buffer.from(b.path))),
  };
  await writeFile(resolve(dist, 'release-manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  return manifest;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  await writeReleaseManifest();
}

import { readFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const floorRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const allowed = new Set(['MIT', 'Apache-2.0', 'ISC', 'BSD-3-Clause', 'MPL-2.0']);
const direct = new Map([
  ['node_modules/@xterm/xterm', '6.0.0'],
  ['node_modules/react', '19.3.1'],
  ['node_modules/react-dom', '19.3.1'],
  ['node_modules/three', '0.186.1'],
  ['node_modules/vite', '8.3.4'],
  ['node_modules/@vitejs/plugin-react', '6.1.2'],
  ['node_modules/typescript', '6.0.2'],
  ['node_modules/@types/react', '19.3.0'],
  ['node_modules/@types/react-dom', '19.3.0'],
  ['node_modules/@types/three', '0.186.0'],
]);
const forbidden = new Set(['node_modules/pixi.js', 'node_modules/zustand']);
const mpl = new Set([
  'node_modules/lightningcss',
  'node_modules/lightningcss-android-arm64',
  'node_modules/lightningcss-darwin-arm64',
  'node_modules/lightningcss-darwin-x64',
  'node_modules/lightningcss-freebsd-x64',
  'node_modules/lightningcss-linux-arm-gnueabihf',
  'node_modules/lightningcss-linux-arm64-gnu',
  'node_modules/lightningcss-linux-arm64-musl',
  'node_modules/lightningcss-linux-x64-gnu',
  'node_modules/lightningcss-linux-x64-musl',
  'node_modules/lightningcss-win32-arm64-msvc',
  'node_modules/lightningcss-win32-x64-msvc',
]);
export function checkLock(lock) {
  let seenMpl = 0;
  let packages = 0;
  for (const [path, pkg] of Object.entries(lock.packages ?? {})) {
    if (path === '') continue;
    packages += 1;
    if (!pkg.license || !allowed.has(pkg.license)) throw new Error(`${path}: unreviewed licence ${pkg.license}`);
    if (forbidden.has(path)) throw new Error(`${path}: outside A01`);
    if (pkg.license === 'MPL-2.0') {
      if (!mpl.has(path) || pkg.version !== '1.33.0') throw new Error(`${path}: unreviewed MPL package`);
      seenMpl += 1;
    }
  }
  if (seenMpl !== mpl.size) throw new Error(`expected ${mpl.size} reviewed MPL packages, saw ${seenMpl}`);
  for (const [path, version] of direct) {
    if (lock.packages?.[path]?.version !== version) throw new Error(`${path}: expected ${version}`);
  }
  return { packages, direct: direct.size, status: 'accepted licence set' };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const lock = JSON.parse(await readFile(resolve(floorRoot, 'package-lock.json'), 'utf8'));
  console.log(JSON.stringify(checkLock(lock)));
}

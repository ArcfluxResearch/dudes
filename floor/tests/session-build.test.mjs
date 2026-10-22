// A link pasted into an open page is exchanged only when the daemon serves the
// build the page runs: the page's code is never connected to a
// different build, and the link is not spent on one.
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { readFile, readdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import {
  BuildMismatch,
  LinkRefused,
  currentFloorSession,
  ownAssetRefs,
  reauthenticateFloorSessionWith,
  resetFloorSession,
  servedAssetRefs,
} from '../src/session.ts';
import { fakeDaemon } from './helpers/fake-daemon.mjs';
import { testSigner } from './helpers/signer.mjs';

const BOOTSTRAP = `A${'b'.repeat(41)}E`;
const OWN = ['/floor/assets/index-aaaa1111.js', '/floor/assets/index-bbbb2222.css'];

function pasted() {
  const location = { hash: `#bootstrap=${BOOTSTRAP}`, pathname: '/floor/', search: '' };
  return { location, history: { replaceState() { location.hash = ''; } } };
}

function indexNaming(refs) {
  const html = refs.map((ref) => ref.endsWith('.css')
    ? `<link rel="stylesheet" crossorigin href="${ref}">`
    : `<script type="module" crossorigin src="${ref}"></script>`).join('');
  return { ok: true, status: 200, async text() { return html; } };
}

/** A daemon serving `index` and answering the exchange, recording what it was sent. */
function ports(index, posts) {
  const daemon = fakeDaemon({ links: [BOOTSTRAP] });
  const { signer } = testSigner();
  const plain = async (url, init) => {
    if (init.method === 'GET') return typeof index === 'function' ? index(url) : index;
    posts.push(url);
    return daemon.plain(url, init);
  };
  return { signer, plain, signed: async () => { throw new Error('no signed request expected'); }, asked: [] };
}

function paste(index, posts) {
  const { location, history } = pasted();
  return reauthenticateFloorSessionWith(location, history, OWN, ports(index, posts));
}

test('a link pasted into a page from another build is refused before it is spent', async () => {
  resetFloorSession();
  const posts = [];
  await assert.rejects(paste(indexNaming(['/floor/assets/index-cccc3333.js', OWN[1]]), posts), BuildMismatch);
  await assert.rejects(paste(indexNaming(OWN.slice(0, 1)), posts), BuildMismatch, 'a served index naming fewer files');
  assert.deepEqual(posts, [], 'a link was sent to a daemon serving another build');
  assert.equal(currentFloorSession(), null, 'a refused link signed the page in');

  const session = await paste(indexNaming([...OWN].reverse()), posts);
  assert.equal(typeof session.device, 'string', 'the control: the same build takes the link');
  assert.deepEqual(posts, ['/floor/session']);
});

test('a link is not spent when the daemon’s index cannot be read', async () => {
  resetFloorSession();
  const posts = [];
  await assert.rejects(paste({ ok: false, status: 500 }, posts), LinkRefused);
  await assert.rejects(paste({ ok: true, status: 200, async text() { return '<html></html>'; } }, posts), LinkRefused);
  assert.deepEqual(posts, []);
});

test('the page names the built files it was loaded from', () => {
  const element = (attribute, value) => ({ getAttribute: (name) => (name === attribute ? value : null) });
  const doc = {
    querySelectorAll: () => [
      element('src', OWN[0]),
      element('href', OWN[1]),
      element('href', 'https://elsewhere.example/x.css'),
    ],
  };
  assert.deepEqual(ownAssetRefs(doc), OWN);
});

test('an element that is not a script or a stylesheet link is not part of the served build', async () => {
  resetFloorSession();
  const posts = [];
  const html = `<!-- <script src="/floor/assets/old-0000.js"></script> -->
    <img src="/floor/assets/logo-9999.png"><a href="/floor/assets/notes-8888.html">notes</a>
    <script type="module" crossorigin src="/floor/assets/index-aaaa1111.js"></script>
    <link rel="stylesheet" crossorigin href="/floor/assets/index-bbbb2222.css">`;
  const session = await paste({ ok: true, status: 200, async text() { return html; } }, posts);
  assert.equal(typeof session.device, 'string', 'markup the page does not read as a build file changed the build');
  assert.deepEqual(posts, ['/floor/session']);
});

test('a served tag names its file whatever the order of its attributes', () => {
  const html = '<script src="/floor/assets/index-aaaa1111.js" type="module" crossorigin></script><link href="/floor/assets/index-bbbb2222.css" rel="stylesheet">';
  assert.deepEqual(servedAssetRefs(html), OWN);
});

test('a page that loaded with no build files refuses the link without asking the daemon', async () => {
  resetFloorSession();
  const posts = [];
  const asked = [];
  const { location, history } = pasted();
  const held = ports(indexNaming(OWN), posts);
  const plain = async (url, init) => { asked.push(`${init.method} ${url}`); return held.plain(url, init); };
  await assert.rejects(reauthenticateFloorSessionWith(location, history, [], { ...held, plain }), LinkRefused);
  assert.deepEqual(posts, [], 'a link was spent by a page that cannot tell which build it is');
  assert.deepEqual(asked, [], 'the daemon was asked about a build the page cannot name');
});

const BUILT_INDEX = resolve(import.meta.dirname, '../dist/index.html');

test('the built index names its files so the served-index reader finds them all, and every one is built', { skip: !existsSync(BUILT_INDEX) && 'dist is not built' }, async () => {
  const html = await readFile(BUILT_INDEX, 'utf8');
  const served = servedAssetRefs(html);
  const named = [...new Set([...html.matchAll(/\/floor\/assets\/[^"'\s>]+/g)].map((match) => match[0]))].sort();
  assert.deepEqual([...new Set(served)].sort(), named, 'the reader misses or invents a file the built index names');
  assert.ok(served.some((ref) => ref.endsWith('.js')) && served.some((ref) => ref.endsWith('.css')), 'the built index names no script or no stylesheet');
  const built = await readdir(resolve(import.meta.dirname, '../dist/assets'));
  for (const ref of served) assert.ok(built.includes(ref.slice('/floor/assets/'.length)), `${ref} is not in dist/assets`);
});

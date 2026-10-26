// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test, { before, after } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { classesIn, themeTokens, utilityColour } from './helpers/theme-colour.mjs';
import { legacyStyles } from './helpers/legacy-css.mjs';

const lineColour = async (name, theme) => utilityColour(
  await classesIn('StreamNotice.tsx', new RegExp(`const ${name} = cn\\('([^']*)'\\)`)), theme);
const source = resolve(import.meta.dirname, '../src/renderer/OfficeCanvas.tsx');

/** The office box's own background — the surface these lines are kept off. */
const BOX = '#241f1a';
/**
 * The floor's readability bar, stated as a threshold and not a colour: a mark
 * reads at >= 4.5:1 against any surface it can sit on, measured rather than
 * chosen by eye. Every line on this page is held to it.
 */
const READABLE = 4.5;

function contrast(foreground, background) {
  const channel = (value) => {
    const c = value / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  const luminance = (hex) => {
    const n = Number.parseInt(hex.slice(1), 16);
    return 0.2126 * channel((n >> 16) & 255)
      + 0.7152 * channel((n >> 8) & 255)
      + 0.0722 * channel(n & 255);
  };
  const [lighter, darker] = [luminance(foreground), luminance(background)].sort((a, b) => b - a);
  return (lighter + 0.05) / (darker + 0.05);
}

let server;
let StreamNotice;
let store;
let stream;
before(async () => {
  const root = resolve(import.meta.dirname, '..');
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-disconnected-line'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ StreamNotice } = await server.ssrLoadModule('/src/renderer/StreamNotice.tsx'));
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  stream = await server.ssrLoadModule('/src/renderer/floor/stream.ts');
});
after(async () => { await server?.close(); });
function notice(phase, stale = false) {
  return renderToStaticMarkup(createElement(StreamNotice,
    { floor: { ...store.getFloorState(), phase, stale, epoch: 1, snapshotEpoch: 1 } }));
}

test('the mirror publishes the phase, so an empty floor says which empty it is', async () => {
  const text = await readFile(source, 'utf8');
  const list = /<ul\b[\s\S]*?>/.exec(text);
  assert.ok(list, 'the cast mirror is still a ul');
  assert.match(
    list[0],
    /data-phase=\{floor\.phase\}/,
    'the cast mirror does not publish the phase, so connecting, resyncing and given-up '
    + 'all render as the same empty list',
  );
});

test('a shared disconnected notice appears only when the stream is closed', () => {
  assert.match(notice('closed'), /office-closed stream-notice/);
  for (const phase of ['live', 'connecting', 'resyncing']) assert.doesNotMatch(notice(phase), /office-closed/);
});

test('a closed stale view shows the ended-session notice without a stale waiting notice', () => {
  const closed = notice('closed', true);
  assert.match(closed, /Dudes stopped\./);
  assert.doesNotMatch(closed, /office-stale|View unconfirmed/);
  assert.match(notice('live', true), /Nothing has come from Dudes/);
});

test('the reconnecting notice names the attempts actually spent rather than a static maximum', () => {
  store.resetFloorStore();
  const sockets = [];
  let scheduled;
  const dispose = stream.openFloorStream({ device: 'fixture' }, { protocol: 'http:', host: '127.0.0.1:7777' }, () => {
    const events = new Map();
    const socket = { addEventListener: (name, fn) => events.set(name, fn), send() {}, close() {},
      emit: (name) => events.get(name)?.() };
    sockets.push(socket);
    return socket;
  }, { now: () => 0, repeat: () => () => {}, after: (_delay, run) => { scheduled = run; return () => {}; } });
  sockets[0].emit('close');
  scheduled();
  sockets[1].emit('close');
  dispose();
  assert.match(notice('resyncing'), /attempt 2\)/);
  assert.doesNotMatch(notice('resyncing'), /attempt 5/);
});

test('the disconnected line reads against the page at the brief threshold', async () => {
  const css = await legacyStyles();
  for (const theme of ['light', 'dark']) {
    const colour = await lineColour('CLOSED', theme);
    const onPage = contrast(colour, themeTokens(css, theme).surface);
    assert.ok(onPage >= READABLE,
      `${colour} reads at ${onPage.toFixed(2)}:1 on the ${theme} page, under the ${READABLE}:1 bar`);
  }
  const colour = await lineColour('CLOSED', 'light');
  assert.ok(
    contrast(colour, BOX) < READABLE,
    `${colour} clears ${READABLE}:1 against the box too, so this line's placement is not `
    + 'what makes it readable',
  );
});

test('all three lines below the floor are on the same footing', async () => {
  const css = await legacyStyles();
  for (const theme of ['light', 'dark']) {
    const failure = await utilityColour(await classesIn('ReadError.tsx', /className=\{cn\(className, '([^']*)'\)\}/), theme);
    const read = [['office-failure', failure], ['office-stale', await lineColour('STALE', theme)], ['office-closed', await lineColour('CLOSED', theme)]]
      .map(([name, colour]) => [name, contrast(colour, themeTokens(css, theme).surface)]);
    const ratios = read.map(([, ratio]) => ratio);
    assert.ok(Math.max(...ratios) - Math.min(...ratios) < 1,
      `the ${theme} lines read at ` + read.map(([name, ratio]) => `${name} ${ratio.toFixed(2)}:1`).join(', ')
      + ', which is not the same footing');
  }
});

test('the closed notice says how to start Dudes again and offers one Reconnect', async () => {
  const closed = notice('closed');
  assert.match(closed, /Start it again with <span class="open-command"><code>dudes open<\/code>/);
  assert.doesNotMatch(closed, /dudes floor open/);
  assert.equal((closed.match(/<button/g) ?? []).length, 2, 'Copy for the command and Reconnect, and no other action');
  assert.match(closed, /<button type="button"[^>]*class="[^"]*notice-action[^"]*">Reconnect<\/button>/);
  assert.doesNotMatch(notice('live'), /office-closed/);
  const root = resolve(import.meta.dirname, '..');
  const saying = [];
  async function scan(directory) {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const path = resolve(directory, entry.name);
      if (entry.isDirectory()) await scan(path);
      else if (/\.(tsx?|css|html)$/.test(entry.name) && (await readFile(path, 'utf8')).includes('Reload to connect')) {
        saying.push(path);
      }
    }
  }
  await scan(resolve(root, 'src'));
  assert.deepEqual(saying, []);
});

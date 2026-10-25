// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createServer } from 'vite';
import { chromePath, ScratchRun } from '../../xtask/src/smoke.mjs';

const floor = resolve(import.meta.dirname, '..');
const root = await mkdtemp(join(tmpdir(), 'dudes-steps-browser-'));
const screenshots = process.argv[2] === undefined
  ? await mkdtemp(join(tmpdir(), 'dudes-steps-screenshots-')) : resolve(process.argv[2]);
const chrome = await chromePath(undefined, true);
const run = new ScratchRun(root, resolve(floor, '..'), '', '', chrome);
const server = await createServer({ root: floor, server: { host: '127.0.0.1', port: 0, hmr: false }, logLevel: 'silent' });
try {
  for (const directory of [run.home, run.repo, run.env.TMPDIR, screenshots]) await mkdir(directory, { recursive: true });
  await server.listen();
  const address = server.httpServer.address();
  await run.launchChrome();
  await run.cdp.send('Emulation.setDeviceMetricsOverride', { width: 1000, height: 1000, deviceScaleFactor: 1, mobile: false }, run.cdp.session);
  await run.cdp.send('Page.navigate', {
    url: `http://127.0.0.1:${address.port}/floor/tests/fixtures/steps-browser.html`,
  }, run.cdp.session);
  const ready = Date.now() + 5000;
  while (!(await run.cdp.evaluate('document.querySelectorAll(".step").length === 5'))) {
    assert.ok(Date.now() < ready, 'the Steps fixture rendered');
    await new Promise((done) => setTimeout(done, 25));
  }
  const unsupported = await run.cdp.evaluate(`new Promise((done) => {
    const rows = [...document.querySelectorAll('.step')];
    for (const row of rows.slice(0, 4)) row.querySelector('details').open = true;
    requestAnimationFrame(() => requestAnimationFrame(() => done({
      requests: [...window.stepFixture.requests],
      buttons: rows.slice(0, 4).reduce((count, row) => count + row.querySelectorAll('button').length, 0),
    })));
  })`);
  await run.cdp.evaluate(`new Promise((done) => {
    document.querySelectorAll('.step')[4].querySelector('details').open = true;
    requestAnimationFrame(() => requestAnimationFrame(done));
  })`);
  for (const theme of ['light', 'dark']) {
    await run.cdp.evaluate(`new Promise((done) => {
      document.querySelector('.shell').dataset.theme = ${JSON.stringify(theme)};
      window.scrollTo(0, 0);
      requestAnimationFrame(() => requestAnimationFrame(done));
    })`);
    const { data } = await run.cdp.send('Page.captureScreenshot', { format: 'png' }, run.cdp.session);
    await writeFile(join(screenshots, `${theme}.png`), Buffer.from(data, 'base64'));
  }
  assert.deepEqual(unsupported.requests, [], 'unsupported steps request no output when expanded');
  assert.equal(unsupported.buttons, 0, 'unsupported steps offer no output button');
  assert.deepEqual(await run.cdp.evaluate('window.stepFixture.requests.map((request) => request.step_id)'), ['command']);
  assert.equal(await run.cdp.evaluate('document.querySelector(".step-output")?.textContent'), 'Visible command output');
  console.log('PASS unsupported steps have no output control or fetch; a supported command shows its output');
  console.log(`Screenshots: ${screenshots}`);
} finally {
  await run.cdp?.evaluate('window.stepFixture?.stop()').catch(() => {});
  const failures = await run.closeChrome();
  await server.close();
  await rm(root, { recursive: true, force: true });
  assert.deepEqual(failures, [], 'the proof browser and helpers ended');
}

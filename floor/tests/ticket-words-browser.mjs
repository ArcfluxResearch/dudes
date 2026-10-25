// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createServer } from 'vite';
import { chromePath, ScratchRun } from '../../xtask/src/smoke.mjs';

const floor = resolve(import.meta.dirname, '..');
const scratch = await mkdtemp(join(tmpdir(), 'dudes-ticket-words-'));
const screenshots = process.argv[2] === undefined
  ? await mkdtemp(join(tmpdir(), 'dudes-ticket-words-screenshots-')) : resolve(process.argv[2]);
const chrome = await chromePath(undefined, true);
const run = new ScratchRun(scratch, resolve(floor, '..'), '', '', chrome);
const server = await createServer({ root: floor, server: { host: '127.0.0.1', port: 0, hmr: false }, logLevel: 'silent' });
const waitFor = async (expression, message) => {
  const deadline = Date.now() + 5000;
  while (!(await run.cdp.evaluate(expression))) {
    assert.ok(Date.now() < deadline, message);
    await new Promise((done) => setTimeout(done, 25));
  }
};
try {
  for (const directory of [run.home, run.repo, run.env.TMPDIR, screenshots]) await mkdir(directory, { recursive: true });
  await server.listen();
  await run.launchChrome();
  await run.cdp.send('Emulation.setDeviceMetricsOverride', { width: 1100, height: 1200, deviceScaleFactor: 1, mobile: false }, run.cdp.session);
  await run.cdp.send('Page.navigate', {
    url: `http://127.0.0.1:${server.httpServer.address().port}/floor/tests/fixtures/ticket-words-browser.html`,
  }, run.cdp.session);
  await waitFor('document.querySelector("[data-ticket-status=review]") !== null', 'the ready ticket rendered');
  const ready = await run.cdp.evaluate(`(() => {
    document.querySelector('.ticket-facts').open = true;
    return { warning: document.querySelector('[aria-label="What Dudes could not see"]') !== null,
      limits: document.querySelector('.ticket-facts').textContent,
      accept: !document.querySelector('[data-action=accept]').disabled };
  })()`);
  for (const state of ['ready', 'held']) {
    if (state === 'held') {
      await run.cdp.evaluate('document.querySelector("#held-ticket").click()');
      await waitFor('document.querySelector("[data-ticket-status=blocked]") !== null', 'the held ticket rendered');
      await run.cdp.evaluate('document.querySelector(".ticket-facts").open = true');
    }
    for (const theme of ['light', 'dark']) {
      await run.cdp.evaluate(`new Promise((done) => {
        document.querySelector('.shell').dataset.theme = ${JSON.stringify(theme)};
        window.scrollTo(0, 0);
        requestAnimationFrame(() => requestAnimationFrame(done));
      })`);
      const { data } = await run.cdp.send('Page.captureScreenshot', { format: 'png' }, run.cdp.session);
      await writeFile(join(screenshots, `${state}-${theme}.png`), Buffer.from(data, 'base64'));
    }
  }
  const held = await run.cdp.evaluate(`(() => {
    const review = document.querySelector('[aria-label="Instruction review"]');
    const accept = document.querySelector('[data-action=accept]');
    accept.focus();
    return { text: review.textContent, accept: !accept.disabled,
      focus: document.activeElement === accept, changes: !document.querySelector('[data-action=changes]').disabled };
  })()`);
  assert.equal(ready.warning, false, 'standing limits do not show a generic warning');
  assert.match(ready.limits, /Codex's settings in its home folder, fallback file names or root markers/);
  assert.equal(ready.accept, true, 'standing limits leave Accept available');
  assert.match(held.text, /\/project\/AGENTS\.md/);
  assert.match(held.text, /could not read|could not be read/);
  assert.match(held.text, /Ask for changes/);
  assert.deepEqual([held.accept, held.focus, held.changes], [true, true, true], 'review controls remain reachable');
  console.log('PASS standing limits stay in Details; observed findings and review controls remain available');
  console.log(`Screenshots: ${screenshots}`);
} finally {
  const failures = await run.closeChrome();
  await server.close();
  await rm(scratch, { recursive: true, force: true });
  assert.deepEqual(failures, [], 'the browser and helpers ended');
}

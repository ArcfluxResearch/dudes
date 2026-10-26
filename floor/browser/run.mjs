// The browser checks: builds the component lab, serves it under the Floor's strict page policy and drives it in a real
// headless Chrome with real key and mouse events. Run it with `npm run test:browser`. It is not part of `npm test`, which
// has no browser. Set FLOOR_BROWSER to the Chrome or Chromium executable when it is not in a usual place.

import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { build } from 'vite';

import { POLICY_SCRIPT, checks } from './checks.mjs';
import { launch } from './cdp.mjs';
import { pageOf } from './page.mjs';
import { serve } from './serve.mjs';

const floor = resolve(import.meta.dirname, '..');
const started = Date.now();
const out = await mkdtemp(join(tmpdir(), 'floor-lab-'));
let browser;
let server;
let failed = 0;

process.on('unhandledRejection', (error) => { console.error('unhandled rejection', error); process.exitCode = 1; });

try {
  await build({ configFile: resolve(floor, 'vite.lab.config.mjs'), logLevel: 'silent', build: { outDir: out } });
  server = await serve(out);
  browser = await launch();
  const cdp = browser.page;
  const log = { errors: [] };
  cdp.on((message) => {
    if (message.method === 'Runtime.exceptionThrown') log.errors.push(message.params.exceptionDetails.exception?.description ?? message.params.exceptionDetails.text);
    if (message.method === 'Runtime.consoleAPICalled' && message.params.type === 'error') log.errors.push(message.params.args.map((arg) => arg.value ?? arg.description).join(' '));
    if (message.method === 'Log.entryAdded' && message.params.entry.level === 'error') log.errors.push(`${message.params.entry.text} ${message.params.entry.url ?? ''}`);
  });
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  await cdp.send('Log.enable');
  await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
  await cdp.send('Page.addScriptToEvaluateOnNewDocument', { source: POLICY_SCRIPT });
  const navigation = await cdp.send('Page.navigate', { url: server.url });
  if (navigation.errorText) throw new Error(`the lab did not load: ${navigation.errorText}`);
  const page = pageOf(cdp);
  try {
    await page.waitFor(`document.getElementById('lab') && document.fonts.status === 'loaded'`, 'the lab to load', 15_000);
  } catch (error) {
    const state = await page.js(`JSON.stringify({ href: location.href, ready: document.readyState, lab: Boolean(document.getElementById('lab')), fonts: document.fonts.status })`).catch(() => 'unreadable');
    throw new Error(`${error.message}; the page is ${state} and said: ${log.errors.join(' | ') || 'nothing'}`);
  }
  await page.settle();
  await cdp.send('Emulation.setFocusEmulationEnabled', { enabled: true });

  const list = checks({ page, cdp, log });
  for (const { name, run } of list) {
    const at = Date.now();
    try {
      await run();
      console.log(`ok   ${name} (${Date.now() - at} ms)`);
    } catch (error) {
      failed += 1;
      console.log(`FAIL ${name}\n     ${String(error.message ?? error).split('\n').join('\n     ')}`);
    }
  }
  console.log(`${list.length - failed} of ${list.length} browser checks passed in ${((Date.now() - started) / 1000).toFixed(1)} s`);
} catch (error) {
  failed += 1;
  console.error(error);
} finally {
  await browser?.close();
  await server?.close();
  await rm(out, { recursive: true, force: true });
}
process.exit(failed === 0 ? 0 : 1);

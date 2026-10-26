// A small Chrome DevTools Protocol client: starts a headless browser, opens one page and sends it commands.

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const CANDIDATES = [
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/Applications/Chromium.app/Contents/MacOS/Chromium',
  '/usr/bin/google-chrome',
  '/usr/bin/chromium',
  '/usr/bin/chromium-browser',
];

/** The browser to run: `FLOOR_BROWSER` when set (any executable that takes Chrome's arguments), else the first one installed. */
export function browserPath() {
  const chosen = process.env.FLOOR_BROWSER ?? CANDIDATES.find((path) => existsSync(path));
  if (chosen === undefined) throw new Error('no Chrome or Chromium found: set FLOOR_BROWSER to its executable');
  return chosen;
}

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

async function readPort(profile, waitMs) {
  const file = join(profile, 'DevToolsActivePort');
  for (let waited = 0; waited < waitMs; waited += 100) {
    try {
      const [port, path] = (await readFile(file, 'utf8')).split('\n');
      if (port && path) return { port: Number(port), path };
    } catch { /* not written yet */ }
    await sleep(100);
  }
  throw new Error(`the browser did not open a debugging port in ${waitMs} ms`);
}

export async function launch({ width = 1280, height = 900 } = {}) {
  const profile = await mkdtemp(join(tmpdir(), 'floor-browser-'));
  const child = spawn(browserPath(), [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--no-first-run', '--no-default-browser-check',
    '--use-mock-keychain', '--password-store=basic', '--disable-extensions', '--disable-background-networking',
    `--window-size=${width},${height}`, '--force-device-scale-factor=1', 'about:blank',
  ], { stdio: 'ignore' });
  const exited = new Promise((done) => child.once('exit', done));
  const { port, path } = await readPort(profile, Number(process.env.FLOOR_BROWSER_WAIT_MS ?? 60_000));
  const socket = new WebSocket(`ws://127.0.0.1:${port}${path}`);
  await new Promise((done, fail) => { socket.addEventListener('open', done, { once: true }); socket.addEventListener('error', fail, { once: true }); });

  let next = 0;
  const pending = new Map();
  const listeners = new Set();
  socket.addEventListener('message', ({ data }) => {
    const message = JSON.parse(String(data));
    if (message.id !== undefined) {
      const wait = pending.get(message.id);
      pending.delete(message.id);
      if (message.error) wait?.fail(new Error(`${wait.method}: ${message.error.message}`));
      else wait?.done(message.result);
    } else for (const listener of listeners) listener(message);
  });

  function send(method, params = {}, sessionId = undefined) {
    next += 1;
    const id = next;
    return new Promise((done, fail) => {
      const timer = setTimeout(() => { pending.delete(id); fail(new Error(`${method} took more than 120 s`)); }, 120_000);
      timer.unref();
      pending.set(id, { method, done: (value) => { clearTimeout(timer); done(value); }, fail: (error) => { clearTimeout(timer); fail(error); } });
      socket.send(JSON.stringify({ id, method, params, sessionId }));
    });
  }

  async function close() {
    // Closing the browser ends the socket without an answer, so the command is sent and not awaited.
    send('Browser.close').catch(() => {});
    await Promise.race([exited, sleep(10_000)]);
    if (child.exitCode === null) child.kill('SIGKILL');
    await rm(profile, { recursive: true, force: true });
  }

  const { targetId } = await send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await send('Target.attachToTarget', { targetId, flatten: true });
  const page = {
    send: (method, params) => send(method, params, sessionId),
    on: (listener) => { const wrapped = (message) => { if (message.sessionId === sessionId) listener(message); }; listeners.add(wrapped); return () => listeners.delete(wrapped); },
  };
  return { page, close };
}

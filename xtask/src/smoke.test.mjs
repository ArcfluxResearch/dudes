import assert from 'node:assert/strict';
import childProcess, { execFile, spawn } from 'node:child_process';
import { EventEmitter, once } from 'node:events';
import { chmod, copyFile, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { syncBuiltinESMExports } from 'node:module';
import { tmpdir } from 'node:os';
import { delimiter, dirname, join } from 'node:path';
import { PassThrough } from 'node:stream';
import { test } from 'node:test';
import { promisify } from 'node:util';
import { chromePath, DevTools, navigateTimeout, ScratchRun } from './smoke.mjs';

const pause = (ms) => new Promise((done) => setTimeout(done, ms));
const execute = promisify(execFile);

async function eventually(action) {
  const end = Date.now() + 5000;
  while (Date.now() < end) {
    if (await action()) return;
    await pause(20);
  }
  assert.fail('process did not reach the expected state');
}

async function stopChild(child) {
  const exited = child.exitCode !== null || child.signalCode !== null;
  const exit = exited ? Promise.resolve() : once(child, 'exit');
  if (!exited) {
    try { process.kill(-child.pid, 'SIGKILL'); } catch {}
    child.kill('SIGKILL');
  }
  await exit;
}

test('daemon identity resolves the executable path on Linux', { skip: process.platform !== 'linux' }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  let child;
  try {
    const run = new ScratchRun(root, process.cwd(), '', '', null);
    await mkdir(run.repo);
    await mkdir(join(run.prefix, 'bin'), { recursive: true });
    const program = join(run.prefix, 'bin/duded');
    await copyFile('/bin/sleep', program);
    await chmod(program, 0o700);
    child = spawn(program, ['120'], { detached: true, stdio: 'ignore' });
    run.daemonPids = async () => [child.pid];
    await run.rememberManaged();
    assert.equal(run.owned.get(child.pid)?.program, await realpath(program));
    assert.equal(await run.ownedAlive(run.owned.get(child.pid)), true);
  } finally {
    if (child) await stopChild(child);
    await rm(root, { recursive: true, force: true });
  }
});

test('stand-in ownership waits for exec and cleanup kills the sleep', async () => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  let child;
  let release;
  try {
    const run = new ScratchRun(root, process.cwd(), '', '', null);
    await mkdir(run.repo);
    const ready = join(root, 'ready');
    const script = join(root, 'engine');
    await writeFile(script, `#!/bin/sh\nprintf '%s\\n' "$$" > '${join(root, 'engine-pids')}'\nwhile [ ! -f '${ready}' ]; do /bin/sleep 0.02; done\nexec /bin/sleep 120\n`, { mode: 0o700 });
    child = spawn(script, [], { detached: true, stdio: 'ignore' });
    run.daemonPids = async () => [];
    run.owned.set(process.pid, { ...await run.processInfo(process.pid), kind: 'daemon' });
    await eventually(async () => (await run.enginePids()).includes(child.pid));
    release = setTimeout(() => { void writeFile(ready, 'exec'); }, 200);
    await run.rememberManaged();
    const identity = run.owned.get(child.pid);
    assert.equal(identity.program, process.platform === 'linux' ? await realpath('/bin/sleep') : '/bin/sleep');
    assert.equal(await run.ownedAlive(identity), true);
    await run.killOwned(identity);
    await eventually(() => child.exitCode !== null || child.signalCode !== null);
    assert.equal(run.alive(child.pid), false);
  } finally {
    clearTimeout(release);
    if (child) await stopChild(child);
    await rm(root, { recursive: true, force: true });
  }
});

test('Chrome helper readlink denial preserves group cleanup and other identity failures', async (t) => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  let child;
  try {
    const run = new ScratchRun(root, process.cwd(), '', '', null);
    await mkdir(run.repo);
    child = spawn(process.execPath, ['-e', `
      const { spawn } = require('node:child_process');
      const helper = spawn('/bin/sleep', ['120'], { stdio: 'ignore' });
      process.stdout.write(String(helper.pid) + '\\n');
      setInterval(() => {}, 1000);
    `], { detached: true, stdio: ['ignore', 'pipe', 'inherit'] });
    const [bytes] = await once(child.stdout, 'data');
    const helper = Number(bytes.toString().trim());
    run.chrome = child;
    run.chromeChild = child;
    run.chromeIdentity = await run.processInfo(child.pid);
    const processInfo = run.processInfo.bind(run);
    let code = 'EACCES';
    t.mock.method(run, 'processInfo', async (pid) => {
      if (pid === helper) throw Object.assign(new Error('helper executable denied'), {
        code, syscall: 'readlink', path: `/proc/${pid}/exe`,
      });
      return processInfo(pid);
    });
    await run.rememberChromeChildren();
    assert.equal(run.chromeChildren.get(child.pid)?.program, run.chromeIdentity.program);
    assert.equal(run.chromeChildren.has(helper), false);
    await assert.rejects(run.processInfo(helper), { code: 'EACCES' });
    code = 'EIO';
    await assert.rejects(run.rememberChromeChildren(), { code: 'EIO' });
    await run.stopGroup(child.pid);
    await eventually(() => child.exitCode !== null || child.signalCode !== null);
    assert.equal(run.groupAlive(child.pid), false);
  } finally {
    if (child) await stopChild(child);
    await rm(root, { recursive: true, force: true });
  }
});

test('strict release check refuses missing Chrome', async () => {
  const previous = process.env.DUDES_SMOKE_REQUIRE_CHROME;
  try {
    process.env.DUDES_SMOKE_REQUIRE_CHROME = '1';
    await assert.rejects(chromePath([]), /Chrome is required/);
    process.env.DUDES_SMOKE_REQUIRE_CHROME = '0';
    assert.equal(await chromePath([]), null);
  } finally {
    if (previous === undefined) delete process.env.DUDES_SMOKE_REQUIRE_CHROME;
    else process.env.DUDES_SMOKE_REQUIRE_CHROME = previous;
  }
});

test('null DevTools reply closes the pipes and rejects pending commands', async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  const cdp = new DevTools(input, output);
  const rejected = assert.rejects(cdp.send('Browser.getVersion'), /Chrome connection closed/);
  try {
    input.write('null\0');
    await rejected;
    assert.equal(input.destroyed, true);
    assert.equal(output.destroyed, true);
  } finally {
    cdp.close();
    await rejected;
  }
});

for (const pipe of ['input', 'output']) {
  test(`closed DevTools ${pipe} rejects pending commands without spending their deadline`, async (t) => {
    const input = new PassThrough();
    const output = new PassThrough();
    const cdp = new DevTools(input, output);
    t.mock.timers.enable({ apis: ['setTimeout'] });
    let rejected = false;
    const pending = cdp.send('Browser.getVersion').catch((error) => {
      rejected = true;
      return error;
    });
    try {
      const closed = once(cdp[pipe], 'close');
      cdp[pipe].destroy();
      await closed;
      const beforeDeadline = rejected;
      t.mock.timers.runAll();
      assert.equal((await pending).message, 'Chrome connection closed');
      assert.equal(beforeDeadline, true);
      assert.equal(cdp.pending.size, 0);
      assert.throws(() => cdp.send('Browser.getVersion'), /Chrome connection closed/);
    } finally {
      t.mock.timers.reset();
      cdp.close();
      await pending;
    }
  });
}

test('an open DevTools pipe keeps the command deadline and a completed reply', async (t) => {
  const cdp = new DevTools(new PassThrough(), new PassThrough());
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const pending = cdp.send('Browser.getVersion').catch((error) => error);
  try {
    t.mock.timers.tick(29999);
    assert.equal(cdp.pending.size, 1);
    assert.equal(cdp.closed, false);
    t.mock.timers.tick(1);
    assert.equal((await pending).message, 'Chrome Browser.getVersion timed out');
    assert.equal(cdp.closed, false);
    const answered = cdp.send('Browser.getVersion');
    cdp.input.write(JSON.stringify({ id: 2, result: { product: 'pipe control' } }) + '\0');
    cdp.close();
    assert.deepEqual(await answered, { product: 'pipe control' });
  } finally {
    t.mock.timers.reset();
    cdp.close();
    await pending;
  }
});

test('Chrome spawn failure has an error handler before stdio is available', async (t) => {
  const child = new EventEmitter();
  t.mock.method(childProcess, 'spawn', () => child);
  syncBuiltinESMExports();
  try {
    const run = new ScratchRun('', process.cwd(), '', '', 'chrome-stand-in');
    await assert.rejects(run.browser(), TypeError);
    assert.doesNotThrow(() => child.emit('error', new Error('spawn EMFILE')));
  } finally {
    t.mock.restoreAll();
    syncBuiltinESMExports();
  }
});

test('Chrome exit rejects the handshake while a helper retains the reply pipe', async (t) => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  const program = join(root, 'chrome-stand-in');
  const witness = join(root, 'request.json');
  const run = new ScratchRun(root, process.cwd(), '', '', program);
  let helper;
  let handshake;
  try {
    for (const path of [run.repo, run.home, run.env.TMPDIR]) await mkdir(path);
    await writeFile(program, `#!/usr/bin/env node
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const input = fs.createReadStream(null, { fd: 3 });
let buffer = '';
input.on('data', (chunk) => {
  buffer += chunk;
  if (!buffer.includes('\\0')) return;
  input.pause();
  const request = JSON.parse(buffer.slice(0, buffer.indexOf('\\0')));
  const helper = spawn(process.execPath, ['-e',
    'setTimeout(() => {}, 10000);'],
    { stdio: ['ignore', 'ignore', 'ignore', 'ignore', 4] });
  fs.writeFileSync(${JSON.stringify(witness)}, JSON.stringify({ request, helper: helper.pid }));
  process.exit(0);
});
`, { mode: 0o700 });
    run.env.PATH = `${dirname(process.execPath)}${delimiter}${run.env.PATH}`;
    t.mock.timers.enable({ apis: ['setTimeout'] });
    let rejected = false;
    handshake = run.browser().then(() => null, (error) => {
      rejected = true;
      return error;
    });
    await once(run.chrome, 'exit', { signal: AbortSignal.timeout(5000) });
    await new Promise((done) => setImmediate(done));
    const beforeDeadline = rejected;
    const observed = JSON.parse(await readFile(witness, 'utf8'));
    helper = await run.processInfo(observed.helper);
    assert.deepEqual(observed.request, { id: 1, method: 'Browser.getVersion', params: {} });
    assert.equal(run.chrome.exitCode, 0);
    assert.equal(helper?.group, run.chrome.pid);
    assert.equal(run.cdp.input.readableEnded, false);
    t.mock.timers.runAll();
    const error = await handshake;
    t.diagnostic(`child exit with retained reply pipe: ${error?.message}; rejected before deadline: ${beforeDeadline}`);
    assert.equal(error?.message, 'Chrome connection closed');
    assert.equal(beforeDeadline, true);
  } finally {
    t.mock.timers.reset();
    run.cdp?.close();
    if (!helper) {
      const observed = await readFile(witness, 'utf8').then(JSON.parse, () => null);
      if (observed) helper = await run.processInfo(observed.helper);
    }
    if (helper) await run.killOwned(helper);
    if (run.chrome) {
      await stopChild(run.chrome);
      await eventually(() => !run.groupAlive(run.chrome.pid));
    }
    await handshake;
    await rm(root, { recursive: true, force: true });
  }
});

test('soft stop can await closing while ping retains its normal command budget', async () => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  const run = new ScratchRun(root, process.cwd(), '', '', null);
  const calls = [];
  try {
    run.capture = async (program, args, options) => {
      calls.push({ program, args, options });
      if (args[0] === 'stop') {
        const receiptAt = 360000 + 5000;
        assert.ok(options.timeout >= receiptAt, 'a closing receipt after the saving bound remains reachable');
        assert.ok(options.timeout <= 390000, 'the addressed stop retains its finite bound');
        return { stdout: 'the daemon stopped' };
      }
      throw new Error('the stopped daemon has no listener');
    };
    await run.stop();
    assert.deepEqual(calls.map(({ args }) => args), [['stop'], ['ping']]);
    assert.equal(calls[0].program, join(run.prefix, 'bin/dudes'));
    assert.equal(calls[0].options.cwd, run.repo);
    assert.equal(calls[0].options.env, run.env);
    assert.equal(calls[1].options.timeout, 30000);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('cleanup requests hard stop without waiting through a saving interval', async () => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  const run = new ScratchRun(root, process.cwd(), '', '', null);
  const calls = [];
  try {
    run.rememberManaged = async () => {};
    run.cli = async (args) => {
      calls.push(args);
      if (args.includes('--hard')) return { stdout: 'the daemon stopped' };
      throw new Error('soft stop leaves a pending saving interval');
    };
    await run.cleanup();
    assert.deepEqual(calls, [['stop', '--hard']]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('release build keeps the selected Rustup compiler in a dependency directory', async (t) => {
  t.mock.method(console, 'log', () => {});
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  const previous = process.env.RUSTUP_TOOLCHAIN;
  let compiler;
  try {
    const dependency = join(root, 'dependency');
    await mkdir(dependency);
    await mkdir(join(root, 'tmp'));
    await writeFile(join(dependency, 'rust-toolchain.toml'), '[toolchain]\nchannel = "smoke-missing-toolchain"\n');
    const toolchain = (await readFile(new URL('../../rust-toolchain.toml', import.meta.url), 'utf8'))
      .match(/^channel\s*=\s*"([^"]+)"/m)?.[1];
    assert.ok(toolchain, 'rust-toolchain.toml has a channel');
    process.env.RUSTUP_TOOLCHAIN = toolchain;
    const run = new ScratchRun(root, process.cwd(), '', '', null);
    run.capture = async (_program, _args, options) => {
      ({ stdout: compiler } = await execute('rustc', ['--version'], { ...options, cwd: dependency }));
      throw new Error('compiler probe completed');
    };
    await assert.rejects(run.build(), /compiler probe completed/);
    const { stdout: selected } = await execute('rustc', ['--version']);
    assert.equal(compiler, selected);
  } finally {
    if (previous === undefined) delete process.env.RUSTUP_TOOLCHAIN;
    else process.env.RUSTUP_TOOLCHAIN = previous;
    await rm(root, { recursive: true, force: true });
  }
});

// Worst case stacks two navigateTimeout waits (the navigate ack, then the frame-commit poll that
// follows it) plus the lighter default-timeout waits after, so this outer deadline needs enough
// room that a real hang fails inside the harness with a specific stage name, never via this cutoff.
test('Chrome drives the page over pipes without a DevTools TCP endpoint', { timeout: navigateTimeout * 2 + 60_000 }, async (t) => {
  const chrome = await chromePath();
  if (!chrome) return t.skip('Chrome absent');
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  const server = createServer((_request, response) => {
    response.end('<!doctype html><title>Smoke pipe</title><button id="hire-open-office">Hire</button>');
  });
  const run = new ScratchRun(root, process.cwd(), '', '', chrome);
  try {
    await mkdir(run.repo);
    await mkdir(run.home);
    await mkdir(run.env.TMPDIR);
    server.listen(0, '127.0.0.1');
    await once(server, 'listening');
    run.cli = async () => ({ stdout: `http://127.0.0.1:${server.address().port}/floor/#bootstrap=smoke` });
    await run.browser();
    assert.equal(await run.cdp.evaluate('document.title'), 'Smoke pipe');
    await assert.rejects(readFile(join(root, 'chrome/DevToolsActivePort')), { code: 'ENOENT' });
    assert.equal(run.chrome.spawnargs.includes('--remote-debugging-pipe'), true);
    assert.equal(run.chrome.spawnargs.some((arg) => arg.startsWith('--remote-debugging-port')), false);
  } finally {
    run.server = server;
    try {
      await run.cleanup();
      if (run.chrome?.pid) assert.equal(run.groupAlive(run.chrome.pid), false);
    } finally {
      if (run.chrome) await stopChild(run.chrome);
      await rm(root, { recursive: true, force: true });
    }
  }
});

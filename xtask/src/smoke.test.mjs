import assert from 'node:assert/strict';
import childProcess, { execFile, spawn } from 'node:child_process';
import { EventEmitter, once } from 'node:events';
import { chmod, copyFile, mkdir, mkdtemp, readFile, realpath, rm, symlink, writeFile } from 'node:fs/promises';
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
  const previousChrome = process.env.DUDES_SMOKE_CHROME;
  try {
    delete process.env.DUDES_SMOKE_CHROME;
    process.env.DUDES_SMOKE_REQUIRE_CHROME = '1';
    await assert.rejects(chromePath([]), /Chrome is required/);
    process.env.DUDES_SMOKE_REQUIRE_CHROME = '0';
    assert.equal(await chromePath([]), null);
  } finally {
    if (previous === undefined) delete process.env.DUDES_SMOKE_REQUIRE_CHROME;
    else process.env.DUDES_SMOKE_REQUIRE_CHROME = previous;
    if (previousChrome === undefined) delete process.env.DUDES_SMOKE_CHROME;
    else process.env.DUDES_SMOKE_CHROME = previousChrome;
  }
});

test('configured Chrome selects one executable path before discovery', async () => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  const previous = process.env.DUDES_SMOKE_CHROME;
  try {
    const configured = join(root, 'selected Chrome');
    const discovered = join(root, 'discovered Chrome');
    for (const [program, name] of [[configured, 'configured'], [discovered, 'discovered']]) {
      await writeFile(program, `#!${process.execPath}\nprocess.stdout.write(JSON.stringify({ name: ${JSON.stringify(name)}, args: process.argv.slice(2) }));\n`, { mode: 0o700 });
    }
    process.env.DUDES_SMOKE_CHROME = configured;
    assert.equal(await chromePath([discovered], false), configured);
    const selected = await chromePath([discovered], true);
    const { stdout } = await execute(selected, ['one argument', '--probe']);
    assert.deepEqual(JSON.parse(stdout), { name: 'configured', args: ['one argument', '--probe'] });
    const link = join(root, 'Chrome link');
    await symlink(configured, link);
    process.env.DUDES_SMOKE_CHROME = link;
    assert.equal(await chromePath([discovered], false), link);
    const linked = await execute(await chromePath([discovered], true), ['symlink argument']);
    assert.deepEqual(JSON.parse(linked.stdout), { name: 'configured', args: ['symlink argument'] });
    delete process.env.DUDES_SMOKE_CHROME;
    assert.equal(await chromePath([discovered], false), discovered);
    const fallback = await execute(await chromePath([discovered], true), ['discovery argument']);
    assert.deepEqual(JSON.parse(fallback.stdout), { name: 'discovered', args: ['discovery argument'] });
    assert.equal(await chromePath([], false), null);
    await assert.rejects(chromePath([], true), /Chrome is required/);
  } finally {
    if (previous === undefined) delete process.env.DUDES_SMOKE_CHROME;
    else process.env.DUDES_SMOKE_CHROME = previous;
    await rm(root, { recursive: true, force: true });
  }
});

test('configured Chrome refuses invalid paths without discovery fallback', async (t) => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  const previous = process.env.DUDES_SMOKE_CHROME;
  try {
    const discovered = join(root, 'discovered Chrome');
    const nonexecutable = join(root, 'nonexecutable');
    const directory = join(root, 'directory');
    const directoryLink = join(root, 'directory link');
    const dangling = join(root, 'dangling');
    await writeFile(discovered, `#!${process.execPath}\nprocess.stdout.write('discovered');\n`, { mode: 0o700 });
    await writeFile(nonexecutable, 'not executable', { mode: 0o600 });
    await mkdir(directory);
    await symlink(directory, directoryLink);
    await symlink(join(root, 'missing'), dangling);
    for (const [name, configured] of [
      ['empty', ''], ['relative', './chrome'], ['missing', join(root, 'missing')],
      ['directory', directory], ['nonexecutable', nonexecutable],
      ['directory symlink', directoryLink], ['dangling symlink', dangling],
    ]) {
      await t.test(name, async () => {
        process.env.DUDES_SMOKE_CHROME = configured;
        for (const required of [false, true]) {
          await assert.rejects(chromePath([discovered], required), /DUDES_SMOKE_CHROME.*absolute executable file/);
        }
      });
    }
    delete process.env.DUDES_SMOKE_CHROME;
    const { stdout } = await execute(await chromePath([discovered], false), []);
    assert.equal(stdout, 'discovered');
  } finally {
    if (previous === undefined) delete process.env.DUDES_SMOKE_CHROME;
    else process.env.DUDES_SMOKE_CHROME = previous;
    await rm(root, { recursive: true, force: true });
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

test('release build passes standard Cargo settings through isolated child environments', async (t) => {
  t.mock.method(console, 'log', () => {});
  const keys = ['HOME', 'PATH', 'CARGO', 'CARGO_HOME', 'RUSTUP_HOME', 'RUSTUP_TOOLCHAIN',
    'CARGO_TARGET_DIR', 'MACOSX_DEPLOYMENT_TARGET', 'DUDES_SMOKE_ORIGINAL_PATH',
    'CARGO_BUILD_JOBS', 'CARGO_INCREMENTAL', 'DUDES_SMOKE_TEST_UNRELATED'];
  const previous = Object.fromEntries(keys.map((key) => [key, process.env[key]]));
  try {
    for (const [name, settings] of [
      ['defined', { CARGO_BUILD_JOBS: '3', CARGO_INCREMENTAL: '0' }],
      ['absent', {}],
      ['empty', { CARGO_BUILD_JOBS: '', CARGO_INCREMENTAL: '' }],
    ]) {
      await t.test(name, async () => {
        const root = await realpath(await mkdtemp(join(tmpdir(), 'dudes-smoke-test-')));
        try {
          const workspace = join(root, 'workspace');
          const bin = join(root, 'tools');
          const originalPath = join(root, 'original tools');
          const xtask = join(bin, 'dist recorder');
          const cargo = join(bin, 'cargo recorder');
          const target = join(root, 'target');
          const run = new ScratchRun(root, workspace, xtask, '', null);
          for (const path of [bin, originalPath, run.home, run.env.TMPDIR, join(workspace, 'floor')]) {
            await mkdir(path, { recursive: true });
          }
          for (const [program, stage] of [[join(bin, 'npm'), 'floor'], [xtask, 'dist'], [cargo, 'metadata']]) {
            await writeFile(program, `#!${process.execPath}
const fs = require('node:fs');
fs.writeFileSync(${JSON.stringify(join(root, `${stage}.json`))}, JSON.stringify({ args: process.argv.slice(2), cwd: process.cwd(), env: process.env }));
if (${JSON.stringify(stage)} === 'metadata') process.stdout.write(JSON.stringify({ target_directory: ${JSON.stringify(target)} }));
`, { mode: 0o700 });
          }
          for (const key of keys) delete process.env[key];
          Object.assign(process.env, {
            HOME: join(root, 'parent home'), PATH: bin, CARGO: cargo,
            CARGO_HOME: join(root, 'parent cargo'), RUSTUP_HOME: join(root, 'rustup'),
            RUSTUP_TOOLCHAIN: 'selected-toolchain', CARGO_TARGET_DIR: target,
            MACOSX_DEPLOYMENT_TARGET: '12.0', DUDES_SMOKE_ORIGINAL_PATH: originalPath,
            DUDES_SMOKE_TEST_UNRELATED: 'must stay outside the child', ...settings,
          });
          await run.build();
          const expected = {
            HOME: run.home, TMPDIR: run.env.TMPDIR, PATH: originalPath, CARGO: cargo,
            CARGO_HOME: join(root, 'cargo'), RUSTUP_HOME: join(root, 'rustup'),
            CARGO_NET_OFFLINE: 'true', NPM_CONFIG_OFFLINE: 'true', NPM_CONFIG_UPDATE_NOTIFIER: 'false',
            RUSTUP_TOOLCHAIN: 'selected-toolchain', DUDES_SMOKE_ORIGINAL_PATH: originalPath,
            CARGO_TARGET_DIR: target, MACOSX_DEPLOYMENT_TARGET: '12.0', ...settings,
          };
          for (const [stage, args, cwd] of [
            ['floor', ['run', 'build'], join(workspace, 'floor')],
            ['dist', ['dist'], workspace],
            ['metadata', ['metadata', '--locked', '--format-version=1', '--no-deps'], workspace],
          ]) {
            const observed = JSON.parse(await readFile(join(root, `${stage}.json`), 'utf8'));
            if (process.platform === 'darwin') delete observed.env.__CF_USER_TEXT_ENCODING;
            assert.deepEqual(observed, { args, cwd, env: { ...expected, PATH: stage === 'floor' ? bin : originalPath } });
          }
          assert.equal(run.dist, join(target, 'dist'));
        } finally {
          await rm(root, { recursive: true, force: true });
        }
      });
    }
  } finally {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
});

test('generic child capture refuses a recycled group after its child exits', async (t) => {
  const run = new ScratchRun('', process.cwd(), '', '', null);
  const signals = [];
  let observations = 0;
  t.mock.method(run, 'groupAbsent', () => ++observations !== 1);
  t.mock.method(run, 'signalGroup', (pid, signal) => signals.push({ pid, signal }));
  const error = await run.capture(process.execPath, ['-e', 'process.stdout.write("owned recorder")'], {
    cwd: process.cwd(), env: {}, timeout: 5000,
  }).then(() => null, (failure) => failure);
  assert.deepEqual(signals, []);
  assert.match(error?.message ?? '', /child group ownership is unproven/);
});

test('generic child cleanup refuses lost or replaced anchors', async (t) => {
  for (const [name, status] of [
    ['exited', { exitCode: 0 }], ['signaled', { signalCode: 'SIGTERM' }],
    ['unregistered', {}], ['replaced', {}],
  ]) {
    await t.test(name, async (subtest) => {
      const child = { pid: 800001, exitCode: null, signalCode: null, ...status };
      const run = new ScratchRun('', process.cwd(), '', '', null);
      run.groups = new Map(name === 'unregistered' ? [] : [[child.pid,
        name === 'replaced' ? { ...child } : child]]);
      const signals = [];
      let observations = 0;
      subtest.mock.method(run, 'groupAbsent', () => ++observations !== 1);
      subtest.mock.method(run, 'signalGroup', (pid, signal) => signals.push({ pid, signal }));
      subtest.mock.method(run, 'processInfo', () => assert.fail('generic cleanup must not recurse through capture'));
      const error = await run.stopGroup(child.pid, child).then(() => null, (failure) => failure);
      assert.deepEqual(signals, []);
      assert.match(error?.message ?? '', /child group ownership is unproven/);
      assert.equal(run.groups.has(child.pid), name !== 'unregistered');
    });
  }
});

test('generic child cleanup rechecks its anchor before escalation', async (t) => {
  for (const name of ['exited', 'replaced', 'live']) {
    await t.test(name, async (subtest) => {
      subtest.mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 0 });
      const child = { pid: 800002, exitCode: null, signalCode: null };
      const run = new ScratchRun('', process.cwd(), '', '', null);
      run.groups = new Map([[child.pid, child]]);
      const signals = [];
      const waiting = Promise.withResolvers();
      let alive = true;
      subtest.mock.method(run, 'groupAbsent', () => {
        if (signals.length) waiting.resolve();
        return !alive;
      });
      subtest.mock.method(run, 'signalGroup', (_pid, signal) => {
        signals.push(signal);
        if (signal === 'SIGKILL') alive = false;
      });
      const pending = run.stopGroup(child.pid, child).then(() => null, (failure) => failure);
      await waiting.promise;
      assert.deepEqual(signals, ['SIGTERM']);
      if (name === 'exited') child.exitCode = 0;
      if (name === 'replaced') run.groups.set(child.pid, { ...child });
      subtest.mock.timers.tick(5000);
      const error = await pending;
      assert.deepEqual(signals, name === 'live' ? ['SIGTERM', 'SIGKILL'] : ['SIGTERM']);
      if (name === 'live') {
        assert.equal(error, null);
        assert.equal(run.groups.has(child.pid), false);
      } else {
        assert.match(error?.message ?? '', /child group ownership is unproven/);
        assert.equal(run.groups.has(child.pid), true);
      }
    });
  }
});

test('generic child interruption signals only the registered live child', (t) => {
  const run = new ScratchRun('', process.cwd(), '', '', null);
  const live = { pid: 800003, exitCode: null, signalCode: null };
  const exited = { pid: 800004, exitCode: 0, signalCode: null };
  const signaled = { pid: 800005, exitCode: null, signalCode: 'SIGTERM' };
  const replaced = { pid: 800006, exitCode: null, signalCode: null };
  run.groups = new Map([live, exited, signaled, replaced].map((child) => [child.pid, child]));
  const signals = [];
  t.mock.method(run, 'signalGroup', (pid, signal) => signals.push({ pid, signal }));
  assert.equal(run.signalChildGroup({ ...replaced }, 'SIGKILL'), false);
  run.signalChildren('SIGTERM');
  assert.deepEqual(signals, [
    { pid: live.pid, signal: 'SIGTERM' }, { pid: replaced.pid, signal: 'SIGTERM' },
  ]);
});

test('generic child group observation accepts only ESRCH as absence', async (t) => {
  for (const code of ['ESRCH', 'EPERM', 'EIO']) {
    await t.test(code, async (subtest) => {
      const child = { pid: 800007, exitCode: 0, signalCode: null };
      const run = new ScratchRun('', process.cwd(), '', '', null);
      run.groups = new Map([[child.pid, child]]);
      const signals = [];
      subtest.mock.method(process, 'kill', (pid, signal) => {
        assert.equal(pid, -child.pid);
        assert.equal(signal, 0);
        throw Object.assign(new Error(`group observation ${code}`), { code });
      });
      subtest.mock.method(run, 'signalGroup', (pid, signal) => signals.push({ pid, signal }));
      const error = await run.stopGroup(child.pid, child).then(() => null, (failure) => failure);
      assert.deepEqual(signals, []);
      if (code === 'ESRCH') {
        assert.equal(error, null);
        assert.equal(run.groups.has(child.pid), false);
      } else {
        assert.equal(error?.code, code);
        assert.equal(run.groups.has(child.pid), true);
      }
    });
  }
});

test('generic child observation failure during shutdown refuses escalation', async (t) => {
  const child = { pid: 800010, exitCode: null, signalCode: null };
  const run = new ScratchRun('', process.cwd(), '', '', null);
  run.groups = new Map([[child.pid, child]]);
  const signals = [];
  let absent = false;
  t.mock.method(process, 'kill', (pid, signal) => {
    assert.equal(pid, -child.pid);
    assert.equal(signal, 0);
    if (signals.length === 0) return true;
    throw Object.assign(new Error('group observation failed during shutdown'), { code: absent ? 'ESRCH' : 'EPERM' });
  });
  t.mock.method(run, 'signalGroup', (_pid, signal) => {
    signals.push(signal);
    if (signal === 'SIGKILL') absent = true;
  });
  const error = await run.stopGroup(child.pid, child).then(() => null, (failure) => failure);
  assert.deepEqual(signals, ['SIGTERM']);
  assert.equal(error?.code, 'EPERM');
  assert.equal(run.groups.has(child.pid), true);
});

test('generic child live cleanup can stop its owned group', async (t) => {
  const child = { pid: 800008, exitCode: null, signalCode: null };
  const run = new ScratchRun('', process.cwd(), '', '', null);
  run.groups = new Map([[child.pid, child]]);
  const signals = [];
  let alive = true;
  t.mock.method(run, 'groupAbsent', () => !alive);
  t.mock.method(run, 'signalGroup', (pid, signal) => {
    signals.push({ pid, signal });
    child.exitCode = 0;
    alive = false;
  });
  await run.stopGroup(child.pid, child);
  assert.deepEqual(signals, [{ pid: child.pid, signal: 'SIGTERM' }]);
  assert.equal(run.groups.has(child.pid), false);
});

test('generic child release timer refuses a child that already exited', async (t) => {
  const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-test-'));
  const run = new ScratchRun(root, process.cwd(), 'dist-stand-in', '', null);
  const child = Object.assign(new EventEmitter(), { pid: 800009, exitCode: null, signalCode: null });
  const spawned = Promise.withResolvers();
  const signals = [];
  t.mock.method(console, 'log', () => {});
  t.mock.method(run, 'capture', async (_program, args) => ({
    stdout: args[0] === 'metadata' ? JSON.stringify({ target_directory: root }) : '',
  }));
  t.mock.method(run, 'groupAbsent', () => true);
  t.mock.method(run, 'signalGroup', (pid, signal) => signals.push({ pid, signal }));
  t.mock.method(childProcess, 'spawn', () => {
    spawned.resolve();
    return child;
  });
  syncBuiltinESMExports();
  t.mock.timers.enable({ apis: ['setTimeout'] });
  try {
    const pending = run.build();
    await spawned.promise;
    child.exitCode = 0;
    child.emit('exit', 0);
    t.mock.timers.tick(20 * 60_000);
    await pending;
    assert.deepEqual(signals, []);
    assert.equal(run.groups.has(child.pid), false);
  } finally {
    t.mock.timers.reset();
    t.mock.restoreAll();
    syncBuiltinESMExports();
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
    response.end('<!doctype html><title>Smoke pipe</title><button id="hire-open">Hire</button>');
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

import { execFile, spawn } from 'node:child_process';
import { once } from 'node:events';
import { access, chmod, mkdir, mkdtemp, readFile, readlink, realpath, rm, stat, symlink, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { delimiter, isAbsolute, join, resolve } from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import { Journey } from './journey.mjs';

const execute = promisify(execFile);
const pause = (ms) => new Promise((done) => setTimeout(done, ms));
const timeout = 30_000;
// Navigating is the one phase whose work crosses into Chrome's network and renderer processes, so
// under host CPU starvation it queues behind every other process on the box while the lighter
// protocol handshake calls (Browser.getVersion, Target.attachToTarget, ...) stay fast. Measured
// under artificial load (load average 70-140), Page.navigate's ack reliably failed at the 30s mark
// within a few seconds and never completed even given 180s once the host was saturated, so this is
// a generous-but-bounded wait for transient contention, not a fix for a genuinely wedged Chrome.
export const navigateTimeout = 120_000;
let interrupted = false;
let journeyRun;

class WaitTimeout extends Error {}

function check(value, message) {
  if (!value) throw new Error(message);
  return value;
}

async function stage(name, action) {
  try {
    check(!interrupted || name === 'cleanup', 'smoke interrupted');
    const value = await action();
    console.log(`PASS ${name}`);
    return value;
  } catch (error) {
    console.log(`FAIL ${name}`);
    throw error;
  }
}

async function waitFor(action, message, duration = timeout, allowInterrupted = false) {
  const end = Date.now() + duration;
  while (Date.now() < end) {
    check(allowInterrupted || !interrupted, 'smoke interrupted');
    const value = await action();
    if (value) return value;
    await pause(100);
  }
  throw new WaitTimeout(message);
}

export async function chromePath(paths = [
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/Applications/Chromium.app/Contents/MacOS/Chromium',
  ...(process.env.PATH ?? '').split(delimiter).flatMap((dir) =>
    ['google-chrome', 'chromium', 'chromium-browser'].map((name) => join(dir, name))),
], required = process.env.DUDES_SMOKE_REQUIRE_CHROME === '1', configured = process.env.DUDES_SMOKE_CHROME) {
  if (configured !== undefined) {
    check(isAbsolute(configured)
      && await stat(configured).then((info) => info.isFile(), () => false)
      && await access(configured, constants.X_OK).then(() => true, () => false),
    'DUDES_SMOKE_CHROME must name an absolute executable file');
    return configured;
  }
  for (const path of paths) {
    if (await access(path, constants.X_OK).then(() => true, () => false)) return path;
  }
  check(!required, 'Chrome is required for the release smoke check');
  return null;
}

export class DevTools {
  constructor(input, output) {
    this.input = input;
    this.output = output;
    this.sequence = 0;
    this.pending = new Map();
    this.buffer = '';
    this.closed = false;
    input.setEncoding('utf8');
    input.on('data', (chunk) => {
      this.buffer += chunk;
      let end;
      while ((end = this.buffer.indexOf('\0')) !== -1) {
        const message = this.buffer.slice(0, end);
        this.buffer = this.buffer.slice(end + 1);
        let answer;
        try { answer = JSON.parse(message); } catch { this.close(); return; }
        if (answer === null || typeof answer !== 'object' || Array.isArray(answer)) { this.close(); return; }
        const call = this.pending.get(answer.id);
        if (!call) continue;
        this.pending.delete(answer.id);
        clearTimeout(call.timer);
        if (answer.error) call.reject(new Error('Chrome refused a DevTools command'));
        else call.resolve(answer.result);
      }
    });
    input.on('end', () => this.close());
    input.on('close', () => this.close());
    input.on('error', () => this.close());
    output.on('close', () => this.close());
    output.on('error', () => this.close());
  }

  send(method, params = {}, sessionId, duration = timeout) {
    check(!this.closed, 'Chrome connection closed');
    const id = ++this.sequence;
    return new Promise((resolveCall, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Chrome ${method} timed out`));
      }, duration);
      this.pending.set(id, { resolve: resolveCall, reject, timer });
      this.output.write(`${JSON.stringify({ id, method, params, sessionId })}\0`);
    });
  }

  async evaluate(expression) {
    const answer = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true }, this.session);
    check(!answer.exceptionDetails, 'Floor interaction failed');
    return answer.result?.value;
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    for (const call of this.pending.values()) {
      clearTimeout(call.timer);
      call.reject(new Error('Chrome connection closed'));
    }
    this.pending.clear();
    this.input.destroy();
    this.output.destroy();
  }
}

export class ScratchRun {
  constructor(root, workspace, xtask, version, chrome, prefix = join(root, 'prefix')) {
    this.root = root;
    this.workspace = workspace;
    this.xtask = xtask;
    this.version = version;
    this.chromeProgram = chrome;
    this.groups = new Map();
    this.owned = new Map();
    this.chromeChildren = new Map();
    this.prefix = prefix;
    this.repo = join(root, 'repo');
    this.home = join(root, 'home');
    this.runtime = join(root, 'run');
    this.env = {
      HOME: this.home,
      TMPDIR: join(root, 'tmp'),
      PATH: `${join(root, 'engines')}${delimiter}${join(this.prefix, 'bin')}${delimiter}/usr/bin${delimiter}/bin`,
      DUDES_STATE_DIR: join(root, 'state'),
      DUDES_RUNTIME_DIR: this.runtime,
      SSH_TTY: 'smoke-no-browser',
    };
  }

  async command(program, args, cwd = this.repo) {
    return this.capture(program, args, { cwd, env: this.env, timeout });
  }

  groupAlive(pid) {
    try { process.kill(-pid, 0); return true; } catch { return false; }
  }

  groupAbsent(pid) {
    try { process.kill(-pid, 0); return false; } catch (error) {
      if (error.code === 'ESRCH') return true;
      throw error;
    }
  }

  signalGroup(pid, signal) {
    try { process.kill(-pid, signal); } catch {}
  }

  signalChildGroup(child, signal) {
    if (!child?.pid || this.groups.get(child.pid) !== child
      || child.exitCode !== null || child.signalCode !== null) return false;
    this.signalGroup(child.pid, signal);
    return true;
  }

  signalChildren(signal) {
    for (const child of this.groups.values()) this.signalChildGroup(child, signal);
  }

  async stopGroup(pid, child = this.groups.get(pid)) {
    if (!pid) return;
    const chrome = pid === this.chrome?.pid;
    if (chrome && !this.chromeIdentity) {
      await this.stopUnanchoredChrome();
      return;
    }
    if (chrome && !await this.chromeGroupOwned()) return;
    const absent = chrome ? () => !this.groupAlive(pid) : () => this.groupAbsent(pid);
    if (!absent()) {
      if (chrome) await this.signalChrome('SIGTERM');
      else check(child?.pid === pid && this.signalChildGroup(child, 'SIGTERM'), 'child group ownership is unproven');
      await waitFor(absent, 'child group survived shutdown', 5000, true).catch((error) => {
        if (!(error instanceof WaitTimeout)) throw error;
        if (chrome) return this.signalChrome('SIGKILL');
        check(this.signalChildGroup(child, 'SIGKILL'), 'child group ownership is unproven');
      });
      await waitFor(absent, 'child group survived kill', 5000, true);
    }
    if (this.groups.get(pid) === child) this.groups.delete(pid);
  }

  async capture(program, args, options) {
    const pending = execute(program, args, { ...options, detached: true, killSignal: 'SIGKILL', maxBuffer: 2_000_000 });
    const child = pending.child;
    const pid = child.pid;
    if (pid) this.groups.set(pid, child);
    try {
      return await pending;
    } catch {
      throw new Error(`${program.split('/').at(-1)} failed or timed out`);
    } finally {
      await this.stopGroup(pid, child);
    }
  }

  cli(args) {
    return this.command(join(this.prefix, 'bin/dudes'), args);
  }

  async prepare() {
    for (const path of [this.home, this.repo, this.env.TMPDIR, join(this.root, 'engines')]) {
      await mkdir(path, { recursive: true, mode: 0o700 });
    }
    await this.command('git', ['init', '--quiet']);
    const pidFile = join(this.root, 'engine-pids').replaceAll("'", "'\\''");
    for (const engine of ['claude', 'codex']) {
      const file = join(this.root, 'engines', engine);
      await writeFile(file, `#!/bin/sh
case "$*" in
  '--version') printf 'smoke stand-in\\n'; exit 0 ;;
  'auth status'|'login status') exit 0 ;;
esac
printf '%s\\n' "$$" >> '${pidFile}'
printf '%s\\n' '{"type":"thread.started","thread_id":"11111111-1111-4111-8111-111111111111"}'
exec /bin/sleep 120
`, { mode: 0o700 });
      await chmod(file, 0o700);
    }
  }

  async build() {
    const cargoHome = join(this.root, 'cargo');
    await mkdir(cargoHome, { mode: 0o700 });
    await symlink(join(process.env.CARGO_HOME ?? join(process.env.HOME, '.cargo'), 'registry'), join(cargoHome, 'registry'));
    this.buildEnv = {
      HOME: this.home,
      TMPDIR: this.env.TMPDIR,
      PATH: process.env.DUDES_SMOKE_ORIGINAL_PATH ?? process.env.PATH,
      CARGO: process.env.CARGO ?? 'cargo',
      CARGO_HOME: cargoHome,
      RUSTUP_HOME: process.env.RUSTUP_HOME ?? join(process.env.HOME, '.rustup'),
      CARGO_NET_OFFLINE: 'true',
      NPM_CONFIG_OFFLINE: 'true',
      NPM_CONFIG_UPDATE_NOTIFIER: 'false',
    };
    if (process.env.RUSTUP_TOOLCHAIN !== undefined) this.buildEnv.RUSTUP_TOOLCHAIN = process.env.RUSTUP_TOOLCHAIN;
    if (process.env.DUDES_SMOKE_ORIGINAL_PATH) this.buildEnv.DUDES_SMOKE_ORIGINAL_PATH = process.env.DUDES_SMOKE_ORIGINAL_PATH;
    if (process.env.CARGO_TARGET_DIR) this.buildEnv.CARGO_TARGET_DIR = process.env.CARGO_TARGET_DIR;
    if (process.env.CARGO_BUILD_JOBS !== undefined) this.buildEnv.CARGO_BUILD_JOBS = process.env.CARGO_BUILD_JOBS;
    if (process.env.CARGO_INCREMENTAL !== undefined) this.buildEnv.CARGO_INCREMENTAL = process.env.CARGO_INCREMENTAL;
    if (process.env.MACOSX_DEPLOYMENT_TARGET) this.buildEnv.MACOSX_DEPLOYMENT_TARGET = process.env.MACOSX_DEPLOYMENT_TARGET;
    await stage('Floor build', async () => {
      await this.capture('npm', ['run', 'build'], {
        cwd: join(this.workspace, 'floor'), env: { ...this.buildEnv, PATH: process.env.PATH }, timeout: 120_000,
      });
    });
    await stage('release', async () => {
      const child = spawn(this.xtask, ['dist'], { cwd: this.workspace, env: this.buildEnv, detached: true, stdio: ['ignore', 'inherit', 'inherit'] });
      if (child.pid) this.groups.set(child.pid, child);
      const timer = setTimeout(() => this.signalChildGroup(child, 'SIGKILL'), 20 * 60_000);
      try {
        const [code] = await once(child, 'exit');
        check(code === 0, 'release build failed');
      } finally {
        clearTimeout(timer);
        await this.stopGroup(child.pid, child);
      }
    });
    const { stdout } = await this.capture(this.buildEnv.CARGO, ['metadata', '--locked', '--format-version=1', '--no-deps'], {
      cwd: this.workspace, env: this.buildEnv, timeout,
    }).catch(() => { throw new Error('release target lookup failed'); });
    this.dist = join(JSON.parse(stdout).target_directory, 'dist');
  }

  async serve() {
    const sums = await readFile(join(this.dist, 'SHA256SUMS'), 'utf8');
    const archives = sums.trim().split('\n').map((line) => line.trim().split(/\s+/)[1]);
    check(archives.length > 0 && archives.every((name) => name?.startsWith(`dudes-${this.version}-`)
      && /^dudes-[A-Za-z0-9.+_-]+\.tar\.gz$/.test(name)), 'invalid release archive inventory');
    const files = new Map(await Promise.all(['SHA256SUMS', ...archives].map(async (name) =>
      [name, await readFile(join(this.dist, name))])));
    this.server = createServer((request, response) => {
      const bytes = files.get(request.url?.slice(1));
      response.writeHead(bytes ? 200 : 404, { 'Content-Type': 'application/octet-stream' });
      response.end(bytes);
    });
    const listening = once(this.server, 'listening');
    this.server.listen(0, '127.0.0.1');
    await listening;
    this.env.DUDES_INSTALL_BASE_URL = `http://127.0.0.1:${this.server.address().port}`;
  }

  async install() {
    await this.command('/bin/sh', [join(this.workspace, 'install.sh'), '--version', this.version, '--prefix', this.prefix]);
    await this.command(join(this.prefix, 'bin/duded'), ['--check-floor', join(this.prefix, 'share/dudes/floor')]);
    check((await readFile(join(this.prefix, 'bin/dudes'))).equals(
      await readFile(join(this.dist, `dudes-${this.version}`, 'dudes'))), 'installed CLI differs from the release');
  }

  async quickstart() {
    await this.cli(['quickstart', '--set', 'providers.codex', 'on',
      '--set', 'floor.hire', 'on', '--enroll', '--check-label', 'smoke completion', '--', '/usr/bin/false']);
    await this.cli(['ping']);
    const pids = await this.daemonPids();
    check(pids.length === 1 && this.alive(pids[0]), 'quickstart did not start one live daemon');
    await this.rememberManaged();
  }

  async launchChrome() {
    const profile = join(this.root, 'chrome');
    this.chrome = spawn(this.chromeProgram, [
      ...(process.platform === 'darwin' ? ['--use-mock-keychain'] : ['--password-store=basic']),
      '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
      '--disable-background-networking', '--disable-component-update', '--disable-sync',
      '--disable-extensions', '--disable-crash-reporter', '--metrics-recording-only',
      '--proxy-server=http://127.0.0.1:9', '--proxy-bypass-list=127.0.0.1;localhost',
      '--remote-debugging-pipe', `--user-data-dir=${profile}`, 'about:blank',
    ], { env: this.env, detached: true, stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe'] });
    this.chromeChild = this.chrome;
    let cdp;
    this.chrome.on('error', () => cdp?.close());
    this.chrome.on('exit', () => cdp?.close());
    cdp = new DevTools(this.chrome.stdio[4], this.chrome.stdio[3]);
    this.cdp = cdp;
    await this.cdp.send('Browser.getVersion');
    check(this.chrome.exitCode === null && this.chrome.signalCode === null, 'Chrome exited before ownership was recorded');
    const identity = await this.processInfo(this.chrome.pid);
    check(identity && identity.parent === process.pid && identity.group === this.chrome.pid
      && this.chrome.exitCode === null && this.chrome.signalCode === null, 'Chrome process identity is unproven');
    this.chromeIdentity = identity;
    await this.rememberChromeChildren();
    const { targetId } = await this.cdp.send('Target.createTarget', { url: 'about:blank', forTab: false });
    const { sessionId } = await this.cdp.send('Target.attachToTarget', { targetId, flatten: true });
    this.cdp.session = sessionId;
    await this.cdp.send('Page.enable', {}, sessionId);
    await this.cdp.send('Runtime.enable', {}, sessionId);
  }

  async openFloor(link) {
    const sessionId = this.cdp.session;
    const navigation = await this.cdp.send('Page.navigate', { url: link, referrerPolicy: 'noReferrer' }, sessionId, navigateTimeout);
    check(!navigation.errorText && !navigation.isDownload, 'Chrome could not navigate to the Floor');
    const page = new URL(link);
    await waitFor(async () => {
      const { frameTree } = await this.cdp.send('Page.getFrameTree', {}, sessionId);
      let committed;
      try { committed = new URL(frameTree.frame.url); } catch { return false; }
      return committed.origin === page.origin && committed.pathname === '/floor/'
        && (!navigation.loaderId || frameTree.frame.loaderId === navigation.loaderId);
    }, 'Chrome did not commit the Floor navigation', navigateTimeout);
    await waitFor(() => this.cdp.evaluate(`Boolean(document.querySelector('#hire-open'))`), 'Floor bundle did not render Hire');
  }

  async browser() {
    await this.launchChrome();
    const { stdout } = await this.cli(['floor', 'open', '--no-browser']);
    const link = stdout.match(/http:\/\/127\.0\.0\.1:\d+\/floor\/#bootstrap=[A-Za-z0-9_-]+/)?.[0];
    check(link, 'Floor open returned no bootstrap link');
    await this.openFloor(link);
  }

  async hire() {
    const click = (selector) => this.cdp.evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return false; el.click(); return true; })()`);
    const fill = (selector, value) => this.cdp.evaluate(`(() => {
      const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return false;
      Object.getOwnPropertyDescriptor(el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value').set.call(el, ${JSON.stringify(value)});
      el.dispatchEvent(new Event('input', { bubbles: true })); return true;
    })()`);
    const next = () => this.cdp.evaluate(`(() => { const el = [...document.querySelectorAll('.hire-actions button')].find(el => el.textContent === 'Next'); if (!el) return false; el.click(); return true; })()`);
    check(await click('#hire-open'), 'Hire opener missing');
    await waitFor(() => this.cdp.evaluate(`Boolean(document.querySelector('#hire-name'))`), 'Hire wizard did not open');
    check(await fill('#hire-name', 'Smoke agent'), 'Hire name missing');
    check(await fill('#hire-role', 'First-run smoke'), 'Hire role missing');
    await next();
    await waitFor(() => this.cdp.evaluate(`Boolean(document.querySelector('input[name="workspace_id"]'))`), 'quickstart did not enroll the scratch repository');
    await click('input[name="workspace_id"]');
    await next();
    await waitFor(() => this.cdp.evaluate(`Boolean(document.querySelector('input[name="engine"][value="codex"]'))`), 'stand-in engine was not offered');
    await click('input[name="engine"][value="codex"]');
    await next();
    await waitFor(() => this.cdp.evaluate(`Boolean(document.querySelector('#hire-goal'))`), 'Hire briefing missing');
    await fill('#hire-goal', 'Keep the smoke stand-in alive');
    await click('.hire-submit');
    this.agent = await waitFor(async () => {
      const result = await this.cdp.evaluate(`(() => {
        const outcome = document.querySelector('[data-hire-outcome]');
        return outcome ? { kind: outcome.dataset.hireOutcome, agent: outcome.querySelector('[data-hire-agent-id]')?.dataset.hireAgentId } : null;
      })()`);
      check(!result || result.kind === 'complete', 'Hire was refused or incomplete');
      return result?.agent;
    }, 'Hire did not complete');
    const selector = `.crew-row[data-agent-id="${this.agent}"] [data-live-activity]`;
    await waitFor(async () => ['Working', 'Tool step'].includes(await this.cdp.evaluate(`document.querySelector(${JSON.stringify(selector)})?.textContent`)), 'hired agent has no live activity label');
    const agents = JSON.parse((await this.cli(['--json', 'agents'])).stdout);
    check(Array.isArray(agents) && agents.some((agent) => agent.id === this.agent), 'installed dudes agents does not list the hired agent');
    await this.rememberManaged();
  }

  async daemonPids() {
    const log = await readFile(join(this.runtime, 'duded.log'), 'utf8').catch(() => '');
    return [...log.matchAll(/^quickstart started duded pid (\d+)$/gm)].map((match) => Number(match[1]));
  }

  async enginePids() {
    return (await readFile(join(this.root, 'engine-pids'), 'utf8').catch(() => '')).trim().split('\n').map(Number).filter((pid) => pid > 0);
  }

  alive(pid) {
    try { process.kill(pid, 0); return true; } catch { return false; }
  }

  async processInfo(pid) {
    if (!this.alive(pid)) return null;
    let stdout;
    try {
      ({ stdout } = await this.command('/bin/ps', ['-p', String(pid), '-o', 'pid=,ppid=,pgid=,lstart=,comm=']));
    } catch (error) {
      if (!this.alive(pid)) return null;
      throw error;
    }
    const row = stdout.match(/^\s*(\d+)\s+(\d+)\s+(\d+)\s+(.{24})\s+(.+)$/m);
    if (!row && !this.alive(pid)) return null;
    check(row, 'could not verify an owned process identity');
    let program = row[5];
    if (process.platform === 'linux') {
      program = await readlink(`/proc/${pid}/exe`).catch((error) => {
        if (error.code === 'ENOENT' || error.code === 'ESRCH') return null;
        throw error;
      });
      if (program === null) return null;
    }
    return { pid: Number(row[1]), parent: Number(row[2]), group: Number(row[3]), birth: row[4], program };
  }

  async rememberManaged() {
    const daemons = await this.daemonPids();
    for (const pid of daemons) {
      if (this.owned.has(pid)) continue;
      const info = await this.processInfo(pid);
      if (!info) continue;
      const program = join(this.prefix, 'bin/duded');
      const expected = process.platform === 'linux' ? await realpath(program) : program;
      check(info.program === expected && info.group === pid, 'daemon process identity is unproven');
      this.owned.set(pid, { ...info, kind: 'daemon' });
    }
    for (const pid of await this.enginePids()) {
      if (this.owned.has(pid)) continue;
      const info = await this.processInfo(pid);
      if (!info) continue;
      const daemon = this.owned.get(info.parent);
      check(daemon?.kind === 'daemon' && await this.ownedAlive(daemon) && info.group === pid, 'stand-in process identity is unproven');
      const expected = process.platform === 'linux' ? await realpath('/bin/sleep') : '/bin/sleep';
      const executed = await waitFor(async () => {
        const current = await this.processInfo(pid);
        if (!current) return { exited: true };
        check(current.birth === info.birth && current.parent === info.parent && current.group === pid,
          'stand-in process identity changed before exec');
        return current.program === expected ? current : null;
      }, 'stand-in did not exec sleep', timeout, true);
      if (!executed.exited) this.owned.set(pid, { ...executed, kind: 'engine' });
    }
  }

  async ownedAlive(identity) {
    const current = await this.processInfo(identity.pid);
    return current !== null && current.birth === identity.birth
      && current.group === identity.group && current.program === identity.program;
  }

  async killOwned(identity) {
    if (!await this.ownedAlive(identity)) return;
    if (identity.group === identity.pid) this.signalGroup(identity.group, 'SIGKILL');
    try { process.kill(identity.pid, 'SIGKILL'); } catch {}
  }

  async chromeGroupOwned() {
    if (this.chromeIdentity && await this.ownedAlive(this.chromeIdentity)) return true;
    for (const identity of this.chromeChildren.values()) {
      if (identity.group === this.chrome?.pid && await this.ownedAlive(identity)) return true;
    }
    return false;
  }

  async signalChrome(signal) {
    if (!this.chromeIdentity) {
      this.signalUnanchoredChrome(signal);
      return;
    }
    if (await this.chromeGroupOwned()) this.signalGroup(this.chrome.pid, signal);
  }

  signalUnanchoredChrome(signal) {
    const child = this.chromeChild;
    if (child !== this.chrome || child?.exitCode !== null || child.signalCode !== null) return;
    this.signalGroup(child.pid, signal);
    child.kill(signal);
  }

  async stopUnanchoredChrome() {
    const child = this.chromeChild;
    if (child !== this.chrome || !child?.pid) return;
    this.signalUnanchoredChrome('SIGTERM');
    await waitFor(() => child.exitCode !== null || child.signalCode !== null, 'unanchored Chrome survived shutdown', 5000, true).catch(() => {
      this.signalUnanchoredChrome('SIGKILL');
    });
    await waitFor(() => child.exitCode !== null || child.signalCode !== null, 'unanchored Chrome survived kill', 5000, true);
    check(!this.groupAlive(child.pid), 'unanchored Chrome helpers need manual cleanup');
  }

  async rememberChromeChildren() {
    if (!this.chromeIdentity || !await this.ownedAlive(this.chromeIdentity)) return;
    const { stdout } = await this.command('/bin/ps', ['-axo', 'pid=,ppid=,pgid=,lstart=,comm=']);
    const rows = stdout.trim().split('\n').map((line) => {
      const row = line.match(/^\s*(\d+)\s+(\d+)\s+(\d+)\s+(.{24})\s+(.+)$/);
      return row ? { pid: Number(row[1]), parent: Number(row[2]), group: Number(row[3]), birth: row[4], program: row[5] } : null;
    }).filter(Boolean);
    const anchor = await this.processInfo(this.chromeIdentity.pid);
    if (!anchor || anchor.birth !== this.chromeIdentity.birth || anchor.group !== this.chromeIdentity.group
      || anchor.program !== this.chromeIdentity.program || !await this.ownedAlive(this.chromeIdentity)) return;
    const owned = new Map([[anchor.pid, anchor]]);
    let size;
    do {
      size = owned.size;
      for (const row of rows) {
        if (owned.has(row.parent)) owned.set(row.pid, row);
      }
    } while (size !== owned.size);
    for (const [pid, row] of owned) {
      const current = await this.processInfo(pid).catch((error) => {
        if (pid !== anchor.pid && row.group === anchor.group && error.code === 'EACCES'
          && error.syscall === 'readlink' && error.path === `/proc/${pid}/exe`) return null;
        throw error;
      });
      if (current && current.birth === row.birth && current.parent === row.parent && current.group === row.group) {
        this.chromeChildren.set(pid, current);
      }
    }
  }

  async stop() {
    await this.capture(join(this.prefix, 'bin/dudes'), ['stop'], {
      cwd: this.repo, env: this.env, timeout: 390000,
    });
    const identities = [...this.owned.values()];
    await waitFor(async () => (await Promise.all(identities.map((identity) => this.ownedAlive(identity)))).every((alive) => !alive), 'daemon or stand-in survived stop');
    const stopped = await this.cli(['ping']).then(() => false, () => true);
    check(stopped, 'daemon still answers after stop');
  }

  // Ends Chrome and every helper it started. Returns what went wrong, so cleanup can report it.
  async closeChrome() {
    const failures = [];
    const clean = async (action) => {
      try { await action(); } catch (error) { failures.push(error.message); }
    };
    if (this.chrome?.pid) {
      await clean(() => this.rememberChromeChildren());
      const chromeIdentities = [...this.chromeChildren.values()];
      if (this.chromeIdentity) chromeIdentities.push(this.chromeIdentity);
      await clean(() => this.cdp?.close());
      await clean(() => this.stopGroup(this.chrome.pid));
      for (const identity of chromeIdentities) await clean(() => this.killOwned(identity));
      await clean(() => waitFor(async () => (await Promise.all(chromeIdentities.map((identity) => this.ownedAlive(identity)))).every((alive) => !alive), 'Chrome helper survived cleanup', 10_000, true));
    }
    return failures;
  }

  async cleanup() {
    const failures = await this.closeChrome();
    const clean = async (action) => {
      try { await action(); } catch (error) { failures.push(error.message); }
    };
    for (const child of [...this.groups.values()]) await clean(() => this.stopGroup(child.pid, child));
    await clean(() => this.rememberManaged());
    await this.cli(['stop', '--hard']).catch(() => {});
    const identities = [...this.owned.values()];
    for (const identity of identities) await clean(() => this.killOwned(identity));
    await clean(() => waitFor(async () => (await Promise.all(identities.map((identity) => this.ownedAlive(identity)))).every((alive) => !alive), 'smoke left a process behind', 10_000, true));
    if (this.server) await clean(() => new Promise((done) => this.server.close(done)));
    await clean(() => rm(this.root, { recursive: true, force: true }));
    check(failures.length === 0, failures.join('; '));
  }
}

// A newcomer's whole path on a second scratch home, sharing the install: quickstart, open, then
// only the page, up to an accepted ticket.
async function newcomerJourney(first, chrome) {
  const root = await mkdtemp(join(tmpdir(), 'dudes-jrny-'));
  await chmod(root, 0o700);
  const second = new ScratchRun(root, first.workspace, first.xtask, first.version, chrome, first.prefix);
  // The same Chrome: a wrapper that admits one proof browser at a time would refuse a second.
  Object.assign(second, { chrome: first.chrome, chromeChild: first.chromeChild, chromeIdentity: first.chromeIdentity,
    chromeChildren: first.chromeChildren, cdp: first.cdp });
  journeyRun = second;
  try {
    await second.prepare();
    await new Journey(second).walk();
    await second.stop();
  } finally {
    await second.cleanup();
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  let run;
  const interrupt = () => {
    interrupted = true;
    for (const each of [run, journeyRun]) {
      if (each?.chrome?.pid) void each.signalChrome('SIGTERM').catch(() => {});
      each?.signalChildren('SIGTERM');
    }
  };
  process.once('SIGINT', interrupt);
  process.once('SIGTERM', interrupt);
  try {
    const [xtask, version] = process.argv.slice(2);
    check(xtask && version, 'smoke requires its xtask executable and version');
    const chrome = await chromePath();
    const root = await mkdtemp(join(tmpdir(), 'dudes-smoke-'));
    run = new ScratchRun(root, process.cwd(), resolve(xtask), version, chrome);
    await chmod(root, 0o700);
    await stage('scratch setup', () => run.prepare());
    await run.build();
    await stage('release server', () => run.serve());
    await stage('install', () => run.install());
    await stage('quickstart', () => run.quickstart());
    if (chrome) {
      await stage('browser', () => run.browser());
      await stage('hire and live activity', () => run.hire());
    } else console.log('SKIP browser (Chrome absent)');
    await stage('stop', () => run.stop());
    if (chrome) await stage('journey from quickstart to an accepted ticket', () => newcomerJourney(run, chrome));
  } catch (error) {
    console.error(`smoke: ${error.message}`);
    process.exitCode = 1;
  } finally {
    if (run) {
      await stage('cleanup', () => run.cleanup()).catch((error) => {
        console.error(`smoke: ${error.message}`);
        process.exitCode = 1;
      });
    }
  }
}

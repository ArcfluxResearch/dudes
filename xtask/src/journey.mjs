import { chmod, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { terminalCommandsOnScreen } from '../../floor/tests/helpers/terminal-commands.mjs';

const pause = (ms) => new Promise((done) => setTimeout(done, ms));

/**
 * Screens that still send a person to a terminal. Each entry names the exit, the screen that
 * shows it and the text it shows, so the run still proves every other screen. Remove an entry
 * when the screen stops showing the command: an entry that matches nothing fails the run.
 */
export const ALLOWED_EXITS = [
  { name: 'Settings shows the remove and reload commands for a project', screen: 'settings', text: /^dudes project (?:remove|reload)/ },
  { name: 'Settings sends a restart to the terminal', screen: 'settings', text: /^dudes quickstart --restart/ },
  { name: 'Settings sends the update check to the terminal', screen: 'settings', text: /^dudes upgrade/ },
];

// Stands in for the engine: it edits a file in the project, like an agent doing a task, and ends
// its turn. It needs no account.
const ENGINE = `#!/bin/sh
case "$*" in
  '--version') printf 'journey stand-in\\n'; exit 0 ;;
  'auth status'|'login status') exit 0 ;;
esac
cat >/dev/null 2>&1 || true
printf 'written by the stand-in engine\\n' >> journey-note.txt
printf '%s\\n' '{"type":"thread.started","thread_id":"11111111-1111-4111-8111-111111111111"}'
printf '%s\\n' '{"type":"turn.started"}'
printf '%s\\n' '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"stand-in reply"}}'
printf '%s\\n' '{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2}}'
`;

function check(value, message) {
  if (!value) throw new Error(message);
  return value;
}

/** Drive the page the way a newcomer would, and look at every screen for a terminal command. */
export class Journey {
  /** `run` is a prepared `ScratchRun` whose daemon is not yet running. */
  constructor(run, allowed = ALLOWED_EXITS) {
    this.run = run;
    this.allowed = allowed;
    this.used = new Set();
    this.findings = [];
    this.screens = [];
  }

  evaluate(expression) {
    return this.run.cdp.evaluate(expression);
  }

  async until(expression, message, duration = 30_000) {
    const end = Date.now() + duration;
    while (Date.now() < end) {
      const value = await this.evaluate(expression);
      if (value) return value;
      await pause(100);
    }
    throw new Error(message);
  }

  async click(selector, message = `${selector} is missing`) {
    await this.until(`(() => { const el = document.querySelector(${JSON.stringify(selector)});
      if (!el || el.disabled || el.getAttribute('aria-disabled') === 'true') return false; el.click(); return true; })()`, message);
  }

  async clickButton(label, scope = 'button') {
    await this.until(`(() => { const el = [...document.querySelectorAll(${JSON.stringify(scope)})]
      .find((el) => el.textContent.trim() === ${JSON.stringify(label)} && !el.disabled && el.getAttribute('aria-disabled') !== 'true');
      if (!el) return false; el.click(); return true; })()`, `no "${label}" button`);
  }

  async fill(selector, value) {
    await this.until(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return false;
      Object.getOwnPropertyDescriptor(el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype, 'value').set.call(el, ${JSON.stringify(value)});
      el.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`, `${selector} is missing`);
  }

  /** Look at the screen now: save a picture and record any terminal command it shows. */
  async screen(name) {
    const { text, codeBlocks } = await this.evaluate(`({
      text: document.body.innerText + '\\n' + [...document.querySelectorAll('code')].map((el) => el.textContent).join('\\n'),
      codeBlocks: [...document.querySelectorAll('.guide-command')].map((el) => el.textContent),
    })`);
    const shot = await this.run.cdp.send('Page.captureScreenshot', { format: 'png' }, this.run.cdp.session);
    const file = join(this.pictures, `${String(this.screens.length + 1).padStart(2, '0')}-${name.replace(/[^a-z0-9]+/gi, '-').toLowerCase()}.png`);
    await writeFile(file, Buffer.from(shot.data, 'base64'));
    this.screens.push(name);
    const seen = new Set();
    for (const found of terminalCommandsOnScreen({ text, codeBlocks })) {
      if (seen.has(found.text)) continue;
      seen.add(found.text);
      const exit = this.allowed.find((entry) => entry.screen === name && entry.text.test(found.text));
      if (exit) this.used.add(exit);
      else this.findings.push(`${name}: ${found.kind}: ${found.text}`);
    }
  }

  async prepare() {
    const { run } = this;
    delete run.env.SSH_TTY;
    this.pictures = await mkdtemp(join(tmpdir(), 'dudes-journey-pictures-'));
    await writeFile(join(run.root, 'engines/codex'), ENGINE, { mode: 0o700 });
    await chmod(join(run.root, 'engines/codex'), 0o700);
    this.linkFile = join(run.root, 'link');
    const opener = join(run.root, 'opener.sh');
    await writeFile(opener, `#!/bin/sh\numask 077\nprintf '%s' "$1" > '${this.linkFile.replaceAll("'", "'\\''")}'\n`, { mode: 0o700 });
    run.env.DUDES_OPENER = opener;
    await run.command('sh', ['-c', 'printf "a project\\n" > README.md && git add README.md && git -c user.name=journey -c user.email=journey@example.invalid commit -qm start']);
  }

  /** Start and open Dudes as a newcomer does. Nothing else is typed after this. */
  async start() {
    const { run } = this;
    await run.cli(['quickstart', '--yes', '--set', 'providers.codex', 'on']);
    await run.rememberManaged();
    if (!run.cdp) await run.launchChrome();
    await run.cdp.send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false }, run.cdp.session);
    const { stdout: opened } = await run.cli(['open']);
    let link;
    const end = Date.now() + 15_000;
    while (!link && Date.now() < end) {
      link = (await readFile(this.linkFile, 'utf8').catch(() => '')).trim() || undefined;
      if (!link) await pause(100);
    }
    check(link, `dudes open did not hand a link to the opener: ${opened.replace(/#bootstrap=\S+/g, '#bootstrap=...').trim()}`);
    await rm(this.linkFile, { force: true });
    await run.openFloor(link);
  }

  async addProject() {
    await this.until(`Boolean(document.querySelector('input[name="folder"]'))`, 'the first screen offers no folder box');
    await this.screen('first visit');
    await this.fill('input[name="folder"]', this.run.repo);
    await this.clickButton('Look at folder');
    await this.until(`Boolean(document.querySelector('.add-project-submit'))`, 'the folder was not offered for adding');
    await this.screen('folder found');
    await this.click('.add-project-submit');
    await this.until(`Boolean(document.querySelector('.first-task-start'))`, 'the project was not added');
    await this.screen('project added');
  }

  async hire() {
    await this.click('.first-task-start');
    await this.until(`Boolean(document.querySelector('#hire-name'))`, 'the hire dialog did not open');
    await this.screen('hire: who');
    await this.fill('#hire-name', 'Journey agent');
    await this.fill('#hire-role', 'Journey');
    await this.clickButton('Next', '.hire-actions button');
    await this.until(`Boolean(document.querySelector('input[name="workspace_id"]'))`, 'the new project was not offered for the hire');
    await this.screen('hire: project');
    await this.click('input[name="workspace_id"]');
    await this.clickButton('Next', '.hire-actions button');
    await this.until(`Boolean(document.querySelector('input[name="engine"][value="codex"]'))`, 'the stand-in engine was not offered');
    await this.screen('hire: app');
    await this.click('input[name="engine"][value="codex"]');
    await this.clickButton('Next', '.hire-actions button');
    await this.until(`Boolean(document.querySelector('#hire-goal'))`, 'the first task was not asked for');
    await this.screen('hire: first task');
    await this.fill('#hire-goal', 'Write a note file');
    await this.click('.hire-submit');
    await this.until(`document.querySelector('[data-hire-outcome]')?.dataset.hireOutcome === 'complete'`, 'the hire did not complete');
    await this.screen('hire: done');
    await this.clickButton('Close');
    await this.until(`/Checks passed today\\s+1\\b/.test(document.body.innerText)`, 'the first task did not finish', 60_000);
    await this.screen('one agent');
  }

  async ticket() {
    await this.click('#view-board');
    await this.until(`Boolean(document.querySelector('.ticket-new-toggle'))`, 'the board offers no new ticket');
    await this.screen('board');
    await this.click('.ticket-new-toggle');
    await this.fill('input[name="title"]', 'Add a second note');
    await this.fill('textarea[name="note"]', 'Add one more line to the notes.');
    await this.screen('new ticket');
    await this.click('.ticket-create');
    await this.until(`Boolean(document.querySelector('.ticket-panel'))`, 'the ticket was not created');
    await this.screen('ticket to do');
    await this.clickButton('Give to agent', '.ticket-action');
    await this.until(`Boolean(document.querySelector('form[aria-label="Give to agent"]'))`, 'the give form did not open');
    await this.screen('give to agent');
    await this.click('form[aria-label="Give to agent"] button[type="submit"]');
    await this.until(`Boolean([...document.querySelectorAll('.ticket-action')].find((el) => el.textContent.trim() === 'Accept'))`,
      'the agent did not finish the ticket', 90_000);
    await this.screen('ticket ready for review');
    await this.clickButton('Accept', '.ticket-action');
    await this.until(`Boolean(document.querySelector('[data-ticket-column="done"] .ticket-card'))`, 'the accepted ticket is not in Done');
    await this.screen('ticket accepted');
    check((await stat(join(this.run.repo, 'journey-note.txt')).catch(() => null))?.isFile(), 'the agent left no change in the project');
  }

  async otherScreens() {
    for (const [view, name] of [['#view-setup', 'setup'], ['#view-settings', 'settings']]) {
      await this.click(view);
      await pause(500);
      await this.screen(name);
    }
  }

  async finish() {
    const unused = this.allowed.filter((entry) => !this.used.has(entry));
    const problems = [
      ...this.findings.map((line) => `terminal command shown on ${line}`),
      ...unused.map((entry) => `the allowed exit "${entry.name}" no longer shows on ${entry.screen}: remove it from ALLOWED_EXITS`),
    ];
    console.log(`journey screens (${this.screens.length}): ${this.screens.join(', ')}`);
    console.log(`journey pictures: ${this.pictures}`);
    check(problems.length === 0, problems.join('; '));
  }

  /** The whole path from a fresh home to an accepted ticket. */
  async walk() {
    await this.prepare();
    await this.start();
    await this.addProject();
    await this.hire();
    await this.ticket();
    await this.otherScreens();
    await this.finish();
  }
}

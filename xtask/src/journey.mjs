import { createHash } from 'node:crypto';
import { chmod, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { oldProjectWords } from '../../floor/tests/helpers/project-word.mjs';
import { findTerminalCommands } from '../../floor/tests/helpers/terminal-commands.mjs';

const pause = (ms) => new Promise((done) => setTimeout(done, ms));

/**
 * Screens that still send a person to a terminal. Each entry names the exit, the screen that
 * shows it and the text it shows, so the run still proves every other screen. Remove an entry
 * when the screen stops showing the command: an entry that matches nothing fails the run.
 */
export const ALLOWED_EXITS = [
  { name: 'Settings shows the reload command for a project', screen: 'settings', text: /^dudes project reload/ },
  { name: 'Settings sends a restart to the terminal', screen: 'settings', text: /^dudes quickstart --restart/ },
  { name: 'Settings sends the update check to the terminal', screen: 'settings', text: /^dudes upgrade/ },
  { name: 'The signed-out page shows the sign-in command', screen: 'signed out', text: /^dudes open/ },
];

// Stands in for the engine: it edits a file in the project, like an agent doing a task, and ends
// its turn. It needs no account.
const ENGINE = `#!/bin/sh
case "$*" in
  '--version') printf 'journey stand-in\\n'; exit 0 ;;
  'auth status'|'login status') exit 0 ;;
esac
task="$*"
case "$task" in
  *'Summarize our conversation.'*) reply='stand-in chat reply' ;;
  *) reply='stand-in reply' ;;
esac
printf 'written by the stand-in engine\\n' >> journey-note.txt
printf '%s\\n' '{"type":"thread.started","thread_id":"11111111-1111-4111-8111-111111111111"}'
printf '%s\\n' '{"type":"turn.started"}'
printf '%s\\n' "{\\"type\\":\\"item.completed\\",\\"item\\":{\\"id\\":\\"item_0\\",\\"type\\":\\"agent_message\\",\\"text\\":\\"$reply\\"}}"
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
    this.oldWords = [];
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

  /** Type into the New ticket note: the rich editor when it loaded, its plain text box when it did not. */
  async typeNote(value) {
    const found = `[...document.querySelector('[data-ticket-note]')?.getElementsByTagName('*') ?? []]
      .find((el) => el.tagName === 'TEXTAREA' || el.isContentEditable && el.getAttribute('role') === 'textbox')`;
    await this.until(`Boolean(${found})`, 'the note field did not load');
    if (await this.evaluate(`${found}.tagName === 'TEXTAREA'`)) {
      await this.evaluate(`(() => { const el = ${found};
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(el, ${JSON.stringify(value)});
        el.dispatchEvent(new Event('input', { bubbles: true })); })()`);
    } else {
      await this.evaluate(`${found}.focus()`);
      await this.run.cdp.send('Input.insertText', { text: value }, this.run.cdp.session);
    }
    await this.until(`(${found}.value ?? ${found}.textContent) === ${JSON.stringify(value)}`, 'the note was not typed');
  }

  /** Look at the screen now: save a picture and record any terminal command it shows. */
  async screen(name) {
    const text = await this.evaluate(`document.body.innerText + '\\n' + [...document.querySelectorAll('code')].map((el) => el.textContent).join('\\n')`);
    const shot = await this.run.cdp.send('Page.captureScreenshot', { format: 'png' }, this.run.cdp.session);
    const file = join(this.pictures, `${String(this.screens.length + 1).padStart(2, '0')}-${name.replace(/[^a-z0-9]+/gi, '-').toLowerCase()}.png`);
    await writeFile(file, Buffer.from(shot.data, 'base64'));
    this.screens.push(name);
    this.lastPicture = file;
    for (const context of oldProjectWords(text)) this.oldWords.push(`${name}: ${context}`);
    const seen = new Set();
    for (const found of findTerminalCommands(text)) {
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

  /** Start Dudes as a newcomer does: quickstart opens the Floor. Nothing else is typed after this. */
  async start() {
    const { run } = this;
    const { stdout: started } = await run.cli(['quickstart', '--set', 'providers.codex', 'on']);
    await run.rememberManaged();
    if (!run.cdp) await run.launchChrome();
    await run.cdp.send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false }, run.cdp.session);
    let link;
    const end = Date.now() + 15_000;
    while (!link && Date.now() < end) {
      link = (await readFile(this.linkFile, 'utf8').catch(() => '')).trim() || undefined;
      if (!link) await pause(100);
    }
    check(link, `quickstart did not hand a link to the opener: ${started.replace(/#bootstrap=\S+/g, '#bootstrap=...').trim()}`);
    await rm(this.linkFile, { force: true });
    await run.openFloor(link);
  }

  async addProject() {
    await this.until(`Boolean(document.querySelector('input[name="folder"]'))`, 'the first screen offers no folder box');
    await this.screen('first visit');
    await this.fill('input[name="folder"]', this.run.repo);
    await this.clickButton('Look at folder');
    await this.until(`Boolean(document.querySelector('[data-add-project="submit"]'))`, 'the folder was not offered for adding');
    await this.screen('folder found');
    await this.click('[data-add-project="submit"]');
    await this.until(`Boolean(document.querySelector('.first-task-start'))`, 'the project was not added');
    await this.screen('project added');
  }

  async hire() {
    await this.click('.first-task-start');
    await this.until(`Boolean(document.querySelector('#hire-name'))`, 'the hire dialog did not open');
    await this.screen('hire: who');
    await this.fill('#hire-name', 'Journey agent');
    await this.fill('#hire-role', 'Journey');
    await this.clickButton('Next', '[data-hire-action="next"]');
    await this.until(`Boolean(document.querySelector('input[name="workspace_id"]'))`, 'the new project was not offered for the hire');
    await this.screen('hire: project');
    await this.click('input[name="workspace_id"]');
    await this.clickButton('Next', '[data-hire-action="next"]');
    await this.until(`Boolean(document.querySelector('input[name="engine"][value="codex"]'))`, 'the stand-in engine was not offered');
    await this.screen('hire: app');
    await this.click('input[name="engine"][value="codex"]');
    await this.click('[data-hire-action="submit"]');
    await this.until(`!document.querySelector('.hire-dialog')`, 'the hire dialog did not close');
    await this.until(`Boolean(document.querySelector('[data-first-task="task"] .first-task-open'))`,
      'the first-task card did not offer to give the new agent a task');
    await this.screen('hire: done');
    await this.click('[data-first-task="task"] .first-task-open');
    await this.until(`Boolean(document.querySelector('[data-first-task="task"] form textarea'))`, 'the first-task form did not open');
    await this.screen('first task: form');
    await this.fill('[data-first-task="task"] form textarea', 'Write a note file');
    await this.click('[data-first-task="task"] .first-task-start');
    await this.click('#view-board');
    await this.until(`Boolean(document.querySelector('[data-ticket-column="ready"] .ticket-card'))`,
      'the first task did not reach Ready for you', 60_000);
    await this.screen('first task ready for review');
    await this.click('[data-ticket-column="ready"] .ticket-card');
    await this.until(`Boolean([...document.querySelectorAll('.ticket-action')].find((el) => el.textContent.trim() === 'Accept'))`,
      'the first task offers no Accept');
    await this.clickButton('Accept', '.ticket-action');
    await this.until(`Boolean(document.querySelector('[data-ticket-column="done"] .ticket-card'))`, 'the first task is not in Done');
    await this.screen('first task accepted');
  }

  async ticket() {
    // The first task's review already opened the board, and the button toggles it.
    if (!await this.evaluate(`Boolean(document.querySelector('.ticket-new-toggle'))`)) await this.click('#view-board');
    await this.until(`Boolean(document.querySelector('.ticket-new-toggle'))`, 'the board offers no new ticket');
    await this.screen('board');
    await this.click('.ticket-new-toggle');
    await this.fill('input[name="title"]', 'Add a second note');
    await this.typeNote('Add one more line to the notes.');
    await this.screen('new ticket');
    await this.createFromNote();
    await this.until(`Boolean(document.querySelector('.ticket-panel'))`, 'the ticket was not created');
    await this.until(`document.querySelector('.ticket-panel')?.innerText.includes('Add one more line to the notes.')`, 'the ticket does not show its note');
    await this.screen('ticket to do');
    await this.clickButton('Give to agent', '.ticket-action');
    await this.until(`Boolean(document.querySelector('form[aria-label="Give to agent"]'))`, 'the give form did not open');
    await this.screen('give to agent');
    await this.click('form[aria-label="Give to agent"] button[type="submit"]');
    await this.until(`Boolean([...document.querySelectorAll('.ticket-action')].find((el) => el.textContent.trim() === 'Accept'))`,
      'the agent did not finish the ticket', 90_000);
    await this.screen('ticket ready for review');
    await this.until(`document.querySelector('.ticket-summary')?.textContent === 'stand-in reply'`,
      'the ticket does not show its result');
    const card = await this.evaluate(`document.querySelector('.ticket-panel').dataset.selectedCardId`);
    await this.click('.ticket-dialog-close');
    await this.click('#view-board');
    await this.click('[aria-label="Inspect Journey agent"]');
    await this.clickButton('Chat', '[role="tab"]');
    await this.fill('#chat-text', 'Summarize our conversation.');
    await this.click('.chat-send');
    await this.until(`Boolean([...document.querySelectorAll('[data-chat-turn-state="ended"] .chat-final-reply')]
      .find((el) => el.textContent === 'stand-in chat reply'))`, 'the chat did not finish');
    await this.click('#selection-clear');
    await this.click('#view-board');
    await this.click('[data-ticket-column="ready"] .ticket-card');
    await this.until(`document.querySelector('.ticket-panel')?.dataset.selectedCardId === ${JSON.stringify(card)}
      && document.querySelector('.ticket-summary')?.textContent === 'stand-in reply'`,
    'the later chat replaced the ticket result');
    await this.screen('ticket result after chat');
    await this.clickButton('Accept', '.ticket-action');
    await this.until(`Boolean(document.querySelector('[data-ticket-column="done"] .ticket-card'))`, 'the accepted ticket is not in Done');
    await this.screen('ticket accepted');
    check((await stat(join(this.run.repo, 'journey-note.txt')).catch(() => null))?.isFile(), 'the agent left no change in the project');
  }

  /** Ctrl+Enter pressed in the note creates the ticket, and the editor does not take it for a line break. */
  async createFromNote() {
    const inNote = `Boolean(document.activeElement?.closest('[data-ticket-note]'))`;
    await this.until(inNote, 'the note does not have focus');
    for (const type of ['keyDown', 'keyUp']) {
      await this.run.cdp.send('Input.dispatchKeyEvent', { type, key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, modifiers: 2 }, this.run.cdp.session);
    }
  }

  /** An unassigned ticket, so the New ticket dialog has an open ticket to find as similar to what is typed. */
  async ensureOpenTicket() {
    if (this.openTicket) return;
    await this.layoutOffice();
    await this.click('#view-board');
    await this.until(`document.querySelector('.control-panel')?.dataset.view === 'board'`, 'the board did not open for the dialog check');
    await this.click('.ticket-new-toggle');
    await this.fill('input[name="title"]', 'Write the changelog for the notes');
    await this.click('.ticket-create');
    await this.until(`Boolean(document.querySelector('.ticket-panel'))`, 'the open ticket was not created');
    await this.click('.ticket-dialog-close');
    this.openTicket = true;
  }

  /** The New ticket dialog with a title that matches the open ticket, so its similar-ticket hint and outcome line show. */
  async fillNewTicket() {
    await this.click('.ticket-new-toggle');
    await this.until(`document.querySelector('.ticket-dialog')?.open === true && document.querySelector('.ticket-dialog')?.dataset.composer === 'true'`,
      'the New ticket dialog did not open');
    await this.fill('input[name="title"]', 'Update the changelog for notes');
    await this.until(`Boolean(document.querySelector('.ticket-similar')) && Boolean(document.querySelector('.ticket-outcome'))`,
      'the dialog shows no similar ticket or outcome line');
  }

  async leaveNewTicket() {
    await this.click('.ticket-dialog-close');
    await this.until(`document.querySelector('.ticket-dialog')?.open !== true`, 'the New ticket dialog did not close');
  }

  /** Pictures of the dialog empty, with the agent list open and a similar ticket shown, and with Create more on. */
  async dialogPictures() {
    await this.ensureOpenTicket();
    const saved = await this.evaluate(`({ width: innerWidth, height: innerHeight })`);
    const hashes = new Set();
    const hashOf = async () => createHash('sha256').update(await readFile(this.lastPicture)).digest('hex');
    try {
      for (const [width, height] of [[1440, 900], [390, 844]]) {
        await this.run.cdp.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false }, this.run.cdp.session);
        await this.click('.ticket-new-toggle');
        await this.until(`document.querySelector('.ticket-dialog')?.open === true && document.querySelector('.ticket-dialog')?.dataset.composer === 'true'`,
          'the New ticket dialog did not open');
        await this.screen(`new ticket dialog empty ${width}`);
        const batch = [await hashOf()];
        await this.fill('input[name="title"]', 'Update the changelog for notes');
        await this.until(`Boolean(document.querySelector('.ticket-similar')) && Boolean(document.querySelector('.ticket-outcome'))`,
          'the dialog shows no similar ticket or outcome line');
        await this.click('[data-field="agent"]');
        await this.until(`document.querySelector('[data-field="agent"]')?.getAttribute('aria-expanded') === 'true'`, 'the Agent list did not open');
        check(await this.evaluate(`document.getElementById(document.querySelector('[data-field="agent"]').getAttribute('aria-controls')).textContent.includes('Suggested')`),
          'the Agent list suggests no agent');
        await this.until(`document.getAnimations().every((animation) => animation.playState !== 'running')`, 'the Agent list kept animating');
        check(await this.evaluate(`(() => { const list = document.getElementById(document.querySelector('[data-field="agent"]').getAttribute('aria-controls')).getBoundingClientRect();
          return list.left >= 0 && list.right <= innerWidth - 8; })()`), `the Agent list runs past the window at ${width}`);
        check(await this.evaluate(`[...document.querySelectorAll('.ticket-chip span.truncate')].every((label) => label.scrollWidth <= label.clientWidth)`) || width < 1000,
          `a chip label is cut off at ${width}`);
        await this.screen(`new ticket dialog suggestion and similar ${width}`);
        batch.push(await hashOf());
        await this.click('[data-field="agent"]');
        await this.click('.ticket-create-more [role="switch"]');
        await this.until(`document.querySelector('.ticket-create-more [role="switch"]')?.getAttribute('aria-checked') === 'true'`, 'Create more did not turn on');
        await this.screen(`new ticket dialog create more ${width}`);
        batch.push(await hashOf());
        await this.click('.ticket-create-more [role="switch"]');
        await this.leaveNewTicket();
        check(new Set(batch).size === 3, `the dialog pictures at ${width} are not three different pictures`);
        batch.forEach((hash) => hashes.add(hash));
      }
      console.log(`PASS dialog pictures: ${hashes.size} different pictures`);
    } finally {
      await this.run.cdp.send('Emulation.setDeviceMetricsOverride', { width: saved.width, height: saved.height, deviceScaleFactor: 1, mobile: false }, this.run.cdp.session);
    }
  }

  async layoutOffice() {
    const view = await this.evaluate(`document.querySelector('.control-panel')?.dataset.view`);
    if (view === 'board') await this.click('#view-board');
    else if (view) await this.click('#office-open');
    await this.until(`location.hash === '' && !!document.querySelector('.office-canvas') && !document.querySelector('.control-panel')`,
      'Office did not open for the viewport check');
  }

  async viewportHeights(crewSize) {
    const saved = await this.evaluate(`({ width: innerWidth, height: innerHeight,
      view: document.querySelector('.control-panel')?.dataset.view ?? 'office' })`);
    const views = [
      { name: 'office', selector: '#office-open', ready: `location.hash === '' && !!document.querySelector('.office-canvas') && !document.querySelector('.control-panel')` },
      { name: 'board', selector: '#view-board', ready: `document.querySelector('.control-panel')?.dataset.view === 'board'` },
      { name: 'new ticket dialog', selector: '#view-board', ready: `document.querySelector('.control-panel')?.dataset.view === 'board'`,
        open: () => this.fillNewTicket(), leave: () => this.leaveNewTicket() },
      { name: 'projects', selector: '#view-projects', ready: `location.hash === '#/projects' && document.querySelector('.control-panel[data-page]')?.dataset.view === 'projects'` },
      { name: 'log', selector: '#view-log', ready: `location.hash === '#/log' && document.querySelector('.control-panel[data-page]')?.dataset.view === 'log'` },
      { name: 'setup', selector: '#view-setup', ready: `location.hash === '#/setup' && document.querySelector('.control-panel[data-page]')?.dataset.view === 'setup'` },
      { name: 'settings', selector: '#view-settings', ready: `location.hash === '#/settings' && document.querySelector('.control-panel[data-page]')?.dataset.view === 'settings'` },
    ];
    const sizes = [[1440, 900], [800, 500], [801, 500], [390, 844]];
    await this.ensureOpenTicket();
    try {
      for (const [width, height] of sizes) {
        await this.run.cdp.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false }, this.run.cdp.session);
        for (const view of views) {
          await this.layoutOffice();
          if (view.name !== 'office') await this.click(view.selector);
          await this.until(view.ready, `${view.name} did not open for the viewport check`);
          await view.open?.();
          const actual = await this.evaluate(`(async () => {
            await document.fonts.ready;
            await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
            return { scrollHeight: document.documentElement.scrollHeight, windowWidth: innerWidth, windowHeight: innerHeight };
          })()`);
          check(actual.scrollHeight === actual.windowHeight && actual.windowWidth === width && actual.windowHeight === height,
            `viewport ${width}x${height} ${view.name} crew=${crewSize}: scrollHeight=${actual.scrollHeight}, innerWidth=${actual.windowWidth}, innerHeight=${actual.windowHeight}`);
          await view.leave?.();
        }
      }
      console.log(`PASS viewport heights: ${sizes.length * views.length} views, crew=${crewSize}`);
    } finally {
      await this.run.cdp.send('Emulation.setDeviceMetricsOverride', { width: saved.width, height: saved.height, deviceScaleFactor: 1, mobile: false }, this.run.cdp.session);
      await this.layoutOffice();
      const restore = views.find((view) => view.name === saved.view);
      check(restore, 'the viewport check cannot restore this view');
      if (restore.name !== 'office') await this.click(restore.selector);
      await this.until(restore.ready, 'the view did not restore after the viewport check');
    }
  }

  async crowdedViewportHeights() {
    for (let index = 2; index <= 16; index += 1) {
      await this.run.cli(['hire', '--name', `Layout agent ${index}`, '--role', 'Layout check',
        '--department', 'Checks', '--provider', 'codex', '--cwd', this.run.repo]);
    }
    await this.layoutOffice();
    await this.until(`document.querySelectorAll('.crew-row[data-agent-id]').length === 16`, 'the layout crew did not reach the Floor');
    await this.viewportHeights(16);
  }

  async otherScreens() {
    for (const name of ['projects', 'log', 'setup', 'settings']) {
      await this.click(`#view-${name}`);
      await this.until(`location.hash === '#/${name}' && document.querySelector('.control-panel[data-page]')?.dataset.view === '${name}'
        && !document.querySelector('.office-canvas')`, `${name} did not open as a page`);
      await this.screen(name);
    }
    const loaded = await this.evaluate('performance.timeOrigin');
    await this.run.cdp.send('Page.reload', {}, this.run.cdp.session);
    await this.until(`performance.timeOrigin !== ${loaded} && Boolean(document.querySelector('button[aria-label$="(this browser)"]'))`, 'Settings does not list this browser');
    check(await this.evaluate(`location.hash === '#/settings'`), 'reload did not keep the Settings address');
    await this.click('#office-open');
    await this.until(`location.hash === '' && Boolean(document.querySelector('.office-canvas'))`, 'Office did not return to the office');
    await this.click('#view-log');
    await this.until(`location.hash === '#/log'`, 'Log did not set its address');
    await this.evaluate('history.back()');
    await this.until(`location.hash === '' && Boolean(document.querySelector('.office-canvas'))`, 'Back did not return to the office');
    await this.click('#view-settings');
    await this.until(`Boolean(document.querySelector('button[aria-label$="(this browser)"]'))`, 'Settings did not reopen');
    await this.screen('settings');
  }

  /** The last step: Sign out from this browser's row signs the page out, and the page says why. */
  async signOut() {
    await this.until(`(() => { const button = document.querySelector('button[aria-label$="(this browser)"]');
      if (!button) return false; button.click(); return true; })()`,
    'this browser has no Sign out button in the list');
    await this.until(`document.body.innerText.includes('You signed out of this Floor in this browser')`,
      'the page did not say that this browser signed out');
    await this.screen('signed out');
  }

  async finish() {
    const unused = this.allowed.filter((entry) => !this.used.has(entry));
    const problems = [
      ...this.findings.map((line) => `terminal command shown on ${line}`),
      ...this.oldWords.map((line) => `"workspace" shown instead of "project" on ${line}`),
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
    await this.dialogPictures();
    await this.viewportHeights(1);
    await this.crowdedViewportHeights();
    await this.otherScreens();
    await this.signOut();
    await this.finish();
  }
}

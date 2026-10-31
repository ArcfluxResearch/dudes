import { mkdtemp, readFile, writeFile, chmod } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Journey } from './journey.mjs';

const ENGINE = fileURLToPath(new URL('./ask-engine.sh', import.meta.url));

function check(value, message) {
  if (!value) throw new Error(message);
  return value;
}

/**
 * Two chat-only agents on a stand-in Claude. The operator asks the first in its chat to ask the
 * second a question. Each step the loop needs is awaited on the page, so a failure names the step
 * that did not happen. `run` is a prepared `ScratchRun` whose daemon is not yet running.
 */
export async function askATeammate(run) {
  const started = Date.now();
  const page = new Journey(run);
  await writeFile(join(run.root, 'engines/claude'), await readFile(ENGINE), { mode: 0o700 });
  await chmod(join(run.root, 'engines/claude'), 0o700);
  await run.command('sh', ['-c', 'printf "a project\\n" > README.md && git add README.md && git -c user.name=smoke -c user.email=smoke@example.invalid commit -qm start']);
  await run.cli(['quickstart', '--set', 'floor.hire', 'on',
    '--enroll', '--check-label', 'smoke completion', '--', '/usr/bin/false']);
  await run.rememberManaged();
  const { stdout } = await run.cli(['floor', 'open']);
  const link = check(stdout.match(/http:\/\/127\.0\.0\.1:\d+\/floor\/#bootstrap=[A-Za-z0-9_-]+/)?.[0], 'Floor open returned no bootstrap link');
  await run.cdp.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false }, run.cdp.session);
  await run.openFloor(link);

  const ids = {};
  for (const [name, role] of [['Ada', 'Asker'], ['Bruno', 'Answerer']]) {
    await run.cli(['hire', '--name', name, '--role', role, '--department', 'Checks', '--provider', 'claude', '--cwd', run.repo]);
  }
  const agents = JSON.parse((await run.cli(['--json', 'agents'])).stdout);
  for (const name of ['Ada', 'Bruno']) ids[name] = check(agents.find((agent) => agent.name === name)?.id, `${name} was not hired`);

  const pictures = await mkdtemp(join(tmpdir(), 'dudes-ask-pictures-'));
  const chatOf = async (name) => {
    await page.click(`.crew-row[data-agent-id="${ids[name]}"]`, `${name} is not on the Floor`);
    await page.until(`Boolean(document.querySelector('#agent-detail-tab-chat'))`, `${name} offers no Chat`);
    await page.click('#agent-detail-tab-chat');
    await page.until(`document.querySelector('.chat-panel')?.dataset.chatAgentId === ${JSON.stringify(ids[name])}`, `Chat did not open for ${name}`);
  };
  const picture = async (name) => {
    const heights = await page.evaluate(`({ scroll: document.documentElement.scrollHeight, window: innerHeight })`);
    check(heights.scroll === heights.window, `the page is taller than the window with ${name}'s chat open: ${heights.scroll} > ${heights.window}`);
    const shot = await run.cdp.send('Page.captureScreenshot', { format: 'png' }, run.cdp.session);
    await writeFile(join(pictures, `${name.toLowerCase()}-chat.png`), Buffer.from(shot.data, 'base64'));
  };
  const text = (selector) => page.evaluate(`document.querySelector(${JSON.stringify(selector)})?.textContent ?? ''`);

  await chatOf('Ada');
  await page.fill('#chat-text', 'Ask Bruno what 2+2 is');
  await page.click('.chat-send');
  const wait = 60_000;
  await page.until(`document.querySelector('.chat-final-reply')?.textContent === 'I asked Bruno.'`,
    'Ada did not run the mail command to ask Bruno', wait);
  await page.until(`document.querySelector('[data-asked-request]')?.dataset.askedStatus === 'answered'`,
    'the request to Bruno never showed as Answered in Ada\'s chat: Bruno was not woken or did not reply', wait);
  await page.until(`document.querySelector('.chat-exchange [data-chat-mail-cause]') !== null`,
    'Ada got no turn for Bruno\'s answer', wait);
  await page.until(`[...document.querySelectorAll('.chat-exchange')].some((el) => el.querySelector('[data-chat-mail-cause]')
    && el.querySelector('.chat-final-reply')?.textContent === 'Bruno says: The answer is 4')`,
  'Ada did not report Bruno\'s answer in her chat', wait);
  const row = await text('.asked-row');
  check(row === 'Ada asked Bruno a questionAnswered', `the request row does not name who asked whom: ${JSON.stringify(row)}`);
  await picture('Ada');

  await chatOf('Bruno');
  await page.until(`[...document.querySelectorAll('.chat-exchange')].some((el) => el.querySelector('[data-chat-mail-cause]')?.textContent.startsWith('After mail from Ada')
    && el.querySelector('.chat-final-reply')?.textContent === 'I answered the request.')`,
  'Bruno\'s chat does not show the mail-started answer', wait);
  await picture('Bruno');
  console.log(`ask a teammate: ${Date.now() - started} ms, pictures in ${pictures}`);
}

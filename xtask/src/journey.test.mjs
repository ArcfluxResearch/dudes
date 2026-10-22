import assert from 'node:assert/strict';
import { mkdtemp, readdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { Journey } from './journey.mjs';

const PNG = Buffer.from('not really a picture').toString('base64');

// A page that shows `screen` as its text and one copy-this-command row per entry in `blocks`.
async function look(allowed, name, text, blocks = []) {
  const run = { cdp: {
    session: 's',
    evaluate: async () => ({ text, codeBlocks: blocks }),
    send: async () => ({ data: PNG }),
  } };
  const journey = new Journey(run, allowed);
  journey.pictures = await mkdtemp(join(tmpdir(), 'journey-test-'));
  try {
    await journey.screen(name);
    assert.equal((await readdir(journey.pictures)).length, 1, 'one picture per screen');
  } finally {
    await rm(journey.pictures, { recursive: true, force: true });
  }
  return journey;
}

test('a screen that shows a command, a prompt or a copy block fails the journey and names the screen', async () => {
  const journey = await look([], 'board', 'Tickets are off. Run dudes config set floor.board on, then restart.\n$ dudes stop', ['anything']);
  assert.deepEqual(journey.findings, [
    'board: dudes command: dudes config set floor.board on, then restart.',
    'board: dudes command: dudes stop',
    'board: shell prompt: $ dudes stop',
    'board: copy-this-command block: anything',
  ]);
  await assert.rejects(journey.finish(), /terminal command shown on board: dudes command: dudes config set floor\.board on/);
});

test('a screen with plain words passes, and an allowed exit must still be shown', async () => {
  const clean = await look([], 'hire: who', 'Open Dudes and hire a dude. Name and look.');
  assert.deepEqual(clean.findings, []);
  await clean.finish();

  const exit = { name: 'update check', screen: 'settings', text: /^dudes upgrade/ };
  const allowed = await look([exit], 'settings', 'Run dudes upgrade --check to look.');
  assert.deepEqual(allowed.findings, []);
  await allowed.finish();

  const elsewhere = await look([exit], 'board', 'Run dudes upgrade --check to look.');
  assert.equal(elsewhere.findings.length, 1, 'an exit is allowed only on its own screen');
  await assert.rejects(look([exit], 'settings', 'Nothing to run here.').then((journey) => journey.finish()),
    /the allowed exit "update check" no longer shows on settings/);
});

test('a screen that says workspace instead of project fails the journey and names the screen', async () => {
  const journey = await look([], 'hire: where', 'Which workspace? Pick one.');
  assert.deepEqual(journey.oldWords, ['hire: where: Which workspace? Pick one.']);
  await assert.rejects(journey.finish(), /"workspace" shown instead of "project" on hire: where/);
});

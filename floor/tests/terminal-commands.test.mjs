// SPDX-License-Identifier: MIT OR Apache-2.0
//
// How many places in the page can tell a person to use a terminal. The number only goes down:
// remove a place and lower the pin in the same change, add one and this fails.
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import test from 'node:test';
import {
  DUDES_COMMANDS, dudesCommandsIn, findTerminalCommands, showsTerminalCommand, terminalCommandsOnScreen,
} from './helpers/terminal-commands.mjs';

/** Source lines in `src/renderer` that can show a terminal command. Lower it when you remove one. */
const PLACES = 55;
/** The different `dudes ...` commands those lines name. Lower it when the last mention of one goes. */
const DISTINCT_DUDES_COMMANDS = 21;

const renderer = resolve(import.meta.dirname, '../src/renderer');

function sources(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => entry.isDirectory()
    ? sources(join(dir, entry.name))
    : /\.tsx?$/.test(entry.name) ? [join(dir, entry.name)] : []);
}

function census() {
  const places = [];
  const commands = new Set();
  for (const file of sources(renderer)) {
    readFileSync(file, 'utf8').split('\n').forEach((line, index) => {
      if (!showsTerminalCommand(line)) return;
      places.push(`${file.slice(renderer.length + 1)}:${index + 1}`);
      for (const command of dudesCommandsIn(line)) commands.add(command);
    });
  }
  return { places, commands: [...commands].sort() };
}

test('the places the page can show a terminal command only ever go down', () => {
  const { places, commands } = census();
  const listing = `\n${places.join('\n')}\n`;
  assert.ok(places.length <= PLACES, `The page can show a terminal command in ${places.length} places, up from ${PLACES}. `
    + `A new screen must not send a person to a terminal.${listing}`);
  assert.equal(places.length, PLACES, `The page now shows a terminal command in ${places.length} places. `
    + `Lower PLACES to ${places.length} so the count cannot creep back up.`);
  assert.ok(commands.length <= DISTINCT_DUDES_COMMANDS, `The page names ${commands.length} different dudes commands, up from ${DISTINCT_DUDES_COMMANDS}: ${commands.join(', ')}`);
  assert.equal(commands.length, DISTINCT_DUDES_COMMANDS, `The page now names ${commands.length} different dudes commands. `
    + `Lower DISTINCT_DUDES_COMMANDS to ${commands.length}: ${commands.join(', ')}`);
});

test('the command list covers every command the CLI prints in its usage', () => {
  const usage = readFileSync(resolve(import.meta.dirname, '../../bins/dudes/src/args.rs'), 'utf8');
  const block = usage.slice(usage.indexOf('\nCOMMANDS\n'), usage.indexOf('\nOPTIONS\n'));
  const names = new Set([...block.matchAll(/^ {4}([a-z][a-z-]*)(?: |$)/gm)].map((match) => match[1]));
  assert.ok(names.size > 20, 'the usage text lists the commands');
  assert.deepEqual([...names].filter((name) => !DUDES_COMMANDS.includes(name)), [], 'DUDES_COMMANDS misses a CLI command');
});

test('the patterns find a command, a prompt and a copy block, and leave ordinary words alone', () => {
  assert.deepEqual(findTerminalCommands('Run dudes config set floor.hire on first').map((f) => f.kind), ['dudes command']);
  assert.deepEqual(findTerminalCommands('Install it:\n$ npm install -g thing').map((f) => f.kind), ['package manager', 'shell prompt']);
  assert.deepEqual(findTerminalCommands('Open Dudes and hire a dude. Tickets run in a project, not in dudes.'), []);
  assert.deepEqual(dudesCommandsIn('dudes project add <path>, then dudes config to see it'), ['dudes project add', 'dudes config']);
  assert.equal(showsTerminalCommand('  // run dudes stop here'), false, 'a comment is never shown');
  assert.equal(showsTerminalCommand('<Command text={x} />'), true);
  assert.deepEqual(terminalCommandsOnScreen({ text: 'fine', codeBlocks: ['  anything  '] }),
    [{ kind: 'copy-this-command block', text: 'anything' }]);
});

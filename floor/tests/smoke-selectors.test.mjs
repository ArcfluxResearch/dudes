import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { driverSelectors, missingSelectors, renderedElements } from './helpers/smoke-selectors.mjs';

const root = new URL('../src/', import.meta.url);
const files = new Map(readdirSync(root, { recursive: true }).filter((path) => /\.tsx?$/.test(path))
  .map((path) => [fileURLToPath(new URL(path, root)), readFileSync(new URL(path, root), 'utf8')]));
const drivers = ['../../xtask/src/smoke.mjs', '../../xtask/src/journey.mjs'].map((path) => readFileSync(new URL(path, root), 'utf8'));

let elements;

test('every release smoke selector can match Floor markup', () => {
  const selectors = drivers.flatMap(driverSelectors);
  elements = renderedElements(files);
  assert.deepEqual(missingSelectors(selectors, elements), []);
});

test('a removed hook does not survive in a comment or on a different element', () => {
  const source = `function Screen() { return <form className="task"><input name="engine" value="codex" /><button id="start" /></form>; }`;
  const selectors = driverSelectors(`click('#start'); fill('input[name="engine"][value="codex"]', 'x');`);
  assert.deepEqual(missingSelectors(selectors, renderedElements(new Map([['screen.tsx', source]]))), []);
  const removed = source.replace('id="start"', 'id="renamed"') + '\n// <button id="start" />';
  assert.deepEqual(missingSelectors(selectors, renderedElements(new Map([['screen.tsx', removed]]))), ['#start']);
  const impossible = ['button[name="engine"]', 'input#start', '.task button input'];
  assert.deepEqual(missingSelectors(impossible, renderedElements(new Map([['screen.tsx', source]]))), impossible);
});

test('new driver selectors are checked and unsupported syntax fails closed', () => {
  const source = new Map([['screen.tsx', 'function Screen() { return <button id="present" />; }']]);
  const selectors = driverSelectors("this.until(`document.querySelector('#missing')`); this.click('#present');");
  assert.deepEqual(missingSelectors(selectors, renderedElements(source)), ['#missing']);
  assert.throws(() => missingSelectors(['button > input'], renderedElements(source)), /unsupported selector/);
  assert.throws(() => driverSelectors('click(unknownSelector);'), /unsupported driver selector/);
  assert.throws(() => driverSelectors('const views = [{selector: "#present"}]; click(unrelated.selector);'), /unsupported selector table/);
});

test('driver loops expand and runtime attributes keep their fixed prefix', () => {
  const selectors = driverSelectors('for (const name of ["log", "setup", "settings"]) { this.click(`#view-${name}`); } const selector = `.crew-row[data-agent-id="${this.agent}"] [data-live-activity]`; this.evaluate(`document.querySelector(${JSON.stringify(selector)})`);');
  assert.deepEqual(selectors.slice(0, 3), ['#view-log', '#view-setup', '#view-settings']);
  const source = new Map([['screen.tsx', 'function Screen() { return <div className="crew-row" data-agent-id={agent}><span data-live-activity="Working" /></div>; }']]);
  assert.deepEqual(missingSelectors(selectors.slice(3), renderedElements(source)), []);
  assert.deepEqual(missingSelectors(selectors.slice(3), renderedElements(new Map([['screen.tsx', source.get('screen.tsx').replace('crew-row', 'other-row')]]))), selectors.slice(3));
});

test('renamed release hooks are rejected', () => {
  const selectors = ['.hire-actions button', '.hire-submit', '.add-project-submit', '.device-row .device-this', '.primary', '#view-missing'];
  assert.deepEqual(missingSelectors(selectors, elements), selectors);
});

test('real forwarded IDs, compound attributes and descendant hooks cannot disappear', () => {
  const changes = [
    ['shell.tsx', 'id="hire-open"', 'id="hire-renamed"', '#hire-open'],
    ['renderer/onboarding/HireWizard.tsx', 'id="hire-name"', 'id="renamed-name"', '#hire-name'],
    ['renderer/onboarding/HireWizard.tsx', 'name="engine"', 'name="renamed-engine"', 'input[name="engine"][value="codex"]'],
    ['renderer/setup/FirstTask.tsx', 'first-task-open', 'renamed-open', '[data-first-task="task"] .first-task-open'],
    ['renderer/Control.tsx', "{ view: 'settings', label: 'Settings', refresh: true },", '', '#view-settings'],
    ['renderer/ticket/model.ts', "{ key: 'ready', title: 'Ready for you' },", '', '[data-ticket-column="ready"] .ticket-card'],
  ];
  const changed = new Map(files);
  for (const [suffix, before, after] of changes) {
    const path = [...files.keys()].find((path) => path.endsWith(suffix));
    assert.ok(files.get(path).includes(before));
    changed.set(path, changed.get(path).replaceAll(before, after) + `\n// ${before}`);
  }
  const selectors = changes.map((change) => change[3]);
  assert.deepEqual(missingSelectors(selectors, renderedElements(changed)), selectors);
});

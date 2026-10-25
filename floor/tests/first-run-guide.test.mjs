// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { applyServerFrame, beginFloorEpoch, getFloorState, noteStale, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';
import { guideSteps } from '../src/renderer/setup/guide.ts';
import { readGuideClosed, rememberGuideClosed } from '../src/renderer/view.ts';

const ID = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const SETTINGS = {
  folder_picker: false, switches: { answer: false, control: false, recovery: false, goal: false, retire: false, terminal: false, hire: false, git: false, renew: false, board: false, diff: false, project: false },
  budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '0', per_turn_tokens: '0' },
  providers: ['claude'], workspaces: [], about: { daemon_version: '0.12.3', browser_wire_version: 8 },
};
const WORKSPACE = { workspace_id: 'repo', label: '<Parser>', agent_usd_per_day_micros: '1000000',
  agent_tokens_per_day: null, goal_usd_micros: '1000000', goal_tokens: null };
const CATALOG = { floor_usd_per_day_micros: null, workspaces: [{ workspace_id: 'repo', label: 'Parser',
  engines: ['claude'], goal_usd_micros: null,
  agent_usd_per_day_micros: null, agent_tokens_per_day: null, goal_tokens: null, hop_cap: 2 }] };

function snapshot({ settings = SETTINGS, found = false, onboarding = null, agents = [] } = {}) {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn: 1, agents }, settings,
    daemon: { providers: [{ provider: 'claude', program: 'claude', found }] } };
  if (onboarding !== null) frame.onboarding = onboarding;
  applyServerFrame(JSON.stringify(frame), epoch);
  return epoch;
}
const status = (key, actionsSeen = false) =>
  guideSteps(getFloorState(), actionsSeen).find((step) => step.key === key)?.status;
const done = (key) => status(key) === 'Complete';
const indexStatus = (html, label) => html.match(new RegExp(`>${label}<span>([^<]+)</span>`))?.[1];
let server;
let FirstRunGuideView;
let SettingsView;
before(async () => {
  const root = resolve(import.meta.dirname, '..');
  server = await createServer({ root, configFile: false, cacheDir: resolve(root, 'node_modules/.vite-guide'),
    define: { __FLOOR_BUILD_VERSION__: '"0.12.3"' },
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ FirstRunGuideView } = await server.ssrLoadModule('/src/renderer/setup/FirstRunGuide.tsx'));
  ({ SettingsView } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => resetFloorStore());
const view = (step) => renderToStaticMarkup(createElement(FirstRunGuideView, { floor: getFloorState(), step,
  onStep() {}, onClose() {} }));

test('Welcome waits for a confirmed snapshot', () => {
  assert.equal(done('welcome'), false);
  snapshot();
  assert.equal(done('welcome'), true);
  assert.match(view('welcome'), /Dudes supervises coding agents/);
  noteTransport('close');
  assert.equal(done('welcome'), false);
});

test('Engines is done only when Setup finds a hireable program and offers install text, Copy and Recheck', () => {
  snapshot();
  assert.equal(done('engines'), false);
  assert.match(view('engines'), /npm install -g @anthropic-ai\/claude-code/);
  assert.match(view('engines'), />Copy<\/button>/);
  assert.match(view('engines'), />Recheck<\/button>/);
  snapshot({ found: true });
  assert.equal(done('engines'), true);
  assert.match(view('engines'), /Dudes found it at/);
  assert.match(view('engines'), /complete when Dudes finds an app\. Sign in when you hire an agent\./);
  assert.match(view('engines'), /Hire offers Claude, plus Codex and Pi when they are enabled in Settings/);
  for (const [provider, ready] of [['codex', true], ['pi', true], ['grok', false]]) {
    const epoch = beginFloorEpoch();
    noteTransport('open');
    applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [] },
      daemon: { providers: [{ provider, found: true }] } }), epoch);
    assert.equal(done('engines'), ready, `${provider} found`);
  }
});

test('Workspaces changes from not done to done on the enrollment resync without a page reload', () => {
  const epoch = snapshot();
  assert.equal(done('workspaces'), false);
  assert.match(view('workspaces'), /No projects yet/);
  assert.match(view('workspaces'), /Adding a project from this page is off/);
  assert.doesNotMatch(view('workspaces'), /dudes project add/);
  applyServerFrame('{"type":"floor.resync"}', epoch);
  assert.equal(done('workspaces'), false);
  snapshot({ settings: { ...SETTINGS, workspaces: [WORKSPACE] } });
  assert.equal(done('workspaces'), true);
  assert.match(view('workspaces'), /&lt;Parser&gt;/);
  assert.doesNotMatch(view('workspaces'), /<Parser>/);
  snapshot();
  assert.equal(done('workspaces'), false);
});

test('Floor actions uses the Settings switches instead of config commands', () => {
  snapshot();
  const html = view('actions');
  for (const label of ['Hire agents', 'Pause, resume and rename', 'Attach terminals', 'Add and remove projects']) {
    assert.match(html, new RegExp(`<dt>${label}</dt><dd><span class="settings-says">[^<]*</span> Off[^]*?<button type="button">Turn on</button>`));
  }
  assert.doesNotMatch(html, /dudes config|quickstart|>Copy</);
  assert.doesNotMatch(html, /First hire|Open Hire wizard/);
  snapshot({ settings: { ...SETTINGS, switches: { ...SETTINGS.switches, control: true, terminal: true, hire: true } } });
  assert.match(view('actions'), /<dt>Hire agents<\/dt><dd><span class="settings-says">[^<]*<\/span> On <button type="button">Turn off<\/button>/);
});

test('ACTIONS_GATED_ON_SWITCHES mutant: Floor actions is Optional until seen, never Not done, whatever is on', () => {
  for (const control of [false, true]) {
    for (const terminal of [false, true]) {
      for (const onboarding of [null, CATALOG]) {
        snapshot({ settings: { ...SETTINGS, switches: { ...SETTINGS.switches, control, terminal, hire: true } }, onboarding });
        const at = `control ${control}, terminal ${terminal}, Hire ${onboarding === null ? 'unavailable' : 'available'}`;
        assert.equal(indexStatus(view('welcome'), 'Floor actions'), 'Optional', at);
        assert.equal(indexStatus(view('actions'), 'Floor actions'), 'Complete', at);
        assert.match(view('actions'), /<p class="guide-status" role="status">Complete<\/p>/, at);
        assert.equal(status('actions'), 'Optional', at);
        assert.equal(status('actions', true), 'Complete', at);
      }
    }
  }
});

test('ACTIONS_SEEN_UNREPORTED mutant: Floor actions is not seen until the switches are shown on a confirmed snapshot', () => {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [] } }), epoch);
  const unreported = view('actions');
  assert.match(unreported, /Settings are unavailable/);
  assert.equal(indexStatus(unreported, 'Floor actions'), 'Optional');
  snapshot();
  noteStale(true);
  const stale = view('actions');
  assert.notEqual(getFloorState().settings, null, 'a stale view retains its settings report');
  assert.equal(indexStatus(stale, 'Floor actions'), 'Optional');
  assert.doesNotMatch(stale, /class="settings-view"|>Turn on<|>Turn off</, 'the guide waits for a confirmed snapshot before editing retained values');
  noteTransport('close');
  const disconnected = view('actions');
  assert.equal(indexStatus(disconnected, 'Floor actions'), 'Optional');
  assert.match(disconnected, /<p class="guide-status" role="status">Optional<\/p>/);
  assert.doesNotMatch(disconnected, /class="settings-view"|>Turn on<|>Turn off</, 'old switch values cannot be edited while disconnected');
  assert.equal(status('actions', true), 'Optional');
});

test('the guide shares Settings confirmations and keeps distinct headings', async () => {
  snapshot();
  const edit = await server.ssrLoadModule('/src/renderer/settings/edit.ts');
  edit.resetSettingsEdit();
  try {
    await edit.changeSetting('floor.terminal', 'on');
    assert.match(view('actions'), /aria-label="Turn on Attach terminals"/);
    assert.match(view('actions'), /It applies at once and does not end running work\./);
    assert.match(view('actions'), />Cancel<\/button>/);
    const html = view('actions') + renderToStaticMarkup(createElement(SettingsView,
      { floor: getFloorState(), onSetup() {} }));
    const ids = [...html.matchAll(/ id="([^"]+)"/g)].map((match) => match[1]);
    assert.equal(new Set(ids).size, ids.length);
    assert.match(html, /aria-labelledby="guide-switches"/);
    assert.match(html, /aria-labelledby="settings-switches"/);
  } finally { edit.resetSettingsEdit(); }
});

test('Close is remembered in this browser and a storage refusal still allows this visit', () => {
  const values = new Map();
  const storage = () => ({ getItem: (key) => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) });
  assert.equal(readGuideClosed(storage), false);
  rememberGuideClosed(storage);
  assert.equal(readGuideClosed(storage), true);
  assert.deepEqual([...values.entries()], [['dudes.floor.first-run-guide.closed', 'true']]);
  const refused = () => { throw new Error('storage refused'); };
  rememberGuideClosed(refused);
  assert.equal(readGuideClosed(refused), false);
  values.set('dudes.floor.first-run-guide.closed', 'other');
  assert.equal(readGuideClosed(storage), false);
});

test('a disconnected snapshot never claims that a setup step is done', () => {
  snapshot({ found: true, settings: { ...SETTINGS, workspaces: [WORKSPACE],
    switches: { ...SETTINGS.switches, control: true, terminal: true, hire: true } }, onboarding: CATALOG,
  agents: [{ id: ID }] });
  const unseen = ['Complete', 'Complete', 'Complete', 'Optional'];
  assert.deepEqual(guideSteps(getFloorState(), false).map((step) => step.status), unseen);
  assert.deepEqual(guideSteps(getFloorState(), true).map((step) => step.status), ['Complete', 'Complete', 'Complete', 'Complete']);
  const waiting = ['Not done', 'Not done', 'Not done', 'Optional'];
  for (const unconfirmed of [{ ...getFloorState(), stale: true },
    { ...getFloorState(), snapshotEpoch: getFloorState().epoch - 1 }]) {
    assert.deepEqual(guideSteps(unconfirmed, true).map((step) => step.status), waiting);
  }
  noteTransport('close');
  assert.deepEqual(guideSteps(getFloorState(), true).map((step) => step.status), waiting);
});

test('the guide and Settings render together with distinct labelled headings', () => {
  snapshot({ settings: { ...SETTINGS, workspaces: [WORKSPACE] } });
  const html = view('workspaces') + renderToStaticMarkup(createElement(SettingsView,
    { floor: getFloorState(), onSetup() {} }));
  const ids = [...html.matchAll(/ id="([^"]+)"/g)].map((match) => match[1]);
  assert.equal(new Set(ids).size, ids.length, 'each real consumer owns a distinct heading id');
  assert.match(html, /aria-labelledby="settings-workspaces"/);
  assert.match(html, /aria-labelledby="guide-workspaces"/);
});

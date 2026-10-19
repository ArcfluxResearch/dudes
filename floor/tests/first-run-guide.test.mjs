// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';
import { guideSteps } from '../src/renderer/setup/guide.ts';
import { readGuideClosed, rememberGuideClosed } from '../src/renderer/view.ts';

const ID = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const SETTINGS = {
  switches: { answer: false, control: false, recovery: false, goal: false, retire: false, terminal: false, hire: false, git: false, renew: false },
  budget: { floor_usd_per_day_micros: null, per_turn_usd_micros: '0', per_turn_tokens: '0' },
  providers: ['claude'], workspaces: [], about: { daemon_version: '0.12.3', browser_wire_version: 3 },
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
const done = (key) => status(key) === 'Done';
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
  onStep() {}, onClose() {}, onHire() {} }));

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
  assert.match(view('engines'), /Found on the daemon/);
  assert.match(view('engines'), /offers Claude, and Codex and Pi when the daemon turns them on/);
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
  assert.match(view('workspaces'), /No enrolled workspaces/);
  assert.match(view('workspaces'), /dudes workspace add &lt;path&gt;/);
  assert.match(view('workspaces'), />Copy<\/button>/);
  applyServerFrame('{"type":"floor.resync"}', epoch);
  assert.equal(done('workspaces'), false);
  snapshot({ settings: { ...SETTINGS, workspaces: [WORKSPACE] } });
  assert.equal(done('workspaces'), true);
  assert.match(view('workspaces'), /&lt;Parser&gt;/);
  assert.doesNotMatch(view('workspaces'), /<Parser>/);
  snapshot();
  assert.equal(done('workspaces'), false);
});

test('Floor actions reads the real switches, the Hire switch and the Hire offer', () => {
  snapshot();
  const html = view('actions');
  assert.match(html, /<dt>Hire switch<\/dt><dd>Off<\/dd>/);
  assert.match(html, /<p>Hire is off\. Turn it on with dudes config set floor\.hire on, then restart the daemon\.<\/p>/);
  assert.match(html, /<code>dudes config set floor.hire on<\/code>/);
  assert.match(html, /dudes config set floor.control on/);
  assert.match(html, /dudes config set floor.terminal on/);
  assert.doesNotMatch(html, /budget\.|positive ceilings/, 'Hire no longer needs a budget');
  snapshot({ settings: { ...SETTINGS, switches: { ...SETTINGS.switches, hire: true } } });
  assert.match(view('actions'), /<p>Hire needs a workspace enrolled with dudes workspace add, at least one engine/);
  assert.match(view('actions'), /<code>dudes config set floor.hire off<\/code>/);
  snapshot({ settings: { ...SETTINGS, switches: { ...SETTINGS.switches, control: true, terminal: true, hire: true } },
    onboarding: CATALOG });
  assert.match(view('actions'), /Hire<\/dt><dd>Available/);
});

test('ACTIONS_GATED_ON_SWITCHES mutant: Floor actions is Optional until seen, never Not done, whatever is on', () => {
  for (const control of [false, true]) {
    for (const terminal of [false, true]) {
      for (const onboarding of [null, CATALOG]) {
        snapshot({ settings: { ...SETTINGS, switches: { ...SETTINGS.switches, control, terminal, hire: true } }, onboarding });
        const at = `control ${control}, terminal ${terminal}, Hire ${onboarding === null ? 'unavailable' : 'available'}`;
        assert.equal(indexStatus(view('hire'), 'Floor actions'), 'Optional', at);
        assert.equal(indexStatus(view('actions'), 'Floor actions'), 'Done', at);
        assert.match(view('actions'), /<p class="guide-status" role="status">Done<\/p>/, at);
        assert.equal(status('actions'), 'Optional', at);
        assert.equal(status('actions', true), 'Done', at);
      }
    }
  }
});

test('ACTIONS_SEEN_UNREPORTED mutant: Floor actions is not seen until the switches are shown on a confirmed snapshot', () => {
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [] } }), epoch);
  const unreported = view('actions');
  assert.match(unreported, /Settings are not reported/);
  assert.equal(indexStatus(unreported, 'Floor actions'), 'Optional');
  snapshot();
  noteTransport('close');
  const disconnected = view('actions');
  assert.equal(indexStatus(disconnected, 'Floor actions'), 'Optional');
  assert.match(disconnected, /<p class="guide-status" role="status">Optional<\/p>/);
  assert.equal(status('actions', true), 'Optional');
});

test('each Floor actions switch shows its state beside its own dudes config command', () => {
  snapshot({ settings: { ...SETTINGS, switches: { ...SETTINGS.switches, control: true } } });
  const html = view('actions');
  const row = (label, state, command) => new RegExp(`<dt>${label}</dt><dd>${state}</dd><dd class="guide-switch-command">`
    + `<p class="guide-command"><code>${command}</code>`);
  assert.match(html, row('Hire switch', 'Off', 'dudes config set floor.hire on'));
  assert.match(html, row('Control', 'On', 'dudes config set floor.control off floor.goal off floor.retire off'));
  assert.match(html, row('Terminal', 'Off', 'dudes config set floor.terminal on'));
  assert.match(html, /<p>This step is done once you have seen it, whichever switches are on\.<\/p>/);
  assert.doesNotMatch(html, /done when Hire is available/);
});

test('First hire stays not done until the live projection contains an agent and opens the existing wizard', () => {
  snapshot();
  assert.equal(done('hire'), false);
  assert.match(view('hire'), /<button type="button" disabled="">Open Hire wizard<\/button>/);
  snapshot({ onboarding: CATALOG });
  assert.equal(done('hire'), false);
  assert.match(view('hire'), /<button type="button">Open Hire wizard<\/button>/);
  snapshot({ agents: [{ id: ID }] });
  assert.equal(done('hire'), true);
  assert.match(view('hire'), /An agent is on the Floor/);
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
  const unseen = ['Done', 'Done', 'Done', 'Optional', 'Done'];
  assert.deepEqual(guideSteps(getFloorState(), false).map((step) => step.status), unseen);
  assert.deepEqual(guideSteps(getFloorState(), true).map((step) => step.status), ['Done', 'Done', 'Done', 'Done', 'Done']);
  const waiting = ['Not done', 'Not done', 'Not done', 'Optional', 'Not done'];
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
